use chrono::{DateTime, Utc};
use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::Sender;
use tracing::warn;

/// Upper bound of the latency histogram, in milliseconds.
const HISTOGRAM_MAX_MS: u64 = 300_000;

/// Seconds of per-second history kept for the live dashboard trend charts.
const TIMELINE_WINDOW_SECS: usize = 120;

/// Number of recent requests, and recent failures, kept for the live view.
const RECENT_SAMPLE_SIZE: usize = 50;

/// Window used for the "current" request rate reported live, in seconds.
const CURRENT_RPS_WINDOW_SECS: f64 = 5.0;

/// Default number of raw request results kept in memory for reporting.
///
/// Aggregates are computed as results arrive, so this only caps the per-request
/// rows embedded in the JSON/HTML reports. It keeps a long or high-throughput
/// run from growing without bound and exhausting the container.
pub const DEFAULT_MAX_STORED_RESULTS: usize = 10_000;

/// Single request result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestResult {
    pub scenario_name: Option<String>,
    pub latency_ms: u64,
    pub status_code: u16,
    pub error: Option<String>,
    pub request_start_timestamp: DateTime<Utc>,
    pub request_end_timestamp: DateTime<Utc>,
}

/// Metrics collector for aggregating results
///
/// Aggregates (counters, histograms, per-scenario statistics) are updated on
/// every result and are therefore independent of how many raw results are
/// retained. Raw results are kept only for report rows and are capped by
/// `max_stored_results`.
#[derive(Debug)]
pub struct MetricsCollector {
    state: Arc<Mutex<CollectorState>>,
    start_time: DateTime<Utc>,
    max_stored_results: usize,
}

/// Everything guarded by a single lock, so recording a result takes one
/// uncontended critical section rather than several.
#[derive(Debug)]
struct CollectorState {
    /// Bounded sample of raw results, used for report rows.
    results: Vec<RequestResult>,
    /// Results not retained because the cap was reached.
    dropped_results: usize,
    aggregate: Aggregate,
    per_scenario: BTreeMap<String, Aggregate>,
    load_phase: LoadPhase,
    skipped_scenarios: BTreeMap<String, usize>,
    /// Response status distribution over every request, including `0` for
    /// requests that never produced a response.
    status_codes: BTreeMap<u16, usize>,
    /// Per-second history for the live trend charts, bounded to
    /// `TIMELINE_WINDOW_SECS` buckets.
    timeline: VecDeque<TimelineBucket>,
    /// Most recent requests, bounded to `RECENT_SAMPLE_SIZE`.
    recent: VecDeque<RecentResult>,
    /// Most recent failures, bounded to `RECENT_SAMPLE_SIZE`.
    recent_failures: VecDeque<RecentResult>,
    /// Streaming sink for raw rows, dropped by `close_result_stream` so the
    /// writer task can finish. Bounded so a writer that falls behind (a slow
    /// disk, backpressure) cannot buffer unboundedly many rows in memory; a
    /// row that does not fit is dropped and counted in `csv_dropped_rows`
    /// rather than queued.
    csv_sink: Option<Sender<RequestResult>>,
    /// Rows that could not be forwarded to the CSV sink because the channel
    /// was full or the writer had already stopped (for example, after a
    /// disk write failure).
    csv_dropped_rows: usize,
    /// Load profile in effect, when one is configured. `None` for a plain
    /// fixed-concurrency run.
    load_profile: Option<LoadProfileState>,
    /// Metadata for every load-profile stage seen so far, in order.
    stage_meta: Vec<StageMeta>,
    /// Aggregate observed while each stage was active, keyed by stage index.
    per_stage: BTreeMap<usize, Aggregate>,
    /// Index of the stage currently attributed to incoming results.
    current_stage: Option<usize>,
}

/// Load-profile bookkeeping: what kind of profile is running and, for
/// arrival-rate profiles, how pacing is keeping up with the target rate.
#[derive(Debug, Clone)]
struct LoadProfileState {
    kind: String,
    target_rps: Option<f64>,
    /// Pacing ticks attempted (arrival-rate only).
    scheduled_ticks: usize,
    /// Pacing ticks that found every worker busy and were skipped rather
    /// than queued, so concurrency stays bounded.
    saturated_ticks: usize,
}

/// Metadata describing one load-profile stage, recorded when it begins.
#[derive(Debug, Clone)]
struct StageMeta {
    label: String,
    target_concurrency: Option<usize>,
    target_rps: Option<f64>,
    planned_duration_secs: f64,
    /// When this stage actually began, used to compute how long it actually
    /// ran rather than assuming it got the full planned duration.
    started_at: DateTime<Utc>,
}

/// One second of the live timeline.
#[derive(Debug, Clone)]
struct TimelineBucket {
    /// Unix epoch second this bucket covers.
    second: i64,
    requests: usize,
    failed: usize,
    latency_sum: u128,
    max_latency_ms: u64,
}

/// Streaming statistics for a set of requests.
#[derive(Debug)]
struct Aggregate {
    total: usize,
    failed: usize,
    latency_sum: u128,
    min_latency_ms: u64,
    max_latency_ms: u64,
    histogram: Histogram<u64>,
}

/// Tracks when the full-concurrency phase began and how much work it did.
///
/// Everything before `started_at` was produced while workers were still being
/// started (ramp-up), so it is reported separately from the measured load.
#[derive(Debug, Default)]
struct LoadPhase {
    started_at: Option<DateTime<Utc>>,
    requests: usize,
}

/// Summary statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSummary {
    pub total_requests: usize,
    pub successful_requests: usize,
    pub failed_requests: usize,
    /// Wall-clock time of the whole run, including ramp-up.
    pub total_duration_secs: f64,
    /// Time spent starting workers before the measured load began.
    pub ramp_up_secs: f64,
    /// Measured load window: the run at full concurrency, excluding ramp-up.
    pub measured_duration_secs: f64,
    /// Requests issued during the measured load window.
    pub measured_requests: usize,
    /// Throughput over the measured load window (ramp-up excluded).
    pub throughput_rps: f64,
    pub min_latency_ms: u64,
    pub max_latency_ms: u64,
    pub mean_latency_ms: f64,
    pub p50_latency_ms: u64,
    pub p90_latency_ms: u64,
    pub p95_latency_ms: u64,
    pub p99_latency_ms: u64,
    pub error_rate: f64,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub per_scenario: BTreeMap<String, ScenarioMetricsSummary>,
    /// Response status distribution over every request; `0` counts requests
    /// that never produced a response. Reports written before this field
    /// existed simply omit it, and comparisons say so rather than reading the
    /// absence as "no responses".
    #[serde(default)]
    pub status_codes: BTreeMap<u16, usize>,
    /// Scenario steps skipped at runtime because a dependency failed, and how
    /// often. Configuration mistakes never reach this map: they fail the run
    /// before any request is sent.
    #[serde(default)]
    pub skipped_scenarios: BTreeMap<String, usize>,
    /// Raw per-request rows kept for the reports.
    #[serde(default)]
    pub retained_results: usize,
    /// Raw per-request rows discarded once the retention cap was reached. The
    /// statistics above still cover every request.
    #[serde(default)]
    pub dropped_results: usize,
    /// Rows that could not be written to the CSV output because the writer
    /// fell behind (or stopped, for example after a disk error) and the
    /// bounded queue between it and the collector was full. `0` when no CSV
    /// output is configured.
    #[serde(default)]
    pub csv_dropped_rows: usize,
    /// Configured-versus-achieved load, present only when `load_profile` is
    /// configured. Reports written before load profiles existed simply omit
    /// this field.
    #[serde(default)]
    pub load_profile: Option<LoadProfileSummary>,
    /// Per-stage breakdown for a `stages` or `arrival_rate` load profile, in
    /// order. Empty for a plain fixed-concurrency run.
    #[serde(default)]
    pub stages: Vec<StageSummary>,
}

/// Configured-versus-achieved load for the whole run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadProfileSummary {
    /// `"stages"` or `"arrival_rate"`.
    pub kind: String,
    /// Configured target requests per second (`arrival_rate` only).
    pub target_rps: Option<f64>,
    /// Measured requests per second over the run (`arrival_rate` only).
    pub achieved_rps: Option<f64>,
    /// Pacing ticks attempted (`arrival_rate` only).
    pub scheduled_ticks: usize,
    /// Pacing ticks skipped because every worker was busy, meaning the
    /// target rate could not be sustained at that moment.
    pub saturated_ticks: usize,
}

/// Metrics for one load-profile stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageSummary {
    pub label: String,
    /// Configured worker count for this stage (`stages` profile only).
    pub target_concurrency: Option<usize>,
    /// Configured target rate for this stage (`arrival_rate` profile only).
    pub target_rps: Option<f64>,
    pub planned_duration_secs: f64,
    /// Wall-clock time the stage was actually active: from when it began to
    /// when the next stage began, or to the end of the run for the last
    /// stage. Throughput is computed against this, not the planned duration,
    /// so a stage cut short by cancellation — or delayed by slow worker
    /// startup — reports the rate it actually achieved.
    #[serde(default)]
    pub observed_duration_secs: f64,
    pub metrics: ScenarioMetricsSummary,
}

/// Summary statistics for one named scenario step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioMetricsSummary {
    pub total_requests: usize,
    pub successful_requests: usize,
    pub failed_requests: usize,
    pub throughput_rps: f64,
    pub min_latency_ms: u64,
    pub max_latency_ms: u64,
    pub mean_latency_ms: f64,
    pub p50_latency_ms: u64,
    pub p90_latency_ms: u64,
    pub p95_latency_ms: u64,
    pub p99_latency_ms: u64,
    pub error_rate: f64,
}

/// Live metrics for terminal display
#[derive(Debug, Clone)]
pub struct LiveMetrics {
    pub current_rps: f64,
    pub avg_latency_ms: f64,
    pub error_count: usize,
    pub total_requests: usize,
}

/// Point-in-time view of a running test.
///
/// Everything here is bounded in size — aggregates, a fixed-length timeline and
/// per-scenario rows — so serving it repeatedly costs the same no matter how
/// long the run has been going or how many requests it has made.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveSnapshot {
    pub start_time: DateTime<Utc>,
    pub elapsed_secs: f64,
    pub total_requests: usize,
    pub successful_requests: usize,
    pub failed_requests: usize,
    pub error_rate: f64,
    /// Average rate over the whole run so far.
    pub throughput_rps: f64,
    /// Rate over the last few seconds, which is what the dashboard graphs.
    pub current_rps: f64,
    pub min_latency_ms: u64,
    pub max_latency_ms: u64,
    pub mean_latency_ms: f64,
    pub p50_latency_ms: u64,
    pub p90_latency_ms: u64,
    pub p95_latency_ms: u64,
    pub p99_latency_ms: u64,
    /// Response status distribution; `0` counts requests that never got a
    /// response (connection errors, timeouts).
    pub status_codes: BTreeMap<u16, usize>,
    pub per_scenario: BTreeMap<String, ScenarioMetricsSummary>,
    pub skipped_scenarios: BTreeMap<String, usize>,
    /// Per-second history, oldest first, for the trend charts.
    pub timeline: Vec<TimelinePoint>,
    pub retained_results: usize,
    pub dropped_results: usize,
}

/// One second of throughput and latency history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelinePoint {
    /// Unix epoch second this point covers.
    pub second: i64,
    pub requests: usize,
    pub failed: usize,
    pub mean_latency_ms: f64,
    pub max_latency_ms: u64,
}

/// A single recent request, without any request or response payload.
///
/// Only the fields below ever leave the collector: there is no header, cookie
/// or body data to leak. The `error` string can still quote a target URL, so
/// callers that expose it (the dashboard) redact it first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentResult {
    pub scenario_name: Option<String>,
    pub status_code: u16,
    pub latency_ms: u64,
    pub error: Option<String>,
    pub timestamp: DateTime<Utc>,
}

/// Bounded sample of recent requests and recent failures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentActivity {
    /// Maximum number of rows either list can hold.
    pub sample_size: usize,
    /// Most recent requests, newest first.
    pub results: Vec<RecentResult>,
    /// Most recent failures, newest first.
    pub failures: Vec<RecentResult>,
}

impl RecentResult {
    fn from_request(result: &RequestResult) -> Self {
        Self {
            scenario_name: result.scenario_name.clone(),
            status_code: result.status_code,
            latency_ms: result.latency_ms,
            error: result.error.clone(),
            timestamp: result.request_end_timestamp,
        }
    }
}

impl Aggregate {
    fn new() -> Self {
        Self {
            total: 0,
            failed: 0,
            latency_sum: 0,
            min_latency_ms: u64::MAX,
            max_latency_ms: 0,
            histogram: Histogram::<u64>::new_with_bounds(1, HISTOGRAM_MAX_MS, 3)
                .expect("valid histogram bounds"),
        }
    }

    fn observe(&mut self, result: &RequestResult) {
        let latency = result.latency_ms;

        self.total += 1;
        if result.error.is_some() {
            self.failed += 1;
        }
        self.latency_sum += latency as u128;
        self.min_latency_ms = self.min_latency_ms.min(latency);
        self.max_latency_ms = self.max_latency_ms.max(latency);

        let clamped = latency.min(HISTOGRAM_MAX_MS);
        if clamped < latency {
            warn!(
                "Latency {}ms exceeds histogram maximum; clamping to {}ms",
                latency, clamped
            );
        }
        if let Err(e) = self.histogram.record(clamped) {
            warn!("Histogram record error for latency {}ms: {}", clamped, e);
        }
    }

    fn mean_latency_ms(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.latency_sum as f64 / self.total as f64
        }
    }

    fn min(&self) -> u64 {
        if self.total == 0 {
            0
        } else {
            self.min_latency_ms
        }
    }

    fn error_rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.failed as f64 / self.total as f64) * 100.0
        }
    }

    fn summarize_scenario(&self, duration: f64) -> ScenarioMetricsSummary {
        ScenarioMetricsSummary {
            total_requests: self.total,
            successful_requests: self.total - self.failed,
            failed_requests: self.failed,
            throughput_rps: if duration > 0.0 {
                self.total as f64 / duration
            } else {
                0.0
            },
            min_latency_ms: self.min(),
            max_latency_ms: self.max_latency_ms,
            mean_latency_ms: self.mean_latency_ms(),
            p50_latency_ms: self.histogram.value_at_quantile(0.50),
            p90_latency_ms: self.histogram.value_at_quantile(0.90),
            p95_latency_ms: self.histogram.value_at_quantile(0.95),
            p99_latency_ms: self.histogram.value_at_quantile(0.99),
            error_rate: self.error_rate(),
        }
    }
}

impl CollectorState {
    /// Fold a result into the per-second timeline, dropping buckets older than
    /// the window so the history stays a fixed size for the whole run.
    fn observe_timeline(&mut self, result: &RequestResult) {
        let second = result.request_end_timestamp.timestamp();

        match self.timeline.back_mut() {
            // Results can arrive slightly out of order, so anything not newer
            // than the current bucket is folded into it rather than reordering
            // history.
            Some(bucket) if bucket.second >= second => {
                bucket.requests += 1;
                bucket.failed += usize::from(result.error.is_some());
                bucket.latency_sum += result.latency_ms as u128;
                bucket.max_latency_ms = bucket.max_latency_ms.max(result.latency_ms);
            }
            _ => {
                self.timeline.push_back(TimelineBucket {
                    second,
                    requests: 1,
                    failed: usize::from(result.error.is_some()),
                    latency_sum: result.latency_ms as u128,
                    max_latency_ms: result.latency_ms,
                });
                while self.timeline.len() > TIMELINE_WINDOW_SECS {
                    self.timeline.pop_front();
                }
            }
        }
    }

    /// Keep a bounded tail of recent requests and recent failures.
    fn observe_recent(&mut self, result: &RequestResult) {
        push_bounded(&mut self.recent, RecentResult::from_request(result));
        if result.error.is_some() {
            push_bounded(
                &mut self.recent_failures,
                RecentResult::from_request(result),
            );
        }
    }

    /// Requests per second over the last `CURRENT_RPS_WINDOW_SECS`.
    ///
    /// Idle seconds have no bucket, so the window length — not the number of
    /// buckets — is the divisor; a stalled test reports a falling rate rather
    /// than the last busy second forever.
    fn current_rps(&self, now: i64, elapsed_secs: f64) -> f64 {
        let window_start = now - CURRENT_RPS_WINDOW_SECS as i64;
        let requests: usize = self
            .timeline
            .iter()
            .filter(|bucket| bucket.second > window_start && bucket.second <= now)
            .map(|bucket| bucket.requests)
            .sum();
        let window = elapsed_secs.min(CURRENT_RPS_WINDOW_SECS);
        if window <= 0.0 {
            return 0.0;
        }
        requests as f64 / window
    }
}

fn push_bounded(buffer: &mut VecDeque<RecentResult>, result: RecentResult) {
    buffer.push_back(result);
    while buffer.len() > RECENT_SAMPLE_SIZE {
        buffer.pop_front();
    }
}

impl MetricsCollector {
    /// Create a collector that retains every raw result.
    pub fn new() -> Self {
        Self::with_retention(0, None)
    }

    /// Create a collector that retains at most `max_stored_results` raw
    /// results (`0` retains everything) and optionally forwards every result
    /// to a streaming sink, such as the CSV writer.
    pub fn with_retention(
        max_stored_results: usize,
        csv_sink: Option<Sender<RequestResult>>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(CollectorState {
                results: Vec::new(),
                dropped_results: 0,
                aggregate: Aggregate::new(),
                per_scenario: BTreeMap::new(),
                load_phase: LoadPhase::default(),
                skipped_scenarios: BTreeMap::new(),
                status_codes: BTreeMap::new(),
                timeline: VecDeque::new(),
                recent: VecDeque::new(),
                recent_failures: VecDeque::new(),
                csv_sink,
                csv_dropped_rows: 0,
                load_profile: None,
                stage_meta: Vec::new(),
                per_stage: BTreeMap::new(),
                current_stage: None,
            })),
            start_time: Utc::now(),
            max_stored_results,
        }
    }

    /// Stop forwarding results to the streaming sink.
    ///
    /// The collector holds the last sender, so the writer task only sees the
    /// channel close — and can flush and finish — once this is called.
    pub fn close_result_stream(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.csv_sink = None;
        }
    }

    /// Declare the load profile in effect for this run. Called once, before
    /// the first request, by a `stages` or `arrival_rate` executor path. A
    /// plain fixed-concurrency run never calls this, so its summary carries
    /// no `load_profile`.
    pub fn set_load_profile(&self, kind: &str, target_rps: Option<f64>) {
        if let Ok(mut state) = self.state.lock() {
            state.load_profile = Some(LoadProfileState {
                kind: kind.to_string(),
                target_rps,
                scheduled_ticks: 0,
                saturated_ticks: 0,
            });
        }
    }

    /// Record that a pacing tick fired for an arrival-rate profile.
    pub fn record_scheduled_tick(&self) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(profile) = &mut state.load_profile {
                profile.scheduled_ticks += 1;
            }
        }
    }

    /// Record that a pacing tick found every worker busy and was skipped
    /// rather than queued, so an overloaded target shows up as saturation
    /// instead of unbounded task creation.
    pub fn record_saturation(&self) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(profile) = &mut state.load_profile {
                profile.saturated_ticks += 1;
            }
        }
    }

    /// Begin a new load-profile stage: subsequent results are attributed to
    /// it until the next call. Stages are reported in the order they begin.
    pub fn begin_stage(
        &self,
        label: String,
        target_concurrency: Option<usize>,
        target_rps: Option<f64>,
        planned_duration_secs: f64,
    ) {
        if let Ok(mut state) = self.state.lock() {
            let index = state.stage_meta.len();
            state.stage_meta.push(StageMeta {
                label,
                target_concurrency,
                target_rps,
                planned_duration_secs,
                started_at: Utc::now(),
            });
            state.per_stage.insert(index, Aggregate::new());
            state.current_stage = Some(index);
        }
    }

    /// Record a scenario step that was skipped because the step it depends on
    /// failed during this iteration.
    pub fn record_skipped_scenario(&self, name: &str) {
        if let Ok(mut state) = self.state.lock() {
            *state.skipped_scenarios.entry(name.to_string()).or_default() += 1;
        }
    }

    /// Mark the end of ramp-up: every worker has started and the measured
    /// load window begins now. Repeated calls keep the first mark.
    pub fn mark_load_phase_started(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.load_phase.started_at.get_or_insert_with(Utc::now);
        }
    }

    /// Record a request result
    pub fn record(&self, result: RequestResult) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };

        // Streamed output receives every row, including rows that are not
        // retained in memory. The channel is bounded: a writer that falls
        // behind drops the row rather than buffering unboundedly many of
        // them, and the drop is counted so the report can say so.
        if let Some(sink) = &state.csv_sink {
            if sink.try_send(result.clone()).is_err() {
                state.csv_dropped_rows += 1;
            }
        }

        state.aggregate.observe(&result);
        if let Some(name) = &result.scenario_name {
            state
                .per_scenario
                .entry(name.clone())
                .or_insert_with(Aggregate::new)
                .observe(&result);
        }
        *state.status_codes.entry(result.status_code).or_default() += 1;
        if let Some(stage_index) = state.current_stage {
            state
                .per_stage
                .entry(stage_index)
                .or_insert_with(Aggregate::new)
                .observe(&result);
        }
        state.observe_timeline(&result);
        state.observe_recent(&result);
        if state
            .load_phase
            .started_at
            .is_some_and(|started| result.request_start_timestamp >= started)
        {
            state.load_phase.requests += 1;
        }

        if self.max_stored_results == 0 || state.results.len() < self.max_stored_results {
            state.results.push(result);
        } else {
            state.dropped_results += 1;
        }
    }

    /// Get current live metrics
    pub fn get_live_metrics(&self) -> LiveMetrics {
        let Ok(state) = self.state.lock() else {
            return LiveMetrics {
                current_rps: 0.0,
                avg_latency_ms: 0.0,
                error_count: 0,
                total_requests: 0,
            };
        };
        let total = state.aggregate.total;

        if total == 0 {
            return LiveMetrics {
                current_rps: 0.0,
                avg_latency_ms: 0.0,
                error_count: 0,
                total_requests: 0,
            };
        }

        let elapsed = self.elapsed_secs();
        let current_rps = if elapsed > 0.0 {
            total as f64 / elapsed
        } else {
            0.0
        };

        LiveMetrics {
            current_rps,
            avg_latency_ms: state.aggregate.mean_latency_ms(),
            error_count: state.aggregate.failed,
            total_requests: total,
        }
    }

    /// Take a bounded snapshot of the run so far.
    ///
    /// The work done under the lock is proportional to the number of scenarios
    /// and the fixed timeline window, never to the number of requests made, so
    /// polling this from the live dashboard cannot slow the load down as a run
    /// grows. A poisoned lock yields an empty snapshot instead of panicking:
    /// the dashboard is observability, and must never take the run down.
    pub fn snapshot(&self) -> LiveSnapshot {
        let elapsed = self.elapsed_secs();
        let Ok(state) = self.state.lock() else {
            return LiveSnapshot::empty(self.start_time, elapsed);
        };

        let total = state.aggregate.total;
        let failed = state.aggregate.failed;
        let throughput_rps = if elapsed > 0.0 {
            total as f64 / elapsed
        } else {
            0.0
        };

        LiveSnapshot {
            start_time: self.start_time,
            elapsed_secs: elapsed,
            total_requests: total,
            successful_requests: total - failed,
            failed_requests: failed,
            error_rate: state.aggregate.error_rate(),
            throughput_rps,
            current_rps: state.current_rps(Utc::now().timestamp(), elapsed),
            min_latency_ms: state.aggregate.min(),
            max_latency_ms: state.aggregate.max_latency_ms,
            mean_latency_ms: state.aggregate.mean_latency_ms(),
            p50_latency_ms: state.aggregate.histogram.value_at_quantile(0.50),
            p90_latency_ms: state.aggregate.histogram.value_at_quantile(0.90),
            p95_latency_ms: state.aggregate.histogram.value_at_quantile(0.95),
            p99_latency_ms: state.aggregate.histogram.value_at_quantile(0.99),
            status_codes: state.status_codes.clone(),
            per_scenario: state
                .per_scenario
                .iter()
                .map(|(name, aggregate)| (name.clone(), aggregate.summarize_scenario(elapsed)))
                .collect(),
            skipped_scenarios: state.skipped_scenarios.clone(),
            timeline: state
                .timeline
                .iter()
                .map(|bucket| TimelinePoint {
                    second: bucket.second,
                    requests: bucket.requests,
                    failed: bucket.failed,
                    mean_latency_ms: if bucket.requests == 0 {
                        0.0
                    } else {
                        bucket.latency_sum as f64 / bucket.requests as f64
                    },
                    max_latency_ms: bucket.max_latency_ms,
                })
                .collect(),
            retained_results: state.results.len(),
            dropped_results: state.dropped_results,
        }
    }

    /// Take the bounded sample of recent requests and failures, newest first.
    ///
    /// Rows carry no headers, cookies or payloads — only status, latency and
    /// the error string, which the caller redacts before exposing it.
    pub fn recent_activity(&self) -> RecentActivity {
        let Ok(state) = self.state.lock() else {
            return RecentActivity {
                sample_size: RECENT_SAMPLE_SIZE,
                results: Vec::new(),
                failures: Vec::new(),
            };
        };

        RecentActivity {
            sample_size: RECENT_SAMPLE_SIZE,
            results: state.recent.iter().rev().cloned().collect(),
            failures: state.recent_failures.iter().rev().cloned().collect(),
        }
    }

    /// Generate final summary
    ///
    /// Recovers a poisoned lock rather than panicking: this runs after the
    /// load test has finished, so a panic here would discard the results of
    /// a completed run — the single most expensive place for that to happen.
    /// The recovered state is whatever the last observation left behind,
    /// which is still the best summary available.
    pub fn generate_summary(&self) -> MetricsSummary {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let total = state.aggregate.total;
        let failed = state.aggregate.failed;
        let successful = total - failed;

        let end_time = Utc::now();
        let duration = self.elapsed_secs();

        // Ramp-up is warm-up time, so throughput is reported over the window in
        // which every worker was running.
        let load_started_at = state.load_phase.started_at;
        let ramp_up_secs = load_started_at
            .map(|started| {
                started
                    .signed_duration_since(self.start_time)
                    .num_milliseconds() as f64
                    / 1000.0
            })
            .unwrap_or(0.0)
            .max(0.0);
        let (measured_requests, measured_duration) = if load_started_at.is_some() {
            (
                state.load_phase.requests,
                (duration - ramp_up_secs).max(0.0),
            )
        } else {
            (total, duration)
        };

        let throughput = if measured_duration > 0.0 {
            measured_requests as f64 / measured_duration
        } else {
            0.0
        };

        let per_scenario = state
            .per_scenario
            .iter()
            .map(|(name, aggregate)| {
                (
                    name.clone(),
                    aggregate.summarize_scenario(measured_duration),
                )
            })
            .collect();

        let stages = state
            .stage_meta
            .iter()
            .enumerate()
            .map(|(index, meta)| {
                // A stage runs from when it began to when the next stage
                // began, or to the end of the run for the last stage — never
                // the planned duration, which a cancelled or slow-starting
                // stage would not actually have gotten.
                let stage_end = state
                    .stage_meta
                    .get(index + 1)
                    .map(|next| next.started_at)
                    .unwrap_or(end_time);
                let observed_duration_secs = stage_end
                    .signed_duration_since(meta.started_at)
                    .num_milliseconds() as f64
                    / 1000.0;
                let observed_duration_secs = observed_duration_secs.max(0.0);

                let metrics = state
                    .per_stage
                    .get(&index)
                    .map(|aggregate| aggregate.summarize_scenario(observed_duration_secs))
                    .unwrap_or_else(|| Aggregate::new().summarize_scenario(0.0));
                StageSummary {
                    label: meta.label.clone(),
                    target_concurrency: meta.target_concurrency,
                    target_rps: meta.target_rps,
                    planned_duration_secs: meta.planned_duration_secs,
                    observed_duration_secs,
                    metrics,
                }
            })
            .collect();

        let load_profile = state
            .load_profile
            .as_ref()
            .map(|profile| LoadProfileSummary {
                kind: profile.kind.clone(),
                target_rps: profile.target_rps,
                achieved_rps: profile.target_rps.map(|_| throughput),
                scheduled_ticks: profile.scheduled_ticks,
                saturated_ticks: profile.saturated_ticks,
            });

        MetricsSummary {
            total_requests: total,
            successful_requests: successful,
            failed_requests: failed,
            total_duration_secs: duration,
            ramp_up_secs,
            measured_duration_secs: measured_duration,
            measured_requests,
            throughput_rps: throughput,
            min_latency_ms: state.aggregate.min(),
            max_latency_ms: state.aggregate.max_latency_ms,
            mean_latency_ms: state.aggregate.mean_latency_ms(),
            p50_latency_ms: state.aggregate.histogram.value_at_quantile(0.50),
            p90_latency_ms: state.aggregate.histogram.value_at_quantile(0.90),
            p95_latency_ms: state.aggregate.histogram.value_at_quantile(0.95),
            p99_latency_ms: state.aggregate.histogram.value_at_quantile(0.99),
            error_rate: state.aggregate.error_rate(),
            start_time: self.start_time,
            end_time,
            per_scenario,
            status_codes: state.status_codes.clone(),
            skipped_scenarios: state.skipped_scenarios.clone(),
            retained_results: state.results.len(),
            dropped_results: state.dropped_results,
            csv_dropped_rows: state.csv_dropped_rows,
            load_profile,
            stages,
        }
    }

    /// Get the retained results for reporting.
    ///
    /// This is a bounded sample when a retention cap is configured; the summary
    /// reports how many rows were retained and dropped.
    pub fn get_results(&self) -> Vec<RequestResult> {
        self.state
            .lock()
            .map(|state| state.results.clone())
            .unwrap_or_default()
    }

    /// Render a current Prometheus text-format snapshot.
    pub fn render_prometheus(&self) -> String {
        let Ok(state) = self.state.lock() else {
            return String::new();
        };
        let total = state.aggregate.total;
        let failed = state.aggregate.failed;
        let successful = total - failed;
        let elapsed = self.elapsed_secs();
        let rps = if elapsed > 0.0 {
            total as f64 / elapsed
        } else {
            0.0
        };

        format!(
            concat!(
                "# HELP flux_requests_total Total requests completed by outcome.\n",
                "# TYPE flux_requests_total counter\n",
                "flux_requests_total{{status=\"success\"}} {}\n",
                "flux_requests_total{{status=\"failure\"}} {}\n",
                "# HELP flux_request_duration_ms Request latency quantiles in milliseconds.\n",
                "# TYPE flux_request_duration_ms gauge\n",
                "flux_request_duration_ms{{quantile=\"0.5\"}} {}\n",
                "flux_request_duration_ms{{quantile=\"0.9\"}} {}\n",
                "flux_request_duration_ms{{quantile=\"0.95\"}} {}\n",
                "flux_request_duration_ms{{quantile=\"0.99\"}} {}\n",
                "# HELP flux_rps Average requests completed per second for the current run.\n",
                "# TYPE flux_rps gauge\n",
                "flux_rps {:.6}\n"
            ),
            successful,
            failed,
            state.aggregate.histogram.value_at_quantile(0.50),
            state.aggregate.histogram.value_at_quantile(0.90),
            state.aggregate.histogram.value_at_quantile(0.95),
            state.aggregate.histogram.value_at_quantile(0.99),
            rps
        )
    }

    fn elapsed_secs(&self) -> f64 {
        Utc::now()
            .signed_duration_since(self.start_time)
            .num_milliseconds() as f64
            / 1000.0
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveSnapshot {
    /// A snapshot with no observations, used when the collector state cannot be
    /// read.
    fn empty(start_time: DateTime<Utc>, elapsed_secs: f64) -> Self {
        Self {
            start_time,
            elapsed_secs,
            total_requests: 0,
            successful_requests: 0,
            failed_requests: 0,
            error_rate: 0.0,
            throughput_rps: 0.0,
            current_rps: 0.0,
            min_latency_ms: 0,
            max_latency_ms: 0,
            mean_latency_ms: 0.0,
            p50_latency_ms: 0,
            p90_latency_ms: 0,
            p95_latency_ms: 0,
            p99_latency_ms: 0,
            status_codes: BTreeMap::new(),
            per_scenario: BTreeMap::new(),
            skipped_scenarios: BTreeMap::new(),
            timeline: Vec::new(),
            retained_results: 0,
            dropped_results: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(scenario: Option<&str>, latency_ms: u64, error: Option<&str>) -> RequestResult {
        RequestResult {
            scenario_name: scenario.map(ToString::to_string),
            latency_ms,
            status_code: if error.is_some() { 500 } else { 200 },
            error: error.map(ToString::to_string),
            request_start_timestamp: Utc::now(),
            request_end_timestamp: Utc::now(),
        }
    }

    #[test]
    fn test_metrics_collector() {
        let collector = MetricsCollector::new();

        collector.record(result(Some("test"), 100, None));

        let live = collector.get_live_metrics();
        assert_eq!(live.total_requests, 1);
        assert_eq!(live.error_count, 0);

        let summary = collector.generate_summary();
        assert_eq!(summary.total_requests, 1);
        assert_eq!(summary.successful_requests, 1);
        assert_eq!(summary.failed_requests, 0);
        let scenario = summary.per_scenario.get("test").unwrap();
        assert_eq!(scenario.total_requests, 1);
        assert_eq!(scenario.successful_requests, 1);
        assert_eq!(scenario.p95_latency_ms, 100);
    }

    #[test]
    fn test_metrics_error_status_counted_as_failure() {
        let collector = MetricsCollector::new();

        collector.record(result(None, 50, Some("HTTP 500")));

        let summary = collector.generate_summary();
        assert_eq!(summary.failed_requests, 1);
        assert_eq!(summary.successful_requests, 0);
        assert!(summary.per_scenario.is_empty());
    }

    #[test]
    fn test_metrics_large_latency_clamped() {
        let collector = MetricsCollector::new();

        // Record a latency larger than the histogram max (300_000 ms)
        collector.record(result(None, 400_000, None));

        let summary = collector.generate_summary();
        assert_eq!(summary.total_requests, 1);
    }

    #[test]
    fn test_retention_cap_bounds_memory_without_losing_statistics() {
        let collector = MetricsCollector::with_retention(10, None);

        for index in 0..1_000_u64 {
            let error = (index % 10 == 0).then_some("HTTP 500");
            collector.record(result(Some("step"), index + 1, error));
        }

        let summary = collector.generate_summary();

        // Only the cap is retained, but every request is still counted.
        assert_eq!(summary.retained_results, 10);
        assert_eq!(summary.dropped_results, 990);
        assert_eq!(collector.get_results().len(), 10);
        assert_eq!(summary.total_requests, 1_000);
        assert_eq!(summary.failed_requests, 100);
        assert_eq!(summary.successful_requests, 900);
        assert_eq!(summary.error_rate, 10.0);
        assert_eq!(summary.min_latency_ms, 1);
        assert_eq!(summary.max_latency_ms, 1_000);
        assert_eq!(summary.mean_latency_ms, 500.5);

        // Percentiles come from the histogram, which sees every request.
        assert!((490..=510).contains(&summary.p50_latency_ms));
        assert!((940..=960).contains(&summary.p95_latency_ms));

        let scenario = summary.per_scenario.get("step").unwrap();
        assert_eq!(scenario.total_requests, 1_000);
        assert_eq!(scenario.failed_requests, 100);
        assert_eq!(scenario.min_latency_ms, 1);
        assert_eq!(scenario.max_latency_ms, 1_000);
        assert_eq!(scenario.mean_latency_ms, 500.5);
    }

    #[test]
    fn test_unlimited_retention_keeps_every_result() {
        let collector = MetricsCollector::new();

        for index in 0..100 {
            collector.record(result(None, index + 1, None));
        }

        let summary = collector.generate_summary();
        assert_eq!(summary.retained_results, 100);
        assert_eq!(summary.dropped_results, 0);
        assert_eq!(collector.get_results().len(), 100);
    }

    #[tokio::test]
    async fn test_results_are_forwarded_to_the_streaming_sink() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let collector = MetricsCollector::with_retention(1, Some(sender));

        for index in 0..5 {
            collector.record(result(Some("step"), index + 1, None));
        }
        drop(collector);

        let mut streamed = Vec::new();
        while let Some(result) = receiver.recv().await {
            streamed.push(result);
        }

        // Every row reaches the stream even though only one is retained.
        assert_eq!(streamed.len(), 5);
        assert_eq!(streamed[4].latency_ms, 5);
    }

    #[tokio::test]
    async fn test_closing_the_stream_ends_it_while_the_collector_is_alive() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let collector = MetricsCollector::with_retention(0, Some(sender));

        collector.record(result(None, 10, None));
        collector.record(result(None, 20, None));
        collector.close_result_stream();
        // Recording after the stream is closed still updates the statistics.
        collector.record(result(None, 30, None));

        let mut streamed = 0;
        while receiver.recv().await.is_some() {
            streamed += 1;
        }

        // The channel closed even though the collector is still in use, which
        // is what lets the CSV writer flush and finish.
        assert_eq!(streamed, 2);
        assert_eq!(collector.generate_summary().total_requests, 3);
    }

    #[test]
    fn test_csv_sink_drops_and_counts_rows_when_the_writer_falls_behind() {
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        let collector = MetricsCollector::with_retention(0, Some(sender));

        // Nothing ever drains the channel, so only the first row fits.
        for index in 0..5 {
            collector.record(result(None, index + 1, None));
        }

        let summary = collector.generate_summary();
        assert_eq!(summary.total_requests, 5, "statistics still cover every request");
        assert_eq!(summary.csv_dropped_rows, 4);
    }

    #[test]
    fn test_live_metrics_survive_capped_retention() {
        let collector = MetricsCollector::with_retention(2, None);

        collector.record(result(None, 100, None));
        collector.record(result(None, 200, Some("HTTP 500")));
        collector.record(result(None, 300, None));

        let live = collector.get_live_metrics();
        assert_eq!(live.total_requests, 3);
        assert_eq!(live.error_count, 1);
        assert_eq!(live.avg_latency_ms, 200.0);
    }

    #[test]
    fn test_live_snapshot_reports_aggregate_and_scenario_metrics() {
        let collector = MetricsCollector::with_retention(2, None);

        collector.record(result(Some("login"), 100, None));
        collector.record(result(Some("login"), 300, None));
        collector.record(result(Some("checkout"), 200, Some("HTTP 500")));
        collector.record_skipped_scenario("profile");

        let snapshot = collector.snapshot();

        assert_eq!(snapshot.total_requests, 3);
        assert_eq!(snapshot.successful_requests, 2);
        assert_eq!(snapshot.failed_requests, 1);
        assert!((snapshot.error_rate - 100.0 / 3.0).abs() < 0.001);
        assert_eq!(snapshot.mean_latency_ms, 200.0);
        assert_eq!(snapshot.min_latency_ms, 100);
        assert_eq!(snapshot.max_latency_ms, 300);
        assert_eq!(snapshot.status_codes.get(&200), Some(&2));
        assert_eq!(snapshot.status_codes.get(&500), Some(&1));
        assert_eq!(snapshot.skipped_scenarios.get("profile"), Some(&1));
        assert_eq!(snapshot.retained_results, 2);
        assert_eq!(snapshot.dropped_results, 1);

        let login = snapshot.per_scenario.get("login").unwrap();
        assert_eq!(login.total_requests, 2);
        assert_eq!(login.failed_requests, 0);
        let checkout = snapshot.per_scenario.get("checkout").unwrap();
        assert_eq!(checkout.failed_requests, 1);
        assert_eq!(checkout.error_rate, 100.0);
    }

    #[test]
    fn test_live_snapshot_timeline_is_bounded_and_ordered() {
        let collector = MetricsCollector::new();
        let base = Utc::now();

        // One request per second for twice the retained window.
        for index in 0..(TIMELINE_WINDOW_SECS as i64 * 2) {
            let timestamp = base + chrono::Duration::seconds(index);
            collector.record(RequestResult {
                scenario_name: None,
                latency_ms: 10,
                status_code: 200,
                error: None,
                request_start_timestamp: timestamp,
                request_end_timestamp: timestamp,
            });
        }

        let snapshot = collector.snapshot();

        // History stays a fixed size no matter how long the run lasts, and the
        // oldest seconds are the ones dropped.
        assert_eq!(snapshot.timeline.len(), TIMELINE_WINDOW_SECS);
        assert_eq!(snapshot.total_requests, TIMELINE_WINDOW_SECS * 2);
        let seconds: Vec<i64> = snapshot.timeline.iter().map(|point| point.second).collect();
        assert!(seconds.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(*seconds.last().unwrap(), base.timestamp() + 239);
        assert!(snapshot
            .timeline
            .iter()
            .all(|point| point.requests == 1 && point.mean_latency_ms == 10.0));
    }

    #[test]
    fn test_live_snapshot_groups_requests_in_the_same_second() {
        let collector = MetricsCollector::new();
        let timestamp = Utc::now();

        for latency in [10, 20, 60] {
            collector.record(RequestResult {
                scenario_name: None,
                latency_ms: latency,
                status_code: if latency == 60 { 503 } else { 200 },
                error: (latency == 60).then(|| "HTTP 503".to_string()),
                request_start_timestamp: timestamp,
                request_end_timestamp: timestamp,
            });
        }

        let snapshot = collector.snapshot();
        assert_eq!(snapshot.timeline.len(), 1);
        let point = &snapshot.timeline[0];
        assert_eq!(point.requests, 3);
        assert_eq!(point.failed, 1);
        assert_eq!(point.mean_latency_ms, 30.0);
        assert_eq!(point.max_latency_ms, 60);
    }

    #[test]
    fn test_current_rps_falls_off_when_the_run_goes_idle() {
        let collector = MetricsCollector::new();
        let now = Utc::now();

        // Ten requests, all a minute in the past: well outside the live window.
        for _ in 0..10 {
            let timestamp = now - chrono::Duration::seconds(60);
            collector.record(RequestResult {
                scenario_name: None,
                latency_ms: 5,
                status_code: 200,
                error: None,
                request_start_timestamp: timestamp,
                request_end_timestamp: timestamp,
            });
        }

        let snapshot = collector.snapshot();
        assert_eq!(snapshot.current_rps, 0.0);
        // The all-run average still counts them.
        assert_eq!(snapshot.total_requests, 10);
    }

    #[test]
    fn test_recent_activity_is_bounded_and_newest_first() {
        let collector = MetricsCollector::new();

        let recorded = RECENT_SAMPLE_SIZE as u64 * 3;
        for index in 1..=recorded {
            let error = (index % 2 == 0).then_some("HTTP 500");
            collector.record(result(Some("step"), index, error));
        }

        let activity = collector.recent_activity();

        assert_eq!(activity.sample_size, RECENT_SAMPLE_SIZE);
        assert_eq!(activity.results.len(), RECENT_SAMPLE_SIZE);
        assert_eq!(activity.failures.len(), RECENT_SAMPLE_SIZE);
        // Newest first, and only failures land in the failure list.
        assert_eq!(activity.results[0].latency_ms, recorded);
        assert!(activity.results[0].latency_ms > activity.results[1].latency_ms);
        assert!(activity
            .failures
            .iter()
            .all(|failure| failure.error.as_deref() == Some("HTTP 500")));
        assert!(activity.failures[0].latency_ms > activity.failures[1].latency_ms);
    }

    #[test]
    fn test_prometheus_snapshot_counts_every_request() {
        let collector = MetricsCollector::with_retention(1, None);

        collector.record(result(None, 10, None));
        collector.record(result(None, 20, Some("HTTP 500")));
        collector.record(result(None, 30, None));

        let rendered = collector.render_prometheus();
        assert!(
            rendered.contains("flux_requests_total{status=\"success\"} 2"),
            "{rendered}"
        );
        assert!(
            rendered.contains("flux_requests_total{status=\"failure\"} 1"),
            "{rendered}"
        );
    }

    #[test]
    fn test_a_run_with_no_load_profile_omits_it_from_the_summary() {
        let collector = MetricsCollector::new();
        collector.record(result(None, 10, None));

        let summary = collector.generate_summary();
        assert!(summary.load_profile.is_none());
        assert!(summary.stages.is_empty());
    }

    #[test]
    fn test_stage_transitions_attribute_requests_to_the_active_stage() {
        let collector = MetricsCollector::new();
        collector.set_load_profile("stages", None);

        collector.begin_stage("Stage 1".to_string(), Some(2), None, 10.0);
        collector.record(result(None, 10, None));
        collector.record(result(None, 20, None));

        collector.begin_stage("Stage 2".to_string(), Some(5), None, 5.0);
        collector.record(result(None, 30, Some("HTTP 500")));

        let summary = collector.generate_summary();
        assert_eq!(summary.stages.len(), 2);

        assert_eq!(summary.stages[0].label, "Stage 1");
        assert_eq!(summary.stages[0].target_concurrency, Some(2));
        assert_eq!(summary.stages[0].planned_duration_secs, 10.0);
        assert_eq!(summary.stages[0].metrics.total_requests, 2);
        assert_eq!(summary.stages[0].metrics.failed_requests, 0);

        assert_eq!(summary.stages[1].label, "Stage 2");
        assert_eq!(summary.stages[1].target_concurrency, Some(5));
        assert_eq!(summary.stages[1].metrics.total_requests, 1);
        assert_eq!(summary.stages[1].metrics.failed_requests, 1);

        // The overall run still counts every request across every stage.
        assert_eq!(summary.total_requests, 3);

        let profile = summary.load_profile.unwrap();
        assert_eq!(profile.kind, "stages");
        assert!(profile.target_rps.is_none());
        assert!(profile.achieved_rps.is_none());
    }

    #[test]
    fn test_arrival_rate_profile_reports_target_and_achieved_rate() {
        let collector = MetricsCollector::new();
        collector.set_load_profile("arrival_rate", Some(100.0));
        collector.begin_stage(
            "Arrival rate (100.00 req/s)".to_string(),
            None,
            Some(100.0),
            1.0,
        );

        for _ in 0..5 {
            collector.record_scheduled_tick();
        }
        collector.record_saturation();
        collector.record_saturation();
        collector.record(result(None, 5, None));
        collector.record(result(None, 5, None));

        let summary = collector.generate_summary();
        let profile = summary.load_profile.unwrap();
        assert_eq!(profile.kind, "arrival_rate");
        assert_eq!(profile.target_rps, Some(100.0));
        assert_eq!(profile.achieved_rps, Some(summary.throughput_rps));
        assert_eq!(profile.scheduled_ticks, 5);
        assert_eq!(profile.saturated_ticks, 2);

        assert_eq!(summary.stages.len(), 1);
        assert_eq!(summary.stages[0].target_rps, Some(100.0));
        assert_eq!(summary.stages[0].metrics.total_requests, 2);
    }
}
