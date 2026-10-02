//! Configuration for system-resource monitoring during a load test.

use super::scrape::LabelMatcher;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::Duration;

/// Group name used for the load generator's own process metrics.
pub const LOADGEN_GROUP: &str = "flux";

/// Group name used for host metrics read from the local `/proc`.
pub const HOST_GROUP: &str = "host";

/// Shortest interval at which exporters may be scraped. A cAdvisor scrape
/// renders every container on the host, so polling it faster than this costs
/// the machine under test more than it tells you.
const MIN_SCRAPE_INTERVAL: Duration = Duration::from_secs(1);

/// `monitoring` section of the configuration.
///
/// Monitoring is on by default, but only for what needs no setup: the load
/// generator's own process and the host it runs on, both read from `/proc`.
/// Target containers and remote hosts are observed only when `scrape`
/// targets (cAdvisor, node_exporter) are configured.
///
/// Unknown keys are rejected, so a misspelt option fails loudly instead of
/// silently monitoring less than intended.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

    /// cAdvisor and node_exporter `/metrics` endpoints, scraped directly
    /// while the test runs.
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
            scrape: Vec::new(),
        }
    }
}

/// Kind of exporter a scrape target is.
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

/// A `/metrics` endpoint scraped directly while the test runs.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScrapeTargetConfig {
    /// Name used for this target in reports and assertions.
    pub name: String,

    #[serde(rename = "type")]
    pub kind: TargetKind,

    /// URL of the text-format metrics endpoint, e.g.
    /// "http://cadvisor:8080/metrics".
    pub url: String,

    /// Label matchers selecting the series to use, e.g. `name="my-api"`.
    /// Only `=` and `!=` are supported.
    #[serde(default)]
    pub selector: String,

    /// Record one series per CPU core (node targets only).
    #[serde(default = "default_true")]
    pub per_core: bool,

    /// Name of an environment variable holding a bearer token for the
    /// endpoint, if it requires one. The token is read at runtime and never
    /// written to reports.
    #[serde(default)]
    pub bearer_token_env: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_interval() -> String {
    "2s".to_string()
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
        self.enabled && (self.self_metrics || self.host || !self.scrape.is_empty())
    }

    /// Validate the section, so a typo fails the run before any traffic is
    /// sent rather than after the test when there is nothing left to fix.
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let interval = self.parse_interval()?;
        if !self.scrape.is_empty() && interval < MIN_SCRAPE_INTERVAL {
            anyhow::bail!(
                "monitoring.interval must be at least 1s when scrape targets are configured, \
                 got '{}'; faster scraping costs the machine under test more than it shows",
                self.interval
            );
        }

        let mut groups: HashSet<&str> = HashSet::from([LOADGEN_GROUP, HOST_GROUP]);
        for target in &self.scrape {
            validate_name(&target.name, "monitoring.scrape name")?;
            if !groups.insert(target.name.as_str()) {
                anyhow::bail!(
                    "monitoring.scrape name '{}' is already used; target names must be unique \
                     and '{LOADGEN_GROUP}' and '{HOST_GROUP}' are reserved",
                    target.name
                );
            }
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
        assert!(config.scrape.is_empty());
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
scrape:
  - name: api
    type: cadvisor
    url: "http://cadvisor:8080/metrics"
    selector: 'name="my-api"'
  - name: node
    type: node
    url: "http://node-exporter:9100/metrics"
    per_core: false
    bearer_token_env: "NODE_TOKEN"
"#,
        );
        config.validate().unwrap();
        assert_eq!(config.scrape[0].kind, TargetKind::Cadvisor);
        assert!(!config.scrape[1].per_core);
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
scrape:
  - { name: api, type: cadvisor, url: "http://cadvisor:8080/metrics" }
  - { name: api, type: node, url: "http://node:9100/metrics" }
"#,
        );
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn test_rejects_bad_values() {
        assert!(parse("interval: \"100ms\"").validate().is_err());
        assert!(parse(
            "interval: \"500ms\"\nscrape: [{name: a, type: node, url: \"http://n/metrics\"}]"
        )
        .validate()
        .is_err());
        assert!(
            parse("scrape: [{name: a, type: node, url: \"ftp://n/metrics\"}]")
                .validate()
                .is_err()
        );
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
    fn test_unknown_keys_are_rejected() {
        assert!(serde_yaml::from_str::<MonitoringConfig>("prometheus: {url: x}").is_err());
        assert!(serde_yaml::from_str::<MonitoringConfig>(
            "scrape: [{name: a, type: node, url: \"http://n/metrics\", selecter: x}]"
        )
        .is_err());
    }

    #[test]
    fn test_local_only_monitoring_allows_sub_second_interval() {
        parse("interval: \"500ms\"").validate().unwrap();
    }

    #[test]
    fn test_disabled_section_skips_validation() {
        let config = parse("enabled: false\ninterval: \"nonsense\"");
        config.validate().unwrap();
        assert!(!config.is_active());
    }
}
