//! System-resource monitoring during a load test.
//!
//! Latency and throughput say *what* happened during a run; resource usage
//! says *why*. This module records, on the same time axis as the request
//! timeline:
//!
//! - the load generator itself (`/proc/self`), so a saturated Flux is never
//!   mistaken for a slow target;
//! - the host Flux runs on (`/proc/stat`, `/proc/meminfo`), total and per core;
//! - target containers (cAdvisor) and hosts (node_exporter), by scraping
//!   their `/metrics` endpoints directly while the test runs.
//!
//! Monitoring is built to stay out of the way of the load it observes: one
//! lightweight task samples every source per interval, scrape bodies are
//! size-capped, only the metric families Flux uses are parsed, and parsing
//! runs on the blocking pool rather than the runtime threads driving the
//! load. What each scrape cost is itself recorded (`scrape_ms`,
//! `scrape_kib`) so the overhead is visible in every report.
//!
//! Monitoring never fails a run: a source that cannot be reached is reported
//! under `errors` in the resource report and the load test carries on.

pub mod config;
pub mod procfs;
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
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

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

/// Average scrape time, as a share of the interval, worth flagging: the
/// exporter is working hard on the machine under test.
const SCRAPE_TIME_WARN_SHARE: f64 = 0.25;

/// Average scrape size worth flagging: cAdvisor is exporting far more than
/// Flux reads.
const SCRAPE_SIZE_WARN_KIB: f64 = 4.0 * 1024.0;

/// Largest scrape body accepted. Past this the scrape is dropped for that
/// tick rather than letting monitoring hold that much memory.
const MAX_SCRAPE_BYTES: usize = 32 * 1024 * 1024;

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
    /// `self`, `host` or `scrape`.
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
    /// `loadgen`, `host`, `container` or `node`.
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

        let mut secrets: Vec<String> = Vec::new();
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
        }))
    }

    /// Stop sampling and build the report for a run summarised by `summary`.
    pub async fn finish(mut self, summary: &MetricsSummary) -> ResourceReport {
        self.stop.cancel();
        let output = match self.task.take() {
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
                 narrow the scrape selectors",
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
            summary_from_secs: measured_from - run_start,
            summary_to_secs: run_end - run_start,
            groups: self.groups(),
            series,
            derived: DerivedResources::default(),
            per_stage,
            warnings: Vec::new(),
            errors,
        };
        report.derived = derive(&report, summary);
        report.warnings = warnings(&report);
        report
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

/// Fetch a scrape body, refusing anything larger than `MAX_SCRAPE_BYTES`.
async fn fetch(client: &reqwest::Client, url: &str, token: Option<&str>) -> anyhow::Result<String> {
    let mut request = client.get(url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    // reqwest's messages name the URL; the target is named by the caller.
    let mut response = request
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(reqwest::Error::without_url)?;
    let too_large = || {
        anyhow::anyhow!(
            "response is larger than {} MiB; narrow what the exporter publishes",
            MAX_SCRAPE_BYTES / (1024 * 1024)
        )
    };
    if response
        .content_length()
        .is_some_and(|length| length > MAX_SCRAPE_BYTES as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(reqwest::Error::without_url)?
    {
        if body.len() + chunk.len() > MAX_SCRAPE_BYTES {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
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

    let responses = join_all(scrape_targets.iter().map(|(target, token)| async move {
        let started = Instant::now();
        let body = fetch(client, target.config.url.trim(), token.as_deref()).await;
        (body, started.elapsed())
    }))
    .await;

    for ((target, _), (body, fetch_time)) in scrape_targets.iter_mut().zip(responses) {
        let text = match body {
            Ok(text) => text,
            Err(error) => {
                output.error(format!(
                    "scraping '{}' failed: {error:#}",
                    target.config.name
                ));
                continue;
            }
        };
        let time = now_secs();
        let size_kib = text.len() as f64 / 1024.0;

        // Parsing costs CPU in proportion to the scrape size, so it runs on
        // the blocking pool instead of the runtime threads driving the load.
        let families = scrape::families(target.config.kind);
        let matchers = target.matchers().to_vec();
        let parse_started = Instant::now();
        let samples =
            tokio::task::spawn_blocking(move || scrape::extract(&text, families, &matchers)).await;
        let cost_ms = (fetch_time + parse_started.elapsed()).as_secs_f64() * 1000.0;

        match samples {
            Ok(samples) => {
                if let Some(note) = target.observe_samples(time, &samples, &mut output.store) {
                    output.error(note);
                }
            }
            Err(error) => output.error(format!(
                "parsing the scrape of '{}' failed: {error}",
                target.config.name
            )),
        }
        let name = target.config.name.as_str();
        for (metric, unit, value) in [
            ("scrape_ms", "ms", cost_ms),
            ("scrape_kib", "KiB", size_kib),
        ] {
            output.store.record(
                series::SeriesKey::new(name, metric),
                "scrape",
                unit,
                SeriesKind::Gauge,
                time,
                value,
            );
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

    for group in report
        .groups
        .iter()
        .filter(|group| group.source == "scrape")
    {
        let name = &group.name;
        if let Some(cost) = summary_of(report, &format!("{name}.scrape_ms")) {
            let budget_ms = report.interval_secs * 1000.0 * SCRAPE_TIME_WARN_SHARE;
            if cost.avg >= budget_ms {
                warnings.push(format!(
                    "Scraping '{name}' took {:.0} ms on average, over a quarter of the {:.0}s \
                     interval; the exporter is working hard on the machine under test. Raise \
                     monitoring.interval or trim what the exporter publishes.",
                    cost.avg, report.interval_secs
                ));
            }
        }
        if let Some(size) = summary_of(report, &format!("{name}.scrape_kib")) {
            if size.avg >= SCRAPE_SIZE_WARN_KIB {
                warnings.push(format!(
                    "Each scrape of '{name}' was {:.1} MiB on average, mostly metrics Flux does \
                     not read. For cAdvisor, use --docker_only, --store_container_labels=false \
                     and --enable_metrics=cpu,memory,network,diskIO,oom_event,process.",
                    size.avg / 1024.0
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
    use config::{ScrapeTargetConfig, TargetKind};
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

    fn scrape_target(
        name: &str,
        kind: TargetKind,
        url: String,
        selector: &str,
    ) -> ScrapeTargetConfig {
        ScrapeTargetConfig {
            name: name.to_string(),
            kind,
            url,
            selector: selector.to_string(),
            per_core: true,
            bearer_token_env: None,
        }
    }

    #[test]
    fn test_missing_token_variable_fails_before_the_run() {
        let mut target = scrape_target(
            "api",
            TargetKind::Cadvisor,
            "http://127.0.0.1:1/metrics".to_string(),
            "",
        );
        target.bearer_token_env = Some("FLUX_TEST_TOKEN_THAT_IS_NOT_SET".to_string());
        let config = MonitoringConfig {
            scrape: vec![target],
            ..Default::default()
        };
        let error = ResourceMonitor::start(&config).err().unwrap().to_string();
        assert!(error.contains("FLUX_TEST_TOKEN_THAT_IS_NOT_SET"));
    }

    /// Minimal HTTP server standing in for cAdvisor (`/cadvisor`) and
    /// node_exporter (`/node`). Counters grow by one per scrape; requests for
    /// `/huge` advertise a body over the size cap.
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
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let n = scrapes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let body = match path.as_str() {
                        "/cadvisor" => format!(
                            "# HELP container_cpu_usage_seconds_total cpu\n\
                             container_cpu_usage_seconds_total{{name=\"api\",cpu=\"total\"}} {n}\n\
                             container_cpu_usage_seconds_total{{name=\"other\",cpu=\"total\"}} 9999\n\
                             container_memory_usage_bytes{{name=\"api\"}} 1\n\
                             container_memory_working_set_bytes{{name=\"api\"}} 268435456\n"
                        ),
                        "/node" => format!(
                            "node_cpu_seconds_total{{cpu=\"0\",mode=\"idle\"}} {n}\n\
                             node_cpu_seconds_total{{cpu=\"0\",mode=\"user\"}} {n}\n\
                             node_memory_MemTotal_bytes 1000\n\
                             node_memory_MemAvailable_bytes 250\n"
                        ),
                        _ => String::new(),
                    };
                    let length = if path == "/huge" {
                        MAX_SCRAPE_BYTES + 1
                    } else {
                        body.len()
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n{body}"
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn test_monitor_end_to_end_with_cadvisor_and_node_exporter() {
        let base = mock_exporters().await;
        let config = MonitoringConfig {
            interval: "1s".to_string(),
            scrape: vec![
                scrape_target(
                    "api",
                    TargetKind::Cadvisor,
                    format!("{base}/cadvisor"),
                    r#"name="api""#,
                ),
                scrape_target("node", TargetKind::Node, format!("{base}/node"), ""),
            ],
            ..Default::default()
        };
        config.validate().unwrap();

        let collector = MetricsCollector::new();
        let monitor = ResourceMonitor::start(&config).unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(2_300)).await;
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
            .expect("cAdvisor container");
        assert_eq!(api.memory_working_set_mib.as_ref().unwrap().max, 256.0);
        assert!(api.cpu_cores.is_some());
        assert!(api.cpu_ms_per_request.is_some());

        let node = report
            .derived
            .hosts
            .iter()
            .find(|host| host.group == "node")
            .expect("node_exporter host");
        assert_eq!(node.memory_used_percent.as_ref().unwrap().max, 75.0);
        assert!(report.series(r#"node.cpu_percent{cpu="0"}"#).is_some());

        // The cost of every scrape is part of the report.
        for id in ["api.scrape_ms", "api.scrape_kib", "node.scrape_ms"] {
            assert!(report.series(id).is_some(), "missing {id}");
        }

        if cfg!(target_os = "linux") {
            assert!(report.derived.loadgen.is_some());
            assert!(report.series("host.cpu_percent").is_some());
        }
        let group_names: Vec<&str> = report.groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(group_names, vec!["flux", "host", "api", "node"]);
    }

    #[tokio::test]
    async fn test_unreachable_and_oversized_sources_are_reported_not_fatal() {
        let base = mock_exporters().await;
        let config = MonitoringConfig {
            interval: "1s".to_string(),
            self_metrics: false,
            host: false,
            scrape: vec![
                scrape_target(
                    "node",
                    TargetKind::Node,
                    "http://127.0.0.1:1/metrics".to_string(),
                    "",
                ),
                scrape_target("big", TargetKind::Cadvisor, format!("{base}/huge"), ""),
            ],
            ..Default::default()
        };
        let collector = MetricsCollector::new();
        let monitor = ResourceMonitor::start(&config).unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let report = monitor.finish(&collector.generate_summary()).await;

        assert!(report.series.is_empty());
        let errors = report.errors.join("\n");
        assert!(errors.contains("scraping 'node' failed"), "{errors}");
        assert!(
            errors.contains("scraping 'big' failed") && errors.contains("larger than 32 MiB"),
            "{errors}"
        );
    }

    #[test]
    fn test_warns_when_scraping_is_expensive() {
        let mut report = ResourceReport {
            interval_secs: 2.0,
            groups: vec![ResourceGroup {
                name: "api".to_string(),
                kind: "container".to_string(),
                source: "scrape".to_string(),
            }],
            series: vec![
                series(
                    "api",
                    "scrape_ms",
                    &[],
                    summary_of_series(800.0, 900.0, 1_000.0),
                ),
                series(
                    "api",
                    "scrape_kib",
                    &[],
                    summary_of_series(8_192.0, 8_192.0, 8_192.0),
                ),
            ],
            ..Default::default()
        };
        report.derived = derive(&report, &MetricsCollector::new().generate_summary());
        let joined = warnings(&report).join("\n");
        assert!(joined.contains("Scraping 'api' took 800 ms"), "{joined}");
        assert!(joined.contains("8.0 MiB"), "{joined}");
    }
}
