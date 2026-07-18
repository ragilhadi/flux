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
}

/// Output configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OutputConfig {
    /// JSON output file path
    pub json: String,

    /// HTML output file path
    pub html: String,
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

    /// Apply runtime values that override the YAML configuration.
    pub fn apply_overrides(
        &mut self,
        concurrency: Option<usize>,
        duration: Option<String>,
        output_json: Option<String>,
        output_html: Option<String>,
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
        self.validate()
    }

    fn expand_environment_variables(&mut self) -> anyhow::Result<()> {
        self.target = expand_optional(self.target.take())?;
        self.method = expand_optional(self.method.take())?;
        self.headers = expand_map(std::mem::take(&mut self.headers))?;
        self.body = expand_optional(self.body.take())?;
        self.multipart = expand_multipart(self.multipart.take())?;
        self.duration = expand_environment_value(&self.duration)?;
        self.timeout = expand_environment_value(&self.timeout)?;
        self.mode = expand_environment_value(&self.mode)?;
        self.output.json = expand_environment_value(&self.output.json)?;
        self.output.html = expand_environment_value(&self.output.html)?;

        for scenario in &mut self.scenarios {
            scenario.name = expand_environment_value(&scenario.name)?;
            scenario.method = expand_environment_value(&scenario.method)?;
            scenario.url = expand_environment_value(&scenario.url)?;
            scenario.headers = expand_map(std::mem::take(&mut scenario.headers))?;
            scenario.body = expand_optional(scenario.body.take())?;
            scenario.multipart = expand_multipart(scenario.multipart.take())?;
            scenario.extract = expand_map(std::mem::take(&mut scenario.extract))?;
            scenario.depends_on = expand_optional(scenario.depends_on.take())?;
        }

        Ok(())
    }

    /// Check if running in simple mode (single request type)
    pub fn is_simple_mode(&self) -> bool {
        self.scenarios.is_empty()
    }
}

fn parse_duration(value: &str) -> anyhow::Result<Duration> {
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

    if milliseconds == 0 {
        anyhow::bail!("Duration must be greater than zero");
    }

    Ok(Duration::from_millis(milliseconds))
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
            mode: "async".to_string(),
            output: OutputConfig {
                json: "/app/results/output.json".to_string(),
                html: "/app/results/output.html".to_string(),
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
            mode: "async".to_string(),
            output: OutputConfig {
                json: "out.json".to_string(),
                html: "out.html".to_string(),
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
            mode: "async".to_string(),
            output: OutputConfig {
                json: "original.json".to_string(),
                html: "original.html".to_string(),
            },
        };

        config
            .apply_overrides(
                Some(25),
                Some("1m".to_string()),
                Some("override.json".to_string()),
                Some("override.html".to_string()),
            )
            .unwrap();

        assert_eq!(config.concurrency, 25);
        assert_eq!(config.parse_duration().unwrap(), 60);
        assert_eq!(config.output.json, "override.json");
        assert_eq!(config.output.html, "override.html");
    }
}
