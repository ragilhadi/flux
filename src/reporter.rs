use crate::compare::REPORT_SCHEMA_VERSION;
use crate::metrics::{MetricsSummary, RequestResult};
use anyhow::Result;
use serde::Serialize;
use std::fs;
use std::path::Path;
use tera::{Context, Tera};
use tokio::sync::mpsc::{channel, Sender};
use tokio::task::JoinHandle;
use tracing::warn;

/// Rows buffered between the collector and the CSV writer task. Bounded so a
/// writer that falls behind the request rate (a slow disk) cannot make the
/// collector hold an unbounded backlog in memory; a row that does not fit is
/// dropped and counted rather than queued.
const CSV_CHANNEL_CAPACITY: usize = 10_000;

/// Report data structure
#[derive(Debug, Serialize)]
pub struct Report {
    /// Layout of this document, so `flux compare` knows what it is reading.
    /// Reports written before versioning carry no such field and are read as
    /// the legacy schema.
    pub schema_version: u32,
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
            report: Report {
                schema_version: REPORT_SCHEMA_VERSION,
                summary,
                results,
            },
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

/// Incremental CSV writer for per-request rows.
///
/// Rows are written as they are produced, so a full-fidelity CSV never needs
/// every request to be held in memory first.
pub struct CsvResultStream {
    sender: Option<Sender<RequestResult>>,
    writer: JoinHandle<Result<usize>>,
}

impl CsvResultStream {
    /// Open `output_path` and start the background writer task.
    pub fn create(output_path: &str) -> Result<Self> {
        ensure_parent_directory(output_path)?;

        let mut writer = csv::Writer::from_path(output_path)?;
        writer.write_record([
            "timestamp",
            "scenario",
            "latency_ms",
            "status_code",
            "error",
        ])?;

        let (sender, mut receiver) = channel::<RequestResult>(CSV_CHANNEL_CAPACITY);
        let task = tokio::spawn(async move {
            let mut rows = 0_usize;
            while let Some(result) = receiver.recv().await {
                if let Err(e) = writer.write_record([
                    result.request_start_timestamp.to_rfc3339(),
                    result.scenario_name.clone().unwrap_or_default(),
                    result.latency_ms.to_string(),
                    result.status_code.to_string(),
                    result.error.clone().unwrap_or_default(),
                ]) {
                    warn!("CSV writer failed after {rows} rows; stopping: {e}");
                    return Err(e.into());
                }
                rows += 1;
            }
            writer.flush()?;
            Ok(rows)
        });

        Ok(Self {
            sender: Some(sender),
            writer: task,
        })
    }

    /// Channel every recorded result should be forwarded to.
    pub fn sender(&self) -> Sender<RequestResult> {
        self.sender
            .clone()
            .expect("sender is only taken when finishing the stream")
    }

    /// Close the stream and flush the file, returning the number of rows.
    pub async fn finish(mut self) -> Result<usize> {
        // Dropping every sender ends the writer loop.
        self.sender.take();
        self.writer.await?
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
            ramp_up_secs: 0.0,
            measured_duration_secs: 1.0,
            measured_requests: 0,
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
            status_codes: Default::default(),
            skipped_scenarios: Default::default(),
            retained_results: 0,
            dropped_results: 0,
            csv_dropped_rows: 0,
            load_profile: None,
            stages: Vec::new(),
        };

        let reporter = Reporter::new(summary, results);
        let distribution = reporter.calculate_latency_distribution();

        assert_eq!(distribution[0].count, 1); // 0-50ms
        assert_eq!(distribution[1].count, 1); // 50-100ms
        assert_eq!(distribution[2].count, 1); // 100-200ms
    }

    #[tokio::test]
    async fn test_csv_stream_writes_every_row() {
        let path = std::env::temp_dir().join(format!(
            "flux-report-{}-{}.csv",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));

        let stream = CsvResultStream::create(path.to_str().unwrap()).unwrap();
        let sender = stream.sender();
        sender
            .send(RequestResult {
                scenario_name: Some("login".to_string()),
                latency_ms: 42,
                status_code: 500,
                error: Some("failed, temporarily".to_string()),
                request_start_timestamp: Utc::now(),
                request_end_timestamp: Utc::now(),
            })
            .await
            .unwrap();
        for latency in 0..100 {
            sender
                .send(RequestResult {
                    scenario_name: None,
                    latency_ms: latency,
                    status_code: 200,
                    error: None,
                    request_start_timestamp: Utc::now(),
                    request_end_timestamp: Utc::now(),
                })
                .await
                .unwrap();
        }
        drop(sender);

        let rows = stream.finish().await.unwrap();
        let csv = fs::read_to_string(&path).unwrap();
        fs::remove_file(path).unwrap();

        assert_eq!(rows, 101);
        assert_eq!(csv.lines().count(), 102);
        assert!(csv.starts_with("timestamp,scenario,latency_ms,status_code,error"));
        assert!(csv.contains("login,42,500,\"failed, temporarily\""));
    }

    #[test]
    fn test_json_report_is_versioned_and_carries_the_status_distribution() {
        let summary = MetricsSummary {
            total_requests: 2,
            successful_requests: 1,
            failed_requests: 1,
            total_duration_secs: 1.0,
            ramp_up_secs: 0.0,
            measured_duration_secs: 1.0,
            measured_requests: 2,
            throughput_rps: 2.0,
            min_latency_ms: 10,
            max_latency_ms: 20,
            mean_latency_ms: 15.0,
            p50_latency_ms: 10,
            p90_latency_ms: 20,
            p95_latency_ms: 20,
            p99_latency_ms: 20,
            error_rate: 50.0,
            start_time: Utc::now(),
            end_time: Utc::now(),
            per_scenario: Default::default(),
            status_codes: std::collections::BTreeMap::from([(200, 1), (500, 1)]),
            skipped_scenarios: Default::default(),
            retained_results: 0,
            dropped_results: 0,
            csv_dropped_rows: 0,
            load_profile: None,
            stages: Vec::new(),
        };
        let path = std::env::temp_dir().join(format!(
            "flux-report-{}-{}.json",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));

        Reporter::new(summary, Vec::new())
            .generate_json(path.to_str().unwrap())
            .unwrap();
        let report: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        fs::remove_file(&path).unwrap();

        assert_eq!(report["schema_version"], REPORT_SCHEMA_VERSION);
        assert_eq!(report["summary"]["status_codes"]["200"], 1);
        assert_eq!(report["summary"]["status_codes"]["500"], 1);
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
            ramp_up_secs: 0.0,
            measured_duration_secs: 1.0,
            measured_requests: 0,
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
            status_codes: Default::default(),
            skipped_scenarios: Default::default(),
            retained_results: 0,
            dropped_results: 0,
            csv_dropped_rows: 0,
            load_profile: None,
            stages: Vec::new(),
        };
        let reporter = Reporter::new(summary, Vec::new());

        let html = reporter.render_html().unwrap();
        assert!(html.contains("Per-Scenario Metrics"));
        assert!(html.contains("login"));
        // Nothing was dropped, so the retention notice stays out of the report.
        assert!(!html.contains("Retained Results"));
    }

    #[test]
    fn test_html_states_retained_and_dropped_results() {
        let summary = MetricsSummary {
            total_requests: 1_000,
            successful_requests: 1_000,
            failed_requests: 0,
            total_duration_secs: 1.0,
            ramp_up_secs: 0.0,
            measured_duration_secs: 1.0,
            measured_requests: 1_000,
            throughput_rps: 1_000.0,
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
            per_scenario: Default::default(),
            status_codes: Default::default(),
            skipped_scenarios: Default::default(),
            retained_results: 100,
            dropped_results: 900,
            csv_dropped_rows: 0,
            load_profile: None,
            stages: Vec::new(),
        };
        let reporter = Reporter::new(summary, Vec::new());

        let html = reporter.render_html().unwrap();
        assert!(html.contains("Retained Results"));
        assert!(html.contains("100 of"), "{html}");
        assert!(html.contains("900 per-request"), "{html}");
    }

    #[test]
    fn test_html_omits_load_profile_section_for_a_plain_run() {
        let summary = MetricsSummary {
            total_requests: 1,
            successful_requests: 1,
            failed_requests: 0,
            total_duration_secs: 1.0,
            ramp_up_secs: 0.0,
            measured_duration_secs: 1.0,
            measured_requests: 1,
            throughput_rps: 1.0,
            min_latency_ms: 10,
            max_latency_ms: 10,
            mean_latency_ms: 10.0,
            p50_latency_ms: 10,
            p90_latency_ms: 10,
            p95_latency_ms: 10,
            p99_latency_ms: 10,
            error_rate: 0.0,
            start_time: Utc::now(),
            end_time: Utc::now(),
            per_scenario: Default::default(),
            status_codes: Default::default(),
            skipped_scenarios: Default::default(),
            retained_results: 0,
            dropped_results: 0,
            csv_dropped_rows: 0,
            load_profile: None,
            stages: Vec::new(),
        };
        let reporter = Reporter::new(summary, Vec::new());

        let html = reporter.render_html().unwrap();
        assert!(!html.contains("Load Profile"));
        assert!(!html.contains("Per-Stage Metrics"));
    }

    #[test]
    fn test_html_contains_load_profile_and_per_stage_tables() {
        use crate::metrics::{LoadProfileSummary, ScenarioMetricsSummary, StageSummary};

        let stage_metrics = ScenarioMetricsSummary {
            total_requests: 50,
            successful_requests: 48,
            failed_requests: 2,
            throughput_rps: 25.0,
            min_latency_ms: 5,
            max_latency_ms: 100,
            mean_latency_ms: 20.0,
            p50_latency_ms: 15,
            p90_latency_ms: 40,
            p95_latency_ms: 60,
            p99_latency_ms: 90,
            error_rate: 4.0,
        };
        let summary = MetricsSummary {
            total_requests: 50,
            successful_requests: 48,
            failed_requests: 2,
            total_duration_secs: 2.0,
            ramp_up_secs: 0.0,
            measured_duration_secs: 2.0,
            measured_requests: 50,
            throughput_rps: 25.0,
            min_latency_ms: 5,
            max_latency_ms: 100,
            mean_latency_ms: 20.0,
            p50_latency_ms: 15,
            p90_latency_ms: 40,
            p95_latency_ms: 60,
            p99_latency_ms: 90,
            error_rate: 4.0,
            start_time: Utc::now(),
            end_time: Utc::now(),
            per_scenario: Default::default(),
            status_codes: Default::default(),
            skipped_scenarios: Default::default(),
            retained_results: 0,
            dropped_results: 0,
            csv_dropped_rows: 0,
            load_profile: Some(LoadProfileSummary {
                kind: "arrival_rate".to_string(),
                target_rps: Some(30.0),
                achieved_rps: Some(25.0),
                scheduled_ticks: 60,
                saturated_ticks: 5,
            }),
            stages: vec![StageSummary {
                label: "Arrival rate (30.00 req/s)".to_string(),
                target_concurrency: None,
                target_rps: Some(30.0),
                planned_duration_secs: 2.0,
                observed_duration_secs: 2.0,
                metrics: stage_metrics,
            }],
        };
        let reporter = Reporter::new(summary, Vec::new());

        let html = reporter.render_html().unwrap();
        assert!(html.contains("Load Profile"));
        assert!(html.contains("arrival_rate"));
        assert!(html.contains("Per-Stage Metrics"));
        assert!(html.contains("Arrival rate (30.00 req/s)"));
    }
}
