//! System-resource monitoring during a load test.
//!
//! Latency and throughput say *what* happened during a run; resource usage
//! says *why*. This module records, on the same time axis as the request
//! timeline:
//!
//! - the load generator itself (`/proc/self`), so a saturated Flux is never
//!   mistaken for a slow target;
//! - the host Flux runs on (`/proc/stat`, `/proc/meminfo`), total and per core;
//! - target containers and hosts, either from a Prometheus server queried
//!   after the run (cAdvisor and node_exporter presets plus custom PromQL) or
//!   by scraping `/metrics` endpoints directly while the test runs.
//!
//! Monitoring never fails a run: a source that cannot be reached is reported
//! under `errors` in the resource report and the load test carries on.

pub mod config;
pub mod procfs;
pub mod promql;
pub mod scrape;
pub mod series;
pub mod view;

use crate::cancel::Cancellation;
use crate::metrics::MetricsSummary;
use crate::redact::Redactor;
use chrono::{DateTime, Utc};
use config::{MonitoringConfig, HOST_GROUP, LOADGEN_GROUP};
use futures::future::join_all;
use procfs::ProcSampler;
use scrape::ScrapeTarget;
use serde::{Deserialize, Serialize};
use series::{SeriesBuffer, SeriesKind, SeriesStore};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};

/// Load-generator CPU (p95, percent of the cores available to it) at which
/// results are flagged as possibly limited by Flux itself.
const LOADGEN_CPU_WARN_PERCENT: f64 = 85.0;

/// Container CPU (p95, percent of its limit) flagged as saturated.
const CONTAINER_CPU_WARN_PERCENT: f64 = 90.0;

/// CFS throttling (p95, percent of periods) flagged as significant.
const THROTTLE_WARN_PERCENT: f64 = 5.0;

/// Memory (peak, percent of limit) flagged as close to an OOM kill.
const MEMORY_WARN_PERCENT: f64 = 90.0;

/// Memory growth worth flagging as a possible leak, when the trend is steady.
const MEMORY_GROWTH_WARN_MIB_PER_MIN: f64 = 1.0;
const MEMORY_GROWTH_MIN_R2: f64 = 0.8;

/// A memory trend is only meaningful over a long enough window.
const MEMORY_GROWTH_MIN_WINDOW_SECS: f64 = 60.0;

/// One busy core while the rest idle points at a single-threaded bottleneck.
const HOT_CORE_WARN_PERCENT: f64 = 90.0;
const HOT_CORE_HOST_AVG_BELOW_PERCENT: f64 = 50.0;

/// CPU steal (p95) that suggests a noisy neighbour on a shared VM.
const STEAL_WARN_PERCENT: f64 = 10.0;

/// Summary statistics of one series over the measured window.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SeriesSummary {
    /// Observations summarised (after downsampling, a point can stand for
    /// several).
    pub samples: u32,
    pub min: f64,
    pub avg: f64,
    pub p95: f64,
    pub max: f64,
    pub first: f64,
    pub last: f64,
    /// Least-squares slope, in the series' unit per minute.
    #[serde(default)]
    pub trend_per_min: Option<f64>,
    /// How well a straight line fits the series (0 to 1); a high value means
    /// `trend_per_min` describes steady growth rather than noise.
    #[serde(default)]
    pub trend_r2: Option<f64>,
    /// For counters, the increase over the window (resets tolerated).
    #[serde(default)]
    pub increase: Option<f64>,
}

/// One resource time series.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceSeries {
    /// Stable identifier, e.g. `api.cpu_cores` or `host.cpu_percent{cpu="3"}`.
    pub id: String,
    pub group: String,
    pub metric: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// `self`, `host`, `prometheus` or `scrape`.
    pub source: String,
    pub unit: String,
    /// `gauge` or `counter`.
    pub kind: String,
    /// `[seconds since the run started, value]`, oldest first.
    pub points: Vec<[f64; 2]>,
    /// Statistics over the measured window (ramp-up excluded).
    pub summary: SeriesSummary,
}

/// A monitored thing: the load generator, a host or a container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceGroup {
    pub name: String,
    /// `loadgen`, `host`, `container`, `node` or `custom`.
    pub kind: String,
    pub source: String,
}

/// Headline numbers for the load generator.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoadgenResources {
    /// Percent of the cores available to Flux (its cgroup limit, or every
    /// online core).
    pub cpu_percent: Option<SeriesSummary>,
    pub cpu_cores: Option<SeriesSummary>,
    pub memory_rss_mib: Option<SeriesSummary>,
}

/// Headline numbers for a host (local `/proc` or node_exporter).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostResources {
    pub group: String,
    pub cpu_percent: Option<SeriesSummary>,
    /// Core with the highest average utilisation.
    pub hottest_core: Option<String>,
    pub hottest_core_avg_percent: Option<f64>,
    pub iowait_percent: Option<SeriesSummary>,
    pub steal_percent: Option<SeriesSummary>,
    pub memory_used_percent: Option<SeriesSummary>,
}

/// Headline numbers for a target container.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContainerResources {
    pub group: String,
    pub cpu_cores: Option<SeriesSummary>,
    pub cpu_percent_of_limit: Option<SeriesSummary>,
    pub cpu_throttled_percent: Option<SeriesSummary>,
    pub memory_working_set_mib: Option<SeriesSummary>,
    pub memory_percent_of_limit: Option<SeriesSummary>,
    /// Working-set growth over the measured window, when it was long enough
    /// to tell (at least a minute).
    pub memory_growth_mib_per_min: Option<f64>,
    pub memory_growth_r2: Option<f64>,
    /// OOM kills during the run.
    pub oom_events: Option<f64>,
    /// Average CPU time the container spent per request in the measured
    /// window. Only meaningful when this container served the load.
    pub cpu_ms_per_request: Option<f64>,
    /// Measured throughput per core the container used.
    pub requests_per_core: Option<f64>,
}

/// Numbers derived from the raw series, used by assertions and reports.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DerivedResources {
    pub loadgen: Option<LoadgenResources>,
    #[serde(default)]
    pub hosts: Vec<HostResources>,
    #[serde(default)]
    pub containers: Vec<ContainerResources>,
}

/// Average and peak of one series while a load-profile stage was active.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageSeriesStat {
    pub id: String,
    pub unit: String,
    pub avg: f64,
    pub max: f64,
}

/// Resource usage during one load-profile stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageResources {
    pub label: String,
    pub start_offset_secs: f64,
    pub end_offset_secs: f64,
    pub series: Vec<StageSeriesStat>,
}

/// Everything monitoring observed during a run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceReport {
    /// Sampling interval of local and scraped sources, in seconds.
    pub interval_secs: f64,
    /// Resolution of Prometheus series, when a Prometheus source was used.
    #[serde(default)]
    pub prometheus_step_secs: Option<f64>,
    /// Summaries cover `[summary_from_secs, summary_to_secs]` seconds since
    /// the run started: the measured window, with ramp-up excluded.
    pub summary_from_secs: f64,
    pub summary_to_secs: f64,
    pub groups: Vec<ResourceGroup>,
    pub series: Vec<ResourceSeries>,
    pub derived: DerivedResources,
    #[serde(default)]
    pub per_stage: Vec<StageResources>,
    /// Signs of saturation worth a look (load generator maxed out, CPU
    /// throttling, memory near its limit, a possible leak, ...).
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Sources that failed. The load test itself is unaffected.
    #[serde(default)]
    pub errors: Vec<String>,
    /// Queries that returned no data (an exporter that does not publish that
    /// metric, typically).
    #[serde(default)]
    pub missing: Vec<String>,
}

impl ResourceReport {
    /// Find a series by id.
    pub fn series(&self, id: &str) -> Option<&ResourceSeries> {
        self.series.iter().find(|series| series.id == id)
    }
}

/// What the sampling task accumulated.
#[derive(Debug, Default)]
struct SamplerOutput {
    store: SeriesStore,
    /// Distinct error messages and how often each occurred.
    errors: BTreeMap<String, usize>,
}

impl SamplerOutput {
    fn error(&mut self, message: String) {
        *self.errors.entry(message).or_default() += 1;
    }
}

/// Runs the configured sources for the duration of a test.
pub struct ResourceMonitor {
    config: MonitoringConfig,
    interval: Duration,
    stop: Cancellation,
    task: Option<JoinHandle<SamplerOutput>>,
    redactor: Redactor,
    prometheus_token: Option<String>,
}

fn read_token(variable: &Option<String>, field: &str) -> anyhow::Result<Option<String>> {
    let Some(name) = variable else {
        return Ok(None);
    };
    match std::env::var(name) {
        Ok(token) if !token.trim().is_empty() => Ok(Some(token.trim().to_string())),
        _ => anyhow::bail!("{field} names environment variable '{name}', which is not set"),
    }
}

fn now_secs() -> f64 {
    Utc::now().timestamp_millis() as f64 / 1000.0
}

fn to_secs(time: DateTime<Utc>) -> f64 {
    time.timestamp_millis() as f64 / 1000.0
}

impl ResourceMonitor {
    /// Start sampling. Returns `None` when monitoring is disabled.
    ///
    /// Fails only on problems that would otherwise surface after the test,
    /// such as a missing token, so they are fixed before any traffic is sent.
    pub fn start(config: &MonitoringConfig) -> anyhow::Result<Option<Self>> {
        if !config.is_active() {
            return Ok(None);
        }
        let interval = config.parse_interval()?;

        let prometheus_token = match &config.prometheus {
            Some(prometheus) => read_token(
                &prometheus.bearer_token_env,
                "monitoring.prometheus.bearer_token_env",
            )?,
            None => None,
        };
        let mut secrets: Vec<String> = prometheus_token.iter().cloned().collect();
        let mut scrape_targets = Vec::new();
        for target in &config.scrape {
            let token = read_token(
                &target.bearer_token_env,
                &format!("monitoring.scrape '{}' bearer_token_env", target.name),
            )?;
            secrets.extend(token.iter().cloned());
            scrape_targets.push((ScrapeTarget::new(target.clone())?, token));
        }
        let redactor = Redactor::new(secrets);

        // A busy host's cAdvisor `/metrics` can take a few seconds to render,
        // so scrapes get at least 5s; a slow scrape delays the next tick
        // rather than piling up requests.
        let client = reqwest::Client::builder()
            .timeout(interval.clamp(Duration::from_secs(5), Duration::from_secs(10)))
            .build()?;
        let mut proc_sampler = (config.self_metrics || config.host)
            .then(|| ProcSampler::new(config.self_metrics, config.host, config.per_core));

        let stop = Cancellation::new();
        let task_stop = stop.clone();
        let task = tokio::spawn(async move {
            let mut output = SamplerOutput::default();
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    biased;
                    _ = task_stop.cancelled() => break,
                    _ = ticker.tick() => {}
                }
                sample_once(
                    &client,
                    proc_sampler.as_mut(),
                    &mut scrape_targets,
                    &mut output,
                )
                .await;
            }
            // One last sample so the tail of the run is covered.
            sample_once(
                &client,
                proc_sampler.as_mut(),
                &mut scrape_targets,
                &mut output,
            )
            .await;
            output
        });

        Ok(Some(Self {
            config: config.clone(),
            interval,
            stop,
            task: Some(task),
            redactor,
            prometheus_token,
        }))
    }

    /// Stop sampling, query Prometheus for the test window if configured, and
    /// build the report for a run summarised by `summary`.
    pub async fn finish(mut self, summary: &MetricsSummary) -> ResourceReport {
        self.stop.cancel();
        let mut output = match self.task.take() {
            Some(task) => task.await.unwrap_or_else(|error| {
                let mut output = SamplerOutput::default();
                output.error(format!("resource sampler stopped unexpectedly: {error}"));
                output
            }),
            None => SamplerOutput::default(),
        };

        let run_start = to_secs(summary.start_time);
        let run_end = to_secs(summary.end_time);
        let measured_from = run_start + summary.ramp_up_secs;

        let mut missing = Vec::new();
        let mut prometheus_step = None;
        if let Some(prometheus) = &self.config.prometheus {
            prometheus_step = Some(
                self.query_prometheus(prometheus, run_start, run_end, &mut output, &mut missing)
                    .await,
            );
        }

        let mut errors: Vec<String> = output
            .errors
            .iter()
            .map(|(message, count)| {
                let message = self.redactor.redact(message);
                if *count > 1 {
                    format!("{message} (x{count})")
                } else {
                    message
                }
            })
            .collect();
        if output.store.dropped_series > 0 {
            errors.push(format!(
                "{} series were not kept because the limit of {} series was reached; \
                 narrow custom queries with labels or aggregation",
                output.store.dropped_series,
                series::MAX_SERIES
            ));
        }

        let buffers = output.store.into_buffers();
        let per_stage = stage_resources(&buffers, summary, run_start);
        let mut series: Vec<ResourceSeries> = buffers
            .into_iter()
            .map(|buffer| series::finish(buffer, run_start, measured_from, run_end))
            .collect();
        series.sort_by(|a, b| a.id.cmp(&b.id));

        let mut report = ResourceReport {
            interval_secs: self.interval.as_secs_f64(),
            prometheus_step_secs: prometheus_step,
            summary_from_secs: measured_from - run_start,
            summary_to_secs: run_end - run_start,
            groups: self.groups(),
            series,
            derived: DerivedResources::default(),
            per_stage,
            warnings: Vec::new(),
            errors,
            missing,
        };
        report.derived = derive(&report, summary);
        report.warnings = warnings(&report);
        report
    }

    async fn query_prometheus(
        &self,
        prometheus: &config::PrometheusSourceConfig,
        run_start: f64,
        run_end: f64,
        output: &mut SamplerOutput,
        missing: &mut Vec<String>,
    ) -> f64 {
        let configured_step = prometheus
            .step
            .as_deref()
            .and_then(|step| crate::config::parse_duration(step).ok())
            .unwrap_or(self.interval);
        let step = promql::query_step(configured_step, run_start, run_end);

        let delay =
            crate::config::parse_duration_allow_zero(&prometheus.query_delay).unwrap_or_default();
        if !delay.is_zero() {
            info!(
                "Waiting {:.0}s for Prometheus to scrape the end of the test",
                delay.as_secs_f64()
            );
            tokio::time::sleep(delay).await;
        }

        let timeout =
            crate::config::parse_duration(&prometheus.timeout).unwrap_or(Duration::from_secs(15));
        let client = match reqwest::Client::builder().timeout(timeout).build() {
            Ok(client) => client,
            Err(error) => {
                output.error(format!("cannot build Prometheus client: {error}"));
                return step;
            }
        };

        let queries = promql::plan(prometheus);
        let token = self.prometheus_token.as_deref();
        let results = join_all(queries.iter().map(|query| {
            promql::run_query(
                &client,
                &prometheus.url,
                token,
                query,
                run_start,
                run_end,
                step,
            )
        }))
        .await;

        let mut groups_with_data: BTreeMap<String, bool> = BTreeMap::new();
        for (query, result) in queries.iter().zip(results) {
            let label = format!("{}.{}", query.group, query.metric);
            let has_data = groups_with_data.entry(query.group.clone()).or_default();
            match result {
                Ok(buffers) if buffers.is_empty() => missing.push(label),
                Ok(buffers) => {
                    *has_data = true;
                    for buffer in buffers {
                        output.store.insert(buffer);
                    }
                }
                Err(error) => {
                    output.error(format!("Prometheus query for {label} failed: {error:#}"))
                }
            }
        }
        for target in &prometheus.targets {
            if groups_with_data.get(&target.name) == Some(&false) {
                output.error(format!(
                    "Prometheus returned no data for target '{}' (selector '{}') during the test \
                     window; check the selector and that Prometheus scrapes the exporter",
                    target.name, target.selector
                ));
            }
        }
        if !missing.is_empty() {
            warn!(
                "{} Prometheus queries returned no data; see 'missing' in the report",
                missing.len()
            );
        }
        step
    }

    fn groups(&self) -> Vec<ResourceGroup> {
        let mut groups = Vec::new();
        let group = |name: &str, kind: &str, source: &str| ResourceGroup {
            name: name.to_string(),
            kind: kind.to_string(),
            source: source.to_string(),
        };
        if self.config.self_metrics {
            groups.push(group(LOADGEN_GROUP, "loadgen", "self"));
        }
        if self.config.host {
            groups.push(group(HOST_GROUP, "host", "host"));
        }
        if let Some(prometheus) = &self.config.prometheus {
            for target in &prometheus.targets {
                groups.push(group(&target.name, target.kind.group_kind(), "prometheus"));
            }
            for query in &prometheus.queries {
                let name = query.group.clone().unwrap_or_else(|| "custom".to_string());
                if !groups.iter().any(|existing| existing.name == name) {
                    groups.push(group(&name, "custom", "prometheus"));
                }
            }
        }
        for target in &self.config.scrape {
            groups.push(group(&target.name, target.kind.group_kind(), "scrape"));
        }
        groups
    }
}

impl Drop for ResourceMonitor {
    fn drop(&mut self) {
        // A run that exits before `finish` must not leave the sampler behind.
        self.stop.cancel();
    }
}

/// Take one sample of every live source.
async fn sample_once(
    client: &reqwest::Client,
    proc_sampler: Option<&mut ProcSampler>,
    scrape_targets: &mut [(ScrapeTarget, Option<String>)],
    output: &mut SamplerOutput,
) {
    if let Some(sampler) = proc_sampler {
        if let Some(error) = sampler.sample(now_secs(), &mut output.store) {
            output.error(format!("local resource monitoring unavailable: {error}"));
        }
    }

    let responses = join_all(scrape_targets.iter().map(|(target, token)| {
        let mut request = client.get(target.config.url.trim());
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        async move {
            let response = request.send().await?.error_for_status()?;
            let text = response.text().await?;
            Ok::<(f64, String), reqwest::Error>((now_secs(), text))
        }
    }))
    .await;

    for ((target, _), response) in scrape_targets.iter_mut().zip(responses) {
        match response {
            Ok((time, text)) => {
                if let Some(note) = target.observe(time, &text, &mut output.store) {
                    output.error(note);
                }
            }
            Err(error) => output.error(format!(
                "scraping '{}' failed: {}",
                target.config.name,
                // reqwest's message names the URL; strip the query so only
                // the endpoint is reported.
                error.without_url()
            )),
        }
    }
}

/// Average and peak of each whole-group series while each stage ran.
fn stage_resources(
    buffers: &[SeriesBuffer],
    summary: &MetricsSummary,
    run_start: f64,
) -> Vec<StageResources> {
    summary
        .stages
        .iter()
        .map(|stage| {
            let from = run_start + stage.started_offset_secs;
            let to = from + stage.observed_duration_secs;
            let series = buffers
                .iter()
                .filter(|buffer| buffer.key.labels.is_empty() && buffer.kind == SeriesKind::Gauge)
                .filter_map(|buffer| {
                    let points: Vec<_> = buffer
                        .points
                        .iter()
                        .filter(|point| point.time >= from && point.time <= to)
                        .collect();
                    if points.is_empty() {
                        return None;
                    }
                    let weight: f64 = points.iter().map(|point| point.weight as f64).sum();
                    Some(StageSeriesStat {
                        id: buffer.key.id(),
                        unit: buffer.unit.clone(),
                        avg: points
                            .iter()
                            .map(|point| point.value * point.weight as f64)
                            .sum::<f64>()
                            / weight,
                        max: points
                            .iter()
                            .map(|point| point.max)
                            .fold(f64::MIN, f64::max),
                    })
                })
                .collect();
            StageResources {
                label: stage.label.clone(),
                start_offset_secs: stage.started_offset_secs,
                end_offset_secs: stage.started_offset_secs + stage.observed_duration_secs,
                series,
            }
        })
        .collect()
}

fn summary_of(report: &ResourceReport, id: &str) -> Option<SeriesSummary> {
    report
        .series(id)
        .filter(|series| series.summary.samples > 0)
        .map(|series| series.summary.clone())
}

/// Compute the headline numbers from the raw series.
fn derive(report: &ResourceReport, summary: &MetricsSummary) -> DerivedResources {
    let mut derived = DerivedResources::default();

    if report.groups.iter().any(|group| group.kind == "loadgen") {
        let loadgen = LoadgenResources {
            cpu_percent: summary_of(report, &format!("{LOADGEN_GROUP}.cpu_percent")),
            cpu_cores: summary_of(report, &format!("{LOADGEN_GROUP}.cpu_cores")),
            memory_rss_mib: summary_of(report, &format!("{LOADGEN_GROUP}.memory_rss_mib")),
        };
        if loadgen.cpu_percent.is_some() || loadgen.memory_rss_mib.is_some() {
            derived.loadgen = Some(loadgen);
        }
    }

    let window_secs = (report.summary_to_secs - report.summary_from_secs).max(0.0);
    for group in &report.groups {
        let name = group.name.as_str();
        let get = |metric: &str| summary_of(report, &format!("{name}.{metric}"));
        match group.kind.as_str() {
            "host" | "node" => {
                let hottest = report
                    .series
                    .iter()
                    .filter(|series| {
                        series.group == name
                            && series.metric == "cpu_percent"
                            && series.labels.contains_key("cpu")
                            && series.summary.samples > 0
                    })
                    .max_by(|a, b| a.summary.avg.total_cmp(&b.summary.avg));
                let host = HostResources {
                    group: name.to_string(),
                    cpu_percent: get("cpu_percent"),
                    hottest_core: hottest.and_then(|series| series.labels.get("cpu").cloned()),
                    hottest_core_avg_percent: hottest.map(|series| series.summary.avg),
                    iowait_percent: get("iowait_percent"),
                    steal_percent: get("steal_percent"),
                    memory_used_percent: get("memory_used_percent"),
                };
                if host.cpu_percent.is_some() || host.memory_used_percent.is_some() {
                    derived.hosts.push(host);
                }
            }
            "container" => {
                let cpu_cores = get("cpu_cores");
                let memory = get("memory_working_set_mib");
                let steady_window = window_secs >= MEMORY_GROWTH_MIN_WINDOW_SECS
                    && memory.as_ref().is_some_and(|m| m.samples >= 5);
                let cpu_avg = cpu_cores
                    .as_ref()
                    .map(|cpu| cpu.avg)
                    .filter(|avg| *avg > 0.0);
                let container = ContainerResources {
                    group: name.to_string(),
                    cpu_percent_of_limit: get("cpu_percent_of_limit"),
                    cpu_throttled_percent: get("cpu_throttled_percent"),
                    memory_percent_of_limit: get("memory_percent_of_limit"),
                    memory_growth_mib_per_min: memory
                        .as_ref()
                        .filter(|_| steady_window)
                        .and_then(|memory| memory.trend_per_min),
                    memory_growth_r2: memory
                        .as_ref()
                        .filter(|_| steady_window)
                        .and_then(|memory| memory.trend_r2),
                    oom_events: get("oom_events_total").and_then(|ooms| ooms.increase),
                    cpu_ms_per_request: cpu_avg.filter(|_| summary.measured_requests > 0).map(
                        |avg| {
                            avg * summary.measured_duration_secs * 1000.0
                                / summary.measured_requests as f64
                        },
                    ),
                    requests_per_core: cpu_avg.map(|avg| summary.throughput_rps / avg),
                    cpu_cores,
                    memory_working_set_mib: memory,
                };
                if container.cpu_cores.is_some() || container.memory_working_set_mib.is_some() {
                    derived.containers.push(container);
                }
            }
            _ => {}
        }
    }

    derived
}

/// Flag signs of saturation.
fn warnings(report: &ResourceReport) -> Vec<String> {
    let mut warnings = Vec::new();
    let derived = &report.derived;

    if let Some(cpu) = derived
        .loadgen
        .as_ref()
        .and_then(|loadgen| loadgen.cpu_percent.as_ref())
    {
        if cpu.p95 >= LOADGEN_CPU_WARN_PERCENT {
            warnings.push(format!(
                "Load generator CPU reached {:.0}% (p95) of the cores available to it; \
                 latency and throughput may be limited by Flux itself rather than the target. \
                 Give Flux more CPU or lower the load per instance.",
                cpu.p95
            ));
        }
    }

    for container in &derived.containers {
        let name = &container.group;
        if let Some(cpu) = &container.cpu_percent_of_limit {
            if cpu.p95 >= CONTAINER_CPU_WARN_PERCENT {
                warnings.push(format!(
                    "'{name}' CPU reached {:.0}% (p95) of its limit; it is CPU-bound.",
                    cpu.p95
                ));
            }
        }
        if let Some(throttled) = &container.cpu_throttled_percent {
            if throttled.p95 >= THROTTLE_WARN_PERCENT {
                warnings.push(format!(
                    "'{name}' was CPU-throttled in {:.1}% (p95) of scheduling periods; \
                     throttling adds latency even when average CPU looks low.",
                    throttled.p95
                ));
            }
        }
        if let Some(memory) = &container.memory_percent_of_limit {
            if memory.max >= MEMORY_WARN_PERCENT {
                warnings.push(format!(
                    "'{name}' memory peaked at {:.0}% of its limit; it is close to being OOM-killed.",
                    memory.max
                ));
            }
        }
        if let Some(ooms) = container.oom_events.filter(|ooms| *ooms > 0.0) {
            warnings.push(format!(
                "'{name}' was OOM-killed {ooms:.0} time(s) during the run."
            ));
        }
        if let (Some(growth), Some(r2)) = (
            container.memory_growth_mib_per_min,
            container.memory_growth_r2,
        ) {
            if growth >= MEMORY_GROWTH_WARN_MIB_PER_MIN && r2 >= MEMORY_GROWTH_MIN_R2 {
                warnings.push(format!(
                    "'{name}' memory grew steadily by {growth:.1} MiB/min (r²={r2:.2}); \
                     check for a leak with a longer soak test."
                ));
            }
        }
    }

    for host in &derived.hosts {
        let name = &host.group;
        if let (Some(core), Some(core_avg), Some(cpu)) = (
            &host.hottest_core,
            host.hottest_core_avg_percent,
            &host.cpu_percent,
        ) {
            if core_avg >= HOT_CORE_WARN_PERCENT && cpu.avg < HOT_CORE_HOST_AVG_BELOW_PERCENT {
                warnings.push(format!(
                    "'{name}' core {core} averaged {core_avg:.0}% while the host averaged {:.0}%; \
                     a single-threaded component may be the bottleneck.",
                    cpu.avg
                ));
            }
        }
        if let Some(steal) = &host.steal_percent {
            if steal.p95 >= STEAL_WARN_PERCENT {
                warnings.push(format!(
                    "'{name}' CPU steal reached {:.0}% (p95); a noisy neighbour on the \
                     hypervisor is taking CPU from this machine.",
                    steal.p95
                ));
            }
        }
    }

    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::MetricsCollector;
    use config::{PrometheusSourceConfig, PrometheusTargetConfig, ScrapeTargetConfig, TargetKind};
    use std::collections::BTreeMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn summary_of_series(avg: f64, p95: f64, max: f64) -> SeriesSummary {
        SeriesSummary {
            samples: 10,
            min: 0.0,
            avg,
            p95,
            max,
            first: avg,
            last: avg,
            trend_per_min: None,
            trend_r2: None,
            increase: None,
        }
    }

    fn series(
        id_group: &str,
        metric: &str,
        labels: &[(&str, &str)],
        summary: SeriesSummary,
    ) -> ResourceSeries {
        let labels: BTreeMap<String, String> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut key = series::SeriesKey::new(id_group, metric);
        key.labels = labels.clone();
        ResourceSeries {
            id: key.id(),
            group: id_group.to_string(),
            metric: metric.to_string(),
            labels,
            source: "test".to_string(),
            unit: "percent".to_string(),
            kind: "gauge".to_string(),
            points: vec![[0.0, summary.avg]],
            summary,
        }
    }

    fn group(name: &str, kind: &str) -> ResourceGroup {
        ResourceGroup {
            name: name.to_string(),
            kind: kind.to_string(),
            source: "test".to_string(),
        }
    }

    #[test]
    fn test_derive_and_warn_on_saturation() {
        let mut memory = summary_of_series(500.0, 600.0, 700.0);
        memory.trend_per_min = Some(12.0);
        memory.trend_r2 = Some(0.95);
        let mut ooms = summary_of_series(1.0, 1.0, 1.0);
        ooms.increase = Some(2.0);

        let mut report = ResourceReport {
            summary_from_secs: 0.0,
            summary_to_secs: 120.0,
            groups: vec![
                group("flux", "loadgen"),
                group("host", "host"),
                group("api", "container"),
            ],
            series: vec![
                series(
                    "flux",
                    "cpu_percent",
                    &[],
                    summary_of_series(80.0, 92.0, 99.0),
                ),
                series(
                    "host",
                    "cpu_percent",
                    &[],
                    summary_of_series(30.0, 40.0, 50.0),
                ),
                series(
                    "host",
                    "cpu_percent",
                    &[("cpu", "0")],
                    summary_of_series(95.0, 99.0, 100.0),
                ),
                series(
                    "host",
                    "cpu_percent",
                    &[("cpu", "1")],
                    summary_of_series(5.0, 9.0, 10.0),
                ),
                series("api", "cpu_cores", &[], summary_of_series(2.0, 2.5, 3.0)),
                series(
                    "api",
                    "cpu_percent_of_limit",
                    &[],
                    summary_of_series(85.0, 95.0, 100.0),
                ),
                series(
                    "api",
                    "cpu_throttled_percent",
                    &[],
                    summary_of_series(4.0, 12.0, 20.0),
                ),
                series("api", "memory_working_set_mib", &[], memory),
                series(
                    "api",
                    "memory_percent_of_limit",
                    &[],
                    summary_of_series(80.0, 90.0, 95.0),
                ),
                series("api", "oom_events_total", &[], ooms),
            ],
            ..Default::default()
        };

        let mut summary = MetricsCollector::new().generate_summary();
        summary.measured_requests = 1_000;
        summary.measured_duration_secs = 10.0;
        summary.throughput_rps = 100.0;

        report.derived = derive(&report, &summary);
        let container = &report.derived.containers[0];
        // 2 cores for 10s is 20,000 CPU-ms over 1,000 requests.
        assert_eq!(container.cpu_ms_per_request, Some(20.0));
        assert_eq!(container.requests_per_core, Some(50.0));
        assert_eq!(container.memory_growth_mib_per_min, Some(12.0));
        assert_eq!(container.oom_events, Some(2.0));
        let host = &report.derived.hosts[0];
        assert_eq!(host.hottest_core.as_deref(), Some("0"));
        assert_eq!(host.hottest_core_avg_percent, Some(95.0));
        assert!(report.derived.loadgen.is_some());

        let warnings = warnings(&report);
        let joined = warnings.join("\n");
        for needle in [
            "Load generator CPU",
            "CPU-bound",
            "CPU-throttled",
            "close to being OOM-killed",
            "OOM-killed 2 time",
            "grew steadily",
            "single-threaded",
        ] {
            assert!(
                joined.contains(needle),
                "missing warning '{needle}' in:\n{joined}"
            );
        }
    }

    #[test]
    fn test_short_window_has_no_memory_trend() {
        let mut memory = summary_of_series(500.0, 600.0, 700.0);
        memory.trend_per_min = Some(50.0);
        memory.trend_r2 = Some(1.0);
        let report = ResourceReport {
            summary_from_secs: 0.0,
            summary_to_secs: 20.0,
            groups: vec![group("api", "container")],
            series: vec![series("api", "memory_working_set_mib", &[], memory)],
            ..Default::default()
        };
        let summary = MetricsCollector::new().generate_summary();
        let derived = derive(&report, &summary);
        assert_eq!(derived.containers[0].memory_growth_mib_per_min, None);
    }

    #[test]
    fn test_disabled_monitoring_starts_nothing() {
        let config = MonitoringConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(ResourceMonitor::start(&config).unwrap().is_none());
    }

    #[test]
    fn test_missing_token_variable_fails_before_the_run() {
        let config = MonitoringConfig {
            prometheus: Some(PrometheusSourceConfig {
                url: "http://127.0.0.1:1".to_string(),
                bearer_token_env: Some("FLUX_TEST_TOKEN_THAT_IS_NOT_SET".to_string()),
                step: None,
                rate_window: "30s".to_string(),
                query_delay: "0s".to_string(),
                timeout: "1s".to_string(),
                targets: vec![],
                queries: vec![],
            }),
            ..Default::default()
        };
        let error = ResourceMonitor::start(&config).err().unwrap().to_string();
        assert!(error.contains("FLUX_TEST_TOKEN_THAT_IS_NOT_SET"));
    }

    /// Minimal HTTP server standing in for both cAdvisor (`/metrics`) and
    /// Prometheus (`/api/v1/query_range`).
    async fn mock_exporters() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let scrapes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let scrapes = std::sync::Arc::clone(&scrapes);
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 16 * 1024];
                    let read = stream.read(&mut buffer).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let target = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let url = reqwest::Url::parse(&format!("http://x{target}")).unwrap();
                    let body = if url.path() == "/metrics" {
                        // CPU usage grows by one CPU-second per scrape.
                        let n = scrapes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        format!(
                            "container_cpu_usage_seconds_total{{name=\"api\",cpu=\"total\"}} {}\n\
                             container_memory_working_set_bytes{{name=\"api\"}} 268435456\n",
                            n
                        )
                    } else {
                        let params: BTreeMap<String, String> =
                            url.query_pairs().into_owned().collect();
                        let query = params.get("query").cloned().unwrap_or_default();
                        let start: f64 = params["start"].parse().unwrap();
                        let end: f64 = params["end"].parse().unwrap();
                        let step: f64 = params["step"].parse().unwrap();
                        let value = if query.contains("container_cpu_usage_seconds_total")
                            && !query.contains("container_spec_cpu_quota")
                        {
                            Some("0.5")
                        } else if query.contains("container_memory_working_set_bytes")
                            && !query.contains("limit")
                        {
                            Some("256")
                        } else {
                            None
                        };
                        let result = match value {
                            Some(value) => {
                                let mut points = Vec::new();
                                let mut t = start;
                                while t <= end + 1e-9 {
                                    points.push(format!("[{t},\"{value}\"]"));
                                    t += step;
                                }
                                format!("[{{\"metric\":{{}},\"values\":[{}]}}]", points.join(","))
                            }
                            None => "[]".to_string(),
                        };
                        format!(
                            "{{\"status\":\"success\",\"data\":{{\"resultType\":\"matrix\",\"result\":{result}}}}}"
                        )
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn test_monitor_end_to_end_with_prometheus_and_scrape() {
        let base = mock_exporters().await;
        let config = MonitoringConfig {
            interval: "500ms".to_string(),
            prometheus: Some(PrometheusSourceConfig {
                url: base.clone(),
                bearer_token_env: None,
                step: Some("1s".to_string()),
                rate_window: "30s".to_string(),
                query_delay: "0s".to_string(),
                timeout: "5s".to_string(),
                targets: vec![PrometheusTargetConfig {
                    name: "api".to_string(),
                    kind: TargetKind::Cadvisor,
                    selector: r#"name="api""#.to_string(),
                    per_core: true,
                }],
                queries: vec![],
            }),
            scrape: vec![ScrapeTargetConfig {
                name: "api-direct".to_string(),
                kind: TargetKind::Cadvisor,
                url: format!("{base}/metrics"),
                selector: r#"name="api""#.to_string(),
                per_core: true,
                bearer_token_env: None,
            }],
            ..Default::default()
        };
        config.validate().unwrap();

        let collector = MetricsCollector::new();
        let monitor = ResourceMonitor::start(&config).unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(2_200)).await;
        let mut summary = collector.generate_summary();
        summary.measured_requests = 100;
        summary.measured_duration_secs = summary.total_duration_secs;
        summary.throughput_rps = 100.0 / summary.total_duration_secs;

        let report = monitor.finish(&summary).await;

        assert!(
            report.errors.is_empty(),
            "unexpected errors: {:?}",
            report.errors
        );
        let api = report
            .derived
            .containers
            .iter()
            .find(|container| container.group == "api")
            .expect("prometheus container");
        assert_eq!(api.cpu_cores.as_ref().unwrap().avg, 0.5);
        assert_eq!(api.memory_working_set_mib.as_ref().unwrap().max, 256.0);
        assert!(api.cpu_ms_per_request.is_some());
        // Every other preset query came back empty and is listed, not failed.
        assert!(report
            .missing
            .contains(&"api.cpu_throttled_percent".to_string()));
        assert_eq!(report.prometheus_step_secs, Some(1.0));

        let direct = report
            .derived
            .containers
            .iter()
            .find(|container| container.group == "api-direct")
            .expect("scraped container");
        assert_eq!(direct.memory_working_set_mib.as_ref().unwrap().max, 256.0);
        assert!(direct.cpu_cores.is_some());

        if cfg!(target_os = "linux") {
            assert!(report.derived.loadgen.is_some());
            assert!(report.series("host.cpu_percent").is_some());
        }
        let group_names: Vec<&str> = report.groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(group_names, vec!["flux", "host", "api", "api-direct"]);
    }

    #[tokio::test]
    async fn test_unreachable_sources_are_reported_not_fatal() {
        let config = MonitoringConfig {
            interval: "500ms".to_string(),
            self_metrics: false,
            host: false,
            prometheus: Some(PrometheusSourceConfig {
                url: "http://127.0.0.1:1".to_string(),
                bearer_token_env: None,
                step: None,
                rate_window: "30s".to_string(),
                query_delay: "0s".to_string(),
                timeout: "1s".to_string(),
                targets: vec![],
                queries: vec![config::CustomQueryConfig {
                    name: "up".to_string(),
                    query: "up".to_string(),
                    unit: None,
                    group: None,
                }],
            }),
            scrape: vec![ScrapeTargetConfig {
                name: "node".to_string(),
                kind: TargetKind::Node,
                url: "http://127.0.0.1:1/metrics".to_string(),
                selector: String::new(),
                per_core: true,
                bearer_token_env: None,
            }],
            ..Default::default()
        };
        let collector = MetricsCollector::new();
        let monitor = ResourceMonitor::start(&config).unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(700)).await;
        let report = monitor.finish(&collector.generate_summary()).await;

        assert!(report.series.is_empty());
        let errors = report.errors.join("\n");
        assert!(errors.contains("scraping 'node' failed"), "{errors}");
        assert!(
            errors.contains("Prometheus query for custom.up failed"),
            "{errors}"
        );
    }
}
