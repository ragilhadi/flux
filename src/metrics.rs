use chrono::{DateTime, Utc};
use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;
use tracing::warn;

/// Upper bound of the latency histogram, in milliseconds.
const HISTOGRAM_MAX_MS: u64 = 300_000;

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
    /// Streaming sink for raw rows, dropped by `close_result_stream` so the
    /// writer task can finish.
    csv_sink: Option<UnboundedSender<RequestResult>>,
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
        csv_sink: Option<UnboundedSender<RequestResult>>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(CollectorState {
                results: Vec::new(),
                dropped_results: 0,
                aggregate: Aggregate::new(),
                per_scenario: BTreeMap::new(),
                load_phase: LoadPhase::default(),
                skipped_scenarios: BTreeMap::new(),
                csv_sink,
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
        // retained in memory.
        if let Some(sink) = &state.csv_sink {
            let _ = sink.send(result.clone());
        }

        state.aggregate.observe(&result);
        if let Some(name) = &result.scenario_name {
            state
                .per_scenario
                .entry(name.clone())
                .or_insert_with(Aggregate::new)
                .observe(&result);
        }
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

    /// Generate final summary
    pub fn generate_summary(&self) -> MetricsSummary {
        let state = self.state.lock().expect("metrics state poisoned");

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
            skipped_scenarios: state.skipped_scenarios.clone(),
            retained_results: state.results.len(),
            dropped_results: state.dropped_results,
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
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
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
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
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
}
