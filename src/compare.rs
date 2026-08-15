//! Comparison of two Flux JSON reports.
//!
//! A comparison never re-runs a load test: it reads a baseline report and a
//! candidate report from disk, computes aggregate, per-scenario and
//! status-code deltas, and decides whether the configured regression budgets
//! were exceeded.
//!
//! Two runs of the same test never produce identical numbers, so a single
//! comparison is an operational check against a budget, not a statistical
//! test. [`VARIANCE_NOTE`] is rendered with every comparison to say so.

use anyhow::{bail, Context, Result};
use colored::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::Path;

/// Schema version written into every JSON report produced by this build.
///
/// Bump this whenever the report layout changes in a way a comparison cares
/// about. Reports written before versioning existed carry no field at all and
/// are read as [`LEGACY_SCHEMA_VERSION`].
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// Version assigned to reports written before the schema was versioned.
pub const LEGACY_SCHEMA_VERSION: u32 = 0;

/// Caveat rendered with every comparison, in every output format.
pub const VARIANCE_NOTE: &str = "Repeat runs of the same test vary. A comparison of two runs is a \
budget check, not a statistical test: prefer a baseline recorded on the same \
hardware and re-run before acting on a result close to its budget.";

/// How a metric moves when things get worse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Latency, error rate: a higher candidate is a regression.
    LowerIsBetter,
    /// Throughput: a lower candidate is a regression.
    HigherIsBetter,
}

/// Unit of a compared metric, so every delta is rendered with its unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Milliseconds,
    RequestsPerSecond,
    /// Percentage points — the metric is itself a percentage.
    PercentagePoints,
    Requests,
}

impl Unit {
    fn suffix(self) -> &'static str {
        match self {
            Unit::Milliseconds => "ms",
            Unit::RequestsPerSecond => "req/s",
            Unit::PercentagePoints => "pp",
            Unit::Requests => "",
        }
    }

    fn format(self, value: f64) -> String {
        match self {
            Unit::Milliseconds => format!("{value:.0}ms"),
            Unit::RequestsPerSecond => format!("{value:.2} req/s"),
            Unit::PercentagePoints => format!("{value:.2}%"),
            Unit::Requests => format!("{value:.0}"),
        }
    }

    fn format_delta(self, value: f64) -> String {
        let sign = if value > 0.0 { "+" } else { "" };
        match self {
            Unit::Milliseconds => format!("{sign}{value:.0}ms"),
            Unit::RequestsPerSecond => format!("{sign}{value:.2} req/s"),
            Unit::PercentagePoints => format!("{sign}{value:.2}pp"),
            Unit::Requests => format!("{sign}{value:.0}"),
        }
    }
}

/// A regression budget, either relative to the baseline or absolute.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Threshold {
    /// Percentage of the baseline value, for example `10%`.
    Percent(f64),
    /// Absolute movement in the metric's own unit.
    Absolute(f64),
}

impl Threshold {
    /// Parse `10%`, `25ms`, `1.5 req/s` or a bare number.
    ///
    /// A bare number is absolute, in the metric's own unit.
    pub fn parse(raw: &str) -> Result<Self> {
        let text = raw.trim();
        if text.is_empty() {
            bail!("budget is empty; use a value such as 10% or 25");
        }

        if let Some(percent) = text.strip_suffix('%') {
            let value: f64 = percent
                .trim()
                .parse()
                .with_context(|| format!("invalid percentage budget '{raw}'"))?;
            return Self::checked(Threshold::Percent(value), raw);
        }

        let numeric = text
            .trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == '/' || c.is_whitespace());
        let value: f64 = numeric
            .trim()
            .parse()
            .with_context(|| format!("invalid budget '{raw}'; use a value such as 10% or 25"))?;
        Self::checked(Threshold::Absolute(value), raw)
    }

    fn checked(threshold: Threshold, raw: &str) -> Result<Self> {
        let value = match threshold {
            Threshold::Percent(value) | Threshold::Absolute(value) => value,
        };
        if !value.is_finite() || value < 0.0 {
            bail!("budget '{raw}' must be a finite, non-negative number");
        }
        Ok(threshold)
    }

    fn render(self, unit: Unit) -> String {
        match self {
            Threshold::Percent(value) => format!("{value}%"),
            Threshold::Absolute(value) => match unit {
                Unit::Requests => format!("{value}"),
                _ => format!("{value}{}", unit.suffix()),
            },
        }
    }
}

impl fmt::Display for Threshold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Threshold::Percent(value) => write!(f, "{value}%"),
            Threshold::Absolute(value) => write!(f, "{value}"),
        }
    }
}

/// Regression budgets applied to a comparison.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Budgets {
    pub mean_latency: Option<Threshold>,
    pub p50_latency: Option<Threshold>,
    pub p90_latency: Option<Threshold>,
    pub p95_latency: Option<Threshold>,
    pub p99_latency: Option<Threshold>,
    /// Absolute increase in error rate, in percentage points.
    pub error_rate_increase: Option<f64>,
    pub throughput_drop: Option<Threshold>,
    /// Apply the same budgets to every scenario present in both reports.
    pub per_scenario: bool,
}

impl Budgets {
    /// True when no budget is configured, so the comparison is informational.
    pub fn is_empty(&self) -> bool {
        self.mean_latency.is_none()
            && self.p50_latency.is_none()
            && self.p90_latency.is_none()
            && self.p95_latency.is_none()
            && self.p99_latency.is_none()
            && self.error_rate_increase.is_none()
            && self.throughput_drop.is_none()
    }

    fn for_metric(&self, metric: Metric) -> Option<Threshold> {
        match metric {
            Metric::MeanLatency => self.mean_latency,
            Metric::P50Latency => self.p50_latency,
            Metric::P90Latency => self.p90_latency,
            Metric::P95Latency => self.p95_latency,
            Metric::P99Latency => self.p99_latency,
            Metric::ErrorRate => self.error_rate_increase.map(Threshold::Absolute),
            Metric::Throughput => self.throughput_drop,
            _ => None,
        }
    }
}

/// Parse the `--max-error-rate-increase` budget.
///
/// Error rate is already a percentage, so its budget is always in percentage
/// points; a `%` suffix would be ambiguous and is rejected rather than guessed.
pub fn parse_error_rate_budget(raw: &str) -> Result<f64> {
    if raw.trim().ends_with('%') {
        bail!(
            "error-rate budgets are absolute percentage points, so '{raw}' is ambiguous; \
             pass the number without a % sign (0.5 means 0.5 percentage points)"
        );
    }
    match Threshold::parse(raw)? {
        Threshold::Absolute(value) => Ok(value),
        Threshold::Percent(_) => unreachable!("percent form is rejected above"),
    }
}

/// A metric compared between two reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    TotalRequests,
    SuccessfulRequests,
    FailedRequests,
    Throughput,
    ErrorRate,
    MeanLatency,
    P50Latency,
    P90Latency,
    P95Latency,
    P99Latency,
}

impl Metric {
    fn label(self) -> &'static str {
        match self {
            Metric::TotalRequests => "total requests",
            Metric::SuccessfulRequests => "successful requests",
            Metric::FailedRequests => "failed requests",
            Metric::Throughput => "throughput",
            Metric::ErrorRate => "error rate",
            Metric::MeanLatency => "mean latency",
            Metric::P50Latency => "p50 latency",
            Metric::P90Latency => "p90 latency",
            Metric::P95Latency => "p95 latency",
            Metric::P99Latency => "p99 latency",
        }
    }

    fn unit(self) -> Unit {
        match self {
            Metric::TotalRequests | Metric::SuccessfulRequests | Metric::FailedRequests => {
                Unit::Requests
            }
            Metric::Throughput => Unit::RequestsPerSecond,
            Metric::ErrorRate => Unit::PercentagePoints,
            _ => Unit::Milliseconds,
        }
    }

    fn direction(self) -> Direction {
        match self {
            Metric::Throughput | Metric::SuccessfulRequests | Metric::TotalRequests => {
                Direction::HigherIsBetter
            }
            _ => Direction::LowerIsBetter,
        }
    }
}

/// One metric compared between the baseline and the candidate.
#[derive(Debug, Clone, Serialize)]
pub struct MetricDelta {
    pub metric: Metric,
    pub label: String,
    pub unit: Unit,
    pub direction: Direction,
    pub baseline: f64,
    pub candidate: f64,
    /// `candidate - baseline`, in the metric's own unit.
    pub delta: f64,
    /// Relative change against the baseline. `None` when the baseline is zero,
    /// because the denominator does not exist — it is never reported as 0%.
    pub percent_change: Option<f64>,
    /// Movement in the bad direction: positive for a regression, negative for
    /// an improvement, whichever way the metric points.
    pub regression: f64,
}

impl MetricDelta {
    fn new(metric: Metric, baseline: f64, candidate: f64) -> Self {
        let delta = candidate - baseline;
        let percent_change = if baseline == 0.0 {
            None
        } else {
            Some(delta / baseline.abs() * 100.0)
        };
        let regression = match metric.direction() {
            Direction::LowerIsBetter => delta,
            Direction::HigherIsBetter => -delta,
        };

        Self {
            metric,
            label: metric.label().to_string(),
            unit: metric.unit(),
            direction: metric.direction(),
            baseline,
            candidate,
            delta,
            percent_change,
            regression,
        }
    }

    /// Relative movement in the bad direction, as a percentage of the baseline.
    fn regression_percent(&self) -> Option<f64> {
        self.percent_change.map(|percent| match self.direction {
            Direction::LowerIsBetter => percent,
            Direction::HigherIsBetter => -percent,
        })
    }

    fn rendered_change(&self) -> String {
        match self.percent_change {
            Some(percent) => format!("{}{percent:.1}%", if percent > 0.0 { "+" } else { "" }),
            None if self.delta == 0.0 => "0.0%".to_string(),
            // A zero baseline has no denominator, so there is no percentage to
            // report. Saying so beats printing a fabricated number.
            None => "n/a (baseline 0)".to_string(),
        }
    }

    /// Decide this delta against a budget, if one is configured for it.
    fn evaluate(&self, scope: &str, budget: Option<Threshold>) -> Option<Violation> {
        let budget = budget?;
        let exceeded = match budget {
            Threshold::Absolute(limit) => self.regression > limit,
            Threshold::Percent(limit) => match self.regression_percent() {
                Some(percent) => percent > limit,
                // No baseline to be relative to: a move in the bad direction
                // cannot be sized, so it is treated as exceeding any budget.
                None => self.regression > 0.0,
            },
        };

        if !exceeded {
            return None;
        }

        // The observed movement is quoted the way the metric actually moved —
        // a throughput regression is a drop, not a positive number — so the
        // message matches what the tables above it show.
        let observed = match (budget, self.percent_change) {
            (Threshold::Percent(_), Some(_)) => {
                format!(
                    "{} ({})",
                    self.unit.format_delta(self.delta),
                    self.rendered_change()
                )
            }
            (Threshold::Percent(_), None) => format!(
                "{} from a zero baseline (percentage undefined)",
                self.unit.format_delta(self.delta)
            ),
            (Threshold::Absolute(_), _) => self.unit.format_delta(self.delta),
        };

        Some(Violation {
            scope: scope.to_string(),
            metric: self.metric,
            label: self.label.clone(),
            direction: self.direction,
            budget: budget.render(self.unit),
            observed,
            baseline: self.baseline,
            candidate: self.candidate,
        })
    }
}

/// A budget that the candidate exceeded.
#[derive(Debug, Clone, Serialize)]
pub struct Violation {
    /// `aggregate`, or the name of a scenario.
    pub scope: String,
    pub metric: Metric,
    pub label: String,
    pub direction: Direction,
    pub budget: String,
    pub observed: String,
    pub baseline: f64,
    pub candidate: f64,
}

impl Violation {
    /// The word for a move in the bad direction for this metric.
    fn movement(&self) -> &'static str {
        match self.direction {
            Direction::LowerIsBetter => "increase",
            Direction::HigherIsBetter => "drop",
        }
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}: {} exceeds the {} {} budget",
            self.scope,
            self.label,
            self.observed,
            self.budget,
            self.movement()
        )
    }
}

/// Deltas for one scenario present in both reports.
#[derive(Debug, Clone, Serialize)]
pub struct ScenarioComparison {
    pub name: String,
    pub metrics: Vec<MetricDelta>,
}

/// One response status, compared between the two runs.
#[derive(Debug, Clone, Serialize)]
pub struct StatusCodeDelta {
    /// Response status. `0` counts requests that never got a response.
    pub status: u16,
    pub baseline_count: usize,
    pub candidate_count: usize,
    pub delta: i64,
    /// Share of the baseline run's requests, `None` when it made none.
    pub baseline_share_percent: Option<f64>,
    /// Share of the candidate run's requests, `None` when it made none.
    pub candidate_share_percent: Option<f64>,
}

/// A full comparison of two reports.
#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    pub baseline_path: String,
    pub candidate_path: String,
    pub baseline_schema_version: u32,
    pub candidate_schema_version: u32,
    pub aggregate: Vec<MetricDelta>,
    pub scenarios: Vec<ScenarioComparison>,
    /// Scenarios in the candidate that the baseline never ran.
    pub added_scenarios: Vec<String>,
    /// Scenarios in the baseline that the candidate no longer runs.
    pub removed_scenarios: Vec<String>,
    /// Status distribution, or `None` when either report predates status
    /// recording and therefore has nothing to compare.
    pub status_codes: Option<Vec<StatusCodeDelta>>,
    pub budgets: Budgets,
    pub violations: Vec<Violation>,
    pub variance_note: String,
}

impl Comparison {
    /// True when no configured budget was exceeded.
    pub fn passed(&self) -> bool {
        self.violations.is_empty()
    }

    /// Budgets shown against per-scenario rows.
    ///
    /// Unless per-scenario budgets were asked for, the aggregate budgets do not
    /// apply to a single scenario, so the scenario tables show none rather than
    /// implying a limit that is never enforced.
    fn scenario_budgets(&self) -> Budgets {
        if self.budgets.per_scenario {
            self.budgets
        } else {
            Budgets::default()
        }
    }
}

/// A report as the comparison reads it.
///
/// Only the fields a comparison needs are modelled, and everything added after
/// the first schema version is optional, so a report written by an older Flux
/// still compares cleanly.
#[derive(Debug, Clone, Deserialize)]
struct ReportDocument {
    #[serde(default = "legacy_schema_version")]
    schema_version: u32,
    summary: ReportSummary,
}

#[derive(Debug, Clone, Deserialize)]
struct ReportSummary {
    total_requests: usize,
    successful_requests: usize,
    failed_requests: usize,
    throughput_rps: f64,
    mean_latency_ms: f64,
    p50_latency_ms: u64,
    p90_latency_ms: u64,
    p95_latency_ms: u64,
    p99_latency_ms: u64,
    error_rate: f64,
    #[serde(default)]
    per_scenario: BTreeMap<String, ReportScenarioSummary>,
    /// Absent in reports written before status distribution was recorded, which
    /// is different from a run that recorded no statuses at all.
    #[serde(default)]
    status_codes: Option<BTreeMap<u16, usize>>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReportScenarioSummary {
    total_requests: usize,
    successful_requests: usize,
    failed_requests: usize,
    throughput_rps: f64,
    mean_latency_ms: f64,
    p50_latency_ms: u64,
    p90_latency_ms: u64,
    p95_latency_ms: u64,
    p99_latency_ms: u64,
    error_rate: f64,
}

/// Version of a report that carries no `schema_version` field.
fn legacy_schema_version() -> u32 {
    LEGACY_SCHEMA_VERSION
}

/// Read a JSON report, rejecting only schemas this build cannot understand.
fn load_report(path: &Path) -> Result<ReportDocument> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read report {}", path.display()))?;
    let report: ReportDocument = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse report {}", path.display()))?;

    if report.schema_version > REPORT_SCHEMA_VERSION {
        bail!(
            "report {} uses schema version {}, but this build of Flux understands up to version {}; \
             upgrade Flux to compare it",
            path.display(),
            report.schema_version,
            REPORT_SCHEMA_VERSION
        );
    }

    Ok(report)
}

/// Compare a baseline report with a candidate report on disk.
pub fn compare_reports(
    baseline_path: &Path,
    candidate_path: &Path,
    budgets: Budgets,
) -> Result<Comparison> {
    let baseline = load_report(baseline_path)?;
    let candidate = load_report(candidate_path)?;

    Ok(build_comparison(
        &baseline_path.display().to_string(),
        &candidate_path.display().to_string(),
        &baseline,
        &candidate,
        budgets,
    ))
}

fn build_comparison(
    baseline_path: &str,
    candidate_path: &str,
    baseline: &ReportDocument,
    candidate: &ReportDocument,
    budgets: Budgets,
) -> Comparison {
    let aggregate = aggregate_deltas(&baseline.summary, &candidate.summary);

    let baseline_names: BTreeSet<&String> = baseline.summary.per_scenario.keys().collect();
    let candidate_names: BTreeSet<&String> = candidate.summary.per_scenario.keys().collect();

    let scenarios: Vec<ScenarioComparison> = baseline_names
        .intersection(&candidate_names)
        .map(|name| ScenarioComparison {
            name: (*name).clone(),
            metrics: scenario_deltas(
                &baseline.summary.per_scenario[*name],
                &candidate.summary.per_scenario[*name],
            ),
        })
        .collect();

    let added_scenarios: Vec<String> = candidate_names
        .difference(&baseline_names)
        .map(|name| (*name).clone())
        .collect();
    let removed_scenarios: Vec<String> = baseline_names
        .difference(&candidate_names)
        .map(|name| (*name).clone())
        .collect();

    let status_codes = match (
        baseline.summary.status_codes.as_ref(),
        candidate.summary.status_codes.as_ref(),
    ) {
        (Some(before), Some(after)) => Some(status_code_deltas(
            before,
            after,
            baseline.summary.total_requests,
            candidate.summary.total_requests,
        )),
        // One side predates status recording, so there is no distribution to
        // compare. Reporting nothing beats reporting every status as new.
        _ => None,
    };

    let mut violations: Vec<Violation> = aggregate
        .iter()
        .filter_map(|delta| delta.evaluate("aggregate", budgets.for_metric(delta.metric)))
        .collect();

    if budgets.per_scenario {
        for scenario in &scenarios {
            violations.extend(scenario.metrics.iter().filter_map(|delta| {
                delta.evaluate(
                    &format!("scenario {}", scenario.name),
                    budgets.for_metric(delta.metric),
                )
            }));
        }
    }

    Comparison {
        baseline_path: baseline_path.to_string(),
        candidate_path: candidate_path.to_string(),
        baseline_schema_version: baseline.schema_version,
        candidate_schema_version: candidate.schema_version,
        aggregate,
        scenarios,
        added_scenarios,
        removed_scenarios,
        status_codes,
        budgets,
        violations,
        variance_note: VARIANCE_NOTE.to_string(),
    }
}

fn aggregate_deltas(baseline: &ReportSummary, candidate: &ReportSummary) -> Vec<MetricDelta> {
    vec![
        MetricDelta::new(
            Metric::TotalRequests,
            baseline.total_requests as f64,
            candidate.total_requests as f64,
        ),
        MetricDelta::new(
            Metric::SuccessfulRequests,
            baseline.successful_requests as f64,
            candidate.successful_requests as f64,
        ),
        MetricDelta::new(
            Metric::FailedRequests,
            baseline.failed_requests as f64,
            candidate.failed_requests as f64,
        ),
        MetricDelta::new(
            Metric::Throughput,
            baseline.throughput_rps,
            candidate.throughput_rps,
        ),
        MetricDelta::new(Metric::ErrorRate, baseline.error_rate, candidate.error_rate),
        MetricDelta::new(
            Metric::MeanLatency,
            baseline.mean_latency_ms,
            candidate.mean_latency_ms,
        ),
        MetricDelta::new(
            Metric::P50Latency,
            baseline.p50_latency_ms as f64,
            candidate.p50_latency_ms as f64,
        ),
        MetricDelta::new(
            Metric::P90Latency,
            baseline.p90_latency_ms as f64,
            candidate.p90_latency_ms as f64,
        ),
        MetricDelta::new(
            Metric::P95Latency,
            baseline.p95_latency_ms as f64,
            candidate.p95_latency_ms as f64,
        ),
        MetricDelta::new(
            Metric::P99Latency,
            baseline.p99_latency_ms as f64,
            candidate.p99_latency_ms as f64,
        ),
    ]
}

fn scenario_deltas(
    baseline: &ReportScenarioSummary,
    candidate: &ReportScenarioSummary,
) -> Vec<MetricDelta> {
    vec![
        MetricDelta::new(
            Metric::TotalRequests,
            baseline.total_requests as f64,
            candidate.total_requests as f64,
        ),
        MetricDelta::new(
            Metric::SuccessfulRequests,
            baseline.successful_requests as f64,
            candidate.successful_requests as f64,
        ),
        MetricDelta::new(
            Metric::FailedRequests,
            baseline.failed_requests as f64,
            candidate.failed_requests as f64,
        ),
        MetricDelta::new(
            Metric::Throughput,
            baseline.throughput_rps,
            candidate.throughput_rps,
        ),
        MetricDelta::new(Metric::ErrorRate, baseline.error_rate, candidate.error_rate),
        MetricDelta::new(
            Metric::MeanLatency,
            baseline.mean_latency_ms,
            candidate.mean_latency_ms,
        ),
        MetricDelta::new(
            Metric::P50Latency,
            baseline.p50_latency_ms as f64,
            candidate.p50_latency_ms as f64,
        ),
        MetricDelta::new(
            Metric::P90Latency,
            baseline.p90_latency_ms as f64,
            candidate.p90_latency_ms as f64,
        ),
        MetricDelta::new(
            Metric::P95Latency,
            baseline.p95_latency_ms as f64,
            candidate.p95_latency_ms as f64,
        ),
        MetricDelta::new(
            Metric::P99Latency,
            baseline.p99_latency_ms as f64,
            candidate.p99_latency_ms as f64,
        ),
    ]
}

fn status_code_deltas(
    baseline: &BTreeMap<u16, usize>,
    candidate: &BTreeMap<u16, usize>,
    baseline_total: usize,
    candidate_total: usize,
) -> Vec<StatusCodeDelta> {
    let statuses: BTreeSet<u16> = baseline.keys().chain(candidate.keys()).copied().collect();

    statuses
        .into_iter()
        .map(|status| {
            let baseline_count = baseline.get(&status).copied().unwrap_or(0);
            let candidate_count = candidate.get(&status).copied().unwrap_or(0);
            StatusCodeDelta {
                status,
                baseline_count,
                candidate_count,
                delta: candidate_count as i64 - baseline_count as i64,
                baseline_share_percent: share_percent(baseline_count, baseline_total),
                candidate_share_percent: share_percent(candidate_count, candidate_total),
            }
        })
        .collect()
}

/// Share of a run's requests, or `None` when the run made none.
fn share_percent(count: usize, total: usize) -> Option<f64> {
    if total == 0 {
        None
    } else {
        Some(count as f64 / total as f64 * 100.0)
    }
}

fn format_share(share: Option<f64>) -> String {
    match share {
        Some(percent) => format!("{percent:.1}%"),
        None => "n/a".to_string(),
    }
}

/// Render the comparison as a terminal report.
pub fn render_terminal(comparison: &Comparison) -> String {
    let mut out = String::new();
    let rule = "═".repeat(78);

    out.push_str(&format!("\n{}\n", rule.bright_cyan()));
    out.push_str(&format!(
        "{}\n",
        "📐 Flux Report Comparison".bright_white().bold()
    ));
    out.push_str(&format!("{}\n", rule.bright_cyan()));
    out.push_str(&format!(
        "  {:<12} : {} (schema v{})\n",
        "Baseline".bright_yellow(),
        comparison.baseline_path,
        comparison.baseline_schema_version
    ));
    out.push_str(&format!(
        "  {:<12} : {} (schema v{})\n",
        "Candidate".bright_yellow(),
        comparison.candidate_path,
        comparison.candidate_schema_version
    ));

    out.push_str(&format!(
        "\n{}\n",
        "Aggregate Deltas:".bright_green().bold()
    ));
    out.push_str(&render_metric_table(
        &comparison.aggregate,
        &comparison.budgets,
    ));

    let scenario_budgets = comparison.scenario_budgets();
    for scenario in &comparison.scenarios {
        out.push_str(&format!(
            "\n{} {}\n",
            "Scenario:".bright_green().bold(),
            scenario.name.bright_white()
        ));
        out.push_str(&render_metric_table(&scenario.metrics, &scenario_budgets));
    }

    if !comparison.added_scenarios.is_empty() {
        out.push_str(&format!(
            "\n{} {} (not in the baseline, so no delta is available)\n",
            "Added scenarios:".bright_yellow().bold(),
            comparison.added_scenarios.join(", ")
        ));
    }
    if !comparison.removed_scenarios.is_empty() {
        out.push_str(&format!(
            "\n{} {} (in the baseline but missing from the candidate)\n",
            "Removed scenarios:".bright_yellow().bold(),
            comparison.removed_scenarios.join(", ")
        ));
    }
    if comparison.added_scenarios.is_empty()
        && comparison.removed_scenarios.is_empty()
        && comparison.scenarios.is_empty()
    {
        out.push_str("\nNeither report recorded per-scenario metrics.\n");
    }

    match &comparison.status_codes {
        Some(statuses) => {
            out.push_str(&format!(
                "\n{}\n",
                "Status Code Distribution:".bright_green().bold()
            ));
            out.push_str(&format!(
                "  {:<10} {:>12} {:>12} {:>10} {:>10} {:>10}\n",
                "Status", "Baseline", "Candidate", "Delta", "Base %", "Cand %"
            ));
            for status in statuses {
                out.push_str(&format!(
                    "  {:<10} {:>12} {:>12} {:>10} {:>10} {:>10}\n",
                    status_label(status.status),
                    status.baseline_count,
                    status.candidate_count,
                    format!(
                        "{}{}",
                        if status.delta > 0 { "+" } else { "" },
                        status.delta
                    ),
                    format_share(status.baseline_share_percent),
                    format_share(status.candidate_share_percent),
                ));
            }
        }
        None => out.push_str(
            "\nStatus code distribution unavailable: one of the reports predates status recording.\n",
        ),
    }

    out.push_str(&format!("\n{}\n", rule.bright_cyan()));
    if comparison.budgets.is_empty() {
        out.push_str(&format!(
            "{}\n",
            "No regression budgets configured; comparison is informational.".bright_yellow()
        ));
    } else if comparison.passed() {
        out.push_str(&format!(
            "{}\n",
            "✅ PASS — every configured regression budget was met."
                .bright_green()
                .bold()
        ));
    } else {
        out.push_str(&format!(
            "{}\n",
            "❌ FAIL — regression budgets exceeded:".bright_red().bold()
        ));
        for violation in &comparison.violations {
            out.push_str(&format!("  • {}\n", violation.to_string().bright_red()));
        }
    }
    out.push_str(&format!("{}\n", rule.bright_cyan()));
    out.push_str(&format!("{}\n", VARIANCE_NOTE.dimmed()));

    out
}

fn status_label(status: u16) -> String {
    if status == 0 {
        "none".to_string()
    } else {
        status.to_string()
    }
}

fn render_metric_table(metrics: &[MetricDelta], budgets: &Budgets) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "  {:<22} {:>14} {:>14} {:>14} {:>16} {:>12}\n",
        "Metric", "Baseline", "Candidate", "Delta", "Change", "Budget"
    ));

    for delta in metrics {
        let budget = budgets.for_metric(delta.metric);
        let budget_text = match budget {
            Some(threshold) => threshold.render(delta.unit),
            None => "-".to_string(),
        };
        let row = format!(
            "  {:<22} {:>14} {:>14} {:>14} {:>16} {:>12}",
            delta.label,
            delta.unit.format(delta.baseline),
            delta.unit.format(delta.candidate),
            delta.unit.format_delta(delta.delta),
            delta.rendered_change(),
            budget_text,
        );

        let colored_row = if delta.regression > 0.0 {
            row.bright_red().to_string()
        } else if delta.regression < 0.0 {
            row.bright_green().to_string()
        } else {
            row
        };
        out.push_str(&colored_row);
        out.push('\n');
    }

    out
}

/// Render the comparison as Markdown, for a CI job summary or PR comment.
pub fn render_markdown(comparison: &Comparison) -> String {
    let mut out = String::new();
    out.push_str("# Flux report comparison\n\n");
    out.push_str(&format!(
        "- Baseline: `{}` (schema v{})\n",
        comparison.baseline_path, comparison.baseline_schema_version
    ));
    out.push_str(&format!(
        "- Candidate: `{}` (schema v{})\n\n",
        comparison.candidate_path, comparison.candidate_schema_version
    ));

    if comparison.budgets.is_empty() {
        out.push_str("**No regression budgets configured — comparison is informational.**\n\n");
    } else if comparison.passed() {
        out.push_str("**✅ PASS — every configured regression budget was met.**\n\n");
    } else {
        out.push_str("**❌ FAIL — regression budgets exceeded:**\n\n");
        for violation in &comparison.violations {
            out.push_str(&format!("- {violation}\n"));
        }
        out.push('\n');
    }

    out.push_str("## Aggregate\n\n");
    out.push_str(&markdown_metric_table(
        &comparison.aggregate,
        &comparison.budgets,
    ));

    if !comparison.scenarios.is_empty() {
        let scenario_budgets = comparison.scenario_budgets();
        out.push_str("\n## Per scenario\n");
        for scenario in &comparison.scenarios {
            out.push_str(&format!("\n### {}\n\n", scenario.name));
            out.push_str(&markdown_metric_table(&scenario.metrics, &scenario_budgets));
        }
    }

    if !comparison.added_scenarios.is_empty() {
        out.push_str(&format!(
            "\n**Added scenarios** (not in the baseline, so no delta is available): {}\n",
            comparison.added_scenarios.join(", ")
        ));
    }
    if !comparison.removed_scenarios.is_empty() {
        out.push_str(&format!(
            "\n**Removed scenarios** (in the baseline but missing from the candidate): {}\n",
            comparison.removed_scenarios.join(", ")
        ));
    }

    out.push_str("\n## Status codes\n\n");
    match &comparison.status_codes {
        Some(statuses) => {
            out.push_str(
                "| Status | Baseline | Candidate | Delta | Baseline share | Candidate share |\n",
            );
            out.push_str("| --- | ---: | ---: | ---: | ---: | ---: |\n");
            for status in statuses {
                out.push_str(&format!(
                    "| {} | {} | {} | {}{} | {} | {} |\n",
                    status_label(status.status),
                    status.baseline_count,
                    status.candidate_count,
                    if status.delta > 0 { "+" } else { "" },
                    status.delta,
                    format_share(status.baseline_share_percent),
                    format_share(status.candidate_share_percent),
                ));
            }
        }
        None => out.push_str(
            "Status code distribution unavailable: one of the reports predates status recording.\n",
        ),
    }

    out.push_str(&format!("\n> {VARIANCE_NOTE}\n"));
    out
}

fn markdown_metric_table(metrics: &[MetricDelta], budgets: &Budgets) -> String {
    let mut out = String::new();
    out.push_str("| Metric | Baseline | Candidate | Delta | Change | Budget |\n");
    out.push_str("| --- | ---: | ---: | ---: | ---: | ---: |\n");
    for delta in metrics {
        let budget = match budgets.for_metric(delta.metric) {
            Some(threshold) => threshold.render(delta.unit),
            None => "—".to_string(),
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            delta.label,
            delta.unit.format(delta.baseline),
            delta.unit.format(delta.candidate),
            delta.unit.format_delta(delta.delta),
            delta.rendered_change(),
            budget,
        ));
    }
    out
}

/// Render the comparison as JSON, for machine-readable CI artifacts.
pub fn render_json(comparison: &Comparison) -> Result<String> {
    #[derive(Serialize)]
    struct ComparisonDocument<'a> {
        schema_version: u32,
        passed: bool,
        #[serde(flatten)]
        comparison: &'a Comparison,
    }

    let document = ComparisonDocument {
        schema_version: REPORT_SCHEMA_VERSION,
        passed: comparison.passed(),
        comparison,
    };
    Ok(serde_json::to_string_pretty(&document)?)
}

/// Write a rendered artifact, creating the parent directory when needed.
pub fn write_artifact(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
    }
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary_json(overrides: serde_json::Value) -> serde_json::Value {
        let mut base = serde_json::json!({
            "total_requests": 100,
            "successful_requests": 100,
            "failed_requests": 0,
            "total_duration_secs": 10.0,
            "throughput_rps": 10.0,
            "min_latency_ms": 5,
            "max_latency_ms": 200,
            "mean_latency_ms": 50.0,
            "p50_latency_ms": 40,
            "p90_latency_ms": 90,
            "p95_latency_ms": 100,
            "p99_latency_ms": 150,
            "error_rate": 0.0,
            "start_time": "2026-01-01T00:00:00Z",
            "end_time": "2026-01-01T00:00:10Z",
            "per_scenario": {},
            "status_codes": {"200": 100}
        });

        let (serde_json::Value::Object(base_map), serde_json::Value::Object(over)) =
            (&mut base, overrides)
        else {
            panic!("overrides must be an object");
        };
        for (key, value) in over {
            base_map.insert(key, value);
        }
        base
    }

    fn report(schema_version: Option<u32>, summary: serde_json::Value) -> ReportDocument {
        let mut document = serde_json::json!({ "summary": summary, "results": [] });
        if let Some(version) = schema_version {
            document["schema_version"] = serde_json::json!(version);
        }
        serde_json::from_value(document).expect("report parses")
    }

    fn scenario_json(p95: u64, error_rate: f64) -> serde_json::Value {
        serde_json::json!({
            "total_requests": 50,
            "successful_requests": 50,
            "failed_requests": 0,
            "throughput_rps": 5.0,
            "min_latency_ms": 5,
            "max_latency_ms": 200,
            "mean_latency_ms": 50.0,
            "p50_latency_ms": 40,
            "p90_latency_ms": 90,
            "p95_latency_ms": p95,
            "p99_latency_ms": 150,
            "error_rate": error_rate
        })
    }

    fn compare(
        baseline: serde_json::Value,
        candidate: serde_json::Value,
        budgets: Budgets,
    ) -> Comparison {
        build_comparison(
            "baseline.json",
            "candidate.json",
            &report(Some(REPORT_SCHEMA_VERSION), baseline),
            &report(Some(REPORT_SCHEMA_VERSION), candidate),
            budgets,
        )
    }

    fn find(metrics: &[MetricDelta], metric: Metric) -> &MetricDelta {
        metrics
            .iter()
            .find(|delta| delta.metric == metric)
            .expect("metric is compared")
    }

    #[test]
    fn parses_percentage_and_absolute_budgets() {
        assert_eq!(Threshold::parse("10%").unwrap(), Threshold::Percent(10.0));
        assert_eq!(
            Threshold::parse(" 7.5 % ").unwrap(),
            Threshold::Percent(7.5)
        );
        assert_eq!(Threshold::parse("25").unwrap(), Threshold::Absolute(25.0));
        assert_eq!(Threshold::parse("25ms").unwrap(), Threshold::Absolute(25.0));
        assert_eq!(
            Threshold::parse("1.5 req/s").unwrap(),
            Threshold::Absolute(1.5)
        );
    }

    #[test]
    fn rejects_malformed_and_negative_budgets() {
        assert!(Threshold::parse("").is_err());
        assert!(Threshold::parse("soon").is_err());
        assert!(Threshold::parse("-5%").is_err());
        assert!(Threshold::parse("%").is_err());
    }

    #[test]
    fn error_rate_budget_rejects_a_percent_sign() {
        let error = parse_error_rate_budget("0.5%").unwrap_err().to_string();
        assert!(error.contains("percentage points"), "{error}");
        assert_eq!(parse_error_rate_budget("0.5").unwrap(), 0.5);
    }

    #[test]
    fn reports_without_a_schema_version_are_read_as_legacy() {
        let document = report(None, summary_json(serde_json::json!({})));
        assert_eq!(document.schema_version, LEGACY_SCHEMA_VERSION);
    }

    #[test]
    fn newer_schema_versions_are_rejected() {
        let path = write_temp_report(
            "newer-schema",
            &serde_json::json!({
                "schema_version": REPORT_SCHEMA_VERSION + 1,
                "summary": summary_json(serde_json::json!({})),
                "results": []
            }),
        );
        let error = load_report(&path).unwrap_err().to_string();
        std::fs::remove_file(&path).unwrap();
        assert!(error.contains("upgrade Flux"), "{error}");
    }

    #[test]
    fn a_report_missing_compared_fields_is_rejected() {
        let path = write_temp_report(
            "incomplete",
            &serde_json::json!({ "summary": { "total_requests": 10 }, "results": [] }),
        );
        let error = load_report(&path).unwrap_err().to_string();
        std::fs::remove_file(&path).unwrap();
        assert!(error.contains("failed to parse report"), "{error}");
    }

    #[test]
    fn compares_two_reports_from_disk() {
        let baseline = write_temp_report(
            "baseline",
            &serde_json::json!({
                "schema_version": REPORT_SCHEMA_VERSION,
                "summary": summary_json(serde_json::json!({})),
                "results": []
            }),
        );
        let candidate = write_temp_report(
            "candidate",
            &serde_json::json!({
                "schema_version": REPORT_SCHEMA_VERSION,
                "summary": summary_json(serde_json::json!({ "p95_latency_ms": 150 })),
                "results": []
            }),
        );

        let comparison = compare_reports(
            &baseline,
            &candidate,
            Budgets {
                p95_latency: Some(Threshold::Percent(10.0)),
                ..Budgets::default()
            },
        )
        .unwrap();
        std::fs::remove_file(&baseline).unwrap();
        std::fs::remove_file(&candidate).unwrap();

        assert!(!comparison.passed());
        assert_eq!(find(&comparison.aggregate, Metric::P95Latency).delta, 50.0);
    }

    #[test]
    fn percent_change_uses_the_baseline_as_the_denominator() {
        let comparison = compare(
            summary_json(serde_json::json!({})),
            summary_json(serde_json::json!({ "p95_latency_ms": 120 })),
            Budgets::default(),
        );

        let p95 = find(&comparison.aggregate, Metric::P95Latency);
        assert_eq!(p95.delta, 20.0);
        assert_eq!(p95.percent_change, Some(20.0));
        assert_eq!(p95.rendered_change(), "+20.0%");
    }

    #[test]
    fn a_zero_baseline_has_no_percentage() {
        let comparison = compare(
            summary_json(serde_json::json!({ "p95_latency_ms": 0, "error_rate": 0.0 })),
            summary_json(serde_json::json!({ "p95_latency_ms": 30 })),
            Budgets::default(),
        );

        let p95 = find(&comparison.aggregate, Metric::P95Latency);
        assert_eq!(p95.percent_change, None);
        assert_eq!(p95.rendered_change(), "n/a (baseline 0)");
    }

    #[test]
    fn a_percentage_budget_fails_when_the_baseline_is_zero_and_the_candidate_regressed() {
        let comparison = compare(
            summary_json(serde_json::json!({ "p95_latency_ms": 0 })),
            summary_json(serde_json::json!({ "p95_latency_ms": 30 })),
            Budgets {
                p95_latency: Some(Threshold::Percent(10.0)),
                ..Budgets::default()
            },
        );

        assert!(!comparison.passed());
        let violation = &comparison.violations[0];
        assert!(violation.observed.contains("zero baseline"), "{violation}");
    }

    #[test]
    fn a_percentage_budget_passes_when_both_sides_are_zero() {
        let comparison = compare(
            summary_json(serde_json::json!({ "p95_latency_ms": 0 })),
            summary_json(serde_json::json!({ "p95_latency_ms": 0 })),
            Budgets {
                p95_latency: Some(Threshold::Percent(10.0)),
                ..Budgets::default()
            },
        );

        assert!(comparison.passed());
    }

    #[test]
    fn an_empty_baseline_run_compares_without_dividing_by_zero() {
        let empty = summary_json(serde_json::json!({
            "total_requests": 0,
            "successful_requests": 0,
            "failed_requests": 0,
            "throughput_rps": 0.0,
            "mean_latency_ms": 0.0,
            "p50_latency_ms": 0,
            "p90_latency_ms": 0,
            "p95_latency_ms": 0,
            "p99_latency_ms": 0,
            "error_rate": 0.0,
            "status_codes": {}
        }));

        let comparison = compare(
            empty,
            summary_json(serde_json::json!({})),
            Budgets::default(),
        );

        let statuses = comparison
            .status_codes
            .expect("both reports record statuses");
        let ok = statuses.iter().find(|status| status.status == 200).unwrap();
        assert_eq!(ok.baseline_share_percent, None);
        assert_eq!(ok.candidate_share_percent, Some(100.0));
        assert_eq!(format_share(ok.baseline_share_percent), "n/a");
    }

    #[test]
    fn a_latency_budget_within_the_percentage_passes() {
        let comparison = compare(
            summary_json(serde_json::json!({})),
            summary_json(serde_json::json!({ "p95_latency_ms": 105 })),
            Budgets {
                p95_latency: Some(Threshold::Percent(10.0)),
                ..Budgets::default()
            },
        );

        assert!(comparison.passed(), "{:?}", comparison.violations);
    }

    #[test]
    fn an_absolute_latency_budget_is_enforced_in_milliseconds() {
        let comparison = compare(
            summary_json(serde_json::json!({})),
            summary_json(serde_json::json!({ "p95_latency_ms": 131 })),
            Budgets {
                p95_latency: Some(Threshold::Absolute(30.0)),
                ..Budgets::default()
            },
        );

        assert_eq!(comparison.violations.len(), 1);
        assert_eq!(comparison.violations[0].budget, "30ms");
        assert_eq!(comparison.violations[0].observed, "+31ms");
    }

    #[test]
    fn an_improvement_never_violates_a_budget() {
        let comparison = compare(
            summary_json(serde_json::json!({})),
            summary_json(serde_json::json!({ "p95_latency_ms": 40, "throughput_rps": 20.0 })),
            Budgets {
                p95_latency: Some(Threshold::Percent(1.0)),
                throughput_drop: Some(Threshold::Percent(1.0)),
                ..Budgets::default()
            },
        );

        assert!(comparison.passed());
    }

    #[test]
    fn error_rate_budgets_are_percentage_points() {
        let comparison = compare(
            summary_json(serde_json::json!({ "error_rate": 1.0 })),
            summary_json(serde_json::json!({ "error_rate": 1.6 })),
            Budgets {
                error_rate_increase: Some(0.5),
                ..Budgets::default()
            },
        );

        assert_eq!(comparison.violations.len(), 1);
        assert_eq!(comparison.violations[0].metric, Metric::ErrorRate);
        assert_eq!(comparison.violations[0].observed, "+0.60pp");
    }

    #[test]
    fn a_throughput_drop_is_a_regression_and_a_rise_is_not() {
        let dropped = compare(
            summary_json(serde_json::json!({})),
            summary_json(serde_json::json!({ "throughput_rps": 8.0 })),
            Budgets {
                throughput_drop: Some(Threshold::Percent(10.0)),
                ..Budgets::default()
            },
        );
        assert_eq!(dropped.violations.len(), 1);
        assert_eq!(dropped.violations[0].metric, Metric::Throughput);

        let raised = compare(
            summary_json(serde_json::json!({})),
            summary_json(serde_json::json!({ "throughput_rps": 12.0 })),
            Budgets {
                throughput_drop: Some(Threshold::Percent(10.0)),
                ..Budgets::default()
            },
        );
        assert!(raised.passed());
    }

    #[test]
    fn added_and_removed_scenarios_are_reported_explicitly() {
        let baseline = summary_json(serde_json::json!({
            "per_scenario": {
                "login": scenario_json(100, 0.0),
                "checkout": scenario_json(200, 0.0)
            }
        }));
        let candidate = summary_json(serde_json::json!({
            "per_scenario": {
                "login": scenario_json(150, 0.0),
                "search": scenario_json(80, 0.0)
            }
        }));

        let comparison = compare(baseline, candidate, Budgets::default());

        assert_eq!(comparison.added_scenarios, vec!["search".to_string()]);
        assert_eq!(comparison.removed_scenarios, vec!["checkout".to_string()]);
        assert_eq!(comparison.scenarios.len(), 1);
        assert_eq!(comparison.scenarios[0].name, "login");

        let terminal = render_terminal(&comparison);
        assert!(terminal.contains("Added scenarios:"), "{terminal}");
        assert!(terminal.contains("search"), "{terminal}");
        assert!(terminal.contains("checkout"), "{terminal}");
    }

    #[test]
    fn per_scenario_budgets_are_opt_in() {
        let baseline = summary_json(
            serde_json::json!({ "per_scenario": { "login": scenario_json(100, 0.0) } }),
        );
        let candidate = summary_json(
            serde_json::json!({ "per_scenario": { "login": scenario_json(200, 0.0) } }),
        );

        let aggregate_only = compare(
            baseline.clone(),
            candidate.clone(),
            Budgets {
                p95_latency: Some(Threshold::Percent(10.0)),
                ..Budgets::default()
            },
        );
        assert!(aggregate_only.passed());

        let per_scenario = compare(
            baseline,
            candidate,
            Budgets {
                p95_latency: Some(Threshold::Percent(10.0)),
                per_scenario: true,
                ..Budgets::default()
            },
        );
        assert_eq!(per_scenario.violations.len(), 1);
        assert_eq!(per_scenario.violations[0].scope, "scenario login");
    }

    #[test]
    fn scenario_tables_only_show_budgets_that_are_enforced() {
        let baseline = summary_json(
            serde_json::json!({ "per_scenario": { "login": scenario_json(100, 0.0) } }),
        );
        let candidate = summary_json(
            serde_json::json!({ "per_scenario": { "login": scenario_json(105, 0.0) } }),
        );

        let aggregate_only = compare(
            baseline.clone(),
            candidate.clone(),
            Budgets {
                p95_latency: Some(Threshold::Percent(10.0)),
                ..Budgets::default()
            },
        );
        let table = render_metric_table(
            &aggregate_only.scenarios[0].metrics,
            &aggregate_only.scenario_budgets(),
        );
        assert!(!table.contains("10%"), "{table}");

        let per_scenario = compare(
            baseline,
            candidate,
            Budgets {
                p95_latency: Some(Threshold::Percent(10.0)),
                per_scenario: true,
                ..Budgets::default()
            },
        );
        let table = render_metric_table(
            &per_scenario.scenarios[0].metrics,
            &per_scenario.scenario_budgets(),
        );
        assert!(table.contains("10%"), "{table}");
    }

    #[test]
    fn a_throughput_violation_is_described_as_a_drop() {
        let comparison = compare(
            summary_json(serde_json::json!({})),
            summary_json(serde_json::json!({ "throughput_rps": 8.0 })),
            Budgets {
                throughput_drop: Some(Threshold::Percent(10.0)),
                ..Budgets::default()
            },
        );

        let message = comparison.violations[0].to_string();
        assert_eq!(
            message,
            "aggregate throughput: -2.00 req/s (-20.0%) exceeds the 10% drop budget"
        );
    }

    #[test]
    fn status_code_distribution_is_compared_with_explicit_shares() {
        let comparison = compare(
            summary_json(serde_json::json!({ "status_codes": { "200": 100 } })),
            summary_json(serde_json::json!({
                "status_codes": { "200": 90, "500": 9, "0": 1 },
                "failed_requests": 10,
                "successful_requests": 90,
                "error_rate": 10.0
            })),
            Budgets::default(),
        );

        let statuses = comparison.status_codes.clone().expect("statuses compared");
        assert_eq!(statuses.len(), 3);
        let server_error = statuses.iter().find(|s| s.status == 500).unwrap();
        assert_eq!(server_error.baseline_count, 0);
        assert_eq!(server_error.delta, 9);
        assert_eq!(server_error.candidate_share_percent, Some(9.0));

        let terminal = render_terminal(&comparison);
        assert!(terminal.contains("Status Code Distribution"), "{terminal}");
        // A status of 0 means no response arrived, which reads badly as "0".
        assert!(terminal.contains("none"), "{terminal}");
    }

    #[test]
    fn status_codes_are_skipped_when_a_report_predates_them() {
        let legacy = report(
            None,
            summary_json(serde_json::json!({ "status_codes": serde_json::Value::Null })),
        );
        let mut legacy = legacy;
        legacy.summary.status_codes = None;

        let comparison = build_comparison(
            "baseline.json",
            "candidate.json",
            &legacy,
            &report(
                Some(REPORT_SCHEMA_VERSION),
                summary_json(serde_json::json!({})),
            ),
            Budgets::default(),
        );

        assert!(comparison.status_codes.is_none());
        assert!(render_terminal(&comparison).contains("predates status recording"));
        assert!(render_markdown(&comparison).contains("predates status recording"));
    }

    #[test]
    fn markdown_and_json_artifacts_carry_the_verdict_and_the_variance_note() {
        let comparison = compare(
            summary_json(serde_json::json!({})),
            summary_json(serde_json::json!({ "p95_latency_ms": 200 })),
            Budgets {
                p95_latency: Some(Threshold::Percent(10.0)),
                ..Budgets::default()
            },
        );

        let markdown = render_markdown(&comparison);
        assert!(markdown.contains("❌ FAIL"), "{markdown}");
        assert!(markdown.contains("| p95 latency |"), "{markdown}");
        assert!(markdown.contains("Repeat runs"), "{markdown}");

        let json: serde_json::Value =
            serde_json::from_str(&render_json(&comparison).unwrap()).unwrap();
        assert_eq!(json["passed"], false);
        assert_eq!(json["schema_version"], REPORT_SCHEMA_VERSION);
        assert_eq!(json["violations"][0]["metric"], "p95_latency");
        assert!(json["variance_note"]
            .as_str()
            .unwrap()
            .contains("budget check"));
    }

    #[test]
    fn a_comparison_without_budgets_is_informational() {
        let comparison = compare(
            summary_json(serde_json::json!({})),
            summary_json(serde_json::json!({ "p95_latency_ms": 500 })),
            Budgets::default(),
        );

        assert!(comparison.passed());
        assert!(render_terminal(&comparison).contains("informational"));
    }

    fn write_temp_report(name: &str, document: &serde_json::Value) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "flux-compare-{}-{}-{}.json",
            name,
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::write(&path, serde_json::to_string(document).unwrap()).unwrap();
        path
    }
}
