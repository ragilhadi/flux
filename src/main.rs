mod client;
mod config;
mod executor;
mod metrics;
mod reporter;
mod ui;

use anyhow::Result;
use config::Config;
use executor::Executor;
use metrics::MetricsCollector;
use reporter::Reporter;
use signal_hook::consts::SIGTERM;
use signal_hook_tokio::Signals;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{error, info};
use ui::TerminalUI;

/// Main entry point
#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    info!("Starting Flux load testing tool");

    // Load configuration
    let config_path = PathBuf::from("/app/config.yaml");
    let config = match Config::from_file(&config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Failed to load configuration: {}", e);
            std::process::exit(1);
        }
    };

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
        let mut signals = Signals::new([SIGTERM]).expect("Failed to create signal handler");
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

    info!("Flux load test completed successfully");
    Ok(())
}
