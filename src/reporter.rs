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

        // Ensure parent directory exists
        if let Some(parent) = Path::new(output_path).parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(output_path, json)?;
        Ok(())
    }

    /// Generate HTML report
    pub fn generate_html(&self, output_path: &str) -> Result<()> {
        let html = self.render_html()?;

        // Ensure parent directory exists
        if let Some(parent) = Path::new(output_path).parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(output_path, html)?;
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
    fn calculate_latency_distribution(&self) -> Vec<(String, usize)> {
        let mut buckets: Vec<(String, usize)> = vec![
            ("0-50ms".to_string(), 0),
            ("50-100ms".to_string(), 0),
            ("100-200ms".to_string(), 0),
            ("200-500ms".to_string(), 0),
            ("500-1000ms".to_string(), 0),
            ("1000ms+".to_string(), 0),
        ];

        for result in &self.report.results {
            let latency = result.latency_ms;

            if latency < 50 {
                buckets[0].1 += 1;
            } else if latency < 100 {
                buckets[1].1 += 1;
            } else if latency < 200 {
                buckets[2].1 += 1;
            } else if latency < 500 {
                buckets[3].1 += 1;
            } else if latency < 1000 {
                buckets[4].1 += 1;
            } else {
                buckets[5].1 += 1;
            }
        }

        buckets
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        };

        let reporter = Reporter::new(summary, results);
        let distribution = reporter.calculate_latency_distribution();

        assert_eq!(distribution[0].1, 1); // 0-50ms
        assert_eq!(distribution[1].1, 1); // 50-100ms
        assert_eq!(distribution[2].1, 1); // 100-200ms
    }
}
