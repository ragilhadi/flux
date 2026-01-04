use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

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

fn default_mode() -> String {
    "async".to_string()
}

impl Config {
    /// Load configuration from YAML file
    pub fn from_file(path: &PathBuf) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = serde_yaml::from_str(&content)?;
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
        let duration_str = self.duration.trim();

        if let Some(stripped) = duration_str.strip_suffix('s') {
            Ok(stripped.parse()?)
        } else if let Some(stripped) = duration_str.strip_suffix('m') {
            Ok(stripped.parse::<u64>()? * 60)
        } else if let Some(stripped) = duration_str.strip_suffix('h') {
            Ok(stripped.parse::<u64>()? * 3600)
        } else {
            // Default to seconds if no suffix
            Ok(duration_str.parse()?)
        }
    }

    /// Check if running in simple mode (single request type)
    pub fn is_simple_mode(&self) -> bool {
        self.scenarios.is_empty()
    }
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
}
