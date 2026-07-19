use crate::metrics::{MetricsSummary, RequestResult};
use anyhow::Result;
use serde::Serialize;
use std::fs;
use std::path::Path;
use tera::{Context, Tera};

/// Report data structure
#[derive(Debug, Serialize)]
pub struct Report {
    pub summary: MetricsSummary,
    pub results: Vec<RequestResult>,
}

#[derive(Debug, Serialize)]
struct LatencyBucket {
    label: String,
    count: usize,
}

/// Reporter for generating JSON and HTML reports
pub struct Reporter {
    report: Report,
}

impl Reporter {
    /// Create a new reporter
    pub fn new(summary: MetricsSummary, results: Vec<RequestResult>) -> Self {
        Self {
            report: Report { summary, results },
        }
    }

    /// Generate JSON report
    pub fn generate_json(&self, output_path: &str) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.report)?;

        ensure_parent_directory(output_path)?;

        fs::write(output_path, json)?;
        Ok(())
    }

    /// Generate HTML report
    pub fn generate_html(&self, output_path: &str) -> Result<()> {
        let html = self.render_html()?;

        ensure_parent_directory(output_path)?;

        fs::write(output_path, html)?;
        Ok(())
    }

    /// Generate a CSV report with one row per request.
    pub fn generate_csv(&self, output_path: &str) -> Result<()> {
        ensure_parent_directory(output_path)?;

        let mut writer = csv::Writer::from_path(output_path)?;
        writer.write_record([
            "timestamp",
            "scenario",
            "latency_ms",
            "status_code",
            "error",
        ])?;
        for result in &self.report.results {
            writer.write_record([
                result.request_start_timestamp.to_rfc3339(),
                result.scenario_name.clone().unwrap_or_default(),
                result.latency_ms.to_string(),
                result.status_code.to_string(),
                result.error.clone().unwrap_or_default(),
            ])?;
        }
        writer.flush()?;
        Ok(())
    }

    /// Render HTML report
    fn render_html(&self) -> Result<String> {
        let template = include_str!("templates/report.html");

        let mut tera = Tera::default();
        tera.add_raw_template("report.html", template)?;

        let mut context = Context::new();
        context.insert("summary", &self.report.summary);
        context.insert("results", &self.report.results);

        // Prepare data for charts
        let latency_data: Vec<u64> = self.report.results.iter().map(|r| r.latency_ms).collect();

        let status_codes: Vec<u16> = self.report.results.iter().map(|r| r.status_code).collect();

        // Calculate latency distribution
        let latency_distribution = self.calculate_latency_distribution();

        context.insert("latency_data", &latency_data);
        context.insert("status_codes", &status_codes);
        context.insert("latency_distribution", &latency_distribution);

        let html = tera.render("report.html", &context)?;
        Ok(html)
    }

    /// Calculate latency distribution for histogram
    fn calculate_latency_distribution(&self) -> Vec<LatencyBucket> {
        let mut buckets = vec![
            LatencyBucket {
                label: "0-50ms".to_string(),
                count: 0,
            },
            LatencyBucket {
                label: "50-100ms".to_string(),
                count: 0,
            },
            LatencyBucket {
                label: "100-200ms".to_string(),
                count: 0,
            },
            LatencyBucket {
                label: "200-500ms".to_string(),
                count: 0,
            },
            LatencyBucket {
                label: "500-1000ms".to_string(),
                count: 0,
            },
            LatencyBucket {
                label: "1000ms+".to_string(),
                count: 0,
            },
        ];

        for result in &self.report.results {
            let latency = result.latency_ms;

            if latency < 50 {
                buckets[0].count += 1;
            } else if latency < 100 {
                buckets[1].count += 1;
            } else if latency < 200 {
                buckets[2].count += 1;
            } else if latency < 500 {
                buckets[3].count += 1;
            } else if latency < 1000 {
                buckets[4].count += 1;
            } else {
                buckets[5].count += 1;
            }
        }

        buckets
    }
}

fn ensure_parent_directory(output_path: &str) -> Result<()> {
    if let Some(parent) = Path::new(output_path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::ScenarioMetricsSummary;
    use chrono::Utc;

    #[test]
    fn test_latency_distribution() {
        let results = vec![
            RequestResult {
                scenario_name: None,
                latency_ms: 30,
                status_code: 200,
                error: None,
                request_start_timestamp: Utc::now(),
                request_end_timestamp: Utc::now(),
            },
            RequestResult {
                scenario_name: None,
                latency_ms: 75,
                status_code: 200,
                error: None,
                request_start_timestamp: Utc::now(),
                request_end_timestamp: Utc::now(),
            },
            RequestResult {
                scenario_name: None,
                latency_ms: 150,
                status_code: 200,
                error: None,
                request_start_timestamp: Utc::now(),
                request_end_timestamp: Utc::now(),
            },
        ];

        let summary = MetricsSummary {
            total_requests: 3,
            successful_requests: 3,
            failed_requests: 0,
            total_duration_secs: 1.0,
            throughput_rps: 3.0,
            min_latency_ms: 30,
            max_latency_ms: 150,
            mean_latency_ms: 85.0,
            p50_latency_ms: 75,
            p90_latency_ms: 150,
            p95_latency_ms: 150,
            p99_latency_ms: 150,
            error_rate: 0.0,
            start_time: Utc::now(),
            end_time: Utc::now(),
            per_scenario: Default::default(),
        };

        let reporter = Reporter::new(summary, results);
        let distribution = reporter.calculate_latency_distribution();

        assert_eq!(distribution[0].count, 1); // 0-50ms
        assert_eq!(distribution[1].count, 1); // 50-100ms
        assert_eq!(distribution[2].count, 1); // 100-200ms
    }

    #[test]
    fn test_csv_output() {
        let result = RequestResult {
            scenario_name: Some("login".to_string()),
            latency_ms: 42,
            status_code: 500,
            error: Some("failed, temporarily".to_string()),
            request_start_timestamp: Utc::now(),
            request_end_timestamp: Utc::now(),
        };
        let summary = MetricsSummary {
            total_requests: 1,
            successful_requests: 0,
            failed_requests: 1,
            total_duration_secs: 1.0,
            throughput_rps: 1.0,
            min_latency_ms: 42,
            max_latency_ms: 42,
            mean_latency_ms: 42.0,
            p50_latency_ms: 42,
            p90_latency_ms: 42,
            p95_latency_ms: 42,
            p99_latency_ms: 42,
            error_rate: 100.0,
            start_time: Utc::now(),
            end_time: Utc::now(),
            per_scenario: Default::default(),
        };
        let reporter = Reporter::new(summary, vec![result]);
        let path = std::env::temp_dir().join(format!(
            "flux-report-{}-{}.csv",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));

        reporter.generate_csv(path.to_str().unwrap()).unwrap();
        let csv = fs::read_to_string(&path).unwrap();
        fs::remove_file(path).unwrap();

        assert!(csv.starts_with("timestamp,scenario,latency_ms,status_code,error"));
        assert!(csv.contains("login,42,500,\"failed, temporarily\""));
    }

    #[test]
    fn test_html_contains_per_scenario_table() {
        let scenario = ScenarioMetricsSummary {
            total_requests: 2,
            successful_requests: 2,
            failed_requests: 0,
            throughput_rps: 2.0,
            min_latency_ms: 10,
            max_latency_ms: 20,
            mean_latency_ms: 15.0,
            p50_latency_ms: 10,
            p90_latency_ms: 20,
            p95_latency_ms: 20,
            p99_latency_ms: 20,
            error_rate: 0.0,
        };
        let summary = MetricsSummary {
            total_requests: 2,
            successful_requests: 2,
            failed_requests: 0,
            total_duration_secs: 1.0,
            throughput_rps: 2.0,
            min_latency_ms: 10,
            max_latency_ms: 20,
            mean_latency_ms: 15.0,
            p50_latency_ms: 10,
            p90_latency_ms: 20,
            p95_latency_ms: 20,
            p99_latency_ms: 20,
            error_rate: 0.0,
            start_time: Utc::now(),
            end_time: Utc::now(),
            per_scenario: std::collections::BTreeMap::from([("login".to_string(), scenario)]),
        };
        let reporter = Reporter::new(summary, Vec::new());

        let html = reporter.render_html().unwrap();
        assert!(html.contains("Per-Scenario Metrics"));
        assert!(html.contains("login"));
    }
}
