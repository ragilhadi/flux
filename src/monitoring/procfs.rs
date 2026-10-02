//! Load-generator and host metrics read from Linux `/proc` and cgroup files.
//!
//! Everything here is computed from deltas between two samples, using only
//! counters that share one unit (USER_HZ ticks), so no `sysconf` call or libc
//! dependency is needed.

use super::config::{HOST_GROUP, LOADGEN_GROUP};
use super::series::{SeriesKey, SeriesKind, SeriesStore};
use std::path::{Path, PathBuf};

const MIB: f64 = 1024.0 * 1024.0;

/// Tick counters for one CPU line of `/proc/stat`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CpuTicks {
    pub total: u64,
    pub idle: u64,
    pub iowait: u64,
    pub steal: u64,
}

impl CpuTicks {
    /// Busy, iowait and steal percentages between `self` (earlier) and `next`.
    fn percentages(&self, next: &CpuTicks) -> Option<(f64, f64, f64)> {
        let total = next.total.checked_sub(self.total)?;
        if total == 0 {
            return None;
        }
        let total = total as f64;
        let idle = next.idle.saturating_sub(self.idle) as f64;
        let iowait = next.iowait.saturating_sub(self.iowait) as f64;
        let steal = next.steal.saturating_sub(self.steal) as f64;
        Some((
            (100.0 * (total - idle) / total).clamp(0.0, 100.0),
            (100.0 * iowait / total).clamp(0.0, 100.0),
            (100.0 * steal / total).clamp(0.0, 100.0),
        ))
    }
}

/// Parsed `/proc/stat`: the aggregate line and one entry per core, in order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcStat {
    pub all: CpuTicks,
    pub cores: Vec<(String, CpuTicks)>,
}

/// Parse `/proc/stat`. Only `cpu` lines are read.
pub fn parse_proc_stat(text: &str) -> Option<ProcStat> {
    let mut stat = ProcStat::default();
    let mut saw_aggregate = false;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(name) = fields.next() else { continue };
        if !name.starts_with("cpu") {
            continue;
        }
        let values: Vec<u64> = fields.filter_map(|field| field.parse().ok()).collect();
        if values.len() < 4 {
            continue;
        }
        let at = |index: usize| values.get(index).copied().unwrap_or(0);
        // user nice system idle iowait irq softirq steal; guest time is
        // already included in user, so it is not added again.
        let ticks = CpuTicks {
            total: (0..8).map(at).sum(),
            idle: at(3),
            iowait: at(4),
            steal: at(7),
        };
        if name == "cpu" {
            stat.all = ticks;
            saw_aggregate = true;
        } else {
            stat.cores
                .push((name.trim_start_matches("cpu").to_string(), ticks));
        }
    }
    saw_aggregate.then_some(stat)
}

/// CPU ticks and thread count from `/proc/self/stat`.
pub fn parse_self_stat(text: &str) -> Option<(u64, u64)> {
    // The command name is wrapped in parentheses and may itself contain
    // spaces or parentheses, so fields are counted from the last ')'.
    let rest = &text[text.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // `fields[0]` is field 3 (state): utime is field 14, stime 15, threads 20.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let threads: u64 = fields.get(17)?.parse().ok()?;
    Some((utime + stime, threads))
}

/// Value in kB of a `Key:   123 kB` line, as found in `/proc/*/status` and
/// `/proc/meminfo`.
pub fn parse_kb_field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let value = line.strip_prefix(key)?.strip_prefix(':')?;
        value.split_whitespace().next()?.parse().ok()
    })
}

/// CPU limit of the current cgroup, in cores, if one is set.
///
/// Reads cgroup v2 `cpu.max` (`"200000 100000"` or `"max 100000"`) and falls
/// back to cgroup v1 `cpu.cfs_quota_us` / `cpu.cfs_period_us`.
pub fn cgroup_cpu_limit(root: &Path) -> Option<f64> {
    if let Ok(text) = std::fs::read_to_string(root.join("cpu.max")) {
        return parse_cgroup_v2_cpu_max(&text);
    }
    let quota: i64 = std::fs::read_to_string(root.join("cpu/cpu.cfs_quota_us"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let period: i64 = std::fs::read_to_string(root.join("cpu/cpu.cfs_period_us"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (quota > 0 && period > 0).then(|| quota as f64 / period as f64)
}

fn parse_cgroup_v2_cpu_max(text: &str) -> Option<f64> {
    let mut fields = text.split_whitespace();
    let quota = fields.next()?;
    let period: f64 = fields.next()?.parse().ok()?;
    if quota == "max" || period <= 0.0 {
        return None;
    }
    let quota: f64 = quota.parse().ok()?;
    (quota > 0.0).then(|| quota / period)
}

/// Samples the load generator and the host from `/proc`.
#[derive(Debug)]
pub struct ProcSampler {
    root: PathBuf,
    cgroup_root: PathBuf,
    sample_self: bool,
    sample_host: bool,
    per_core: bool,
    previous_stat: Option<ProcStat>,
    previous_self_ticks: Option<u64>,
    /// Cores this process may use: the cgroup CPU limit when there is one,
    /// otherwise every online core.
    available_cores: Option<f64>,
    reported_failure: bool,
}

impl ProcSampler {
    pub fn new(sample_self: bool, sample_host: bool, per_core: bool) -> Self {
        Self::with_roots(
            PathBuf::from("/proc"),
            PathBuf::from("/sys/fs/cgroup"),
            sample_self,
            sample_host,
            per_core,
        )
    }

    pub fn with_roots(
        root: PathBuf,
        cgroup_root: PathBuf,
        sample_self: bool,
        sample_host: bool,
        per_core: bool,
    ) -> Self {
        Self {
            root,
            cgroup_root,
            sample_self,
            sample_host,
            per_core,
            previous_stat: None,
            previous_self_ticks: None,
            available_cores: None,
            reported_failure: false,
        }
    }

    /// Take one sample at Unix time `now`, recording into `store`.
    ///
    /// Rates need two samples, so the first call only records levels (memory,
    /// threads) and primes the counters. Returns an error message the first
    /// time `/proc` cannot be read (not Linux, or `/proc` not mounted).
    pub fn sample(&mut self, now: f64, store: &mut SeriesStore) -> Option<String> {
        let stat_text = match std::fs::read_to_string(self.root.join("stat")) {
            Ok(text) => text,
            Err(error) => return self.fail(format!("cannot read /proc/stat: {error}")),
        };
        let Some(stat) = parse_proc_stat(&stat_text) else {
            return self.fail("cannot parse /proc/stat".to_string());
        };
        if self.available_cores.is_none() {
            let online = stat.cores.len().max(1) as f64;
            let limit = cgroup_cpu_limit(&self.cgroup_root);
            self.available_cores = Some(limit.map_or(online, |limit| limit.min(online)));
        }

        let mut failure = None;
        if self.sample_self {
            failure = self.sample_self_process(now, &stat, store);
        }
        if self.sample_host {
            self.sample_host_metrics(now, &stat, store);
        }
        self.previous_stat = Some(stat);
        failure
    }

    fn fail(&mut self, message: String) -> Option<String> {
        if self.reported_failure {
            return None;
        }
        self.reported_failure = true;
        Some(message)
    }

    fn sample_self_process(
        &mut self,
        now: f64,
        stat: &ProcStat,
        store: &mut SeriesStore,
    ) -> Option<String> {
        let self_dir = self.root.join("self");
        let Some((ticks, threads)) = std::fs::read_to_string(self_dir.join("stat"))
            .ok()
            .as_deref()
            .and_then(parse_self_stat)
        else {
            return self.fail("cannot read /proc/self/stat".to_string());
        };
        let mut record = |metric: &str, unit: &str, value: f64| {
            store.record(
                SeriesKey::new(LOADGEN_GROUP, metric),
                "self",
                unit,
                SeriesKind::Gauge,
                now,
                value,
            );
        };

        if let (Some(previous_ticks), Some(previous_stat)) =
            (self.previous_self_ticks, &self.previous_stat)
        {
            let all_ticks = stat.all.total.saturating_sub(previous_stat.all.total);
            if all_ticks > 0 {
                // `all_ticks` spans every core, so this process's share of it,
                // times the core count, is the number of cores it kept busy.
                let online = stat.cores.len().max(1) as f64;
                let cores = ticks.saturating_sub(previous_ticks) as f64 / all_ticks as f64 * online;
                record("cpu_cores", "cores", cores);
                if let Some(available) = self.available_cores.filter(|cores| *cores > 0.0) {
                    record("cpu_percent", "percent", 100.0 * cores / available);
                }
            }
        }
        self.previous_self_ticks = Some(ticks);

        record("threads", "count", threads as f64);
        if let Some(rss_kb) = std::fs::read_to_string(self_dir.join("status"))
            .ok()
            .and_then(|text| parse_kb_field(&text, "VmRSS"))
        {
            record("memory_rss_mib", "MiB", rss_kb as f64 * 1024.0 / MIB);
        }
        if let Ok(entries) = std::fs::read_dir(self_dir.join("fd")) {
            record("open_fds", "count", entries.count() as f64);
        }
        None
    }

    fn sample_host_metrics(&mut self, now: f64, stat: &ProcStat, store: &mut SeriesStore) {
        let mut record = |key: SeriesKey, unit: &str, value: f64| {
            store.record(key, "host", unit, SeriesKind::Gauge, now, value);
        };

        if let Some(previous) = &self.previous_stat {
            if let Some((busy, iowait, steal)) = previous.all.percentages(&stat.all) {
                record(SeriesKey::new(HOST_GROUP, "cpu_percent"), "percent", busy);
                record(
                    SeriesKey::new(HOST_GROUP, "iowait_percent"),
                    "percent",
                    iowait,
                );
                record(
                    SeriesKey::new(HOST_GROUP, "steal_percent"),
                    "percent",
                    steal,
                );
            }
            if self.per_core {
                for (core, ticks) in &stat.cores {
                    let Some((_, before)) = previous.cores.iter().find(|(name, _)| name == core)
                    else {
                        continue;
                    };
                    if let Some((busy, _, _)) = before.percentages(ticks) {
                        record(
                            SeriesKey::new(HOST_GROUP, "cpu_percent").with_label("cpu", core),
                            "percent",
                            busy,
                        );
                    }
                }
            }
        }

        if let Ok(meminfo) = std::fs::read_to_string(self.root.join("meminfo")) {
            let total = parse_kb_field(&meminfo, "MemTotal");
            let available = parse_kb_field(&meminfo, "MemAvailable");
            if let (Some(total), Some(available)) = (total, available) {
                if total > 0 {
                    record(
                        SeriesKey::new(HOST_GROUP, "memory_used_percent"),
                        "percent",
                        100.0 * (1.0 - available as f64 / total as f64),
                    );
                }
                record(
                    SeriesKey::new(HOST_GROUP, "memory_available_mib"),
                    "MiB",
                    available as f64 * 1024.0 / MIB,
                );
            }
        }

        if let Some(load1) = std::fs::read_to_string(self.root.join("loadavg"))
            .ok()
            .and_then(|text| text.split_whitespace().next()?.parse::<f64>().ok())
        {
            record(SeriesKey::new(HOST_GROUP, "load1"), "load", load1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitoring::series::summarize;
    use std::fs;

    const STAT_1: &str = "cpu  100 0 100 800 0 0 0 0 0 0\n\
cpu0 50 0 50 400 0 0 0 0 0 0\n\
cpu1 50 0 50 400 0 0 0 0 0 0\n\
intr 12345\n";

    const STAT_2: &str = "cpu  250 0 150 900 50 0 0 50 0 0\n\
cpu0 190 0 60 400 0 0 0 0 0 0\n\
cpu1 60 0 90 500 50 0 0 50 0 0\n\
intr 12399\n";

    #[test]
    fn test_parse_proc_stat() {
        let stat = parse_proc_stat(STAT_1).unwrap();
        assert_eq!(stat.all.total, 1_000);
        assert_eq!(stat.all.idle, 800);
        assert_eq!(stat.cores.len(), 2);
        assert_eq!(stat.cores[1].0, "1");
        assert!(parse_proc_stat("intr 1\n").is_none());
    }

    #[test]
    fn test_cpu_percentages() {
        let before = parse_proc_stat(STAT_1).unwrap();
        let after = parse_proc_stat(STAT_2).unwrap();
        // 400 ticks elapsed: 100 idle, 50 iowait, 50 steal.
        let (busy, iowait, steal) = before.all.percentages(&after.all).unwrap();
        assert_eq!(busy, 75.0);
        assert_eq!(iowait, 12.5);
        assert_eq!(steal, 12.5);
        // cpu0 was busy for all of its 150 ticks.
        let (busy0, _, _) = before.cores[0].1.percentages(&after.cores[0].1).unwrap();
        assert_eq!(busy0, 100.0);
        assert!(before.all.percentages(&before.all).is_none());
    }

    #[test]
    fn test_parse_self_stat_handles_odd_command_names() {
        let text = "4242 (flux (worker) x) S 1 4242 4242 0 -1 4194560 100 0 0 0 \
                    120 30 0 0 20 0 17 0 12345 1000000 500 18446744073709551615";
        assert_eq!(parse_self_stat(text), Some((150, 17)));
        assert_eq!(parse_self_stat("garbage"), None);
    }

    #[test]
    fn test_parse_kb_field() {
        let text = "MemTotal:       16000000 kB\nMemAvailable:    4000000 kB\n";
        assert_eq!(parse_kb_field(text, "MemTotal"), Some(16_000_000));
        assert_eq!(parse_kb_field(text, "MemAvailable"), Some(4_000_000));
        assert_eq!(parse_kb_field(text, "Mem"), None);
    }

    #[test]
    fn test_cgroup_cpu_max() {
        assert_eq!(parse_cgroup_v2_cpu_max("200000 100000\n"), Some(2.0));
        assert_eq!(parse_cgroup_v2_cpu_max("max 100000\n"), None);
        assert_eq!(parse_cgroup_v2_cpu_max("50000 100000"), Some(0.5));
    }

    fn fake_proc(dir: &Path, stat: &str, self_ticks: u64) {
        fs::create_dir_all(dir.join("self/fd")).unwrap();
        fs::write(dir.join("stat"), stat).unwrap();
        fs::write(
            dir.join("self/stat"),
            format!("1 (flux) S 1 1 1 0 -1 0 0 0 0 0 {self_ticks} 0 0 0 20 0 9 0 1 1 1"),
        )
        .unwrap();
        fs::write(dir.join("self/status"), "Name: flux\nVmRSS:   20480 kB\n").unwrap();
        fs::write(dir.join("self/fd/0"), "").unwrap();
        fs::write(
            dir.join("meminfo"),
            "MemTotal: 1000 kB\nMemAvailable: 250 kB\n",
        )
        .unwrap();
        fs::write(dir.join("loadavg"), "1.50 1.00 0.50 2/100 999\n").unwrap();
    }

    #[test]
    fn test_sampler_records_loadgen_and_host_series() {
        let dir = std::env::temp_dir().join(format!(
            "flux-procfs-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let proc_root = dir.join("proc");
        let cgroup_root = dir.join("cgroup");
        fs::create_dir_all(&cgroup_root).unwrap();
        // One core available to this cgroup out of the two online.
        fs::write(cgroup_root.join("cpu.max"), "100000 100000\n").unwrap();

        let mut sampler = ProcSampler::with_roots(proc_root.clone(), cgroup_root, true, true, true);
        let mut store = SeriesStore::default();

        fake_proc(&proc_root, STAT_1, 0);
        assert!(sampler.sample(0.0, &mut store).is_none());
        // 400 host ticks over 2 cores; the process used 100 of them, i.e. half
        // a core, which is 50% of the single core its cgroup allows.
        fake_proc(&proc_root, STAT_2, 100);
        assert!(sampler.sample(2.0, &mut store).is_none());

        let buffers = store.into_buffers();
        let find = |id: &str| {
            buffers
                .iter()
                .find(|buffer| buffer.key.id() == id)
                .unwrap_or_else(|| panic!("missing series {id}"))
        };
        let last = |id: &str| summarize(&find(id).points, find(id).kind, 0.0, 10.0).last;

        assert_eq!(last("flux.cpu_cores"), 0.5);
        assert_eq!(last("flux.cpu_percent"), 50.0);
        assert_eq!(last("flux.memory_rss_mib"), 20.0);
        assert_eq!(last("flux.threads"), 9.0);
        assert_eq!(last("flux.open_fds"), 1.0);
        assert_eq!(last("host.cpu_percent"), 75.0);
        assert_eq!(last("host.cpu_percent{cpu=\"0\"}"), 100.0);
        assert_eq!(last("host.memory_used_percent"), 75.0);
        assert_eq!(last("host.load1"), 1.5);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_sampler_reports_missing_proc_once() {
        let mut sampler = ProcSampler::with_roots(
            PathBuf::from("/definitely/not/proc"),
            PathBuf::from("/definitely/not/cgroup"),
            true,
            true,
            true,
        );
        let mut store = SeriesStore::default();
        assert!(sampler.sample(0.0, &mut store).is_some());
        assert!(sampler.sample(1.0, &mut store).is_none());
        assert!(store.is_empty());
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// `cargo test --release proc_sample_cost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn proc_sample_cost() {
        let mut sampler = ProcSampler::new(true, true, true);
        let mut store = SeriesStore::default();
        let rounds = 1_000;
        let started = std::time::Instant::now();
        for round in 0..rounds {
            sampler.sample(round as f64, &mut store);
        }
        println!("one /proc sample: {:?}", started.elapsed() / rounds);
    }
}
