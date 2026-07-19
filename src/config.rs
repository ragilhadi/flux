use crate::metrics::MetricsSummary;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
        if let Some(ramp_up) = self.parse_ramp_up()? {
            if ramp_up > Duration::from_secs(self.parse_duration()?) {
                anyhow::bail!("Ramp-up duration cannot exceed the test duration");
            }
        }
        self.parse_think_time()?;
        self.parse_retry_delay()?;
        validate_status_codes(&self.retry_on_status, "retry_on_status")?;
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
        self.think_time = expand_optional(self.think_time.take())?;
        self.retry_delay = expand_optional(self.retry_delay.take())?;
        self.mode = expand_environment_value(&self.mode)?;
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
            think_time: None,
            retry_count: 0,
            retry_delay: None,
            retry_on_status: vec![],
            assertions: None,
            mode: "async".to_string(),
            output: OutputConfig {
                json: "/app/results/output.json".to_string(),
                html: "/app/results/output.html".to_string(),
                csv: None,
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
            think_time: None,
            retry_count: 0,
            retry_delay: None,
            retry_on_status: vec![],
            assertions: None,
            mode: "async".to_string(),
            output: OutputConfig {
                json: "out.json".to_string(),
                html: "out.html".to_string(),
                csv: None,
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
            think_time: None,
            retry_count: 0,
            retry_delay: None,
            retry_on_status: vec![],
            assertions: None,
            mode: "async".to_string(),
            output: OutputConfig {
                json: "original.json".to_string(),
                html: "original.html".to_string(),
                csv: None,
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
            think_time: Some("250ms".to_string()),
            retry_count: 2,
            retry_delay: Some("0s".to_string()),
            retry_on_status: vec![503],
            assertions: None,
            mode: "async".to_string(),
            output: OutputConfig {
                json: "output.json".to_string(),
                html: "output.html".to_string(),
                csv: None,
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

        config.ramp_up = Some("31s".to_string());
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("Ramp-up"));
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
            mode: "async".to_string(),
            output: OutputConfig {
                json: "output.json".to_string(),
                html: "output.html".to_string(),
                csv: None,
            },
        };
        let summary = MetricsSummary {
            total_requests: 100,
            successful_requests: 95,
            failed_requests: 5,
            total_duration_secs: 1.0,
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
        };

        let failures = config.evaluate_assertions(&summary);
        assert_eq!(failures.len(), 4);
        assert!(failures
            .iter()
            .any(|failure| failure.contains("error rate")));
        assert!(failures.iter().any(|failure| failure.contains("p99")));
    }
}
