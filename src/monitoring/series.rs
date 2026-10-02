//! Bounded time-series storage and summary statistics for resource metrics.

use super::{ResourceSeries, SeriesSummary};
use std::collections::BTreeMap;

/// Most points kept for any one series. Longer runs merge neighbouring points
/// pairwise, so a soak test of any length stays within a fixed budget.
pub const MAX_POINTS_PER_SERIES: usize = 720;

/// Most distinct series kept, bounding memory and report size whatever the
/// number of cores on the monitored hosts.
pub const MAX_SERIES: usize = 512;

/// Whether a series is a level (gauge) or an ever-growing total (counter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeriesKind {
    Gauge,
    /// A monotonically increasing total; its summary reports the increase
    /// over the window, tolerating counter resets.
    Counter,
}

impl SeriesKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SeriesKind::Gauge => "gauge",
            SeriesKind::Counter => "counter",
        }
    }
}

/// Identity of a series.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SeriesKey {
    pub group: String,
    pub metric: String,
    pub labels: BTreeMap<String, String>,
}

impl SeriesKey {
    pub fn new(group: &str, metric: &str) -> Self {
        Self {
            group: group.to_string(),
            metric: metric.to_string(),
            labels: BTreeMap::new(),
        }
    }

    pub fn with_label(mut self, name: &str, value: &str) -> Self {
        self.labels.insert(name.to_string(), value.to_string());
        self
    }

    /// Stable identifier, e.g. `api.cpu_cores` or `host.cpu_percent{cpu="3"}`.
    pub fn id(&self) -> String {
        let mut id = format!("{}.{}", self.group, self.metric);
        if !self.labels.is_empty() {
            let labels: Vec<String> = self
                .labels
                .iter()
                .map(|(name, value)| format!("{name}=\"{value}\""))
                .collect();
            id.push('{');
            id.push_str(&labels.join(","));
            id.push('}');
        }
        id
    }
}

/// One observation of a series. After downsampling, a point stands for
/// several observations, so it remembers their range as well as their mean.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    /// Unix time in seconds.
    pub time: f64,
    pub value: f64,
    pub min: f64,
    pub max: f64,
    /// Observations merged into this point.
    pub weight: u32,
}

impl Point {
    pub fn new(time: f64, value: f64) -> Self {
        Self {
            time,
            value,
            min: value,
            max: value,
            weight: 1,
        }
    }

    fn merge(self, other: Point) -> Point {
        let weight = self.weight + other.weight;
        let mean =
            |a: f64, b: f64| (a * self.weight as f64 + b * other.weight as f64) / weight as f64;
        Point {
            time: mean(self.time, other.time),
            value: mean(self.value, other.value),
            min: self.min.min(other.min),
            max: self.max.max(other.max),
            weight,
        }
    }
}

/// A series under construction.
#[derive(Debug, Clone)]
pub struct SeriesBuffer {
    pub key: SeriesKey,
    pub source: String,
    pub unit: String,
    pub kind: SeriesKind,
    pub points: Vec<Point>,
}

impl SeriesBuffer {
    pub fn push(&mut self, point: Point) {
        self.points.push(point);
        if self.points.len() > MAX_POINTS_PER_SERIES {
            self.points = downsample(&self.points, self.kind);
        }
    }
}

/// Merge neighbouring points pairwise, roughly halving the series.
///
/// Gauges average each pair (keeping its min and max). Counters keep the
/// first point and then the later point of each pair, so the increase over
/// the run is preserved exactly.
fn downsample(points: &[Point], kind: SeriesKind) -> Vec<Point> {
    match kind {
        SeriesKind::Gauge => points
            .chunks(2)
            .map(|pair| match pair {
                [first, second] => first.merge(*second),
                [only] => *only,
                _ => unreachable!("chunks(2) yields one or two points"),
            })
            .collect(),
        SeriesKind::Counter => {
            let Some((first, rest)) = points.split_first() else {
                return Vec::new();
            };
            let mut thinned = vec![*first];
            thinned.extend(rest.chunks(2).map(|pair| pair[pair.len() - 1]));
            thinned
        }
    }
}

/// All series collected during a run, keyed by identity.
#[derive(Debug, Default)]
pub struct SeriesStore {
    series: BTreeMap<SeriesKey, SeriesBuffer>,
    /// Observations discarded because `MAX_SERIES` was reached.
    pub dropped_series: usize,
}

impl SeriesStore {
    /// Record one observation, creating the series on first use. Non-finite
    /// values (a division by a zero limit, say) are skipped.
    pub fn record(
        &mut self,
        key: SeriesKey,
        source: &str,
        unit: &str,
        kind: SeriesKind,
        time: f64,
        value: f64,
    ) {
        if !value.is_finite() {
            return;
        }
        if !self.series.contains_key(&key) && self.series.len() >= MAX_SERIES {
            self.dropped_series += 1;
            return;
        }
        let buffer = self
            .series
            .entry(key.clone())
            .or_insert_with(|| SeriesBuffer {
                key,
                source: source.to_string(),
                unit: unit.to_string(),
                kind,
                points: Vec::new(),
            });
        buffer.push(Point::new(time, value));
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
    }

    pub fn into_buffers(self) -> Vec<SeriesBuffer> {
        self.series.into_values().collect()
    }
}

/// Summarise the points falling in `[from, to]` (Unix seconds).
///
/// When no point falls inside the window — a test shorter than one sampling
/// interval — every point is used instead, so a short run still reports what
/// was observed rather than nothing.
pub fn summarize(points: &[Point], kind: SeriesKind, from: f64, to: f64) -> SeriesSummary {
    let in_window: Vec<Point> = points
        .iter()
        .copied()
        .filter(|point| point.time >= from && point.time <= to)
        .collect();
    let window: &[Point] = if in_window.is_empty() {
        points
    } else {
        &in_window
    };

    if window.is_empty() {
        return SeriesSummary::default();
    }

    let samples: u32 = window.iter().map(|point| point.weight).sum();
    let weight_total = samples as f64;
    let avg = window
        .iter()
        .map(|point| point.value * point.weight as f64)
        .sum::<f64>()
        / weight_total;
    let min = window
        .iter()
        .map(|point| point.min)
        .fold(f64::INFINITY, f64::min);
    let max = window
        .iter()
        .map(|point| point.max)
        .fold(f64::NEG_INFINITY, f64::max);

    let mut values: Vec<f64> = window.iter().map(|point| point.value).collect();
    values.sort_by(|a, b| a.total_cmp(b));
    let rank = ((0.95 * values.len() as f64).ceil() as usize).clamp(1, values.len());
    // p95 is taken over point means, which never exceed the true maximum;
    // the clamp only guards against floating-point rounding.
    let p95 = values[rank - 1].min(max);

    let (slope_per_min, r_squared) = linear_trend(window);

    let increase = (kind == SeriesKind::Counter).then(|| counter_increase(window));

    SeriesSummary {
        samples,
        min,
        avg,
        p95,
        max,
        first: window[0].value,
        last: window[window.len() - 1].value,
        trend_per_min: slope_per_min,
        trend_r2: r_squared,
        increase,
    }
}

/// Increase of a counter across `points`, treating any decrease as a reset
/// (the process restarted and the counter began again from zero).
fn counter_increase(points: &[Point]) -> f64 {
    points
        .windows(2)
        .map(|pair| {
            let delta = pair[1].value - pair[0].value;
            if delta >= 0.0 {
                delta
            } else {
                pair[1].value
            }
        })
        .sum()
}

/// Least-squares slope (per minute) and coefficient of determination.
fn linear_trend(points: &[Point]) -> (Option<f64>, Option<f64>) {
    if points.len() < 3 {
        return (None, None);
    }
    let n = points.len() as f64;
    let mean_t = points.iter().map(|p| p.time).sum::<f64>() / n;
    let mean_v = points.iter().map(|p| p.value).sum::<f64>() / n;
    let mut cov = 0.0;
    let mut var_t = 0.0;
    let mut var_v = 0.0;
    for point in points {
        let dt = point.time - mean_t;
        let dv = point.value - mean_v;
        cov += dt * dv;
        var_t += dt * dt;
        var_v += dv * dv;
    }
    if var_t <= f64::EPSILON {
        return (None, None);
    }
    let slope = cov / var_t;
    let r2 = if var_v <= f64::EPSILON {
        // A perfectly flat series has no trend to explain.
        0.0
    } else {
        (cov * cov) / (var_t * var_v)
    };
    (Some(slope * 60.0), Some(r2))
}

/// Turn a finished buffer into its report form, with offsets relative to
/// `run_start` and a summary over `[from, to]`.
pub fn finish(buffer: SeriesBuffer, run_start: f64, from: f64, to: f64) -> ResourceSeries {
    let summary = summarize(&buffer.points, buffer.kind, from, to);
    ResourceSeries {
        id: buffer.key.id(),
        group: buffer.key.group.clone(),
        metric: buffer.key.metric.clone(),
        labels: buffer.key.labels.clone(),
        source: buffer.source,
        unit: buffer.unit,
        kind: buffer.kind.as_str().to_string(),
        points: buffer
            .points
            .iter()
            .map(|point| [round3(point.time - run_start), round4(point.value)])
            .collect(),
        summary,
    }
}

fn round3(value: f64) -> f64 {
    (value * 1_000.0).round() / 1_000.0
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points(values: &[(f64, f64)]) -> Vec<Point> {
        values.iter().map(|&(t, v)| Point::new(t, v)).collect()
    }

    #[test]
    fn test_key_id_includes_sorted_labels() {
        let key = SeriesKey::new("host", "cpu_percent")
            .with_label("cpu", "3")
            .with_label("a", "b");
        assert_eq!(key.id(), "host.cpu_percent{a=\"b\",cpu=\"3\"}");
        assert_eq!(SeriesKey::new("api", "cpu_cores").id(), "api.cpu_cores");
    }

    #[test]
    fn test_summary_statistics_within_window() {
        let data = points(&[
            (0.0, 100.0), // before the window: excluded
            (10.0, 10.0),
            (11.0, 20.0),
            (12.0, 30.0),
            (13.0, 40.0),
            (99.0, 100.0), // after the window: excluded
        ]);
        let summary = summarize(&data, SeriesKind::Gauge, 10.0, 13.0);
        assert_eq!(summary.samples, 4);
        assert_eq!(summary.min, 10.0);
        assert_eq!(summary.max, 40.0);
        assert_eq!(summary.avg, 25.0);
        assert_eq!(summary.p95, 40.0);
        assert_eq!(summary.first, 10.0);
        assert_eq!(summary.last, 40.0);
        // 10 per second is 600 per minute, perfectly linear.
        assert!((summary.trend_per_min.unwrap() - 600.0).abs() < 1e-9);
        assert!((summary.trend_r2.unwrap() - 1.0).abs() < 1e-9);
        assert!(summary.increase.is_none());
    }

    #[test]
    fn test_summary_falls_back_to_every_point_for_short_runs() {
        let data = points(&[(0.0, 5.0), (1.0, 7.0)]);
        let summary = summarize(&data, SeriesKind::Gauge, 100.0, 200.0);
        assert_eq!(summary.samples, 2);
        assert_eq!(summary.avg, 6.0);
        assert!(summary.trend_per_min.is_none());
    }

    #[test]
    fn test_counter_increase_survives_resets() {
        let data = points(&[(0.0, 10.0), (1.0, 15.0), (2.0, 2.0), (3.0, 5.0)]);
        let summary = summarize(&data, SeriesKind::Counter, 0.0, 3.0);
        // 5 before the reset, 2 counted from zero after it, then 3 more.
        assert_eq!(summary.increase, Some(10.0));
    }

    #[test]
    fn test_flat_series_has_no_trend_fit() {
        let data = points(&[(0.0, 1.0), (1.0, 1.0), (2.0, 1.0)]);
        let summary = summarize(&data, SeriesKind::Gauge, 0.0, 2.0);
        assert_eq!(summary.trend_per_min, Some(0.0));
        assert_eq!(summary.trend_r2, Some(0.0));
    }

    #[test]
    fn test_store_downsamples_but_keeps_extremes() {
        let mut store = SeriesStore::default();
        let key = SeriesKey::new("flux", "cpu_percent");
        for i in 0..(MAX_POINTS_PER_SERIES * 3) {
            let value = if i == 1_000 { 99.0 } else { 1.0 };
            store.record(
                key.clone(),
                "self",
                "percent",
                SeriesKind::Gauge,
                i as f64,
                value,
            );
        }
        let buffer = store.into_buffers().remove(0);
        assert!(buffer.points.len() <= MAX_POINTS_PER_SERIES);
        let summary = summarize(&buffer.points, SeriesKind::Gauge, 0.0, 1e9);
        assert_eq!(summary.max, 99.0);
        assert_eq!(summary.min, 1.0);
        assert_eq!(summary.samples as usize, MAX_POINTS_PER_SERIES * 3);
    }

    #[test]
    fn test_counter_downsampling_preserves_increase() {
        let mut store = SeriesStore::default();
        let key = SeriesKey::new("api", "oom_events_total");
        for i in 0..(MAX_POINTS_PER_SERIES * 4 + 1) {
            store.record(
                key.clone(),
                "scrape",
                "count",
                SeriesKind::Counter,
                i as f64,
                i as f64,
            );
        }
        let buffer = store.into_buffers().remove(0);
        assert!(buffer.points.len() <= MAX_POINTS_PER_SERIES);
        let summary = summarize(&buffer.points, SeriesKind::Counter, 0.0, 1e9);
        assert_eq!(summary.increase, Some((MAX_POINTS_PER_SERIES * 4) as f64));
    }

    #[test]
    fn test_store_caps_series_and_skips_non_finite_values() {
        let mut store = SeriesStore::default();
        for i in 0..(MAX_SERIES + 5) {
            store.record(
                SeriesKey::new("custom", "q").with_label("i", &i.to_string()),
                "scrape",
                "",
                SeriesKind::Gauge,
                0.0,
                1.0,
            );
        }
        store.record(
            SeriesKey::new("custom", "nan"),
            "scrape",
            "",
            SeriesKind::Gauge,
            0.0,
            f64::NAN,
        );
        assert_eq!(store.dropped_series, 5);
        assert_eq!(store.into_buffers().len(), MAX_SERIES);
    }

    #[test]
    fn test_finish_reports_offsets_relative_to_run_start() {
        let buffer = SeriesBuffer {
            key: SeriesKey::new("api", "memory_working_set_mib"),
            source: "scrape".to_string(),
            unit: "MiB".to_string(),
            kind: SeriesKind::Gauge,
            points: points(&[(1_000.0, 1.0), (1_002.5, 2.0)]),
        };
        let series = finish(buffer, 1_000.0, 1_000.0, 1_010.0);
        assert_eq!(series.id, "api.memory_working_set_mib");
        assert_eq!(series.points, vec![[0.0, 1.0], [2.5, 2.0]]);
        assert_eq!(series.kind, "gauge");
    }
}
