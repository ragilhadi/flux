//! Prometheus source: after the run, query the test window with
//! `/api/v1/query_range` for preset targets (cAdvisor containers,
//! node_exporter hosts) and custom PromQL.
//!
//! Querying after the run, rather than polling during it, adds no load while
//! the test is measuring and gets Prometheus's own view of the window.

use super::config::{PrometheusSourceConfig, PrometheusTargetConfig, TargetKind};
use super::series::{Point, SeriesBuffer, SeriesKey, SeriesKind};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::time::Duration;

/// Bytes in a mebibyte, as a PromQL literal.
const MIB: &str = "1048576";

/// Prometheus refuses range queries returning more than 11,000 points per
/// series; staying well below that also keeps reports small.
const MAX_QUERY_POINTS: f64 = 700.0;

/// One query to run and how to record its result.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedQuery {
    pub group: String,
    pub metric: String,
    pub unit: String,
    pub kind: SeriesKind,
    pub query: String,
    /// Labels of the result kept in the series identity. Preset queries
    /// aggregate everything away except, for per-core CPU, the `cpu` label;
    /// custom queries keep every label.
    pub keep_labels: KeepLabels,
}

#[derive(Debug, Clone, PartialEq)]
pub enum KeepLabels {
    None,
    Only(&'static str),
    All,
}

/// Combine fixed matchers with the user's selector into `{...}`.
fn selector(fixed: &[&str], user: &str) -> String {
    let mut parts: Vec<&str> = fixed.to_vec();
    let user = user.trim().trim_matches(',');
    if !user.is_empty() {
        parts.push(user);
    }
    format!("{{{}}}", parts.join(","))
}

/// Queries for one preset target.
pub fn target_queries(target: &PrometheusTargetConfig, rate_window: &str) -> Vec<PlannedQuery> {
    let s = |fixed: &[&str]| selector(fixed, &target.selector);
    let all = s(&[]);
    let w = rate_window;
    let gauge = |metric: &str, unit: &str, query: String| PlannedQuery {
        group: target.name.clone(),
        metric: metric.to_string(),
        unit: unit.to_string(),
        kind: SeriesKind::Gauge,
        query,
        keep_labels: KeepLabels::None,
    };

    match target.kind {
        TargetKind::Cadvisor => {
            // Restrict CPU usage to the `cpu="total"` series when per-CPU
            // metrics are enabled, without dropping exporters that have no
            // `cpu` label at all.
            let cpu = format!(
                "sum(rate(container_cpu_usage_seconds_total{all_cpu}[{w}]))",
                all_cpu = s(&["cpu=~\"total|\""])
            );
            vec![
                gauge("cpu_cores", "cores", cpu.clone()),
                gauge(
                    "cpu_percent_of_limit",
                    "percent",
                    format!(
                        "100 * {cpu} / (sum(container_spec_cpu_quota{all}) / sum(container_spec_cpu_period{all}) > 0)"
                    ),
                ),
                gauge(
                    "cpu_throttled_percent",
                    "percent",
                    format!(
                        "100 * sum(rate(container_cpu_cfs_throttled_periods_total{all}[{w}])) / (sum(rate(container_cpu_cfs_periods_total{all}[{w}])) > 0)"
                    ),
                ),
                gauge(
                    "memory_working_set_mib",
                    "MiB",
                    format!("sum(container_memory_working_set_bytes{all}) / {MIB}"),
                ),
                gauge(
                    "memory_rss_mib",
                    "MiB",
                    format!("sum(container_memory_rss{all}) / {MIB}"),
                ),
                gauge(
                    "memory_cache_mib",
                    "MiB",
                    format!("sum(container_memory_cache{all}) / {MIB}"),
                ),
                gauge(
                    "memory_percent_of_limit",
                    "percent",
                    format!(
                        "100 * sum(container_memory_working_set_bytes{all}) / (sum(container_spec_memory_limit_bytes{all}) > 0 < 1152921504606846976)"
                    ),
                ),
                gauge(
                    "network_rx_mibps",
                    "MiB/s",
                    format!("sum(rate(container_network_receive_bytes_total{all}[{w}])) / {MIB}"),
                ),
                gauge(
                    "network_tx_mibps",
                    "MiB/s",
                    format!("sum(rate(container_network_transmit_bytes_total{all}[{w}])) / {MIB}"),
                ),
                gauge(
                    "network_errors_per_sec",
                    "1/s",
                    format!(
                        "sum(rate(container_network_receive_errors_total{all}[{w}])) + sum(rate(container_network_transmit_errors_total{all}[{w}]))"
                    ),
                ),
                gauge(
                    "fs_read_mibps",
                    "MiB/s",
                    format!("sum(rate(container_fs_reads_bytes_total{all}[{w}])) / {MIB}"),
                ),
                gauge(
                    "fs_write_mibps",
                    "MiB/s",
                    format!("sum(rate(container_fs_writes_bytes_total{all}[{w}])) / {MIB}"),
                ),
                gauge("processes", "count", format!("sum(container_processes{all})")),
                gauge(
                    "open_fds",
                    "count",
                    format!("sum(container_file_descriptors{all})"),
                ),
                PlannedQuery {
                    kind: SeriesKind::Counter,
                    ..gauge(
                        "oom_events_total",
                        "count",
                        format!("sum(container_oom_events_total{all})"),
                    )
                },
            ]
        }
        TargetKind::Node => {
            let idle = s(&["mode=\"idle\""]);
            let mut queries = vec![
                gauge(
                    "cpu_percent",
                    "percent",
                    format!("100 * (1 - avg(rate(node_cpu_seconds_total{idle}[{w}])))"),
                ),
                gauge(
                    "iowait_percent",
                    "percent",
                    format!(
                        "100 * avg(rate(node_cpu_seconds_total{}[{w}]))",
                        s(&["mode=\"iowait\""])
                    ),
                ),
                gauge(
                    "steal_percent",
                    "percent",
                    format!(
                        "100 * avg(rate(node_cpu_seconds_total{}[{w}]))",
                        s(&["mode=\"steal\""])
                    ),
                ),
                gauge(
                    "memory_used_percent",
                    "percent",
                    format!(
                        "100 * (1 - sum(node_memory_MemAvailable_bytes{all}) / sum(node_memory_MemTotal_bytes{all}))"
                    ),
                ),
                gauge(
                    "memory_available_mib",
                    "MiB",
                    format!("sum(node_memory_MemAvailable_bytes{all}) / {MIB}"),
                ),
                gauge("load1", "load", format!("max(node_load1{all})")),
                gauge(
                    "tcp_retransmits_per_sec",
                    "1/s",
                    format!("sum(rate(node_netstat_Tcp_RetransSegs{all}[{w}]))"),
                ),
                gauge(
                    "tcp_sockets_inuse",
                    "count",
                    format!("sum(node_sockstat_TCP_inuse{all})"),
                ),
                gauge(
                    "tcp_time_wait",
                    "count",
                    format!("sum(node_sockstat_TCP_tw{all})"),
                ),
                gauge(
                    "network_rx_mibps",
                    "MiB/s",
                    format!(
                        "sum(rate(node_network_receive_bytes_total{}[{w}])) / {MIB}",
                        s(&["device!=\"lo\""])
                    ),
                ),
                gauge(
                    "network_tx_mibps",
                    "MiB/s",
                    format!(
                        "sum(rate(node_network_transmit_bytes_total{}[{w}])) / {MIB}",
                        s(&["device!=\"lo\""])
                    ),
                ),
                gauge(
                    "disk_busy_percent",
                    "percent",
                    format!("100 * max(rate(node_disk_io_time_seconds_total{all}[{w}]))"),
                ),
            ];
            if target.per_core {
                queries.push(PlannedQuery {
                    keep_labels: KeepLabels::Only("cpu"),
                    ..gauge(
                        "cpu_percent",
                        "percent",
                        format!(
                            "100 * (1 - avg by (cpu) (rate(node_cpu_seconds_total{idle}[{w}])))"
                        ),
                    )
                });
            }
            queries
        }
    }
}

/// Every query a Prometheus source will run.
pub fn plan(config: &PrometheusSourceConfig) -> Vec<PlannedQuery> {
    let mut queries: Vec<PlannedQuery> = config
        .targets
        .iter()
        .flat_map(|target| target_queries(target, config.rate_window.trim()))
        .collect();
    queries.extend(config.queries.iter().map(|custom| PlannedQuery {
        group: custom.group.clone().unwrap_or_else(|| "custom".to_string()),
        metric: custom.name.clone(),
        unit: custom.unit.clone().unwrap_or_default(),
        kind: SeriesKind::Gauge,
        query: custom.query.clone(),
        keep_labels: KeepLabels::All,
    }));
    queries
}

/// Step for a range query: the configured step, widened if needed so no
/// series exceeds `MAX_QUERY_POINTS` points.
pub fn query_step(configured: Duration, start: f64, end: f64) -> f64 {
    let configured = configured.as_secs_f64().max(1.0);
    let span = (end - start).max(0.0);
    configured.max((span / MAX_QUERY_POINTS).ceil())
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    status: String,
    #[serde(default)]
    data: Option<ApiData>,
    #[serde(default, rename = "errorType")]
    error_type: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiData {
    #[serde(rename = "resultType")]
    result_type: String,
    result: Vec<MatrixSeries>,
}

#[derive(Debug, Deserialize)]
struct MatrixSeries {
    #[serde(default)]
    metric: BTreeMap<String, String>,
    #[serde(default)]
    values: Vec<(f64, String)>,
}

/// Turn a `query_range` response body into series buffers.
pub fn parse_range_response(body: &str, query: &PlannedQuery) -> anyhow::Result<Vec<SeriesBuffer>> {
    let response: ApiResponse = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("unexpected response from Prometheus: {e}"))?;
    if response.status != "success" {
        anyhow::bail!(
            "Prometheus returned {}: {}",
            response.error_type.as_deref().unwrap_or("an error"),
            response.error.as_deref().unwrap_or("no message")
        );
    }
    let data = response
        .data
        .ok_or_else(|| anyhow::anyhow!("Prometheus response has no data"))?;
    if data.result_type != "matrix" {
        anyhow::bail!(
            "expected a matrix from a range query, got '{}'",
            data.result_type
        );
    }

    let mut buffers = Vec::new();
    for series in data.result {
        let mut key = SeriesKey::new(&query.group, &query.metric);
        match &query.keep_labels {
            KeepLabels::None => {}
            KeepLabels::Only(label) => {
                if let Some(value) = series.metric.get(*label) {
                    key = key.with_label(label, value);
                }
            }
            KeepLabels::All => {
                for (name, value) in &series.metric {
                    if name != "__name__" {
                        key = key.with_label(name, value);
                    }
                }
            }
        }
        let points: Vec<Point> = series
            .values
            .iter()
            .filter_map(|(time, raw)| {
                let value: f64 = raw.parse().ok()?;
                value.is_finite().then(|| Point::new(*time, value))
            })
            .collect();
        if points.is_empty() {
            continue;
        }
        buffers.push(SeriesBuffer {
            key,
            source: "prometheus".to_string(),
            unit: query.unit.clone(),
            kind: query.kind,
            points,
        });
    }
    Ok(buffers)
}

/// Build the `query_range` URL for one query.
pub fn range_url(
    base: &str,
    query: &str,
    start: f64,
    end: f64,
    step: f64,
) -> anyhow::Result<reqwest::Url> {
    let base = base.trim().trim_end_matches('/');
    let mut url = reqwest::Url::parse(&format!("{base}/api/v1/query_range"))?;
    url.query_pairs_mut()
        .append_pair("query", query)
        .append_pair("start", &format!("{start:.3}"))
        .append_pair("end", &format!("{end:.3}"))
        .append_pair("step", &format!("{step}"));
    Ok(url)
}

/// Run one range query.
pub async fn run_query(
    client: &reqwest::Client,
    base: &str,
    token: Option<&str>,
    query: &PlannedQuery,
    start: f64,
    end: f64,
    step: f64,
) -> anyhow::Result<Vec<SeriesBuffer>> {
    let url = range_url(base, &query.query, start, end, step)?;
    let mut request = client.get(url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() && !body.trim_start().starts_with('{') {
        anyhow::bail!("Prometheus returned HTTP {status}");
    }
    parse_range_response(&body, query)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(kind: TargetKind, selector: &str, per_core: bool) -> PrometheusTargetConfig {
        PrometheusTargetConfig {
            name: "api".to_string(),
            kind,
            selector: selector.to_string(),
            per_core,
        }
    }

    #[test]
    fn test_selector_composition() {
        assert_eq!(selector(&[], ""), "{}");
        assert_eq!(selector(&[], r#"name="api""#), r#"{name="api"}"#);
        assert_eq!(
            selector(&[r#"mode="idle""#], r#" name="api", "#),
            r#"{mode="idle",name="api"}"#
        );
    }

    #[test]
    fn test_cadvisor_queries() {
        let queries = target_queries(&target(TargetKind::Cadvisor, r#"name="api""#, true), "30s");
        let cpu = queries.iter().find(|q| q.metric == "cpu_cores").unwrap();
        assert_eq!(
            cpu.query,
            r#"sum(rate(container_cpu_usage_seconds_total{cpu=~"total|",name="api"}[30s]))"#
        );
        let throttled = queries
            .iter()
            .find(|q| q.metric == "cpu_throttled_percent")
            .unwrap();
        assert!(throttled
            .query
            .contains(r#"container_cpu_cfs_throttled_periods_total{name="api"}[30s]"#));
        let oom = queries
            .iter()
            .find(|q| q.metric == "oom_events_total")
            .unwrap();
        assert_eq!(oom.kind, SeriesKind::Counter);
        assert!(queries.iter().all(|q| q.group == "api"));
    }

    #[test]
    fn test_node_queries_with_and_without_per_core() {
        let with = target_queries(
            &target(TargetKind::Node, r#"instance="n:9100""#, true),
            "15s",
        );
        let per_core = with
            .iter()
            .find(|q| q.keep_labels == KeepLabels::Only("cpu"))
            .unwrap();
        assert_eq!(
            per_core.query,
            r#"100 * (1 - avg by (cpu) (rate(node_cpu_seconds_total{mode="idle",instance="n:9100"}[15s])))"#
        );
        let without = target_queries(&target(TargetKind::Node, "", false), "15s");
        assert!(without.iter().all(|q| q.keep_labels == KeepLabels::None));
        assert_eq!(with.len(), without.len() + 1);
    }

    #[test]
    fn test_query_step_is_widened_for_long_windows() {
        assert_eq!(query_step(Duration::from_secs(2), 0.0, 60.0), 2.0);
        assert_eq!(query_step(Duration::from_millis(200), 0.0, 60.0), 1.0);
        // Two hours at 2s would be 3,600 points; widen to stay under 700.
        assert_eq!(query_step(Duration::from_secs(2), 0.0, 7_200.0), 11.0);
    }

    #[test]
    fn test_range_url_encodes_query() {
        let url = range_url(
            "http://prometheus:9090/",
            r#"sum(rate(x{name="a b"}[30s]))"#,
            100.0,
            160.5,
            5.0,
        )
        .unwrap();
        assert_eq!(url.path(), "/api/v1/query_range");
        let pairs: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs["query"], r#"sum(rate(x{name="a b"}[30s]))"#);
        assert_eq!(pairs["start"], "100.000");
        assert_eq!(pairs["end"], "160.500");
        assert_eq!(pairs["step"], "5");
    }

    fn planned(keep_labels: KeepLabels) -> PlannedQuery {
        PlannedQuery {
            group: "node".to_string(),
            metric: "cpu_percent".to_string(),
            unit: "percent".to_string(),
            kind: SeriesKind::Gauge,
            query: "q".to_string(),
            keep_labels,
        }
    }

    #[test]
    fn test_parse_range_response() {
        let body = r#"{"status":"success","data":{"resultType":"matrix","result":[
            {"metric":{"cpu":"0","instance":"n"},"values":[[100,"10"],[105,"NaN"],[110,"30.5"]]},
            {"metric":{"cpu":"1","instance":"n"},"values":[[100,"+Inf"]]}
        ]}}"#;
        let buffers = parse_range_response(body, &planned(KeepLabels::Only("cpu"))).unwrap();
        // The second series has no finite value and is dropped.
        assert_eq!(buffers.len(), 1);
        assert_eq!(buffers[0].key.id(), r#"node.cpu_percent{cpu="0"}"#);
        assert_eq!(buffers[0].points.len(), 2);
        assert_eq!(buffers[0].points[1].value, 30.5);

        let all = parse_range_response(body, &planned(KeepLabels::All)).unwrap();
        assert_eq!(all[0].key.labels.len(), 2);
        let none = parse_range_response(body, &planned(KeepLabels::None)).unwrap();
        assert!(none[0].key.labels.is_empty());
    }

    #[test]
    fn test_parse_range_response_errors() {
        let error = r#"{"status":"error","errorType":"bad_data","error":"parse error at char 5"}"#;
        let message = parse_range_response(error, &planned(KeepLabels::None))
            .unwrap_err()
            .to_string();
        assert!(message.contains("bad_data") && message.contains("parse error"));
        assert!(parse_range_response("<html>", &planned(KeepLabels::None)).is_err());
        let vector = r#"{"status":"success","data":{"resultType":"vector","result":[]}}"#;
        assert!(parse_range_response(vector, &planned(KeepLabels::None)).is_err());
    }
}
