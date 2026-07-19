mod client;
mod config;
mod executor;
mod metrics;
mod reporter;
mod ui;

use anyhow::Result;
use clap::Parser;
use config::Config;
use executor::Executor;
use metrics::MetricsCollector;
use reporter::Reporter;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook_tokio::Signals;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{error, info};
use ui::TerminalUI;

#[derive(Debug, Parser)]
#[command(name = "flux", about = "A configurable HTTP load-testing tool")]
struct Cli {
    /// Path to the YAML configuration file
    #[arg(short, long, env = "FLUX_CONFIG")]
    config: Option<PathBuf>,

    /// Override configured worker concurrency
    #[arg(short = 'n', long)]
    concurrency: Option<usize>,

    /// Override configured test duration (for example, 30s or 5m)
    #[arg(short, long)]
    duration: Option<String>,

    /// Override the JSON report output path
    #[arg(long)]
    output_json: Option<String>,

    /// Override the HTML report output path
    #[arg(long)]
    output_html: Option<String>,

    /// Override the CSV report output path and enable CSV output
    #[arg(long)]
    output_csv: Option<String>,
}

/// Resolve the configuration file path from CLI args, env var, or default.
fn resolve_config_path(cli: &Cli) -> PathBuf {
    cli.config
        .clone()
        .unwrap_or_else(|| PathBuf::from("/app/config.yaml"))
}

/// Main entry point
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    info!("Starting Flux load testing tool");

    // Load configuration
    let config_path = resolve_config_path(&cli);
    let mut config = match Config::from_file(&config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Failed to load configuration: {}", e);
            std::process::exit(1);
        }
    };

    if let Err(e) = config.apply_overrides(
        cli.concurrency,
        cli.duration,
        cli.output_json,
        cli.output_html,
        cli.output_csv,
    ) {
        eprintln!("Invalid configuration override: {e}");
        std::process::exit(1);
    }

    // Parse duration
    let duration_secs = match config.parse_duration() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to parse duration: {}", e);
            std::process::exit(1);
        }
    };

    // Create metrics collector
    let metrics = Arc::new(MetricsCollector::new());

    // Create terminal UI
    let ui = TerminalUI::new(duration_secs);
    ui.display_banner(&config, duration_secs);

    // Setup graceful shutdown
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_flag_clone = Arc::clone(&shutdown_flag);

    tokio::spawn(async move {
        use futures::stream::StreamExt;
        let mut signals = Signals::new([SIGTERM, SIGINT]).expect("Failed to create signal handler");
        if let Some(signal) = signals.next().await {
            info!("Received signal: {:?}", signal);
            shutdown_flag_clone.store(true, Ordering::SeqCst);
        }
    });

    // Create executor
    let executor = match Executor::new(config.clone(), Arc::clone(&metrics)) {
        Ok(exec) => exec,
        Err(e) => {
            ui.display_error(&format!("Failed to create executor: {}", e));
            std::process::exit(1);
        }
    };

    // Start live metrics update task
    let metrics_clone = Arc::clone(&metrics);
    let ui_handle = tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(1));
        let mut elapsed = 0u64;

        loop {
            ticker.tick().await;
            elapsed += 1;

            let live_metrics = metrics_clone.get_live_metrics();
            ui.update_progress(elapsed, &live_metrics);

            if elapsed >= duration_secs {
                break;
            }
        }

        ui.finish_progress();
    });

    // Run the load test
    info!("Starting load test execution");
    if let Err(e) = executor.run(duration_secs).await {
        error!("Load test execution failed: {}", e);
        std::process::exit(1);
    }

    // Wait for UI updates to complete
    let _ = ui_handle.await;

    // Generate summary
    info!("Generating summary");
    let summary = metrics.generate_summary();
    let results = metrics.get_results();
    let assertion_failures = config.evaluate_assertions(&summary);

    // Display summary in terminal
    let ui = TerminalUI::new(duration_secs);
    ui.display_summary(&summary);

    // Generate reports
    info!("Generating reports");
    let reporter = Reporter::new(summary, results);

    if let Err(e) = reporter.generate_json(&config.output.json) {
        error!("Failed to generate JSON report: {}", e);
    } else {
        ui.display_success(&format!("JSON report saved to: {}", config.output.json));
    }

    if let Err(e) = reporter.generate_html(&config.output.html) {
        error!("Failed to generate HTML report: {}", e);
    } else {
        ui.display_success(&format!("HTML report saved to: {}", config.output.html));
    }

    if let Some(csv_path) = &config.output.csv {
        if let Err(e) = reporter.generate_csv(csv_path) {
            error!("Failed to generate CSV report: {}", e);
        } else {
            ui.display_success(&format!("CSV report saved to: {csv_path}"));
        }
    }

    if !assertion_failures.is_empty() {
        for failure in &assertion_failures {
            ui.display_error(&format!("Assertion failed: {failure}"));
        }
        std::process::exit(1);
    }

    info!("Flux load test completed successfully");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_parses_all_overrides() {
        let cli = Cli::try_parse_from([
            "flux",
            "--config",
            "test.yaml",
            "-n",
            "25",
            "--duration",
            "1m",
            "--output-json",
            "result.json",
            "--output-html",
            "report.html",
            "--output-csv",
            "results.csv",
        ])
        .unwrap();

        assert_eq!(cli.config, Some(PathBuf::from("test.yaml")));
        assert_eq!(cli.concurrency, Some(25));
        assert_eq!(cli.duration.as_deref(), Some("1m"));
        assert_eq!(cli.output_json.as_deref(), Some("result.json"));
        assert_eq!(cli.output_html.as_deref(), Some("report.html"));
        assert_eq!(cli.output_csv.as_deref(), Some("results.csv"));
    }
}
