mod cancel;
mod client;
mod compare;
mod config;
mod dashboard;
mod executor;
mod metrics;
mod prometheus;
mod redact;
mod reporter;
mod ui;

use anyhow::Result;
use cancel::Cancellation;
use clap::{Args, Parser, Subcommand};
use compare::{Budgets, Threshold};
use config::Config;
use dashboard::LiveDashboard;
use executor::Executor;
use metrics::MetricsCollector;
use prometheus::PrometheusServer;
use reporter::{CsvResultStream, Reporter};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook_tokio::Signals;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{error, info};
use ui::TerminalUI;

#[derive(Debug, Parser)]
#[command(
    name = "flux",
    about = "A configurable HTTP load-testing tool",
    version
)]
struct Cli {
    /// Subcommand to run. Without one, Flux runs a load test.
    #[command(subcommand)]
    command: Option<Command>,

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

#[derive(Debug, Subcommand)]
enum Command {
    /// Compare two JSON reports and enforce regression budgets
    ///
    /// Nothing is executed against the target: both runs must already have
    /// been recorded with --output-json.
    Compare(CompareArgs),
}

/// Regression budgets accept either a percentage of the baseline (`10%`) or an
/// absolute movement in the metric's own unit (`25` / `25ms`).
#[derive(Debug, Args)]
struct CompareArgs {
    /// Baseline JSON report
    baseline: PathBuf,

    /// Candidate JSON report to check against the baseline
    candidate: PathBuf,

    /// Budget for the increase in mean latency
    #[arg(long, value_name = "BUDGET")]
    max_mean_regression: Option<String>,

    /// Budget for the increase in p50 latency
    #[arg(long, value_name = "BUDGET")]
    max_p50_regression: Option<String>,

    /// Budget for the increase in p90 latency
    #[arg(long, value_name = "BUDGET")]
    max_p90_regression: Option<String>,

    /// Budget for the increase in p95 latency
    #[arg(long, value_name = "BUDGET")]
    max_p95_regression: Option<String>,

    /// Budget for the increase in p99 latency
    #[arg(long, value_name = "BUDGET")]
    max_p99_regression: Option<String>,

    /// Budget for the increase in error rate, in percentage points (0.5 means
    /// 0.5pp, for example 1.0% to 1.5%)
    #[arg(long, value_name = "POINTS")]
    max_error_rate_increase: Option<String>,

    /// Budget for the drop in throughput
    #[arg(long, value_name = "BUDGET")]
    max_throughput_drop: Option<String>,

    /// Apply the same budgets to every scenario present in both reports
    #[arg(long)]
    per_scenario_budgets: bool,

    /// Write the comparison as JSON to this path
    #[arg(long, value_name = "PATH")]
    output_json: Option<PathBuf>,

    /// Write the comparison as Markdown to this path, for a CI job summary
    #[arg(long, value_name = "PATH")]
    output_markdown: Option<PathBuf>,
}

impl CompareArgs {
    /// Turn the raw budget strings into parsed thresholds.
    fn budgets(&self) -> Result<Budgets> {
        let threshold = |raw: &Option<String>| -> Result<Option<Threshold>> {
            raw.as_deref().map(Threshold::parse).transpose()
        };

        Ok(Budgets {
            mean_latency: threshold(&self.max_mean_regression)?,
            p50_latency: threshold(&self.max_p50_regression)?,
            p90_latency: threshold(&self.max_p90_regression)?,
            p95_latency: threshold(&self.max_p95_regression)?,
            p99_latency: threshold(&self.max_p99_regression)?,
            error_rate_increase: self
                .max_error_rate_increase
                .as_deref()
                .map(compare::parse_error_rate_budget)
                .transpose()?,
            throughput_drop: threshold(&self.max_throughput_drop)?,
            per_scenario: self.per_scenario_budgets,
        })
    }
}

/// Exit code used when a comparison exceeds a configured regression budget.
const EXIT_BUDGET_EXCEEDED: i32 = 1;

/// Exit code used when a comparison could not be made at all.
const EXIT_COMPARISON_FAILED: i32 = 2;

/// Run `flux compare` and exit with the code CI should act on.
fn run_compare(args: &CompareArgs) -> ! {
    let budgets = match args.budgets() {
        Ok(budgets) => budgets,
        Err(e) => {
            eprintln!("Invalid regression budget: {e:#}");
            std::process::exit(EXIT_COMPARISON_FAILED);
        }
    };

    let comparison = match compare::compare_reports(&args.baseline, &args.candidate, budgets) {
        Ok(comparison) => comparison,
        Err(e) => {
            eprintln!("Failed to compare reports: {e:#}");
            std::process::exit(EXIT_COMPARISON_FAILED);
        }
    };

    print!("{}", compare::render_terminal(&comparison));

    if let Some(path) = &args.output_json {
        let rendered = match compare::render_json(&comparison) {
            Ok(rendered) => rendered,
            Err(e) => {
                eprintln!("Failed to render the JSON comparison: {e:#}");
                std::process::exit(EXIT_COMPARISON_FAILED);
            }
        };
        if let Err(e) = compare::write_artifact(path, &rendered) {
            eprintln!("Failed to write the JSON comparison: {e:#}");
            std::process::exit(EXIT_COMPARISON_FAILED);
        }
        println!("JSON comparison saved to: {}", path.display());
    }

    if let Some(path) = &args.output_markdown {
        if let Err(e) = compare::write_artifact(path, &compare::render_markdown(&comparison)) {
            eprintln!("Failed to write the Markdown comparison: {e:#}");
            std::process::exit(EXIT_COMPARISON_FAILED);
        }
        println!("Markdown comparison saved to: {}", path.display());
    }

    if comparison.passed() {
        std::process::exit(0);
    }
    std::process::exit(EXIT_BUDGET_EXCEEDED);
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

    // Comparing two recorded reports sends no traffic anywhere, so it returns
    // before any configuration is loaded or any executor is built.
    if let Some(Command::Compare(args)) = &cli.command {
        run_compare(args);
    }

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

    // The planned wall-clock length of the run: duration + ramp-up for a
    // fixed-concurrency profile, or the sum of stage/arrival-rate durations
    // when `load_profile` is configured.
    let total_secs = match config.total_planned_secs() {
        Ok(secs) => secs,
        Err(e) => {
            eprintln!("Failed to compute planned test duration: {}", e);
            std::process::exit(1);
        }
    };

    // Per-request CSV rows are streamed to disk as they are produced, so the
    // full-fidelity export never has to be held in memory.
    let csv_stream = match &config.output.csv {
        Some(path) => match CsvResultStream::create(path) {
            Ok(stream) => Some(stream),
            Err(e) => {
                eprintln!("Failed to open CSV output {path}: {e}");
                std::process::exit(1);
            }
        },
        None => None,
    };

    // Create metrics collector
    let metrics = Arc::new(MetricsCollector::with_retention(
        config.output.max_results,
        csv_stream.as_ref().map(|stream| stream.sender()),
    ));

    // Create terminal UI
    let ui = TerminalUI::new(total_secs);
    ui.display_banner(&config, duration_secs);

    // Setup graceful shutdown: SIGINT and SIGTERM share one cancellation path.
    let cancellation = Cancellation::new();
    let signal_cancellation = cancellation.clone();

    tokio::spawn(async move {
        use futures::stream::StreamExt;
        let mut signals = Signals::new([SIGTERM, SIGINT]).expect("Failed to create signal handler");
        if let Some(signal) = signals.next().await {
            let name = match signal {
                SIGINT => "SIGINT",
                SIGTERM => "SIGTERM",
                _ => "signal",
            };
            info!("Received {name}; stopping the load test and generating reports");
            signal_cancellation.cancel();
        }
    });

    // Create executor
    let executor = match Executor::new(config.clone(), Arc::clone(&metrics), cancellation.clone()) {
        Ok(exec) => exec,
        Err(e) => {
            ui.display_error(&format!("Failed to create executor: {}", e));
            std::process::exit(1);
        }
    };

    let prometheus_server = match config.prometheus_port {
        Some(port) => match PrometheusServer::start(port, Arc::clone(&metrics)).await {
            Ok(server) => {
                info!(
                    "Prometheus metrics available at http://{}/metrics",
                    server.local_addr()
                );
                Some(server)
            }
            Err(e) => {
                ui.display_error(&format!("Failed to start Prometheus endpoint: {e}"));
                std::process::exit(1);
            }
        },
        None => None,
    };

    // Optional live dashboard. No socket exists unless it is configured.
    let live_dashboard =
        match LiveDashboard::maybe_start(&config, Arc::clone(&metrics), cancellation.clone()).await
        {
            Ok(Some(server)) => {
                info!("Live dashboard available at {}", server.url());
                Some(server)
            }
            Ok(None) => None,
            Err(e) => {
                ui.display_error(&format!("Failed to start live dashboard: {e}"));
                std::process::exit(1);
            }
        };

    // Start live metrics update task
    let metrics_clone = Arc::clone(&metrics);
    let ui_cancellation = cancellation.clone();
    let ui_handle = tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(1));
        let mut elapsed = 0u64;

        loop {
            tokio::select! {
                biased;
                _ = ui_cancellation.cancelled() => break,
                _ = ticker.tick() => {}
            }
            elapsed += 1;

            let live_metrics = metrics_clone.get_live_metrics();
            ui.update_progress(elapsed, &live_metrics);

            if elapsed >= total_secs {
                break;
            }
        }

        ui.finish_progress();
    });

    // Run the load test
    info!("Starting load test execution");
    let execution_result = executor.run(duration_secs).await;

    if let Some(server) = prometheus_server {
        if let Err(e) = server.shutdown().await {
            error!("Failed to stop Prometheus endpoint cleanly: {}", e);
        }
    }

    // The run is over — whether it finished or was cancelled — so the dashboard
    // stops listening instead of serving a frozen view of a dead test.
    if let Some(server) = live_dashboard {
        if let Err(e) = server.shutdown().await {
            error!("Failed to stop the live dashboard cleanly: {}", e);
        }
    }

    if let Err(e) = execution_result {
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
    let ui = TerminalUI::new(total_secs);
    if cancellation.is_cancelled() {
        ui.display_warning("Load test cancelled early; reporting completed requests only");
    }
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

    if let (Some(stream), Some(csv_path)) = (csv_stream, &config.output.csv) {
        // The collector holds the last sender; release it so the writer task
        // sees the channel close, flushes and returns.
        metrics.close_result_stream();
        match stream.finish().await {
            Ok(rows) => {
                ui.display_success(&format!("CSV report saved to: {csv_path} ({rows} rows)"))
            }
            Err(e) => error!("Failed to generate CSV report: {}", e),
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
        assert!(cli.command.is_none());
    }

    fn compare_args(argv: &[&str]) -> CompareArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Some(Command::Compare(args)) => args,
            other => panic!("expected a compare subcommand, got {other:?}"),
        }
    }

    #[test]
    fn test_compare_subcommand_parses_reports_and_budgets() {
        let args = compare_args(&[
            "flux",
            "compare",
            "artifacts/baseline.json",
            "artifacts/current.json",
            "--max-p95-regression",
            "10%",
            "--max-error-rate-increase",
            "0.5",
        ]);

        assert_eq!(args.baseline, PathBuf::from("artifacts/baseline.json"));
        assert_eq!(args.candidate, PathBuf::from("artifacts/current.json"));

        let budgets = args.budgets().unwrap();
        assert_eq!(budgets.p95_latency, Some(Threshold::Percent(10.0)));
        assert_eq!(budgets.error_rate_increase, Some(0.5));
        assert!(!budgets.per_scenario);
    }

    #[test]
    fn test_compare_subcommand_parses_every_budget() {
        let args = compare_args(&[
            "flux",
            "compare",
            "baseline.json",
            "current.json",
            "--max-mean-regression",
            "5%",
            "--max-p50-regression",
            "10ms",
            "--max-p90-regression",
            "15",
            "--max-p99-regression",
            "20%",
            "--max-throughput-drop",
            "8%",
            "--per-scenario-budgets",
            "--output-json",
            "comparison.json",
            "--output-markdown",
            "comparison.md",
        ]);

        let budgets = args.budgets().unwrap();
        assert_eq!(budgets.mean_latency, Some(Threshold::Percent(5.0)));
        assert_eq!(budgets.p50_latency, Some(Threshold::Absolute(10.0)));
        assert_eq!(budgets.p90_latency, Some(Threshold::Absolute(15.0)));
        assert_eq!(budgets.p99_latency, Some(Threshold::Percent(20.0)));
        assert_eq!(budgets.throughput_drop, Some(Threshold::Percent(8.0)));
        assert!(budgets.per_scenario);
        assert_eq!(args.output_json, Some(PathBuf::from("comparison.json")));
        assert_eq!(args.output_markdown, Some(PathBuf::from("comparison.md")));
    }

    #[test]
    fn test_compare_rejects_an_unparseable_budget() {
        let args = compare_args(&[
            "flux",
            "compare",
            "baseline.json",
            "current.json",
            "--max-p95-regression",
            "ten percent",
        ]);

        assert!(args.budgets().is_err());
    }

    #[test]
    fn test_compare_requires_both_reports() {
        assert!(Cli::try_parse_from(["flux", "compare", "baseline.json"]).is_err());
    }
}
