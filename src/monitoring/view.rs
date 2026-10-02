//! Presentation of resource data for the HTML report and the resource CSV.

use super::{ResourceReport, ResourceSeries};
use crate::metrics::MetricsSummary;
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;

/// Most columns in a per-core heatmap; longer runs average neighbouring
/// samples into each column.
const HEATMAP_MAX_COLUMNS: usize = 60;

/// Metrics shown in the per-stage table, in display order.
const STAGE_TABLE_METRICS: &[&str] = &[
    "cpu_percent",
    "cpu_cores",
    "cpu_percent_of_limit",
    "cpu_throttled_percent",
    "memory_working_set_mib",
    "memory_used_percent",
];

/// One dataset of a chart.
#[derive(Debug, Serialize)]
pub struct ChartDataset {
    pub label: String,
    pub points: Vec<[f64; 2]>,
}

/// One time-series chart: every series of one group sharing one unit.
#[derive(Debug, Serialize)]
pub struct ResourceChart {
    pub title: String,
    pub unit: String,
    pub datasets: Vec<ChartDataset>,
}

/// Latency against the most relevant resource series, on one time axis.
#[derive(Debug, Serialize)]
pub struct CorrelationChart {
    pub title: String,
    pub unit: String,
    pub datasets: Vec<ChartDataset>,
}

#[derive(Debug, Serialize)]
pub struct HeatmapCell {
    pub value: Option<f64>,
    /// Background opacity, 0 to 1.
    pub alpha: f64,
}

#[derive(Debug, Serialize)]
pub struct HeatmapRow {
    pub label: String,
    pub cells: Vec<HeatmapCell>,
}

/// Per-core utilisation over time.
#[derive(Debug, Serialize)]
pub struct Heatmap {
    pub title: String,
    pub columns: Vec<String>,
    pub rows: Vec<HeatmapRow>,
}

/// A row of the resource summary table.
#[derive(Debug, Serialize)]
pub struct SummaryRow {
    pub group: String,
    pub metric: String,
    pub source: String,
    pub unit: String,
    pub avg: f64,
    pub p95: f64,
    pub max: f64,
    pub last: f64,
    pub increase: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct StageRow {
    pub label: String,
    pub cells: Vec<String>,
}

/// The per-stage table: one column per key series.
#[derive(Debug, Serialize)]
pub struct StageTable {
    pub headers: Vec<String>,
    pub rows: Vec<StageRow>,
}

/// Everything the HTML template needs for the resource section.
#[derive(Debug, Serialize)]
pub struct ResourceView {
    pub charts: Vec<ResourceChart>,
    pub correlation: Option<CorrelationChart>,
    pub heatmaps: Vec<Heatmap>,
    pub rows: Vec<SummaryRow>,
    pub stage_table: Option<StageTable>,
}

fn is_per_core(series: &ResourceSeries) -> bool {
    series.metric == "cpu_percent" && series.labels.contains_key("cpu")
}

fn unit_title(unit: &str) -> &str {
    match unit {
        "percent" => "Utilisation (%)",
        "cores" => "CPU (cores)",
        "MiB" => "Memory (MiB)",
        "MiB/s" => "I/O (MiB/s)",
        "count" => "Counts",
        "1/s" => "Events per second",
        "load" => "Load average",
        "" => "Values",
        other => other,
    }
}

fn dataset_label(series: &ResourceSeries) -> String {
    if series.labels.is_empty() {
        return series.metric.clone();
    }
    let labels: Vec<String> = series
        .labels
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    format!("{} {{{}}}", series.metric, labels.join(", "))
}

/// Build the view for a report.
pub fn build(summary: &MetricsSummary, resources: &ResourceReport) -> ResourceView {
    // Charts: one per (group, unit), in group order, per-core CPU excluded
    // (that goes into the heatmap).
    let mut charts = Vec::new();
    for group in &resources.groups {
        let mut by_unit: BTreeMap<&str, Vec<&ResourceSeries>> = BTreeMap::new();
        for series in resources
            .series
            .iter()
            .filter(|series| series.group == group.name && !is_per_core(series))
            .filter(|series| series.kind == "gauge")
        {
            by_unit
                .entry(series.unit.as_str())
                .or_default()
                .push(series);
        }
        for (unit, series) in by_unit {
            charts.push(ResourceChart {
                title: format!("{} · {}", group.name, unit_title(unit)),
                unit: unit.to_string(),
                datasets: series
                    .into_iter()
                    .map(|series| ChartDataset {
                        label: dataset_label(series),
                        points: series.points.clone(),
                    })
                    .collect(),
            });
        }
    }

    let rows = resources
        .series
        .iter()
        .filter(|series| !is_per_core(series))
        .map(|series| SummaryRow {
            group: series.group.clone(),
            metric: dataset_label(series),
            source: series.source.clone(),
            unit: series.unit.clone(),
            avg: series.summary.avg,
            p95: series.summary.p95,
            max: series.summary.max,
            last: series.summary.last,
            increase: series.summary.increase,
        })
        .collect();

    ResourceView {
        charts,
        correlation: correlation(summary, resources),
        heatmaps: heatmaps(summary, resources),
        rows,
        stage_table: stage_table(resources),
    }
}

/// p95 latency next to the target's CPU (or, without a target, the host's),
/// plus the load generator's CPU when it shares the unit.
fn correlation(summary: &MetricsSummary, resources: &ResourceReport) -> Option<CorrelationChart> {
    if summary.timeline.is_empty() {
        return None;
    }
    let candidates = resources
        .derived
        .containers
        .iter()
        .flat_map(|container| {
            [
                format!("{}.cpu_percent_of_limit", container.group),
                format!("{}.cpu_cores", container.group),
            ]
        })
        .chain(
            resources
                .derived
                .hosts
                .iter()
                .map(|host| format!("{}.cpu_percent", host.group)),
        );
    let primary = candidates
        .filter_map(|id| resources.series(&id))
        .find(|series| !series.points.is_empty())?;

    let mut datasets = vec![ChartDataset {
        label: format!("{} {}", primary.group, primary.metric),
        points: primary.points.clone(),
    }];
    if primary.unit == "percent" {
        if let Some(loadgen) = resources.series("flux.cpu_percent") {
            if loadgen.id != primary.id {
                datasets.push(ChartDataset {
                    label: "flux cpu_percent".to_string(),
                    points: loadgen.points.clone(),
                });
            }
        }
    }

    Some(CorrelationChart {
        title: format!("p95 latency vs {} {}", primary.group, primary.metric),
        unit: primary.unit.clone(),
        datasets,
    })
}

fn core_order(label: &str) -> (u64, String) {
    (label.parse().unwrap_or(u64::MAX), label.to_string())
}

/// One heatmap per group that has per-core CPU series.
fn heatmaps(summary: &MetricsSummary, resources: &ResourceReport) -> Vec<Heatmap> {
    let span = summary.total_duration_secs.max(1.0);
    let columns = HEATMAP_MAX_COLUMNS.min(span.ceil() as usize).max(1);
    let width = span / columns as f64;

    let mut by_group: BTreeMap<&str, Vec<&ResourceSeries>> = BTreeMap::new();
    for series in resources.series.iter().filter(|series| is_per_core(series)) {
        by_group
            .entry(series.group.as_str())
            .or_default()
            .push(series);
    }

    resources
        .groups
        .iter()
        .filter_map(|group| {
            let mut cores = by_group.remove(group.name.as_str())?;
            cores.sort_by_key(|series| core_order(series.labels.get("cpu").map_or("", |v| v)));
            let rows = cores
                .into_iter()
                .map(|series| {
                    let mut sums = vec![(0.0, 0usize); columns];
                    for [offset, value] in &series.points {
                        if *offset < 0.0 {
                            continue;
                        }
                        let index = ((offset / width) as usize).min(columns - 1);
                        sums[index].0 += value;
                        sums[index].1 += 1;
                    }
                    HeatmapRow {
                        label: format!("cpu{}", series.labels.get("cpu").map_or("?", |v| v)),
                        cells: sums
                            .into_iter()
                            .map(|(sum, count)| {
                                let value = (count > 0).then(|| sum / count as f64);
                                HeatmapCell {
                                    value: value.map(|v| (v * 10.0).round() / 10.0),
                                    alpha: value.map_or(0.0, |v| (v / 100.0).clamp(0.0, 1.0)),
                                }
                            })
                            .collect(),
                    }
                })
                .collect();
            Some(Heatmap {
                title: format!("{} · CPU per core (%)", group.name),
                columns: (0..columns)
                    .map(|index| format!("{:.0}s", index as f64 * width))
                    .collect(),
                rows,
            })
        })
        .collect()
}

/// `avg / max` of the key series during each stage.
fn stage_table(resources: &ResourceReport) -> Option<StageTable> {
    if resources.per_stage.is_empty() {
        return None;
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    for group in &resources.groups {
        for metric in STAGE_TABLE_METRICS {
            let id = format!("{}.{metric}", group.name);
            let present = resources
                .per_stage
                .iter()
                .any(|stage| stage.series.iter().any(|stat| stat.id == id));
            if present {
                let unit = resources
                    .series(&id)
                    .map_or("", |series| series.unit.as_str());
                headers.push((id.clone(), format!("{id} ({unit})")));
            }
        }
    }
    if headers.is_empty() {
        return None;
    }
    let rows = resources
        .per_stage
        .iter()
        .map(|stage| StageRow {
            label: stage.label.clone(),
            cells: headers
                .iter()
                .map(|(id, _)| {
                    stage
                        .series
                        .iter()
                        .find(|stat| &stat.id == id)
                        .map_or("-".to_string(), |stat| {
                            format!("{:.2} / {:.2}", stat.avg, stat.max)
                        })
                })
                .collect(),
        })
        .collect();
    Some(StageTable {
        headers: headers.into_iter().map(|(_, title)| title).collect(),
        rows,
    })
}

/// Serialise `value` for embedding inside a `<script>` element.
///
/// `<` only ever appears inside JSON strings, so escaping it as `<` is
/// always valid JSON and means no label from an exporter can close the script
/// element or open a comment.
pub fn script_json<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?.replace('<', "\\u003c"))
}

/// Write every resource sample as one CSV row. Returns the number of rows.
pub fn write_csv(
    path: &str,
    summary: &MetricsSummary,
    resources: &ResourceReport,
) -> Result<usize> {
    let mut writer = csv::Writer::from_path(path)?;
    writer.write_record([
        "timestamp",
        "offset_secs",
        "group",
        "source",
        "metric",
        "labels",
        "unit",
        "value",
    ])?;
    let start_ms = summary.start_time.timestamp_millis();
    let mut rows = 0;
    for series in &resources.series {
        let labels: Vec<String> = series
            .labels
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        let labels = labels.join(";");
        for [offset, value] in &series.points {
            let timestamp = chrono::DateTime::from_timestamp_millis(
                start_ms + (offset * 1000.0).round() as i64,
            )
            .map(|time| time.to_rfc3339())
            .unwrap_or_default();
            writer.write_record([
                timestamp,
                offset.to_string(),
                series.group.clone(),
                series.source.clone(),
                series.metric.clone(),
                labels.clone(),
                series.unit.clone(),
                value.to_string(),
            ])?;
            rows += 1;
        }
    }
    writer.flush()?;
    Ok(rows)
}
