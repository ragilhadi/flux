use crate::config::{Config, LoadProfile};
use crate::metrics::{LiveMetrics, MetricsSummary};
use colored::*;
use indicatif::{ProgressBar, ProgressStyle};

/// Terminal UI for displaying load test progress
pub struct TerminalUI {
    progress_bar: ProgressBar,
}

impl TerminalUI {
    /// Create a new terminal UI
    pub fn new(duration_secs: u64) -> Self {
        let progress_bar = ProgressBar::new(duration_secs);

        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len}s ({eta})")
                .expect("Failed to set progress bar template")
                .progress_chars("█▓▒░ "),
        );

        Self { progress_bar }
    }

    /// Display initial banner
    pub fn display_banner(&self, config: &Config, duration_secs: u64) {
        println!("\n{}", "═".repeat(70).bright_cyan());
        println!("{}", "⚡ Flux Load Test Started".bright_white().bold());
        println!("{}", "═".repeat(70).bright_cyan());

        if let Some(target) = &config.target {
            println!("{:<20} : {}", "Target".bright_yellow(), target);
        }

        match &config.load_profile {
            Some(LoadProfile::Stages { stages }) => {
                println!(
                    "{:<20} : {} stage(s)",
                    "Load profile".bright_yellow(),
                    stages.len()
                );
                for (index, stage) in stages.iter().enumerate() {
                    println!(
                        "  {:<18} : {} workers for {}",
                        format!("Stage {}", index + 1),
                        stage.target_concurrency,
                        stage.duration
                    );
                }
            }
            Some(LoadProfile::ArrivalRate {
                target_rps,
                duration,
                max_concurrency,
            }) => {
                println!(
                    "{:<20} : {:.2} req/s for {} (max {} concurrent)",
                    "Load profile".bright_yellow(),
                    target_rps,
                    duration,
                    max_concurrency
                );
            }
            None => {
                println!(
                    "{:<20} : {} workers",
                    "Concurrency".bright_yellow(),
                    config.concurrency
                );
                println!("{:<20} : {}s", "Duration".bright_yellow(), duration_secs);
                if let Some(ramp_up) = &config.ramp_up {
                    println!(
                        "{:<20} : {} (warm-up, added before the measured duration)",
                        "Ramp-up".bright_yellow(),
                        ramp_up
                    );
                }
            }
        }
        if let Some(think_time) = &config.think_time {
            println!("{:<20} : {}", "Think time".bright_yellow(), think_time);
        }
        if let Some(port) = config.prometheus_port {
            println!(
                "{:<20} : http://{}:{}/metrics",
                "Prometheus".bright_yellow(),
                config.prometheus_bind,
                port
            );
        }
        println!(
            "{:<20} : {}",
            "Mode".bright_yellow(),
            config.mode.to_uppercase()
        );

        if !config.scenarios.is_empty() {
            let scenario_names: Vec<String> =
                config.scenarios.iter().map(|s| s.name.clone()).collect();
            println!(
                "{:<20} : {}",
                "Scenarios".bright_yellow(),
                scenario_names.join(" → ")
            );
        }

        println!("{}", "═".repeat(70).bright_cyan());
        println!();
    }

    /// Update progress with live metrics
    pub fn update_progress(&self, elapsed_secs: u64, live_metrics: &LiveMetrics) {
        self.progress_bar.set_position(elapsed_secs);

        let message = format!(
            "RPS: {:.0} | Avg Latency: {:.0}ms | Errors: {} ({:.1}%)",
            live_metrics.current_rps,
            live_metrics.avg_latency_ms,
            live_metrics.error_count,
            if live_metrics.total_requests > 0 {
                (live_metrics.error_count as f64 / live_metrics.total_requests as f64) * 100.0
            } else {
                0.0
            }
        );

        self.progress_bar.set_message(message);
    }

    /// Finish progress bar
    pub fn finish_progress(&self) {
        self.progress_bar.finish_with_message("Test completed");
    }

    /// Display final summary
    pub fn display_summary(&self, summary: &MetricsSummary) {
        println!("\n{}", "═".repeat(70).bright_cyan());
        println!("{}", "📊 Final Summary".bright_white().bold());
        println!("{}", "═".repeat(70).bright_cyan());

        // Request statistics
        println!("\n{}", "Request Statistics:".bright_green().bold());
        println!(
            "  {:<25} : {}",
            "Total Requests".bright_white(),
            summary.total_requests.to_string().bright_cyan()
        );
        println!(
            "  {:<25} : {}",
            "Successful".bright_white(),
            summary.successful_requests.to_string().bright_green()
        );
        println!(
            "  {:<25} : {}",
            "Failed".bright_white(),
            if summary.failed_requests > 0 {
                summary.failed_requests.to_string().bright_red()
            } else {
                summary.failed_requests.to_string().bright_green()
            }
        );

        if summary.dropped_results > 0 {
            println!(
                "  {:<25} : {} of {} (per-request rows in reports; \
                 statistics above cover every request)",
                "Retained Results".bright_white(),
                summary.retained_results.to_string().bright_cyan(),
                summary.total_requests
            );
        }

        if !summary.skipped_scenarios.is_empty() {
            println!(
                "\n{}",
                "Skipped Steps (dependency failed at runtime):"
                    .bright_yellow()
                    .bold()
            );
            for (name, count) in &summary.skipped_scenarios {
                println!(
                    "  {:<25} : {}",
                    name.bright_white(),
                    count.to_string().bright_yellow()
                );
            }
        }

        // Performance metrics
        println!("\n{}", "Performance Metrics:".bright_green().bold());
        println!(
            "  {:<25} : {:.2} req/s{}",
            "Throughput".bright_white(),
            summary.throughput_rps,
            if summary.ramp_up_secs > 0.0 {
                " (measured window, ramp-up excluded)"
            } else {
                ""
            }
        );
        println!(
            "  {:<25} : {:.2}%",
            "Error Rate".bright_white(),
            if summary.error_rate > 5.0 {
                format!("{:.2}", summary.error_rate).bright_red()
            } else {
                format!("{:.2}", summary.error_rate).bright_green()
            }
        );
        println!(
            "  {:<25} : {:.2}s",
            "Total Duration".bright_white(),
            summary.total_duration_secs
        );
        if summary.ramp_up_secs > 0.0 {
            println!(
                "  {:<25} : {:.2}s",
                "Ramp-up (excluded)".bright_white(),
                summary.ramp_up_secs
            );
            println!(
                "  {:<25} : {:.2}s ({} requests)",
                "Measured Duration".bright_white(),
                summary.measured_duration_secs,
                summary.measured_requests
            );
        }

        if let Some(profile) = &summary.load_profile {
            println!("\n{}", "Load Profile:".bright_green().bold());
            println!("  {:<25} : {}", "Type".bright_white(), profile.kind);
            if let Some(target) = profile.target_rps {
                println!(
                    "  {:<25} : {:.2} req/s",
                    "Target Rate".bright_white(),
                    target
                );
            }
            if let Some(achieved) = profile.achieved_rps {
                println!(
                    "  {:<25} : {:.2} req/s",
                    "Achieved Rate".bright_white(),
                    achieved
                );
            }
            if profile.scheduled_ticks > 0 {
                let saturation =
                    profile.saturated_ticks as f64 / profile.scheduled_ticks as f64 * 100.0;
                println!(
                    "  {:<25} : {} of {} ({:.1}%)",
                    "Saturated Ticks".bright_white(),
                    profile.saturated_ticks,
                    profile.scheduled_ticks,
                    saturation
                );
            }
        }

        if !summary.stages.is_empty() {
            println!("\n{}", "Per-Stage Metrics:".bright_green().bold());
            for stage in &summary.stages {
                println!(
                    "  {:<25} : {} req | {:.2} req/s | p95 {}ms | errors {:.1}%",
                    stage.label.bright_white(),
                    stage.metrics.total_requests,
                    stage.metrics.throughput_rps,
                    stage.metrics.p95_latency_ms,
                    stage.metrics.error_rate
                );
            }
        }

        // Latency percentiles
        println!("\n{}", "Latency Percentiles:".bright_green().bold());
        println!(
            "  {:<25} : {}ms",
            "Min".bright_white(),
            summary.min_latency_ms
        );
        println!(
            "  {:<25} : {}ms",
            "P50 (Median)".bright_white(),
            summary.p50_latency_ms
        );
        println!(
            "  {:<25} : {}ms",
            "P90".bright_white(),
            summary.p90_latency_ms
        );
        println!(
            "  {:<25} : {}ms",
            "P95".bright_white(),
            summary.p95_latency_ms
        );
        println!(
            "  {:<25} : {}ms",
            "P99".bright_white(),
            summary.p99_latency_ms
        );
        println!(
            "  {:<25} : {}ms",
            "Max".bright_white(),
            summary.max_latency_ms
        );
        println!(
            "  {:<25} : {:.2}ms",
            "Mean".bright_white(),
            summary.mean_latency_ms
        );

        println!("\n{}", "═".repeat(70).bright_cyan());
        println!();
    }

    /// Display error message
    pub fn display_error(&self, message: &str) {
        eprintln!("\n{} {}", "❌ Error:".bright_red().bold(), message);
    }

    /// Display warning message
    pub fn display_warning(&self, message: &str) {
        println!("\n{} {}", "⚠️  Warning:".bright_yellow().bold(), message);
    }

    /// Display success message
    pub fn display_success(&self, message: &str) {
        println!("\n{} {}", "✅".bright_green(), message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{OutputConfig, Stage};
    use crate::metrics::{LoadProfileSummary, ScenarioMetricsSummary, StageSummary};
    use chrono::Utc;
    use std::collections::HashMap;

    #[test]
    fn test_terminal_ui_creation() {
        let _ui = TerminalUI::new(30);
        // Test passes if no panic occurs
    }

    fn base_config() -> Config {
        Config {
            target: Some("http://example.com".to_string()),
            method: Some("GET".to_string()),
            headers: HashMap::new(),
            body: None,
            multipart: None,
            scenarios: vec![],
            concurrency: 10,
            duration: "30s".to_string(),
            timeout: "30s".to_string(),
            ramp_up: None,
            load_profile: None,
            think_time: None,
            retry_count: 0,
            retry_delay: None,
            retry_on_status: vec![],
            assertions: None,
            prometheus_port: None,
            prometheus_bind: "127.0.0.1".to_string(),
            live_dashboard: None,
            mode: "async".to_string(),
            output: OutputConfig {
                json: "output.json".to_string(),
                html: "output.html".to_string(),
                csv: None,
                max_results: 0,
            },
        }
    }

    #[test]
    fn test_display_banner_with_stages_profile_does_not_panic() {
        let ui = TerminalUI::new(30);
        let mut config = base_config();
        config.load_profile = Some(LoadProfile::Stages {
            stages: vec![
                Stage {
                    duration: "30s".to_string(),
                    target_concurrency: 10,
                },
                Stage {
                    duration: "1m".to_string(),
                    target_concurrency: 100,
                },
            ],
        });

        ui.display_banner(&config, 90);
    }

    #[test]
    fn test_display_banner_with_arrival_rate_profile_does_not_panic() {
        let ui = TerminalUI::new(30);
        let mut config = base_config();
        config.load_profile = Some(LoadProfile::ArrivalRate {
            target_rps: 200.0,
            duration: "5m".to_string(),
            max_concurrency: 500,
        });

        ui.display_banner(&config, 300);
    }

    #[test]
    fn test_display_summary() {
        let _ui = TerminalUI::new(30);
        let summary = MetricsSummary {
            total_requests: 1000,
            successful_requests: 950,
            failed_requests: 50,
            total_duration_secs: 30.0,
            ramp_up_secs: 0.0,
            measured_duration_secs: 30.0,
            measured_requests: 0,
            throughput_rps: 33.33,
            min_latency_ms: 10,
            max_latency_ms: 500,
            mean_latency_ms: 85.5,
            p50_latency_ms: 75,
            p90_latency_ms: 150,
            p95_latency_ms: 200,
            p99_latency_ms: 350,
            error_rate: 5.0,
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

        // This will print to stdout, but we're just testing it doesn't panic
        _ui.display_summary(&summary);
    }

    #[test]
    fn test_display_summary_with_load_profile_and_stages_does_not_panic() {
        let ui = TerminalUI::new(30);
        let stage_metrics = ScenarioMetricsSummary {
            total_requests: 100,
            successful_requests: 95,
            failed_requests: 5,
            throughput_rps: 10.0,
            min_latency_ms: 5,
            max_latency_ms: 200,
            mean_latency_ms: 40.0,
            p50_latency_ms: 30,
            p90_latency_ms: 80,
            p95_latency_ms: 120,
            p99_latency_ms: 180,
            error_rate: 5.0,
        };
        let summary = MetricsSummary {
            total_requests: 100,
            successful_requests: 95,
            failed_requests: 5,
            total_duration_secs: 10.0,
            ramp_up_secs: 0.0,
            measured_duration_secs: 10.0,
            measured_requests: 100,
            throughput_rps: 10.0,
            min_latency_ms: 5,
            max_latency_ms: 200,
            mean_latency_ms: 40.0,
            p50_latency_ms: 30,
            p90_latency_ms: 80,
            p95_latency_ms: 120,
            p99_latency_ms: 180,
            error_rate: 5.0,
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
                target_rps: Some(50.0),
                achieved_rps: Some(48.5),
                scheduled_ticks: 100,
                saturated_ticks: 3,
            }),
            stages: vec![StageSummary {
                label: "Arrival rate (50.00 req/s)".to_string(),
                target_concurrency: None,
                target_rps: Some(50.0),
                planned_duration_secs: 10.0,
                observed_duration_secs: 10.0,
                metrics: stage_metrics,
            }],
        };

        ui.display_summary(&summary);
    }
}
