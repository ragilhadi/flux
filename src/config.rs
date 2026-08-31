use crate::metrics::MetricsSummary;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Main configuration structure for Flux load testing
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Base target URL (optional if scenarios use full URLs)
    #[serde(default)]
    pub target: Option<String>,

    /// HTTP method for simple mode
    #[serde(default)]
    pub method: Option<String>,

    /// Headers for simple mode
    #[serde(default)]
    pub headers: HashMap<String, String>,

    /// Request body for simple mode
    #[serde(default)]
    pub body: Option<String>,

    /// Multipart form data for simple mode
    #[serde(default)]
    pub multipart: Option<Vec<MultipartPart>>,

    /// Multi-step scenarios
    #[serde(default)]
    pub scenarios: Vec<Scenario>,

    /// Number of concurrent workers
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,

    /// Test duration (e.g., "30s", "5m")
    #[serde(default = "default_duration")]
    pub duration: String,

    /// Per-request timeout (e.g., "30s", "2m")
    #[serde(default = "default_timeout")]
    pub timeout: String,

    /// Optional duration over which workers are started gradually
    #[serde(default)]
    pub ramp_up: Option<String>,

    /// Optional load profile describing how load changes over the run —
    /// staged concurrency or a target request-arrival rate — as an
    /// alternative to the fixed `concurrency` (with optional `ramp_up`)
    /// above. Mutually exclusive with `ramp_up`.
    #[serde(default)]
    pub load_profile: Option<LoadProfile>,

    /// Optional pause between requests in simple mode
    #[serde(default)]
    pub think_time: Option<String>,

    /// Number of retry attempts after the initial request
    #[serde(default)]
    pub retry_count: u32,

    /// Delay between retry attempts
    #[serde(default)]
    pub retry_delay: Option<String>,

    /// HTTP status codes that should be retried
    #[serde(default)]
    pub retry_on_status: Vec<u16>,

    /// Aggregate pass/fail thresholds evaluated after the test
    #[serde(default)]
    pub assertions: Option<AssertionsConfig>,

    /// Optional port for the live Prometheus metrics endpoint
    #[serde(default)]
    pub prometheus_port: Option<u16>,

    /// Address the Prometheus endpoint binds to.
    ///
    /// The endpoint exposes run metrics over plain HTTP with no
    /// authentication, so it defaults to loopback; binding anything else is
    /// opt-in, same as `live_dashboard.bind`.
    #[serde(default = "default_prometheus_bind")]
    pub prometheus_bind: String,

    /// Optional live web dashboard for the running test.
    ///
    /// Absent by default: no socket is opened unless this section exists.
    #[serde(default)]
    pub live_dashboard: Option<LiveDashboardConfig>,

    /// Execution mode: "async" or "sync"
    #[serde(default = "default_mode")]
    pub mode: String,

    /// Output configuration
    pub output: OutputConfig,
}

/// Multipart form data part
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MultipartPart {
    /// Type: "file" or "field"
    #[serde(rename = "type")]
    pub part_type: String,

    /// Field name
    pub name: String,

    /// File path (for type="file")
    #[serde(default)]
    pub path: Option<String>,

    /// Field value (for type="field")
    #[serde(default)]
    pub value: Option<String>,
}

/// A load profile describes how load should change over the life of a run,
/// as an alternative to a fixed `concurrency`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoadProfile {
    /// Ramp concurrency through a sequence of levels, each held for a fixed
    /// duration — a stepped load, a hold period, or a spike.
    Stages { stages: Vec<Stage> },

    /// Pace request starts at a fixed target rate instead of driving a fixed
    /// worker count, bounded by `max_concurrency` in-flight requests.
    ArrivalRate {
        target_rps: f64,
        duration: String,
        #[serde(default = "default_arrival_max_concurrency")]
        max_concurrency: usize,
    },
}

/// One step of a `stages` load profile.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Stage {
    /// How long this stage is held (e.g. "30s", "2m").
    pub duration: String,

    /// Number of concurrent workers active during this stage.
    pub target_concurrency: usize,
}

fn default_arrival_max_concurrency() -> usize {
    1_000
}

/// Scenario step definition
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Scenario {
    /// Step name
    pub name: String,

    /// HTTP method
    pub method: String,

    /// URL path or full URL
    pub url: String,

    /// Headers specific to this step
    #[serde(default)]
    pub headers: HashMap<String, String>,

    /// Request body
    #[serde(default)]
    pub body: Option<String>,

    /// Multipart form data
    #[serde(default)]
    pub multipart: Option<Vec<MultipartPart>>,

    /// Variable extraction rules
    #[serde(default)]
    pub extract: HashMap<String, String>,

    /// Dependency on previous step
    #[serde(default)]
    pub depends_on: Option<String>,

    /// Optional pause after this scenario step
    #[serde(default)]
    pub think_time: Option<String>,

    /// Scenario-specific retry count override
    #[serde(default)]
    pub retry_count: Option<u32>,

    /// Scenario-specific retry delay override
    #[serde(default)]
    pub retry_delay: Option<String>,

    /// Scenario-specific retryable status-code override
    #[serde(default)]
    pub retry_on_status: Option<Vec<u16>>,

    /// Response expectations for this scenario step
    #[serde(default, rename = "assert")]
    pub assertions: Option<ResponseAssertions>,
}

/// Live web dashboard settings.
///
/// The dashboard exposes run metrics over plain HTTP with no authentication,
/// so it binds to loopback by default and every other address is opt-in.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LiveDashboardConfig {
    /// Address to listen on, as `ip:port`.
    #[serde(default = "default_dashboard_bind")]
    pub bind: String,

    /// How often the page refreshes its data, in milliseconds.
    #[serde(default = "default_dashboard_refresh_ms")]
    pub refresh_ms: u64,

    /// Extra literal values to redact from anything the dashboard shows.
    ///
    /// Credential headers, request bodies and multipart field values are
    /// redacted automatically; this covers secrets Flux cannot infer.
    #[serde(default)]
    pub redact: Vec<String>,
}

impl Default for LiveDashboardConfig {
    fn default() -> Self {
        Self {
            bind: default_dashboard_bind(),
            refresh_ms: default_dashboard_refresh_ms(),
            redact: Vec::new(),
        }
    }
}

impl LiveDashboardConfig {
    /// Parse the configured bind address.
    pub fn parse_bind(&self) -> anyhow::Result<SocketAddr> {
        self.bind.trim().parse::<SocketAddr>().map_err(|_| {
            anyhow::anyhow!(
                "Invalid live_dashboard.bind '{}'; expected an address such as \
                 '127.0.0.1:9090', '0.0.0.0:9090' or '[::1]:9090'",
                self.bind
            )
        })
    }
}

/// Aggregate test-level quality gates.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AssertionsConfig {
    #[serde(default)]
    pub max_error_rate: Option<f64>,
    #[serde(default)]
    pub max_p99_ms: Option<u64>,
    #[serde(default)]
    pub max_p95_ms: Option<u64>,
    #[serde(default)]
    pub max_avg_ms: Option<f64>,
}

/// Response assertions for a scenario step.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ResponseAssertions {
    #[serde(default)]
    pub status_code: Option<u16>,
    #[serde(default)]
    pub body_contains: Option<String>,
}

/// Output configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OutputConfig {
    /// JSON output file path
    pub json: String,

    /// HTML output file path
    pub html: String,

    /// Optional CSV output file path
    #[serde(default)]
    pub csv: Option<String>,

    /// Maximum per-request rows kept in memory for the JSON/HTML reports.
    ///
    /// Aggregate statistics always cover every request; this only bounds the
    /// raw rows so a long or high-throughput run cannot exhaust memory. Set to
    /// `0` to retain everything.
    #[serde(default = "default_max_results")]
    pub max_results: usize,
}

fn default_concurrency() -> usize {
    10
}

fn default_duration() -> String {
    "30s".to_string()
}

fn default_timeout() -> String {
    "30s".to_string()
}

fn default_mode() -> String {
    "async".to_string()
}

fn default_max_results() -> usize {
    crate::metrics::DEFAULT_MAX_STORED_RESULTS
}

fn default_dashboard_bind() -> String {
    "127.0.0.1:9090".to_string()
}

fn default_prometheus_bind() -> String {
    "127.0.0.1".to_string()
}

fn default_dashboard_refresh_ms() -> u64 {
    1_000
}

impl Config {
    /// Load configuration from YAML file
    pub fn from_file(path: &PathBuf) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut config: Config = serde_yaml::from_str(&content)?;
        config.expand_environment_variables()?;
        config.validate()?;
        Ok(config)
    }

    /// Validate configuration
    fn validate(&self) -> anyhow::Result<()> {
        // Check if we have either simple mode or scenarios
        if self.scenarios.is_empty() && self.target.is_none() {
            anyhow::bail!("Either 'target' or 'scenarios' must be specified");
        }

        // Validate mode
        if self.mode != "async" && self.mode != "sync" {
            anyhow::bail!("Mode must be either 'async' or 'sync'");
        }

        // Validate concurrency
        if self.concurrency == 0 {
            anyhow::bail!("Concurrency must be greater than 0");
        }

        self.parse_duration()?;
        self.parse_timeout()?;
        // Ramp-up is warm-up time added in front of the measured window, so it
        // is allowed to be longer than the configured duration.
        self.parse_ramp_up()?;
        self.validate_load_profile()?;
        self.parse_think_time()?;
        self.parse_retry_delay()?;
        validate_status_codes(&self.retry_on_status, "retry_on_status")?;
        if self.prometheus_port == Some(0) {
            anyhow::bail!("prometheus_port must be greater than 0");
        }
        self.parse_prometheus_bind()?;
        if let Some(dashboard) = &self.live_dashboard {
            let address = dashboard.parse_bind()?;
            if address.port() == 0 {
                anyhow::bail!("live_dashboard.bind must name a port greater than 0");
            }
            // A page that refreshes hundreds of times a second would compete
            // with the load generator it is supposed to observe.
            if !(100..=60_000).contains(&dashboard.refresh_ms) {
                anyhow::bail!(
                    "live_dashboard.refresh_ms must be between 100 and 60000, got {}",
                    dashboard.refresh_ms
                );
            }
        }
        if let Some(assertions) = &self.assertions {
            if let Some(max_error_rate) = assertions.max_error_rate {
                if !(0.0..=100.0).contains(&max_error_rate) {
                    anyhow::bail!("max_error_rate must be between 0 and 100");
                }
            }
            if assertions.max_avg_ms.is_some_and(|value| value < 0.0) {
                anyhow::bail!("max_avg_ms cannot be negative");
            }
        }

        // Validate multipart parts
        if let Some(ref parts) = self.multipart {
            for part in parts {
                if part.part_type == "file" && part.path.is_none() {
                    anyhow::bail!("Multipart file type requires 'path' field");
                }
                if part.part_type == "field" && part.value.is_none() {
                    anyhow::bail!("Multipart field type requires 'value' field");
                }
            }
        }

        self.validate_scenario_graph()?;

        // Validate scenarios
        for scenario in &self.scenarios {
            parse_optional_duration(scenario.think_time.as_deref())?;
            parse_optional_retry_delay(scenario.retry_delay.as_deref())?;
            if let Some(statuses) = &scenario.retry_on_status {
                validate_status_codes(
                    statuses,
                    &format!("retry_on_status in scenario '{}'", scenario.name),
                )?;
            }
            if let Some(expected) = scenario
                .assertions
                .as_ref()
                .and_then(|assertions| assertions.status_code)
            {
                validate_status_codes(
                    &[expected],
                    &format!("assert.status_code in scenario '{}'", scenario.name),
                )?;
            }

            if let Some(ref parts) = scenario.multipart {
                for part in parts {
                    if part.part_type == "file" && part.path.is_none() {
                        anyhow::bail!(
                            "Multipart file type requires 'path' field in scenario '{}'",
                            scenario.name
                        );
                    }
                    if part.part_type == "field" && part.value.is_none() {
                        anyhow::bail!(
                            "Multipart field type requires 'value' field in scenario '{}'",
                            scenario.name
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// Validate scenario names and the `depends_on` graph.
    ///
    /// Scenarios run in declaration order, so a dependency must name an
    /// earlier step. Enforcing that here turns typos, duplicate names and
    /// cycles into configuration errors instead of silently skipped work.
    fn validate_scenario_graph(&self) -> anyhow::Result<()> {
        let mut positions: HashMap<&str, usize> = HashMap::new();

        for (index, scenario) in self.scenarios.iter().enumerate() {
            let step = index + 1;
            if scenario.name.trim().is_empty() {
                anyhow::bail!("Scenario at step {step} has an empty 'name'");
            }
            if let Some(previous) = positions.insert(scenario.name.as_str(), index) {
                anyhow::bail!(
                    "Duplicate scenario name '{}' at steps {} and {}; \
                     scenario names must be unique because 'depends_on' refers to them",
                    scenario.name,
                    previous + 1,
                    step
                );
            }
        }

        for (index, scenario) in self.scenarios.iter().enumerate() {
            let Some(depends_on) = scenario.depends_on.as_deref() else {
                continue;
            };

            if depends_on.trim().is_empty() {
                anyhow::bail!(
                    "Scenario '{}' has an empty 'depends_on'; remove it or name an earlier step",
                    scenario.name
                );
            }
            if depends_on == scenario.name {
                anyhow::bail!(
                    "Scenario '{}' depends on itself; a step cannot wait for its own result",
                    scenario.name
                );
            }

            match positions.get(depends_on) {
                None => anyhow::bail!(
                    "Scenario '{}' depends on unknown scenario '{}'; declared scenarios are: {}",
                    scenario.name,
                    depends_on,
                    self.scenarios
                        .iter()
                        .map(|scenario| format!("'{}'", scenario.name))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                Some(&dependency_index) if dependency_index > index => anyhow::bail!(
                    "Scenario '{}' (step {}) depends on '{}' (step {}), which runs later; \
                     scenarios execute in declaration order, so a dependency must be \
                     declared before the step that uses it",
                    scenario.name,
                    index + 1,
                    depends_on,
                    dependency_index + 1
                ),
                Some(_) => {}
            }
        }

        Ok(())
    }

    /// Validate the optional load profile.
    ///
    /// A load profile replaces the fixed `concurrency` (with optional
    /// `ramp_up`) model, so the two cannot be combined — warm-up should be
    /// expressed as a stage instead.
    fn validate_load_profile(&self) -> anyhow::Result<()> {
        let Some(profile) = &self.load_profile else {
            return Ok(());
        };

        if self.ramp_up.is_some() {
            anyhow::bail!(
                "ramp_up cannot be combined with load_profile; express warm-up \
                 as a leading stage instead"
            );
        }

        match profile {
            LoadProfile::Stages { stages } => {
                if stages.is_empty() {
                    anyhow::bail!("load_profile.stages must contain at least one stage");
                }
                for (index, stage) in stages.iter().enumerate() {
                    // `parse_duration` itself rejects a zero duration, so
                    // stages can never be given a no-op hold time.
                    parse_duration(&stage.duration).map_err(|e| {
                        anyhow::anyhow!("Invalid duration for stage {}: {e}", index + 1)
                    })?;
                }
            }
            LoadProfile::ArrivalRate {
                target_rps,
                duration,
                max_concurrency,
            } => {
                if !target_rps.is_finite() || *target_rps <= 0.0 {
                    anyhow::bail!("load_profile.target_rps must be greater than 0");
                }
                parse_duration(duration)
                    .map_err(|e| anyhow::anyhow!("Invalid load_profile.duration: {e}"))?;
                if *max_concurrency == 0 {
                    anyhow::bail!("load_profile.max_concurrency must be greater than 0");
                }
            }
        }

        Ok(())
    }

    /// Parsed `(duration, target_concurrency)` for each stage, in order, when
    /// a `stages` load profile is configured.
    pub fn parsed_stages(&self) -> anyhow::Result<Option<Vec<(Duration, usize)>>> {
        match &self.load_profile {
            Some(LoadProfile::Stages { stages }) => Ok(Some(
                stages
                    .iter()
                    .map(|stage| Ok((parse_duration(&stage.duration)?, stage.target_concurrency)))
                    .collect::<anyhow::Result<Vec<_>>>()?,
            )),
            _ => Ok(None),
        }
    }

    /// Parsed `(target_rps, duration, max_concurrency)` when an
    /// `arrival_rate` load profile is configured.
    pub fn parsed_arrival_rate(&self) -> anyhow::Result<Option<(f64, Duration, usize)>> {
        match &self.load_profile {
            Some(LoadProfile::ArrivalRate {
                target_rps,
                duration,
                max_concurrency,
            }) => Ok(Some((
                *target_rps,
                parse_duration(duration)?,
                *max_concurrency,
            ))),
            _ => Ok(None),
        }
    }

    /// Total wall-clock time the run is expected to take, across every
    /// profile: `duration + ramp_up` for fixed concurrency, or the sum of
    /// stage durations / the arrival-rate duration when `load_profile` is
    /// configured.
    pub fn total_planned_secs(&self) -> anyhow::Result<u64> {
        if let Some(stages) = self.parsed_stages()? {
            return Ok(stages.iter().map(|(duration, _)| duration.as_secs()).sum());
        }
        if let Some((_, duration, _)) = self.parsed_arrival_rate()? {
            return Ok(duration.as_secs());
        }
        let duration = self.parse_duration()?;
        let ramp_up = self.parse_ramp_up()?.map(|d| d.as_secs()).unwrap_or(0);
        Ok(duration + ramp_up)
    }

    /// Parse duration string to seconds
    pub fn parse_duration(&self) -> anyhow::Result<u64> {
        let seconds = parse_duration(&self.duration)?.as_secs();
        if seconds == 0 {
            anyhow::bail!("Test duration must be at least one second");
        }
        Ok(seconds)
    }

    /// Parse the configured per-request timeout.
    pub fn parse_timeout(&self) -> anyhow::Result<Duration> {
        parse_duration(&self.timeout)
    }

    /// Parse the configured Prometheus bind address.
    pub fn parse_prometheus_bind(&self) -> anyhow::Result<std::net::IpAddr> {
        self.prometheus_bind
            .trim()
            .parse::<std::net::IpAddr>()
            .map_err(|_| {
                anyhow::anyhow!(
                    "Invalid prometheus_bind '{}'; expected an address such as \
                 '127.0.0.1' or '0.0.0.0'",
                    self.prometheus_bind
                )
            })
    }

    /// Parse the optional worker ramp-up duration.
    pub fn parse_ramp_up(&self) -> anyhow::Result<Option<Duration>> {
        parse_optional_duration(self.ramp_up.as_deref())
    }

    /// Parse the optional simple-mode think time.
    pub fn parse_think_time(&self) -> anyhow::Result<Option<Duration>> {
        parse_optional_duration(self.think_time.as_deref())
    }

    /// Parse the think time configured for a scenario step.
    pub fn parse_think_time_for(&self, scenario: &Scenario) -> anyhow::Result<Option<Duration>> {
        parse_optional_duration(scenario.think_time.as_deref())
    }

    /// Parse the top-level retry delay, defaulting to no delay.
    pub fn parse_retry_delay(&self) -> anyhow::Result<Duration> {
        parse_optional_retry_delay(self.retry_delay.as_deref())
    }

    /// Resolve a scenario's retry count against the top-level default.
    pub fn retry_count_for(&self, scenario: &Scenario) -> u32 {
        scenario.retry_count.unwrap_or(self.retry_count)
    }

    /// Resolve a scenario's retry delay against the top-level default.
    pub fn retry_delay_for(&self, scenario: &Scenario) -> anyhow::Result<Duration> {
        match scenario.retry_delay.as_deref() {
            Some(value) => parse_duration_allow_zero(value),
            None => self.parse_retry_delay(),
        }
    }

    /// Resolve a scenario's retryable statuses against the top-level default.
    pub fn retry_statuses_for<'a>(&'a self, scenario: &'a Scenario) -> &'a [u16] {
        scenario
            .retry_on_status
            .as_deref()
            .unwrap_or(&self.retry_on_status)
    }

    /// Apply runtime values that override the YAML configuration.
    pub fn apply_overrides(
        &mut self,
        concurrency: Option<usize>,
        duration: Option<String>,
        output_json: Option<String>,
        output_html: Option<String>,
        output_csv: Option<String>,
    ) -> anyhow::Result<()> {
        if self.load_profile.is_some() && (concurrency.is_some() || duration.is_some()) {
            anyhow::bail!(
                "--concurrency/--duration cannot override a configuration that \
                 defines load_profile; edit load_profile in the YAML instead"
            );
        }
        if let Some(concurrency) = concurrency {
            self.concurrency = concurrency;
        }
        if let Some(duration) = duration {
            self.duration = duration;
        }
        if let Some(output_json) = output_json {
            self.output.json = output_json;
        }
        if let Some(output_html) = output_html {
            self.output.html = output_html;
        }
        if let Some(output_csv) = output_csv {
            self.output.csv = Some(output_csv);
        }
        self.validate()
    }

    /// Evaluate configured aggregate assertions against a final summary.
    pub fn evaluate_assertions(&self, summary: &MetricsSummary) -> Vec<String> {
        let Some(assertions) = &self.assertions else {
            return Vec::new();
        };
        let mut failures = Vec::new();

        if let Some(max) = assertions.max_error_rate {
            if summary.error_rate > max {
                failures.push(format!(
                    "error rate {:.2}% exceeds maximum {:.2}%",
                    summary.error_rate, max
                ));
            }
        }
        if let Some(max) = assertions.max_p99_ms {
            if summary.p99_latency_ms > max {
                failures.push(format!(
                    "p99 latency {}ms exceeds maximum {}ms",
                    summary.p99_latency_ms, max
                ));
            }
        }
        if let Some(max) = assertions.max_p95_ms {
            if summary.p95_latency_ms > max {
                failures.push(format!(
                    "p95 latency {}ms exceeds maximum {}ms",
                    summary.p95_latency_ms, max
                ));
            }
        }
        if let Some(max) = assertions.max_avg_ms {
            if summary.mean_latency_ms > max {
                failures.push(format!(
                    "mean latency {:.2}ms exceeds maximum {:.2}ms",
                    summary.mean_latency_ms, max
                ));
            }
        }

        failures
    }

    fn expand_environment_variables(&mut self) -> anyhow::Result<()> {
        self.target = expand_optional(self.target.take())?;
        self.method = expand_optional(self.method.take())?;
        self.headers = expand_map(std::mem::take(&mut self.headers))?;
        self.body = expand_optional(self.body.take())?;
        self.multipart = expand_multipart(self.multipart.take())?;
        self.duration = expand_environment_value(&self.duration)?;
        self.timeout = expand_environment_value(&self.timeout)?;
        self.ramp_up = expand_optional(self.ramp_up.take())?;
        self.prometheus_bind = expand_environment_value(&self.prometheus_bind)?;
        if let Some(profile) = &mut self.load_profile {
            match profile {
                LoadProfile::Stages { stages } => {
                    for stage in stages {
                        stage.duration = expand_environment_value(&stage.duration)?;
                    }
                }
                LoadProfile::ArrivalRate { duration, .. } => {
                    *duration = expand_environment_value(duration)?;
                }
            }
        }
        self.think_time = expand_optional(self.think_time.take())?;
        self.retry_delay = expand_optional(self.retry_delay.take())?;
        self.mode = expand_environment_value(&self.mode)?;
        if let Some(dashboard) = &mut self.live_dashboard {
            dashboard.bind = expand_environment_value(&dashboard.bind)?;
            for secret in &mut dashboard.redact {
                *secret = expand_environment_value(secret)?;
            }
        }
        self.output.json = expand_environment_value(&self.output.json)?;
        self.output.html = expand_environment_value(&self.output.html)?;
        self.output.csv = expand_optional(self.output.csv.take())?;

        for scenario in &mut self.scenarios {
            scenario.name = expand_environment_value(&scenario.name)?;
            scenario.method = expand_environment_value(&scenario.method)?;
            scenario.url = expand_environment_value(&scenario.url)?;
            scenario.headers = expand_map(std::mem::take(&mut scenario.headers))?;
            scenario.body = expand_optional(scenario.body.take())?;
            scenario.multipart = expand_multipart(scenario.multipart.take())?;
            scenario.extract = expand_map(std::mem::take(&mut scenario.extract))?;
            scenario.depends_on = expand_optional(scenario.depends_on.take())?;
            scenario.think_time = expand_optional(scenario.think_time.take())?;
            scenario.retry_delay = expand_optional(scenario.retry_delay.take())?;
            if let Some(assertions) = &mut scenario.assertions {
                assertions.body_contains = expand_optional(assertions.body_contains.take())?;
            }
        }

        Ok(())
    }

    /// Check if running in simple mode (single request type)
    pub fn is_simple_mode(&self) -> bool {
        self.scenarios.is_empty()
    }
}

fn parse_duration(value: &str) -> anyhow::Result<Duration> {
    let duration = parse_duration_allow_zero(value)?;
    if duration.is_zero() {
        anyhow::bail!("Duration must be greater than zero");
    }
    Ok(duration)
}

fn parse_duration_allow_zero(value: &str) -> anyhow::Result<Duration> {
    let value = value.trim();
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1_u64)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000)
    } else if let Some(number) = value.strip_suffix('h') {
        (number, 3_600_000)
    } else {
        (value, 1_000)
    };
    let milliseconds = number
        .trim()
        .parse::<u64>()?
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow::anyhow!("Duration is too large: {value}"))?;

    Ok(Duration::from_millis(milliseconds))
}

fn parse_optional_duration(value: Option<&str>) -> anyhow::Result<Option<Duration>> {
    value.map(parse_duration).transpose()
}

fn parse_optional_retry_delay(value: Option<&str>) -> anyhow::Result<Duration> {
    value
        .map(parse_duration_allow_zero)
        .transpose()
        .map(Option::unwrap_or_default)
}

fn validate_status_codes(statuses: &[u16], field: &str) -> anyhow::Result<()> {
    if let Some(invalid) = statuses
        .iter()
        .copied()
        .find(|status| !(100..=599).contains(status))
    {
        anyhow::bail!("Invalid HTTP status code {invalid} in {field}");
    }
    Ok(())
}

fn expand_optional(value: Option<String>) -> anyhow::Result<Option<String>> {
    value
        .map(|value| expand_environment_value(&value))
        .transpose()
}

fn expand_map(values: HashMap<String, String>) -> anyhow::Result<HashMap<String, String>> {
    values
        .into_iter()
        .map(|(key, value)| {
            Ok((
                expand_environment_value(&key)?,
                expand_environment_value(&value)?,
            ))
        })
        .collect()
}

fn expand_multipart(
    parts: Option<Vec<MultipartPart>>,
) -> anyhow::Result<Option<Vec<MultipartPart>>> {
    parts
        .map(|parts| {
            parts
                .into_iter()
                .map(|mut part| {
                    part.part_type = expand_environment_value(&part.part_type)?;
                    part.name = expand_environment_value(&part.name)?;
                    part.path = expand_optional(part.path)?;
                    part.value = expand_optional(part.value)?;
                    Ok(part)
                })
                .collect()
        })
        .transpose()
}

fn expand_environment_value(value: &str) -> anyhow::Result<String> {
    expand_with_lookup(value, |name| std::env::var(name).ok())
}

fn expand_with_lookup<F>(value: &str, lookup: F) -> anyhow::Result<String>
where
    F: Fn(&str) -> Option<String>,
{
    let mut expanded = String::with_capacity(value.len());
    let mut remaining = value;

    while let Some(start) = remaining.find("${") {
        expanded.push_str(&remaining[..start]);
        let after_start = &remaining[start + 2..];
        let end = after_start.find('}').ok_or_else(|| {
            anyhow::anyhow!("Unclosed environment variable placeholder in '{value}'")
        })?;
        let name = &after_start[..end];
        if name.is_empty() {
            anyhow::bail!("Environment variable placeholder cannot be empty in '{value}'");
        }
        let replacement = lookup(name)
            .ok_or_else(|| anyhow::anyhow!("Environment variable '{name}' is not set"))?;
        expanded.push_str(&replacement);
        remaining = &after_start[end + 1..];
    }

    expanded.push_str(remaining);
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::DEFAULT_MAX_STORED_RESULTS;

    #[test]
    fn test_parse_duration() {
        let config = Config {
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
                json: "/app/results/output.json".to_string(),
                html: "/app/results/output.html".to_string(),
                csv: None,
                max_results: 0,
            },
        };

        assert_eq!(config.parse_duration().unwrap(), 30);

        let config_min = Config {
            duration: "5m".to_string(),
            ..config.clone()
        };
        assert_eq!(config_min.parse_duration().unwrap(), 300);

        let config_hour = Config {
            duration: "2h".to_string(),
            ..config
        };
        assert_eq!(config_hour.parse_duration().unwrap(), 7200);
    }

    #[test]
    fn test_parse_timeout() {
        let config = Config {
            target: Some("http://example.com".to_string()),
            method: Some("GET".to_string()),
            headers: HashMap::new(),
            body: None,
            multipart: None,
            scenarios: vec![],
            concurrency: 1,
            duration: "1s".to_string(),
            timeout: "250ms".to_string(),
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
                json: "out.json".to_string(),
                html: "out.html".to_string(),
                csv: None,
                max_results: 0,
            },
        };
        assert_eq!(config.parse_timeout().unwrap(), Duration::from_millis(250));
    }

    #[test]
    fn test_environment_expansion() {
        let values = HashMap::from([("HOST", "example.test"), ("TOKEN", "secret")]);
        let expanded = expand_with_lookup("https://${HOST}/?token=${TOKEN}", |name| {
            values.get(name).map(ToString::to_string)
        })
        .unwrap();
        assert_eq!(expanded, "https://example.test/?token=secret");
    }

    #[test]
    fn test_environment_expansion_rejects_missing_variable() {
        let error = expand_with_lookup("${MISSING}", |_| None).unwrap_err();
        assert!(error.to_string().contains("MISSING"));
    }

    #[test]
    fn test_overrides_take_precedence() {
        let mut config = Config {
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
                json: "original.json".to_string(),
                html: "original.html".to_string(),
                csv: None,
                max_results: 0,
            },
        };

        config
            .apply_overrides(
                Some(25),
                Some("1m".to_string()),
                Some("override.json".to_string()),
                Some("override.html".to_string()),
                Some("override.csv".to_string()),
            )
            .unwrap();

        assert_eq!(config.concurrency, 25);
        assert_eq!(config.parse_duration().unwrap(), 60);
        assert_eq!(config.output.json, "override.json");
        assert_eq!(config.output.html, "override.html");
        assert_eq!(config.output.csv.as_deref(), Some("override.csv"));
    }

    #[test]
    fn test_load_modeling_durations_and_retry_delay() {
        let mut config = Config {
            target: Some("http://example.com".to_string()),
            method: Some("GET".to_string()),
            headers: HashMap::new(),
            body: None,
            multipart: None,
            scenarios: vec![],
            concurrency: 10,
            duration: "30s".to_string(),
            timeout: "5s".to_string(),
            ramp_up: Some("10s".to_string()),
            load_profile: None,
            think_time: Some("250ms".to_string()),
            retry_count: 2,
            retry_delay: Some("0s".to_string()),
            retry_on_status: vec![503],
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
        };

        config.validate().unwrap();
        assert_eq!(
            config.parse_ramp_up().unwrap(),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            config.parse_think_time().unwrap(),
            Some(Duration::from_millis(250))
        );
        assert_eq!(config.parse_retry_delay().unwrap(), Duration::ZERO);

        // Ramp-up is warm-up time in front of the measured window, so a ramp-up
        // longer than the duration is valid: the run simply takes longer.
        config.ramp_up = Some("31s".to_string());
        config.validate().unwrap();
        assert_eq!(
            config.parse_ramp_up().unwrap(),
            Some(Duration::from_secs(31))
        );
    }

    fn scenario(name: &str, depends_on: Option<&str>) -> Scenario {
        Scenario {
            name: name.to_string(),
            method: "GET".to_string(),
            url: "/".to_string(),
            headers: HashMap::new(),
            body: None,
            multipart: None,
            extract: HashMap::new(),
            depends_on: depends_on.map(ToString::to_string),
            think_time: None,
            retry_count: None,
            retry_delay: None,
            retry_on_status: None,
            assertions: None,
        }
    }

    fn scenario_config(scenarios: Vec<Scenario>) -> Config {
        Config {
            target: Some("http://example.com".to_string()),
            method: Some("GET".to_string()),
            headers: HashMap::new(),
            body: None,
            multipart: None,
            scenarios,
            concurrency: 1,
            duration: "1s".to_string(),
            timeout: "1s".to_string(),
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
    fn test_max_results_defaults_to_a_bounded_value() {
        let yaml = r#"
target: "http://example.com"
output:
  json: "output.json"
  html: "output.html"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.output.max_results, DEFAULT_MAX_STORED_RESULTS);

        let unlimited = r#"
target: "http://example.com"
output:
  json: "output.json"
  html: "output.html"
  max_results: 0
"#;
        let config: Config = serde_yaml::from_str(unlimited).unwrap();
        assert_eq!(config.output.max_results, 0);
    }

    #[test]
    fn test_live_dashboard_is_absent_unless_configured() {
        let yaml = r#"
target: "http://example.com"
output:
  json: "output.json"
  html: "output.html"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.live_dashboard.is_none());
    }

    #[test]
    fn test_live_dashboard_defaults_to_loopback() {
        let yaml = r#"
target: "http://example.com"
live_dashboard: {}
output:
  json: "output.json"
  html: "output.html"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let dashboard = config.live_dashboard.as_ref().unwrap();

        assert_eq!(dashboard.bind, "127.0.0.1:9090");
        assert_eq!(dashboard.refresh_ms, 1_000);
        assert!(dashboard.redact.is_empty());
        assert!(dashboard.parse_bind().unwrap().ip().is_loopback());
        config.validate().unwrap();
    }

    #[test]
    fn test_live_dashboard_accepts_explicit_settings() {
        let yaml = r#"
target: "http://example.com"
live_dashboard:
  bind: "0.0.0.0:8080"
  refresh_ms: 500
  redact:
    - "my-secret"
output:
  json: "output.json"
  html: "output.html"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        config.validate().unwrap();
        let dashboard = config.live_dashboard.as_ref().unwrap();

        let address = dashboard.parse_bind().unwrap();
        assert_eq!(address.port(), 8080);
        assert!(!address.ip().is_loopback());
        assert_eq!(dashboard.refresh_ms, 500);
        assert_eq!(dashboard.redact, vec!["my-secret".to_string()]);
    }

    #[test]
    fn test_live_dashboard_rejects_invalid_settings() {
        let mut config = scenario_config(vec![]);
        config.live_dashboard = Some(LiveDashboardConfig {
            bind: "localhost:9090".to_string(),
            ..Default::default()
        });
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("Invalid live_dashboard.bind"), "{error}");

        config.live_dashboard = Some(LiveDashboardConfig {
            bind: "127.0.0.1:0".to_string(),
            ..Default::default()
        });
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("greater than 0"), "{error}");

        config.live_dashboard = Some(LiveDashboardConfig {
            refresh_ms: 10,
            ..Default::default()
        });
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("refresh_ms"), "{error}");
    }

    #[test]
    fn test_prometheus_bind_defaults_to_loopback_and_rejects_garbage() {
        let config = scenario_config(vec![]);
        assert_eq!(config.prometheus_bind, "127.0.0.1");
        assert_eq!(
            config.parse_prometheus_bind().unwrap(),
            std::net::IpAddr::from([127, 0, 0, 1])
        );

        let mut config = config;
        config.prometheus_bind = "not-an-ip".to_string();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("Invalid prometheus_bind"), "{error}");
    }

    #[test]
    fn test_valid_dependency_chain_is_accepted() {
        let config = scenario_config(vec![
            scenario("login", None),
            scenario("get-profile", Some("login")),
            scenario("logout", Some("get-profile")),
        ]);

        config.validate().unwrap();
    }

    #[test]
    fn test_unknown_dependency_is_rejected() {
        let config = scenario_config(vec![
            scenario("login", None),
            scenario("get-profile", Some("logn")),
        ]);

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("unknown scenario 'logn'"), "{error}");
        assert!(error.contains("'login'"), "{error}");
    }

    #[test]
    fn test_duplicate_scenario_names_are_rejected() {
        let config = scenario_config(vec![
            scenario("login", None),
            scenario("checkout", None),
            scenario("login", None),
        ]);

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("Duplicate scenario name 'login'"), "{error}");
        assert!(error.contains("steps 1 and 3"), "{error}");
    }

    #[test]
    fn test_empty_scenario_name_is_rejected() {
        let config = scenario_config(vec![scenario("  ", None)]);

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("step 1 has an empty 'name'"), "{error}");
    }

    #[test]
    fn test_self_dependency_is_rejected() {
        let config = scenario_config(vec![scenario("login", Some("login"))]);

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("depends on itself"), "{error}");
    }

    #[test]
    fn test_empty_dependency_is_rejected() {
        let config = scenario_config(vec![scenario("login", Some("  "))]);

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("empty 'depends_on'"), "{error}");
    }

    #[test]
    fn test_forward_dependency_is_rejected() {
        let config = scenario_config(vec![
            scenario("get-profile", Some("login")),
            scenario("login", None),
        ]);

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("which runs later"), "{error}");
        assert!(error.contains("declaration order"), "{error}");
    }

    #[test]
    fn test_dependency_cycle_is_rejected() {
        // A cycle always contains at least one step that depends on a later
        // step, so it cannot survive validation.
        let config = scenario_config(vec![
            scenario("a", Some("b")),
            scenario("b", Some("c")),
            scenario("c", Some("a")),
        ]);

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("runs later"), "{error}");
    }

    #[test]
    fn test_aggregate_assertion_evaluation() {
        let config = Config {
            target: Some("http://example.com".to_string()),
            method: Some("GET".to_string()),
            headers: HashMap::new(),
            body: None,
            multipart: None,
            scenarios: vec![],
            concurrency: 1,
            duration: "1s".to_string(),
            timeout: "1s".to_string(),
            ramp_up: None,
            load_profile: None,
            think_time: None,
            retry_count: 0,
            retry_delay: None,
            retry_on_status: vec![],
            assertions: Some(AssertionsConfig {
                max_error_rate: Some(1.0),
                max_p99_ms: Some(500),
                max_p95_ms: Some(300),
                max_avg_ms: Some(200.0),
            }),
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
        };
        let summary = MetricsSummary {
            total_requests: 100,
            successful_requests: 95,
            failed_requests: 5,
            total_duration_secs: 1.0,
            ramp_up_secs: 0.0,
            measured_duration_secs: 1.0,
            measured_requests: 0,
            throughput_rps: 100.0,
            min_latency_ms: 10,
            max_latency_ms: 800,
            mean_latency_ms: 250.0,
            p50_latency_ms: 100,
            p90_latency_ms: 200,
            p95_latency_ms: 350,
            p99_latency_ms: 600,
            error_rate: 5.0,
            start_time: chrono::Utc::now(),
            end_time: chrono::Utc::now(),
            per_scenario: Default::default(),
            status_codes: Default::default(),
            skipped_scenarios: Default::default(),
            retained_results: 0,
            dropped_results: 0,
            csv_dropped_rows: 0,
            load_profile: None,
            stages: Vec::new(),
        };

        let failures = config.evaluate_assertions(&summary);
        assert_eq!(failures.len(), 4);
        assert!(failures
            .iter()
            .any(|failure| failure.contains("error rate")));
        assert!(failures.iter().any(|failure| failure.contains("p99")));
    }

    fn stages_config(stages: Vec<Stage>) -> Config {
        let mut config = scenario_config(vec![]);
        config.load_profile = Some(LoadProfile::Stages { stages });
        config
    }

    fn arrival_rate_config(target_rps: f64, duration: &str, max_concurrency: usize) -> Config {
        let mut config = scenario_config(vec![]);
        config.load_profile = Some(LoadProfile::ArrivalRate {
            target_rps,
            duration: duration.to_string(),
            max_concurrency,
        });
        config
    }

    #[test]
    fn test_stages_profile_parses_and_validates() {
        let config = stages_config(vec![
            Stage {
                duration: "30s".to_string(),
                target_concurrency: 10,
            },
            Stage {
                duration: "2m".to_string(),
                target_concurrency: 100,
            },
            Stage {
                duration: "30s".to_string(),
                target_concurrency: 0,
            },
        ]);

        config.validate().unwrap();
        let stages = config.parsed_stages().unwrap().unwrap();
        assert_eq!(
            stages,
            vec![
                (Duration::from_secs(30), 10),
                (Duration::from_secs(120), 100),
                (Duration::from_secs(30), 0),
            ]
        );
        assert_eq!(config.total_planned_secs().unwrap(), 180);
        assert!(config.parsed_arrival_rate().unwrap().is_none());
    }

    #[test]
    fn test_stages_profile_rejects_empty_stage_list() {
        let config = stages_config(vec![]);
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("at least one stage"), "{error}");
    }

    #[test]
    fn test_stages_profile_rejects_zero_duration_stage() {
        let config = stages_config(vec![Stage {
            duration: "0s".to_string(),
            target_concurrency: 10,
        }]);
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("Invalid duration for stage 1"), "{error}");
        assert!(error.contains("greater than zero"), "{error}");
    }

    #[test]
    fn test_stages_profile_rejects_unparseable_duration() {
        let config = stages_config(vec![Stage {
            duration: "not-a-duration".to_string(),
            target_concurrency: 10,
        }]);
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("Invalid duration for stage 1"), "{error}");
    }

    #[test]
    fn test_arrival_rate_profile_parses_and_validates() {
        let config = arrival_rate_config(200.0, "5m", 500);
        config.validate().unwrap();

        let (target_rps, duration, max_concurrency) =
            config.parsed_arrival_rate().unwrap().unwrap();
        assert_eq!(target_rps, 200.0);
        assert_eq!(duration, Duration::from_secs(300));
        assert_eq!(max_concurrency, 500);
        assert_eq!(config.total_planned_secs().unwrap(), 300);
        assert!(config.parsed_stages().unwrap().is_none());
    }

    #[test]
    fn test_arrival_rate_defaults_max_concurrency() {
        let yaml = r#"
target: "http://example.com"
load_profile:
  type: arrival_rate
  target_rps: 50
  duration: "1m"
output:
  json: "output.json"
  html: "output.html"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        config.validate().unwrap();
        let (_, _, max_concurrency) = config.parsed_arrival_rate().unwrap().unwrap();
        assert_eq!(max_concurrency, 1_000);
    }

    #[test]
    fn test_arrival_rate_rejects_non_positive_target_rps() {
        let config = arrival_rate_config(0.0, "1m", 10);
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("target_rps"), "{error}");

        let config = arrival_rate_config(-5.0, "1m", 10);
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_arrival_rate_rejects_zero_max_concurrency() {
        let config = arrival_rate_config(10.0, "1m", 0);
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("max_concurrency"), "{error}");
    }

    #[test]
    fn test_load_profile_rejects_ramp_up_combination() {
        let mut config = stages_config(vec![Stage {
            duration: "10s".to_string(),
            target_concurrency: 5,
        }]);
        config.ramp_up = Some("5s".to_string());

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("ramp_up cannot be combined"), "{error}");
    }

    #[test]
    fn test_overrides_reject_concurrency_and_duration_with_load_profile() {
        let mut config = stages_config(vec![Stage {
            duration: "10s".to_string(),
            target_concurrency: 5,
        }]);

        let error = config
            .apply_overrides(Some(50), None, None, None, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("load_profile"), "{error}");

        let error = config
            .apply_overrides(None, Some("1m".to_string()), None, None, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("load_profile"), "{error}");

        // Overriding only output paths is unaffected.
        config
            .apply_overrides(None, None, Some("out.json".to_string()), None, None)
            .unwrap();
        assert_eq!(config.output.json, "out.json");
    }

    #[test]
    fn test_stage_durations_expand_environment_variables() {
        std::env::set_var("FLUX_TEST_STAGE_DURATION", "15s");
        let yaml = r#"
target: "http://example.com"
load_profile:
  type: stages
  stages:
    - duration: "${FLUX_TEST_STAGE_DURATION}"
      target_concurrency: 5
output:
  json: "output.json"
  html: "output.html"
"#;
        let config = Config::from_file(&{
            let path = std::env::temp_dir().join(format!(
                "flux-load-profile-{}-{}.yaml",
                std::process::id(),
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
            ));
            std::fs::write(&path, yaml).unwrap();
            path
        })
        .unwrap();
        std::env::remove_var("FLUX_TEST_STAGE_DURATION");

        let stages = config.parsed_stages().unwrap().unwrap();
        assert_eq!(stages, vec![(Duration::from_secs(15), 5)]);
    }

    #[test]
    fn test_total_planned_secs_covers_every_profile() {
        let constant = scenario_config(vec![]);
        assert_eq!(constant.total_planned_secs().unwrap(), 1);

        let mut with_ramp_up = scenario_config(vec![]);
        with_ramp_up.ramp_up = Some("4s".to_string());
        assert_eq!(with_ramp_up.total_planned_secs().unwrap(), 5);

        let staged = stages_config(vec![
            Stage {
                duration: "10s".to_string(),
                target_concurrency: 1,
            },
            Stage {
                duration: "20s".to_string(),
                target_concurrency: 2,
            },
        ]);
        assert_eq!(staged.total_planned_secs().unwrap(), 30);

        let arrival = arrival_rate_config(100.0, "45s", 50);
        assert_eq!(arrival.total_planned_secs().unwrap(), 45);
    }

    #[test]
    fn test_sample_staged_load_profile_config_is_valid() {
        let config = Config::from_file(&PathBuf::from("samples/staged-load-profile.yaml")).unwrap();
        let stages = config.parsed_stages().unwrap().unwrap();
        assert_eq!(stages.len(), 3);
        assert_eq!(config.total_planned_secs().unwrap(), 30 + 120 + 30);
    }

    #[test]
    fn test_sample_arrival_rate_profile_config_is_valid() {
        let config =
            Config::from_file(&PathBuf::from("samples/arrival-rate-profile.yaml")).unwrap();
        let (target_rps, duration, max_concurrency) =
            config.parsed_arrival_rate().unwrap().unwrap();
        assert_eq!(target_rps, 200.0);
        assert_eq!(duration, Duration::from_secs(300));
        assert_eq!(max_concurrency, 500);
    }
}
