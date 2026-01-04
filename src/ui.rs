use crate::config::Config;
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

        println!(
            "{:<20} : {} workers",
            "Concurrency".bright_yellow(),
            config.concurrency
        );
        println!("{:<20} : {}s", "Duration".bright_yellow(), duration_secs);
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

        // Performance metrics
        println!("\n{}", "Performance Metrics:".bright_green().bold());
        println!(
            "  {:<25} : {:.2} req/s",
            "Throughput".bright_white(),
            summary.throughput_rps
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

    /// Display success message
    pub fn display_success(&self, message: &str) {
        println!("\n{} {}", "✅".bright_green(), message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn test_terminal_ui_creation() {
        let _ui = TerminalUI::new(30);
        // Test passes if no panic occurs
    }

    #[test]
    fn test_display_summary() {
        let _ui = TerminalUI::new(30);
        let summary = MetricsSummary {
            total_requests: 1000,
            successful_requests: 950,
            failed_requests: 50,
            total_duration_secs: 30.0,
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
        };

        // This will print to stdout, but we're just testing it doesn't panic
        _ui.display_summary(&summary);
    }
}
