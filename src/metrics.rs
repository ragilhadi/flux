use chrono::{DateTime, Utc};
use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

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
}

/// Summary statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSummary {
    pub total_requests: usize,
    pub successful_requests: usize,
    pub failed_requests: usize,
    pub total_duration_secs: f64,
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
                Histogram::<u64>::new_with_bounds(1, 60_000, 3).unwrap(),
            )),
            start_time: Utc::now(),
        }
    }

    /// Record a request result
    pub fn record(&self, result: RequestResult) {
        let latency = result.latency_ms;

        // Store result
        if let Ok(mut results) = self.results.lock() {
            results.push(result);
        }

        // Update histogram
        if let Ok(mut hist) = self.histogram.lock() {
            let _ = hist.record(latency);
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

        let throughput = if duration > 0.0 {
            total as f64 / duration
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

        MetricsSummary {
            total_requests: total,
            successful_requests: successful,
            failed_requests: failed,
            total_duration_secs: duration,
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
        }
    }

    /// Get all results for reporting
    pub fn get_results(&self) -> Vec<RequestResult> {
        self.results.lock().unwrap().clone()
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
    }
}
