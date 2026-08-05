use chrono::{DateTime, Utc};
use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tracing::warn;

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
#[derive(Debug)]
pub struct MetricsCollector {
    results: Arc<Mutex<Vec<RequestResult>>>,
    histogram: Arc<Mutex<Histogram<u64>>>,
    start_time: DateTime<Utc>,
    load_phase: Arc<Mutex<LoadPhase>>,
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

impl MetricsCollector {
    /// Create a new metrics collector
    pub fn new() -> Self {
        Self {
            results: Arc::new(Mutex::new(Vec::new())),
            histogram: Arc::new(Mutex::new(
                Histogram::<u64>::new_with_bounds(1, 300_000, 3).unwrap(),
            )),
            start_time: Utc::now(),
            load_phase: Arc::new(Mutex::new(LoadPhase::default())),
        }
    }

    /// Mark the end of ramp-up: every worker has started and the measured
    /// load window begins now. Repeated calls keep the first mark.
    pub fn mark_load_phase_started(&self) {
        if let Ok(mut load_phase) = self.load_phase.lock() {
            load_phase.started_at.get_or_insert_with(Utc::now);
        }
    }

    /// Record a request result
    pub fn record(&self, result: RequestResult) {
        let latency = result.latency_ms;

        if let Ok(mut load_phase) = self.load_phase.lock() {
            if load_phase
                .started_at
                .is_some_and(|started| result.request_start_timestamp >= started)
            {
                load_phase.requests += 1;
            }
        }

        // Store result
        if let Ok(mut results) = self.results.lock() {
            results.push(result);
        }

        // Update histogram
        if let Ok(mut hist) = self.histogram.lock() {
            let clamped = latency.min(300_000);
            if clamped < latency {
                warn!(
                    "Latency {}ms exceeds histogram maximum; clamping to {}ms",
                    latency, clamped
                );
            }
            if let Err(e) = hist.record(clamped) {
                warn!("Histogram record error for latency {}ms: {}", clamped, e);
            }
        }
    }

    /// Get current live metrics
    pub fn get_live_metrics(&self) -> LiveMetrics {
        let results = self.results.lock().unwrap();
        let total = results.len();

        if total == 0 {
            return LiveMetrics {
                current_rps: 0.0,
                avg_latency_ms: 0.0,
                error_count: 0,
                total_requests: 0,
            };
        }

        let elapsed = Utc::now()
            .signed_duration_since(self.start_time)
            .num_milliseconds() as f64
            / 1000.0;

        let error_count = results.iter().filter(|r| r.error.is_some()).count();

        let sum_latency: u64 = results.iter().map(|r| r.latency_ms).sum();
        let avg_latency = sum_latency as f64 / total as f64;

        let current_rps = if elapsed > 0.0 {
            total as f64 / elapsed
        } else {
            0.0
        };

        LiveMetrics {
            current_rps,
            avg_latency_ms: avg_latency,
            error_count,
            total_requests: total,
        }
    }

    /// Generate final summary
    pub fn generate_summary(&self) -> MetricsSummary {
        let results = self.results.lock().unwrap();
        let histogram = self.histogram.lock().unwrap();

        let total = results.len();
        let successful = results.iter().filter(|r| r.error.is_none()).count();
        let failed = total - successful;

        let end_time = Utc::now();
        let duration = end_time
            .signed_duration_since(self.start_time)
            .num_milliseconds() as f64
            / 1000.0;

        // Ramp-up is warm-up time, so throughput is reported over the window in
        // which every worker was running.
        let (load_started_at, measured_requests) = match self.load_phase.lock() {
            Ok(load_phase) => (load_phase.started_at, load_phase.requests),
            Err(_) => (None, total),
        };
        let ramp_up_secs = load_started_at
            .map(|started| {
                started
                    .signed_duration_since(self.start_time)
                    .num_milliseconds() as f64
                    / 1000.0
            })
            .unwrap_or(0.0)
            .max(0.0);
        let measured_duration = (duration - ramp_up_secs).max(0.0);
        let (measured_requests, measured_duration) = if load_started_at.is_some() {
            (measured_requests, measured_duration)
        } else {
            (total, duration)
        };

        let throughput = if measured_duration > 0.0 {
            measured_requests as f64 / measured_duration
        } else {
            0.0
        };

        let error_rate = if total > 0 {
            (failed as f64 / total as f64) * 100.0
        } else {
            0.0
        };

        let min = histogram.min();
        let max = histogram.max();
        let mean = histogram.mean();
        let p50 = histogram.value_at_quantile(0.50);
        let p90 = histogram.value_at_quantile(0.90);
        let p95 = histogram.value_at_quantile(0.95);
        let p99 = histogram.value_at_quantile(0.99);

        let mut grouped: BTreeMap<String, Vec<&RequestResult>> = BTreeMap::new();
        for result in results.iter() {
            if let Some(name) = &result.scenario_name {
                grouped.entry(name.clone()).or_default().push(result);
            }
        }
        let per_scenario = grouped
            .into_iter()
            .map(|(name, results)| (name, summarize_scenario(&results, measured_duration)))
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
            min_latency_ms: min,
            max_latency_ms: max,
            mean_latency_ms: mean,
            p50_latency_ms: p50,
            p90_latency_ms: p90,
            p95_latency_ms: p95,
            p99_latency_ms: p99,
            error_rate,
            start_time: self.start_time,
            end_time,
            per_scenario,
        }
    }

    /// Get all results for reporting
    pub fn get_results(&self) -> Vec<RequestResult> {
        self.results.lock().unwrap().clone()
    }

    /// Render a current Prometheus text-format snapshot.
    pub fn render_prometheus(&self) -> String {
        let results = self.results.lock().unwrap();
        let histogram = self.histogram.lock().unwrap();
        let total = results.len();
        let failed = results
            .iter()
            .filter(|result| result.error.is_some())
            .count();
        let successful = total - failed;
        let elapsed = Utc::now()
            .signed_duration_since(self.start_time)
            .num_milliseconds() as f64
            / 1000.0;
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
            histogram.value_at_quantile(0.50),
            histogram.value_at_quantile(0.90),
            histogram.value_at_quantile(0.95),
            histogram.value_at_quantile(0.99),
            rps
        )
    }
}

fn summarize_scenario(results: &[&RequestResult], duration: f64) -> ScenarioMetricsSummary {
    let total = results.len();
    let successful = results
        .iter()
        .filter(|result| result.error.is_none())
        .count();
    let failed = total - successful;
    let mut latencies: Vec<u64> = results.iter().map(|result| result.latency_ms).collect();
    latencies.sort_unstable();
    let mean = latencies.iter().sum::<u64>() as f64 / total as f64;

    ScenarioMetricsSummary {
        total_requests: total,
        successful_requests: successful,
        failed_requests: failed,
        throughput_rps: if duration > 0.0 {
            total as f64 / duration
        } else {
            0.0
        },
        min_latency_ms: latencies[0],
        max_latency_ms: latencies[total - 1],
        mean_latency_ms: mean,
        p50_latency_ms: percentile(&latencies, 0.50),
        p90_latency_ms: percentile(&latencies, 0.90),
        p95_latency_ms: percentile(&latencies, 0.95),
        p99_latency_ms: percentile(&latencies, 0.99),
        error_rate: (failed as f64 / total as f64) * 100.0,
    }
}

fn percentile(sorted_values: &[u64], quantile: f64) -> u64 {
    let index = ((sorted_values.len() as f64 * quantile).ceil() as usize)
        .saturating_sub(1)
        .min(sorted_values.len() - 1);
    sorted_values[index]
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_collector() {
        let collector = MetricsCollector::new();

        let result = RequestResult {
            scenario_name: Some("test".to_string()),
            latency_ms: 100,
            status_code: 200,
            error: None,
            request_start_timestamp: Utc::now(),
            request_end_timestamp: Utc::now(),
        };

        collector.record(result.clone());

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

        let result = RequestResult {
            scenario_name: None,
            latency_ms: 50,
            status_code: 500,
            error: Some("HTTP 500".to_string()),
            request_start_timestamp: Utc::now(),
            request_end_timestamp: Utc::now(),
        };

        collector.record(result);

        let summary = collector.generate_summary();
        assert_eq!(summary.failed_requests, 1);
        assert_eq!(summary.successful_requests, 0);
        assert!(summary.per_scenario.is_empty());
    }

    #[test]
    fn test_metrics_large_latency_clamped() {
        let collector = MetricsCollector::new();

        // Record a latency larger than the histogram max (300_000 ms)
        let result = RequestResult {
            scenario_name: None,
            latency_ms: 400_000,
            status_code: 200,
            error: None,
            request_start_timestamp: Utc::now(),
            request_end_timestamp: Utc::now(),
        };

        // Should not panic; large latency is clamped
        collector.record(result);

        let summary = collector.generate_summary();
        assert_eq!(summary.total_requests, 1);
    }
}
