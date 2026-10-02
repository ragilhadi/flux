//! Direct scraping of cAdvisor and node_exporter `/metrics` endpoints.
//!
//! Counters are turned into rates locally, from the difference between two
//! consecutive scrapes, with counter resets treated as a fresh start.
//!
//! A cAdvisor scrape describes every container on the host and can run to
//! megabytes, so extraction is built to be cheap: only the metric families
//! used below are parsed at all, labels are matched against the selector
//! without allocating, and only matching series are copied out.

use super::config::{ScrapeTargetConfig, TargetKind};
use super::series::{SeriesKey, SeriesKind, SeriesStore};
use std::borrow::Cow;
use std::collections::BTreeMap;

const MIB: f64 = 1024.0 * 1024.0;

/// cAdvisor reports an unlimited memory limit as 0 or as a huge sentinel.
const UNLIMITED_MEMORY_BYTES: f64 = (1u64 << 60) as f64;

/// cAdvisor metric families Flux reads; every other line is skipped before
/// its labels are parsed.
const CADVISOR_FAMILIES: &[&str] = &[
    "container_cpu_usage_seconds_total",
    "container_cpu_cfs_throttled_periods_total",
    "container_cpu_cfs_periods_total",
    "container_spec_cpu_quota",
    "container_spec_cpu_period",
    "container_memory_working_set_bytes",
    "container_memory_rss",
    "container_memory_cache",
    "container_spec_memory_limit_bytes",
    "container_network_receive_bytes_total",
    "container_network_transmit_bytes_total",
    "container_network_receive_errors_total",
    "container_network_transmit_errors_total",
    "container_fs_reads_bytes_total",
    "container_fs_writes_bytes_total",
    "container_processes",
    "container_file_descriptors",
    "container_oom_events_total",
];

/// node_exporter metric families Flux reads.
const NODE_FAMILIES: &[&str] = &[
    "node_cpu_seconds_total",
    "node_memory_MemTotal_bytes",
    "node_memory_MemAvailable_bytes",
    "node_load1",
    "node_netstat_Tcp_RetransSegs",
    "node_sockstat_TCP_inuse",
    "node_sockstat_TCP_tw",
    "node_network_receive_bytes_total",
    "node_network_transmit_bytes_total",
    "node_disk_io_time_seconds_total",
];

/// Metric families read for a kind of target.
pub fn families(kind: TargetKind) -> &'static [&'static str] {
    match kind {
        TargetKind::Cadvisor => CADVISOR_FAMILIES,
        TargetKind::Node => NODE_FAMILIES,
    }
}

/// One sample from a text-format exposition.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub value: f64,
}

/// Labels of one line, borrowed from the scrape where possible.
type BorrowedLabels<'a> = Vec<(&'a str, Cow<'a, str>)>;

/// Extract the samples of `families` (every family when empty) whose labels
/// satisfy `matchers`, from a text-format exposition.
///
/// Comments, blank lines and malformed lines are skipped: one odd line from an
/// exporter should not throw away an entire scrape.
pub fn extract(text: &str, families: &[&str], matchers: &[LabelMatcher]) -> Vec<Sample> {
    let mut samples = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let name_end = line
            .find(|c: char| c == '{' || c.is_whitespace())
            .unwrap_or(line.len());
        let name = &line[..name_end];
        if name.is_empty() || (!families.is_empty() && !families.contains(&name)) {
            continue;
        }
        if let Some(sample) = parse_sample(name, &line[name_end..], matchers) {
            samples.push(sample);
        }
    }
    samples
}

/// Every sample of an exposition.
#[cfg(test)]
pub fn parse_exposition(text: &str) -> Vec<Sample> {
    extract(text, &[], &[])
}

fn parse_sample(name: &str, rest: &str, matchers: &[LabelMatcher]) -> Option<Sample> {
    let (labels, rest) = match rest.strip_prefix('{') {
        Some(inner) => parse_labels(inner)?,
        None => (Vec::new(), rest),
    };
    if !matchers.iter().all(|matcher| matcher.matches(&labels)) {
        return None;
    }
    let value = parse_value(rest.split_whitespace().next()?)?;
    Some(Sample {
        name: name.to_string(),
        labels: labels
            .into_iter()
            .map(|(name, value)| (name.to_string(), value.into_owned()))
            .collect(),
        value,
    })
}

/// Parse `a="x",b="y"}` and return the labels plus the text after `}`.
/// Values without escape sequences are borrowed rather than copied.
fn parse_labels(mut text: &str) -> Option<(BorrowedLabels<'_>, &str)> {
    let mut labels = Vec::new();
    loop {
        text = text.trim_start();
        if let Some(after) = text.strip_prefix('}') {
            return Some((labels, after));
        }
        let eq = text.find('=')?;
        let name = text[..eq].trim();
        text = text[eq + 1..].trim_start().strip_prefix('"')?;

        let end = find_closing_quote(text)?;
        let raw = &text[..end];
        let value = if raw.contains('\\') {
            Cow::Owned(unescape(raw))
        } else {
            Cow::Borrowed(raw)
        };
        labels.push((name, value));
        text = text[end + 1..].trim_start();
        if let Some(after) = text.strip_prefix(',') {
            text = after;
        }
    }
}

/// Byte index of the quote closing a label value, skipping escaped quotes.
fn find_closing_quote(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return Some(index),
            _ => index += 1,
        }
    }
    None
}

fn unescape(raw: &str) -> String {
    let mut value = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => value.push('\n'),
                Some(other) => value.push(other),
                None => {}
            }
        } else {
            value.push(c);
        }
    }
    value
}

fn parse_value(raw: &str) -> Option<f64> {
    match raw {
        "NaN" => Some(f64::NAN),
        "+Inf" | "Inf" => Some(f64::INFINITY),
        "-Inf" => Some(f64::NEG_INFINITY),
        _ => raw.parse().ok(),
    }
}

/// One `label="value"` or `label!="value"` matcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelMatcher {
    pub name: String,
    pub value: String,
    pub negate: bool,
}

impl LabelMatcher {
    /// Parse a selector such as `name="api", image!=""` (no braces). Regex
    /// matchers are not supported.
    pub fn parse_selector(selector: &str) -> anyhow::Result<Vec<LabelMatcher>> {
        let selector = selector.trim();
        if selector.is_empty() {
            return Ok(Vec::new());
        }
        let mut matchers = Vec::new();
        let mut text = selector;
        loop {
            text = text.trim_start();
            if text.is_empty() {
                break;
            }
            let op_at = text
                .find(['=', '!'])
                .ok_or_else(|| anyhow::anyhow!("expected label=\"value\" in '{selector}'"))?;
            let name = text[..op_at].trim();
            if name.is_empty() {
                anyhow::bail!("missing label name in '{selector}'");
            }
            let after_name = &text[op_at..];
            let (negate, after_op) = if let Some(rest) = after_name.strip_prefix("!=") {
                (true, rest)
            } else if after_name.starts_with("=~") || after_name.starts_with("!~") {
                anyhow::bail!("regex matchers (=~, !~) are not supported; use = or !=");
            } else if let Some(rest) = after_name.strip_prefix('=') {
                (false, rest)
            } else {
                anyhow::bail!("expected = or != after '{name}' in '{selector}'");
            };
            let quoted = after_op.trim_start().strip_prefix('"').ok_or_else(|| {
                anyhow::anyhow!("label values must be double-quoted in '{selector}'")
            })?;
            let close = quoted
                .find('"')
                .ok_or_else(|| anyhow::anyhow!("unterminated label value in '{selector}'"))?;
            matchers.push(LabelMatcher {
                name: name.to_string(),
                value: quoted[..close].to_string(),
                negate,
            });
            text = quoted[close + 1..].trim_start();
            if let Some(rest) = text.strip_prefix(',') {
                text = rest;
            } else if !text.is_empty() {
                anyhow::bail!("expected ',' between matchers in '{selector}'");
            }
        }
        Ok(matchers)
    }

    fn matches(&self, labels: &[(&str, Cow<'_, str>)]) -> bool {
        // A missing label matches the empty string, as in PromQL.
        let actual = labels
            .iter()
            .find(|(name, _)| *name == self.name)
            .map_or("", |(_, value)| value.as_ref());
        (actual == self.value) != self.negate
    }
}

/// The container a cAdvisor sample describes.
fn container_key(sample: &Sample) -> &str {
    sample
        .labels
        .get("id")
        .or_else(|| sample.labels.get("name"))
        .map_or("", String::as_str)
}

/// Value of `family` for each container, keyed by [`container_key`].
fn per_container<'a>(samples: &'a [Sample], family: &str) -> BTreeMap<&'a str, f64> {
    samples
        .iter()
        .filter(|sample| sample.name == family)
        .map(|sample| (container_key(sample), sample.value))
        .collect()
}

/// Combined CPU limit, in cores, of every matched container.
///
/// Each container's quota is divided by its own period before summing, so
/// three one-core replicas make three cores. `None` unless every container
/// with CPU usage has a limit: one unlimited replica makes the total
/// unlimited, and cAdvisor omits the quota series for such a container.
fn cpu_limit_cores(samples: &[Sample]) -> Option<f64> {
    let quotas = per_container(samples, "container_spec_cpu_quota");
    let periods = per_container(samples, "container_spec_cpu_period");
    let containers: std::collections::BTreeSet<&str> = samples
        .iter()
        .filter(|sample| sample.name == "container_cpu_usage_seconds_total")
        .map(container_key)
        .collect();
    if containers.is_empty() {
        return None;
    }
    containers.iter().try_fold(0.0, |total, container| {
        let quota = *quotas.get(container)?;
        let period = *periods.get(container)?;
        (quota > 0.0 && period > 0.0).then(|| total + quota / period)
    })
}

/// Combined memory limit of every matched container, or `None` unless each
/// container with a working set has one.
fn memory_limit_bytes(samples: &[Sample]) -> Option<f64> {
    let limits = per_container(samples, "container_spec_memory_limit_bytes");
    let containers = per_container(samples, "container_memory_working_set_bytes");
    if containers.is_empty() {
        return None;
    }
    containers.keys().try_fold(0.0, |total, container| {
        let limit = *limits.get(container)?;
        (limit > 0.0 && limit < UNLIMITED_MEMORY_BYTES).then_some(total + limit)
    })
}

/// Counter readings from the previous scrape, keyed by a logical name.
type CounterState = BTreeMap<String, f64>;

/// Rate of a counter between two scrapes; `None` on the first scrape or after
/// a reset, where no meaningful rate exists for this interval.
fn rate(previous: &CounterState, current: &CounterState, key: &str, elapsed: f64) -> Option<f64> {
    let before = previous.get(key)?;
    let now = current.get(key)?;
    if elapsed <= 0.0 || now < before {
        return None;
    }
    Some((now - before) / elapsed)
}

/// State for one scraped target.
#[derive(Debug)]
pub struct ScrapeTarget {
    pub config: ScrapeTargetConfig,
    matchers: Vec<LabelMatcher>,
    previous: Option<(f64, CounterState)>,
    warned_empty: bool,
}

impl ScrapeTarget {
    pub fn new(config: ScrapeTargetConfig) -> anyhow::Result<Self> {
        let matchers = LabelMatcher::parse_selector(&config.selector)?;
        Ok(Self {
            config,
            matchers,
            previous: None,
            warned_empty: false,
        })
    }

    /// Label matchers of this target's selector.
    pub fn matchers(&self) -> &[LabelMatcher] {
        &self.matchers
    }

    /// Fold one raw scrape taken at Unix time `now` into `store`.
    #[cfg(test)]
    pub fn observe(&mut self, now: f64, text: &str, store: &mut SeriesStore) -> Option<String> {
        let samples = extract(text, families(self.config.kind), &self.matchers);
        self.observe_samples(now, &samples, store)
    }

    /// Fold the samples [`extract`]ed from one scrape taken at Unix time
    /// `now` into `store`.
    ///
    /// Returns a note the first time the selector matches nothing, which is
    /// almost always a typo in the container name or labels.
    pub fn observe_samples(
        &mut self,
        now: f64,
        samples: &[Sample],
        store: &mut SeriesStore,
    ) -> Option<String> {
        let counters = match self.config.kind {
            TargetKind::Cadvisor => self.observe_cadvisor(now, samples, store),
            TargetKind::Node => self.observe_node(now, samples, store),
        };

        let note = if counters.is_empty() && !self.warned_empty {
            self.warned_empty = true;
            Some(format!(
                "scrape target '{}' returned no {} series matching selector '{}'",
                self.config.name,
                match self.config.kind {
                    TargetKind::Cadvisor => "cAdvisor container",
                    TargetKind::Node => "node_exporter",
                },
                self.config.selector
            ))
        } else {
            None
        };

        self.previous = Some((now, counters));
        note
    }

    fn record(&self, store: &mut SeriesStore, key: SeriesKey, unit: &str, now: f64, value: f64) {
        store.record(key, "scrape", unit, SeriesKind::Gauge, now, value);
    }

    fn observe_cadvisor(
        &self,
        now: f64,
        samples: &[Sample],
        store: &mut SeriesStore,
    ) -> CounterState {
        let group = self.config.name.as_str();
        let sum = |name: &str| -> Option<f64> {
            let mut values = samples.iter().filter(|s| s.name == name).peekable();
            values.peek()?;
            Some(values.map(|s| s.value).filter(|v| v.is_finite()).sum())
        };
        // With per-CPU metrics enabled cAdvisor exports one CPU series per core
        // as well as `cpu="total"`; summing all of them would double count.
        let cpu_usage = {
            let total: Vec<&Sample> = samples
                .iter()
                .filter(|s| {
                    s.name == "container_cpu_usage_seconds_total"
                        && s.labels.get("cpu").map(String::as_str) == Some("total")
                })
                .collect();
            if total.is_empty() {
                sum("container_cpu_usage_seconds_total")
            } else {
                Some(total.iter().map(|s| s.value).sum())
            }
        };

        let mut counters = CounterState::new();
        let counter_sources = [
            ("cpu", cpu_usage),
            (
                "throttled",
                sum("container_cpu_cfs_throttled_periods_total"),
            ),
            ("periods", sum("container_cpu_cfs_periods_total")),
            ("rx", sum("container_network_receive_bytes_total")),
            ("tx", sum("container_network_transmit_bytes_total")),
            (
                "net_errors",
                match (
                    sum("container_network_receive_errors_total"),
                    sum("container_network_transmit_errors_total"),
                ) {
                    (None, None) => None,
                    (rx, tx) => Some(rx.unwrap_or(0.0) + tx.unwrap_or(0.0)),
                },
            ),
            ("fs_read", sum("container_fs_reads_bytes_total")),
            ("fs_write", sum("container_fs_writes_bytes_total")),
        ];
        for (key, value) in counter_sources {
            if let Some(value) = value {
                counters.insert(key.to_string(), value);
            }
        }

        if let Some((before, previous)) = &self.previous {
            let elapsed = now - before;
            let rate = |key: &str| rate(previous, &counters, key, elapsed);
            let cpu_cores = rate("cpu");
            if let Some(cores) = cpu_cores {
                self.record(
                    store,
                    SeriesKey::new(group, "cpu_cores"),
                    "cores",
                    now,
                    cores,
                );
                if let Some(limit) = cpu_limit_cores(samples) {
                    self.record(
                        store,
                        SeriesKey::new(group, "cpu_percent_of_limit"),
                        "percent",
                        now,
                        100.0 * cores / limit,
                    );
                }
            }
            if let (Some(throttled), Some(periods)) = (rate("throttled"), rate("periods")) {
                if periods > 0.0 {
                    self.record(
                        store,
                        SeriesKey::new(group, "cpu_throttled_percent"),
                        "percent",
                        now,
                        100.0 * throttled / periods,
                    );
                }
            }
            for (key, metric) in [
                ("rx", "network_rx_mibps"),
                ("tx", "network_tx_mibps"),
                ("fs_read", "fs_read_mibps"),
                ("fs_write", "fs_write_mibps"),
            ] {
                if let Some(bytes_per_sec) = rate(key) {
                    self.record(
                        store,
                        SeriesKey::new(group, metric),
                        "MiB/s",
                        now,
                        bytes_per_sec / MIB,
                    );
                }
            }
            if let Some(errors) = rate("net_errors") {
                self.record(
                    store,
                    SeriesKey::new(group, "network_errors_per_sec"),
                    "1/s",
                    now,
                    errors,
                );
            }
        }

        let working_set = sum("container_memory_working_set_bytes");
        if let Some(bytes) = working_set {
            self.record(
                store,
                SeriesKey::new(group, "memory_working_set_mib"),
                "MiB",
                now,
                bytes / MIB,
            );
            if let Some(limit) = memory_limit_bytes(samples) {
                self.record(
                    store,
                    SeriesKey::new(group, "memory_percent_of_limit"),
                    "percent",
                    now,
                    100.0 * bytes / limit,
                );
            }
        }
        for (name, metric) in [
            ("container_memory_rss", "memory_rss_mib"),
            ("container_memory_cache", "memory_cache_mib"),
        ] {
            if let Some(bytes) = sum(name) {
                self.record(
                    store,
                    SeriesKey::new(group, metric),
                    "MiB",
                    now,
                    bytes / MIB,
                );
            }
        }
        for (name, metric) in [
            ("container_processes", "processes"),
            ("container_file_descriptors", "open_fds"),
        ] {
            if let Some(value) = sum(name) {
                self.record(store, SeriesKey::new(group, metric), "count", now, value);
            }
        }
        if let Some(ooms) = sum("container_oom_events_total") {
            store.record(
                SeriesKey::new(group, "oom_events_total"),
                "scrape",
                "count",
                SeriesKind::Counter,
                now,
                ooms,
            );
        }
        if working_set.is_some() {
            counters.entry("_present".to_string()).or_insert(1.0);
        }
        counters
    }

    fn observe_node(&self, now: f64, samples: &[Sample], store: &mut SeriesStore) -> CounterState {
        let group = self.config.name.as_str();
        let gauge = |name: &str| -> Option<f64> {
            samples.iter().find(|s| s.name == name).map(|s| s.value)
        };

        let mut counters = CounterState::new();
        // Per-core counters: total time across modes, idle, iowait and steal.
        for sample in samples
            .iter()
            .filter(|s| s.name == "node_cpu_seconds_total")
        {
            let (Some(cpu), Some(mode)) = (sample.labels.get("cpu"), sample.labels.get("mode"))
            else {
                continue;
            };
            *counters.entry(format!("cpu/{cpu}/total")).or_default() += sample.value;
            if matches!(mode.as_str(), "idle" | "iowait" | "steal") {
                *counters.entry(format!("cpu/{cpu}/{mode}")).or_default() += sample.value;
            }
        }
        for sample in samples.iter() {
            let key = match sample.name.as_str() {
                "node_netstat_Tcp_RetransSegs" => "retrans".to_string(),
                "node_network_receive_bytes_total"
                    if sample.labels.get("device").map(String::as_str) != Some("lo") =>
                {
                    "rx".to_string()
                }
                "node_network_transmit_bytes_total"
                    if sample.labels.get("device").map(String::as_str) != Some("lo") =>
                {
                    "tx".to_string()
                }
                "node_disk_io_time_seconds_total" => format!(
                    "disk/{}",
                    sample.labels.get("device").cloned().unwrap_or_default()
                ),
                _ => continue,
            };
            *counters.entry(key).or_default() += sample.value;
        }

        if let Some((before, previous)) = &self.previous {
            let elapsed = now - before;
            let cpus: Vec<String> = counters
                .keys()
                .filter_map(|key| {
                    key.strip_prefix("cpu/")?
                        .strip_suffix("/total")
                        .map(str::to_string)
                })
                .collect();

            let mut busy_sum = 0.0;
            let mut iowait_sum = 0.0;
            let mut steal_sum = 0.0;
            let mut measured = 0usize;
            for cpu in &cpus {
                let delta = |mode: &str| -> Option<f64> {
                    let key = format!("cpu/{cpu}/{mode}");
                    let delta = counters.get(&key)? - previous.get(&key)?;
                    (delta >= 0.0).then_some(delta)
                };
                let Some(total) = delta("total").filter(|total| *total > 0.0) else {
                    continue;
                };
                let idle = delta("idle").unwrap_or(0.0);
                let busy = (100.0 * (1.0 - idle / total)).clamp(0.0, 100.0);
                busy_sum += busy;
                iowait_sum += (100.0 * delta("iowait").unwrap_or(0.0) / total).clamp(0.0, 100.0);
                steal_sum += (100.0 * delta("steal").unwrap_or(0.0) / total).clamp(0.0, 100.0);
                measured += 1;
                if self.config.per_core {
                    self.record(
                        store,
                        SeriesKey::new(group, "cpu_percent").with_label("cpu", cpu),
                        "percent",
                        now,
                        busy,
                    );
                }
            }
            if measured > 0 {
                let n = measured as f64;
                self.record(
                    store,
                    SeriesKey::new(group, "cpu_percent"),
                    "percent",
                    now,
                    busy_sum / n,
                );
                self.record(
                    store,
                    SeriesKey::new(group, "iowait_percent"),
                    "percent",
                    now,
                    iowait_sum / n,
                );
                self.record(
                    store,
                    SeriesKey::new(group, "steal_percent"),
                    "percent",
                    now,
                    steal_sum / n,
                );
            }

            let rate = |key: &str| rate(previous, &counters, key, elapsed);
            if let Some(retrans) = rate("retrans") {
                self.record(
                    store,
                    SeriesKey::new(group, "tcp_retransmits_per_sec"),
                    "1/s",
                    now,
                    retrans,
                );
            }
            for (key, metric) in [("rx", "network_rx_mibps"), ("tx", "network_tx_mibps")] {
                if let Some(bytes_per_sec) = rate(key) {
                    self.record(
                        store,
                        SeriesKey::new(group, metric),
                        "MiB/s",
                        now,
                        bytes_per_sec / MIB,
                    );
                }
            }
            let busiest_disk = counters
                .keys()
                .filter(|key| key.starts_with("disk/"))
                .filter_map(|key| rate(key))
                .fold(None, |acc: Option<f64>, busy| {
                    Some(acc.map_or(busy, |a| a.max(busy)))
                });
            if let Some(busy) = busiest_disk {
                self.record(
                    store,
                    SeriesKey::new(group, "disk_busy_percent"),
                    "percent",
                    now,
                    (100.0 * busy).clamp(0.0, 100.0),
                );
            }
        }

        if let (Some(total), Some(available)) = (
            gauge("node_memory_MemTotal_bytes"),
            gauge("node_memory_MemAvailable_bytes"),
        ) {
            if total > 0.0 {
                self.record(
                    store,
                    SeriesKey::new(group, "memory_used_percent"),
                    "percent",
                    now,
                    100.0 * (1.0 - available / total),
                );
            }
            self.record(
                store,
                SeriesKey::new(group, "memory_available_mib"),
                "MiB",
                now,
                available / MIB,
            );
            counters.entry("_present".to_string()).or_insert(1.0);
        }
        if let Some(load1) = gauge("node_load1") {
            self.record(store, SeriesKey::new(group, "load1"), "load", now, load1);
            counters.entry("_present".to_string()).or_insert(1.0);
        }
        for (name, metric) in [
            ("node_sockstat_TCP_inuse", "tcp_sockets_inuse"),
            ("node_sockstat_TCP_tw", "tcp_time_wait"),
        ] {
            if let Some(value) = gauge(name) {
                self.record(store, SeriesKey::new(group, metric), "count", now, value);
            }
        }
        counters
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitoring::series::summarize;

    fn target(kind: TargetKind, selector: &str) -> ScrapeTarget {
        ScrapeTarget::new(ScrapeTargetConfig {
            name: "t".to_string(),
            kind,
            url: "http://localhost/metrics".to_string(),
            selector: selector.to_string(),
            per_core: true,
            bearer_token_env: None,
        })
        .unwrap()
    }

    fn last(store: SeriesStore, id: &str) -> Option<f64> {
        store
            .into_buffers()
            .into_iter()
            .find(|buffer| buffer.key.id() == id)
            .map(|buffer| summarize(&buffer.points, buffer.kind, 0.0, 1e12).last)
    }

    #[test]
    fn test_parse_exposition() {
        let text = r#"
# HELP container_cpu_usage_seconds_total Cumulative cpu time consumed.
# TYPE container_cpu_usage_seconds_total counter
container_cpu_usage_seconds_total{cpu="total",name="api",id="/docker/1"} 12.5 1700000000000
weird_label{path="a\"b\\c",x="new\nline"} 1
plain_metric 42
inf_metric +Inf
nan_metric NaN
broken{name="unterminated} 1
"#;
        let samples = parse_exposition(text);
        assert_eq!(samples.len(), 5);
        assert_eq!(samples[0].name, "container_cpu_usage_seconds_total");
        assert_eq!(samples[0].labels["name"], "api");
        assert_eq!(samples[0].value, 12.5);
        assert_eq!(samples[1].labels["path"], "a\"b\\c");
        assert_eq!(samples[1].labels["x"], "new\nline");
        assert_eq!(samples[2].value, 42.0);
        assert!(samples[3].value.is_infinite());
        assert!(samples[4].value.is_nan());
    }

    #[test]
    fn test_extract_skips_unused_families_and_unescapes_matches() {
        let text = concat!(
            "container_memory_usage_bytes{name=\"api\"} 1\n",
            "container_memory_working_set_bytes{name=\"api\",path=\"a\\\"b\"} 2\n",
            "container_memory_working_set_bytes{name=\"db\"} 3\n",
        );
        let matchers = LabelMatcher::parse_selector(r#"name="api""#).unwrap();
        let samples = extract(text, CADVISOR_FAMILIES, &matchers);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].name, "container_memory_working_set_bytes");
        assert_eq!(samples[0].labels["path"], "a\"b");
    }

    #[test]
    fn test_selector_parsing_and_matching() {
        let matchers = LabelMatcher::parse_selector(r#"name="api", image!="""#).unwrap();
        assert_eq!(matchers.len(), 2);
        assert!(matchers[1].negate);

        let text = "m{name=\"api\"} 1\nm{name=\"api\",image=\"api:1\"} 2\nm{name=\"db\",image=\"db:1\"} 3\n";
        let matched = extract(text, &[], &matchers);
        // An absent image label is the empty string, which image!="" rejects.
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].value, 2.0);

        assert!(LabelMatcher::parse_selector("").unwrap().is_empty());
        assert!(LabelMatcher::parse_selector(r#"name=~"a.*""#).is_err());
        assert!(LabelMatcher::parse_selector(r#"name=api"#).is_err());
        assert!(LabelMatcher::parse_selector(r#"name="a" image="b""#).is_err());
    }

    fn cadvisor_scrape(cpu: f64, throttled: f64, periods: f64, rx: f64, memory: f64) -> String {
        format!(
            r#"container_cpu_usage_seconds_total{{cpu="total",name="api"}} {cpu}
container_cpu_usage_seconds_total{{cpu="total",name="other"}} 999
container_cpu_cfs_throttled_periods_total{{name="api"}} {throttled}
container_cpu_cfs_periods_total{{name="api"}} {periods}
container_spec_cpu_quota{{name="api"}} 200000
container_spec_cpu_period{{name="api"}} 100000
container_network_receive_bytes_total{{name="api",interface="eth0"}} {rx}
container_memory_working_set_bytes{{name="api"}} {memory}
container_spec_memory_limit_bytes{{name="api"}} 1073741824
container_oom_events_total{{name="api"}} 0
"#
        )
    }

    #[test]
    fn test_cadvisor_rates_and_levels() {
        let mut target = target(TargetKind::Cadvisor, r#"name="api""#);
        let mut store = SeriesStore::default();
        assert!(target
            .observe(
                100.0,
                &cadvisor_scrape(10.0, 0.0, 0.0, 0.0, 512.0 * MIB),
                &mut store
            )
            .is_none());
        // 2s later: 3 CPU-seconds used (1.5 cores of a 2-core limit), 5 of 20
        // periods throttled, 4 MiB received.
        target.observe(
            102.0,
            &cadvisor_scrape(13.0, 5.0, 20.0, 4.0 * MIB, 768.0 * MIB),
            &mut store,
        );

        let ids = [
            ("t.cpu_cores", 1.5),
            ("t.cpu_percent_of_limit", 75.0),
            ("t.cpu_throttled_percent", 25.0),
            ("t.network_rx_mibps", 2.0),
            ("t.memory_working_set_mib", 768.0),
            ("t.memory_percent_of_limit", 75.0),
        ];
        let buffers = store.into_buffers();
        for (id, expected) in ids {
            let buffer = buffers
                .iter()
                .find(|buffer| buffer.key.id() == id)
                .unwrap_or_else(|| panic!("missing {id}"));
            let value = summarize(&buffer.points, buffer.kind, 0.0, 1e12).last;
            assert!(
                (value - expected).abs() < 1e-9,
                "{id}: {value} != {expected}"
            );
        }
        assert!(buffers.iter().any(|b| b.key.id() == "t.oom_events_total"));
    }

    fn sample(name: &str, id: &str, value: f64) -> Sample {
        Sample {
            name: name.to_string(),
            labels: BTreeMap::from([("id".to_string(), id.to_string())]),
            value,
        }
    }

    #[test]
    fn test_limits_are_combined_per_container() {
        // Three replicas with a one-core, 256 MiB limit each.
        let mut samples = Vec::new();
        for id in ["a", "b", "c"] {
            samples.push(sample("container_cpu_usage_seconds_total", id, 1.0));
            samples.push(sample("container_spec_cpu_quota", id, 100_000.0));
            samples.push(sample("container_spec_cpu_period", id, 100_000.0));
            samples.push(sample("container_memory_working_set_bytes", id, MIB));
            samples.push(sample("container_spec_memory_limit_bytes", id, 256.0 * MIB));
        }
        assert_eq!(cpu_limit_cores(&samples), Some(3.0));
        assert_eq!(memory_limit_bytes(&samples), Some(768.0 * MIB));

        // A fourth replica without limits makes the total unlimited.
        samples.push(sample("container_cpu_usage_seconds_total", "d", 1.0));
        samples.push(sample("container_memory_working_set_bytes", "d", MIB));
        samples.push(sample("container_spec_memory_limit_bytes", "d", 0.0));
        assert_eq!(cpu_limit_cores(&samples), None);
        assert_eq!(memory_limit_bytes(&samples), None);
    }

    #[test]
    fn test_cadvisor_counter_reset_skips_interval() {
        let mut target = target(TargetKind::Cadvisor, r#"name="api""#);
        let mut store = SeriesStore::default();
        target.observe(0.0, &cadvisor_scrape(50.0, 0.0, 0.0, 0.0, MIB), &mut store);
        // The container restarted: CPU usage went backwards.
        target.observe(1.0, &cadvisor_scrape(1.0, 0.0, 0.0, 0.0, MIB), &mut store);
        assert_eq!(last(store, "t.cpu_cores"), None);
    }

    #[test]
    fn test_selector_matching_nothing_is_reported_once() {
        let mut target = target(TargetKind::Cadvisor, r#"name="missing""#);
        let mut store = SeriesStore::default();
        let scrape = cadvisor_scrape(1.0, 0.0, 0.0, 0.0, MIB);
        assert!(target.observe(0.0, &scrape, &mut store).is_some());
        assert!(target.observe(1.0, &scrape, &mut store).is_none());
        assert!(store.is_empty());
    }

    fn node_scrape(
        cpu0_idle: f64,
        cpu0_user: f64,
        cpu1_idle: f64,
        cpu1_user: f64,
        retrans: f64,
    ) -> String {
        format!(
            r#"node_cpu_seconds_total{{cpu="0",mode="idle"}} {cpu0_idle}
node_cpu_seconds_total{{cpu="0",mode="user"}} {cpu0_user}
node_cpu_seconds_total{{cpu="1",mode="idle"}} {cpu1_idle}
node_cpu_seconds_total{{cpu="1",mode="user"}} {cpu1_user}
node_memory_MemTotal_bytes 1000
node_memory_MemAvailable_bytes 400
node_load1 2.5
node_netstat_Tcp_RetransSegs {retrans}
node_sockstat_TCP_tw 12
"#
        )
    }

    #[test]
    fn test_node_cpu_per_core_and_total() {
        let mut target = target(TargetKind::Node, "");
        let mut store = SeriesStore::default();
        target.observe(0.0, &node_scrape(100.0, 0.0, 100.0, 0.0, 0.0), &mut store);
        // cpu0 fully busy for 2s, cpu1 half busy.
        target.observe(2.0, &node_scrape(100.0, 2.0, 101.0, 1.0, 10.0), &mut store);

        let buffers = store.into_buffers();
        let value = |id: &str| {
            let buffer = buffers
                .iter()
                .find(|b| b.key.id() == id)
                .unwrap_or_else(|| panic!("missing {id}"));
            summarize(&buffer.points, buffer.kind, 0.0, 1e12).last
        };
        assert_eq!(value("t.cpu_percent{cpu=\"0\"}"), 100.0);
        assert_eq!(value("t.cpu_percent{cpu=\"1\"}"), 50.0);
        assert_eq!(value("t.cpu_percent"), 75.0);
        assert_eq!(value("t.memory_used_percent"), 60.0);
        assert_eq!(value("t.load1"), 2.5);
        assert_eq!(value("t.tcp_retransmits_per_sec"), 5.0);
        assert_eq!(value("t.tcp_time_wait"), 12.0);
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// `cargo test --release scrape_cost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn scrape_cost() {
        let mut text = String::new();
        let families = [
            "container_cpu_usage_seconds_total",
            "container_cpu_system_seconds_total",
            "container_cpu_user_seconds_total",
            "container_memory_working_set_bytes",
            "container_memory_usage_bytes",
            "container_memory_rss",
            "container_memory_cache",
            "container_memory_swap",
            "container_memory_mapped_file",
            "container_memory_failcnt",
            "container_network_receive_bytes_total",
            "container_network_transmit_bytes_total",
            "container_network_receive_packets_total",
            "container_network_transmit_packets_total",
            "container_fs_reads_bytes_total",
            "container_fs_writes_bytes_total",
            "container_fs_usage_bytes",
            "container_fs_limit_bytes",
            "container_spec_cpu_quota",
            "container_spec_memory_limit_bytes",
            "container_last_seen",
            "container_start_time_seconds",
        ];
        for family in families {
            text.push_str(&format!(
                "# HELP {family} help text\n# TYPE {family} gauge\n"
            ));
            for container in 0..200 {
                for device in 0..3 {
                    text.push_str(&format!(
                        "{family}{{container_label_com_docker_compose_project=\"demo\",\
                         container_label_com_docker_compose_service=\"svc{container}\",\
                         device=\"/dev/sd{device}\",id=\"/docker/{container:064x}\",\
                         image=\"registry.example.com/team/app:{container}\",\
                         name=\"app-{container}\"}} {} 1700000000000\n",
                        container * 1000 + device
                    ));
                }
            }
        }
        let matchers = LabelMatcher::parse_selector(r#"name="app-42""#).unwrap();
        let rounds = 20;

        let started = std::time::Instant::now();
        let mut kept = 0;
        for _ in 0..rounds {
            kept = extract(&text, CADVISOR_FAMILIES, &matchers).len();
        }
        let filtered = started.elapsed() / rounds;

        let started = std::time::Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(extract(&text, &[], &[]).len());
        }
        let everything = started.elapsed() / rounds;

        println!(
            "payload {:.1} MiB, {} lines; filtered extract {:?} ({} samples kept); \
             parsing every line {:?}",
            text.len() as f64 / 1024.0 / 1024.0,
            text.lines().count(),
            filtered,
            kept,
            everything
        );
    }
}
