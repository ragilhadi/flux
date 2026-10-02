//! Configuration for system-resource monitoring during a load test.

use super::scrape::LabelMatcher;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::Duration;

/// Group name used for the load generator's own process metrics.
pub const LOADGEN_GROUP: &str = "flux";

/// Group name used for host metrics read from the local `/proc`.
pub const HOST_GROUP: &str = "host";

/// `monitoring` section of the configuration.
///
/// Monitoring is on by default, but only for what needs no setup: the load
/// generator's own process and the host it runs on, both read from `/proc`.
/// Target containers and remote hosts are observed only when a `prometheus`
/// or `scrape` source is configured.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MonitoringConfig {
    /// Master switch. `false` collects nothing and adds nothing to reports.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// How often local and scraped sources are sampled (e.g. "2s").
    #[serde(default = "default_interval")]
    pub interval: String,

    /// Sample the load generator's own CPU, memory, threads and file
    /// descriptors, so a saturated load generator is never mistaken for a
    /// slow target.
    #[serde(default = "default_true", rename = "self")]
    pub self_metrics: bool,

    /// Sample CPU (total and per core), memory and load average of the host
    /// Flux runs on, from `/proc`.
    #[serde(default = "default_true")]
    pub host: bool,

    /// Record one series per CPU core for host CPU usage.
    #[serde(default = "default_true")]
    pub per_core: bool,

    /// Query a Prometheus server for the test window after the run.
    #[serde(default)]
    pub prometheus: Option<PrometheusSourceConfig>,

    /// Scrape Prometheus-format `/metrics` endpoints (cAdvisor,
    /// node_exporter) directly while the test runs.
    #[serde(default)]
    pub scrape: Vec<ScrapeTargetConfig>,
}

impl Default for MonitoringConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: default_interval(),
            self_metrics: true,
            host: true,
            per_core: true,
            prometheus: None,
            scrape: Vec::new(),
        }
    }
}

/// Where target metrics come from in Prometheus.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PrometheusSourceConfig {
    /// Base URL of the Prometheus HTTP API, e.g. "http://prometheus:9090".
    pub url: String,

    /// Name of an environment variable holding a bearer token. The token is
    /// read at runtime and never written to reports.
    #[serde(default)]
    pub bearer_token_env: Option<String>,

    /// Resolution of the queried series. Defaults to `monitoring.interval`.
    #[serde(default)]
    pub step: Option<String>,

    /// Range used inside `rate()`. It must span at least two scrapes of the
    /// underlying exporter, or rates come back empty.
    #[serde(default = "default_rate_window")]
    pub rate_window: String,

    /// How long to wait after the run before querying, so the scrape that
    /// covers the end of the test has landed in Prometheus.
    #[serde(default = "default_query_delay")]
    pub query_delay: String,

    /// Timeout for each query.
    #[serde(default = "default_query_timeout")]
    pub timeout: String,

    /// Preset targets (cAdvisor containers, node_exporter hosts).
    #[serde(default)]
    pub targets: Vec<PrometheusTargetConfig>,

    /// Arbitrary PromQL queries, recorded as-is.
    #[serde(default)]
    pub queries: Vec<CustomQueryConfig>,
}

/// Kind of exporter a preset target reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    /// Container metrics from cAdvisor (or a kubelet's embedded cAdvisor).
    Cadvisor,
    /// Host metrics from node_exporter.
    Node,
}

impl TargetKind {
    pub fn group_kind(self) -> &'static str {
        match self {
            TargetKind::Cadvisor => "container",
            TargetKind::Node => "node",
        }
    }
}

/// A preset target read through Prometheus.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PrometheusTargetConfig {
    /// Name used for this target in reports and assertions.
    pub name: String,

    #[serde(rename = "type")]
    pub kind: TargetKind,

    /// PromQL label matchers selecting the target, without braces, e.g.
    /// `name="my-api"` or `container="api",namespace="prod"`.
    #[serde(default)]
    pub selector: String,

    /// Record one series per CPU core (node targets only).
    #[serde(default = "default_true")]
    pub per_core: bool,
}

/// A custom PromQL query.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CustomQueryConfig {
    /// Metric name used in reports.
    pub name: String,

    /// PromQL expression evaluated over the test window.
    pub query: String,

    /// Free-form unit label for display (e.g. "count", "ms", "percent").
    #[serde(default)]
    pub unit: Option<String>,

    /// Group the series belongs to in reports. Defaults to "custom".
    #[serde(default)]
    pub group: Option<String>,
}

/// A `/metrics` endpoint scraped directly while the test runs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScrapeTargetConfig {
    /// Name used for this target in reports and assertions.
    pub name: String,

    #[serde(rename = "type")]
    pub kind: TargetKind,

    /// URL of the Prometheus text-format endpoint, e.g.
    /// "http://cadvisor:8080/metrics".
    pub url: String,

    /// Label matchers selecting the series to use, e.g. `name="my-api"`.
    /// Only `=` and `!=` are supported when scraping directly.
    #[serde(default)]
    pub selector: String,

    /// Record one series per CPU core (node targets only).
    #[serde(default = "default_true")]
    pub per_core: bool,

    /// Name of an environment variable holding a bearer token for the
    /// endpoint, if it requires one.
    #[serde(default)]
    pub bearer_token_env: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_interval() -> String {
    "2s".to_string()
}

fn default_rate_window() -> String {
    "30s".to_string()
}

fn default_query_delay() -> String {
    "5s".to_string()
}

fn default_query_timeout() -> String {
    "15s".to_string()
}

impl MonitoringConfig {
    /// Sampling interval for local and scraped sources.
    pub fn parse_interval(&self) -> anyhow::Result<Duration> {
        let interval = crate::config::parse_duration(&self.interval)
            .map_err(|e| anyhow::anyhow!("Invalid monitoring.interval: {e}"))?;
        if interval < Duration::from_millis(500) {
            anyhow::bail!(
                "monitoring.interval must be at least 500ms, got '{}'",
                self.interval
            );
        }
        Ok(interval)
    }

    /// Whether anything needs to run at all.
    pub fn is_active(&self) -> bool {
        self.enabled
            && (self.self_metrics
                || self.host
                || self.prometheus.is_some()
                || !self.scrape.is_empty())
    }

    /// Validate the section, so a typo fails the run before any traffic is
    /// sent rather than after the test when there is nothing left to fix.
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        self.parse_interval()?;

        let mut groups: HashSet<String> = HashSet::new();
        groups.insert(LOADGEN_GROUP.to_string());
        groups.insert(HOST_GROUP.to_string());
        let mut claim = |name: &str, field: &str| -> anyhow::Result<()> {
            validate_name(name, field)?;
            if !groups.insert(name.to_string()) {
                anyhow::bail!(
                    "{field} '{name}' is already used; target names must be unique and \
                     '{LOADGEN_GROUP}' and '{HOST_GROUP}' are reserved"
                );
            }
            Ok(())
        };

        if let Some(prometheus) = &self.prometheus {
            let url = reqwest::Url::parse(prometheus.url.trim()).map_err(|e| {
                anyhow::anyhow!(
                    "Invalid monitoring.prometheus.url '{}': {e}",
                    prometheus.url
                )
            })?;
            if !matches!(url.scheme(), "http" | "https") {
                anyhow::bail!("monitoring.prometheus.url must use http or https");
            }
            if let Some(step) = &prometheus.step {
                let step = crate::config::parse_duration(step)
                    .map_err(|e| anyhow::anyhow!("Invalid monitoring.prometheus.step: {e}"))?;
                if step < Duration::from_secs(1) {
                    anyhow::bail!("monitoring.prometheus.step must be at least 1s");
                }
            }
            prometheus.parse_rate_window()?;
            crate::config::parse_duration_allow_zero(&prometheus.query_delay)
                .map_err(|e| anyhow::anyhow!("Invalid monitoring.prometheus.query_delay: {e}"))?;
            crate::config::parse_duration(&prometheus.timeout)
                .map_err(|e| anyhow::anyhow!("Invalid monitoring.prometheus.timeout: {e}"))?;
            if prometheus.targets.is_empty() && prometheus.queries.is_empty() {
                anyhow::bail!(
                    "monitoring.prometheus needs at least one entry under 'targets' or 'queries'"
                );
            }
            for target in &prometheus.targets {
                claim(&target.name, "monitoring.prometheus.targets name")?;
                if target.selector.contains('{') || target.selector.contains('}') {
                    anyhow::bail!(
                        "monitoring.prometheus.targets '{}': write the selector without braces, \
                         e.g. name=\"my-api\"",
                        target.name
                    );
                }
            }
            let mut query_names: HashSet<(String, String)> = HashSet::new();
            for query in &prometheus.queries {
                validate_name(&query.name, "monitoring.prometheus.queries name")?;
                if query.query.trim().is_empty() {
                    anyhow::bail!(
                        "monitoring.prometheus.queries '{}' has an empty query",
                        query.name
                    );
                }
                let group = query.group.clone().unwrap_or_else(|| "custom".to_string());
                validate_name(&group, "monitoring.prometheus.queries group")?;
                if !query_names.insert((group.clone(), query.name.clone())) {
                    anyhow::bail!(
                        "monitoring.prometheus.queries '{}' is defined twice in group '{group}'",
                        query.name
                    );
                }
            }
        }

        for target in &self.scrape {
            claim(&target.name, "monitoring.scrape name")?;
            let url = reqwest::Url::parse(target.url.trim()).map_err(|e| {
                anyhow::anyhow!("Invalid monitoring.scrape '{}' url: {e}", target.name)
            })?;
            if !matches!(url.scheme(), "http" | "https") {
                anyhow::bail!(
                    "monitoring.scrape '{}' url must use http or https",
                    target.name
                );
            }
            LabelMatcher::parse_selector(&target.selector).map_err(|e| {
                anyhow::anyhow!("monitoring.scrape '{}' selector: {e}", target.name)
            })?;
        }

        Ok(())
    }
}

impl PrometheusSourceConfig {
    /// Range used inside `rate()`.
    pub fn parse_rate_window(&self) -> anyhow::Result<Duration> {
        let window = crate::config::parse_duration(&self.rate_window)
            .map_err(|e| anyhow::anyhow!("Invalid monitoring.prometheus.rate_window: {e}"))?;
        if window < Duration::from_secs(1) {
            anyhow::bail!("monitoring.prometheus.rate_window must be at least 1s");
        }
        Ok(window)
    }
}

/// Names end up in series ids, CSV rows and assertion messages, so keep them
/// to a predictable character set.
fn validate_name(name: &str, field: &str) -> anyhow::Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        anyhow::bail!(
            "{field} '{name}' must be non-empty and contain only letters, digits, '_', '-' or '.'"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> MonitoringConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn test_defaults_enable_local_sources_only() {
        let config = MonitoringConfig::default();
        assert!(config.enabled && config.self_metrics && config.host && config.per_core);
        assert!(config.prometheus.is_none() && config.scrape.is_empty());
        assert!(config.is_active());
        config.validate().unwrap();
    }

    #[test]
    fn test_full_section_parses_and_validates() {
        let config = parse(
            r#"
interval: "1s"
self: true
host: false
prometheus:
  url: "http://prometheus:9090"
  bearer_token_env: "PROM_TOKEN"
  step: "5s"
  targets:
    - name: api
      type: cadvisor
      selector: 'name="my-api"'
    - name: node
      type: node
      selector: 'instance="node:9100"'
      per_core: false
  queries:
    - name: db_connections
      query: 'sum(pg_stat_activity_count)'
      unit: count
scrape:
  - name: api-direct
    type: cadvisor
    url: "http://cadvisor:8080/metrics"
    selector: 'name="my-api"'
"#,
        );
        config.validate().unwrap();
        let prometheus = config.prometheus.as_ref().unwrap();
        assert_eq!(prometheus.targets[0].kind, TargetKind::Cadvisor);
        assert!(!prometheus.targets[1].per_core);
        assert!(!config.host);
    }

    #[test]
    fn test_rejects_reserved_and_duplicate_names() {
        let reserved = parse(
            r#"
scrape:
  - name: flux
    type: node
    url: "http://node:9100/metrics"
"#,
        );
        assert!(reserved.validate().is_err());

        let duplicate = parse(
            r#"
prometheus:
  url: "http://prometheus:9090"
  targets:
    - { name: api, type: cadvisor, selector: 'name="a"' }
scrape:
  - { name: api, type: cadvisor, url: "http://cadvisor:8080/metrics" }
"#,
        );
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn test_rejects_bad_values() {
        assert!(parse("interval: \"100ms\"").validate().is_err());
        assert!(
            parse("prometheus: { url: \"ftp://x\", queries: [{name: a, query: up}] }")
                .validate()
                .is_err()
        );
        assert!(parse("prometheus: { url: \"http://x:9090\" }")
            .validate()
            .is_err());
        assert!(parse(
            "prometheus: { url: \"http://x:9090\", targets: [{name: a, type: cadvisor, selector: '{name=\"a\"}'}] }"
        )
        .validate()
        .is_err());
        assert!(parse(
            "scrape: [{name: a, type: cadvisor, url: \"http://c/metrics\", selector: 'name=~\"a.*\"'}]"
        )
        .validate()
        .is_err());
        assert!(
            parse("scrape: [{name: 'bad name', type: node, url: \"http://n/metrics\"}]")
                .validate()
                .is_err()
        );
    }

    #[test]
    fn test_disabled_section_skips_validation() {
        let config = parse("enabled: false\ninterval: \"nonsense\"");
        config.validate().unwrap();
        assert!(!config.is_active());
    }
}
