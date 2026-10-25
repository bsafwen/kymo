//! The desktop card's figures, kept in memory so that /api/card never reads the history: the last
//! hour of a few figures at 10 s, and the last five minutes of apps. Loaded from the history once
//! at start-up, then kept up by each sample.
//!
//! Like the dashboards' "Right now" tiles, "now" is the mean of the last 30 s (three samples);
//! "avg5" is the last 5 minutes, "last" the newest sample. Disk latency is per operation: the time
//! waited divided by the operations, over the same window.

use crate::json::{key, number, numbers, string};
use crate::procs::AppRow;
use crate::store::{self, Tier, APP_N, APP_SECONDS};
use crate::system::Sample;
use crate::units::Unit;
use crate::win;
use std::collections::{HashMap, VecDeque};
use std::path::Path;

/// The figures kept per sample, in this order (the indices below).
pub const FIGS: [&str; 24] = [
    "cpu",
    "cpu_power",
    "cpu_limit",
    "mem_used_pct",
    "mem_total",
    "committed",
    "commit_limit",
    "pages_in",
    "pagefile_size",
    "gpu",
    "gpu_temp",
    "vram_pct",
    "gpu_slow",
    "disk_active",
    "disk_reads",
    "disk_writes",
    "disk_read_time",
    "disk_write_time",
    "disk_free",
    "disk_size",
    "net_rx",
    "net_tx",
    "net_errors",
    "page_reads",
];
const N: usize = FIGS.len();
const CPU: usize = 0;
const CPU_POWER: usize = 1;
const CPU_LIMIT: usize = 2;
const MEM_USED: usize = 3;
const MEM_TOTAL: usize = 4;
const COMMITTED: usize = 5;
const COMMIT_LIMIT: usize = 6;
const PAGES_IN: usize = 7;
const PAGEFILE: usize = 8;
const GPU: usize = 9;
const GPU_TEMP: usize = 10;
const VRAM: usize = 11;
const GPU_SLOW: usize = 12;
const DISK_ACTIVE: usize = 13;
const DISK_READS: usize = 14;
const DISK_WRITES: usize = 15;
const DISK_READ_TIME: usize = 16;
const DISK_WRITE_TIME: usize = 17;
const DISK_FREE: usize = 18;
const DISK_SIZE: usize = 19;
const NET_RX: usize = 20;
const NET_TX: usize = 21;
const NET_ERRORS: usize = 22;
const PAGE_READS: usize = 23;

/// The card's last-hour lines, one point per minute.
const HOUR: [usize; 6] = [CPU, MEM_USED, COMMITTED, GPU, DISK_ACTIVE, NET_RX];
/// "now": the last 30 s, or the last sample at intervals longer than that.
fn now_ms() -> i64 {
    crate::interval_ms().max(30_000)
}
const AVG_MS: i64 = 300_000;
const KEEP_MS: i64 = 3_600_000;
/// Apps: the top CPU is averaged over the last five ended minutes.
const APPS_MS: i64 = 300_000;
/// The process holding other apps' memory, compressed: never the top app by memory, commit or paging.
const COMPRESSION: &str = "Memory Compression";
/// A minute of apps ends with the first sample after it: older than this, the apps are not current.
fn apps_fresh_ms() -> i64 {
    (70_000 + 2 * crate::interval_ms()).max(130_000)
}

/// The Windows drive ("C:"), from %SystemDrive%.
pub fn system_drive() -> String {
    std::env::var("SystemDrive")
        .ok()
        .map(|d| d.trim().to_ascii_uppercase())
        .filter(|d| d.len() == 2 && d.ends_with(':') && d.as_bytes()[0].is_ascii_uppercase())
        .unwrap_or_else(|| "C:".to_string())
}

fn minute_end(t: i64) -> i64 {
    ((t - 1).div_euclid(60_000) + 1) * 60_000
}

/// The figures of one sample, from its series (key, value); NaN where a series is missing. Also
/// returns the number of logical processors in it.
fn figures<'a>(drive: &str, series: impl Iterator<Item = (&'a str, f64)>) -> ([f64; N], usize) {
    let mut f = [f64::NAN; N];
    let (mut available, mut vram_used, mut vram_total) = (f64::NAN, f64::NAN, f64::NAN);
    let mut adapter_errors = f64::NAN;
    let mut cores = 0;
    for (k, v) in series {
        if !v.is_finite() {
            continue;
        }
        let i = match k {
            "cpu.busy" => CPU,
            "power.package" => CPU_POWER,
            "cpu.limit" => CPU_LIMIT,
            "mem.total" => MEM_TOTAL,
            "mem.committed" => COMMITTED,
            "mem.commit_limit" => COMMIT_LIMIT,
            "mem.pages_in" => PAGES_IN,
            "mem.page_reads" => PAGE_READS,
            "pagefile.size" => PAGEFILE,
            "gpu.util" => GPU,
            "gpu.temp" => GPU_TEMP,
            "net.rx_bps" => NET_RX,
            "net.tx_bps" => NET_TX,
            // Every adapter's errors per second (recorded whether adapters are or not).
            "net.errors" => NET_ERRORS,
            "mem.available" => {
                available = v;
                continue;
            }
            "gpu.mem_used" => {
                vram_used = v;
                continue;
            }
            "gpu.mem_total" => {
                vram_total = v;
                continue;
            }
            // Thermal or hardware slowdown, as the dashboards' "why the GPU is slowing down" (the
            // power limit under load is normal): % of the time any of them was active.
            "gpu.slow_sw_thermal"
            | "gpu.slow_hw_thermal"
            | "gpu.slow_hw"
            | "gpu.slow_hw_power_brake" => {
                f[GPU_SLOW] = f[GPU_SLOW].max(v);
                continue;
            }
            _ => {
                if let Some(field) = k
                    .strip_prefix("disk.")
                    .and_then(|r| r.strip_prefix(drive))
                    .and_then(|r| r.strip_prefix('.'))
                {
                    match field {
                        "active" => DISK_ACTIVE,
                        "reads" => DISK_READS,
                        "writes" => DISK_WRITES,
                        "read_time" => DISK_READ_TIME,
                        "write_time" => DISK_WRITE_TIME,
                        "free" => DISK_FREE,
                        "size" => DISK_SIZE,
                        _ => continue,
                    }
                } else if k.starts_with("net.")
                    && (k.ends_with(".rx_errors") || k.ends_with(".tx_errors"))
                {
                    // Each adapter's: the total before it was recorded on its own.
                    adapter_errors = if adapter_errors.is_nan() {
                        v
                    } else {
                        adapter_errors + v
                    };
                    continue;
                } else {
                    if k.starts_with("cpu.core.") && k.ends_with(".busy") {
                        cores += 1;
                    }
                    continue;
                }
            }
        };
        f[i] = v;
    }
    if available.is_finite() && f[MEM_TOTAL] > 0.0 {
        f[MEM_USED] = (100.0 * (1.0 - available / f[MEM_TOTAL])).clamp(0.0, 100.0);
    }
    if vram_used.is_finite() && vram_total > 0.0 {
        f[VRAM] = 100.0 * vram_used / vram_total;
    }
    if f[NET_ERRORS].is_nan() {
        f[NET_ERRORS] = adapter_errors;
    }
    (f, cores)
}

/// One ended minute of apps: its end, then each app's name, values (store::APP_FIELDS) and hard
/// faults per second (NaN for a minute read back from the history, which does not keep them).
type AppMinute = (i64, Vec<(String, [f64; APP_N], f64)>);

pub struct Recent {
    drive: String,
    points: VecDeque<(i64, [f64; N])>,
    apps: VecDeque<AppMinute>,
    /// Logical processors (app CPU is stored as a % of one of them).
    cores: usize,
}

impl Default for Recent {
    fn default() -> Recent {
        Recent::new(system_drive())
    }
}

impl Recent {
    pub fn new(drive: String) -> Recent {
        Recent {
            drive,
            points: VecDeque::new(),
            apps: VecDeque::new(),
            cores: 0,
        }
    }

    /// The last hour of samples and the last minutes of apps before `now`, from the history, for
    /// the card's disk `drive`.
    pub fn load(root: &Path, now: i64, drive: String) -> Recent {
        let mut r = Recent::new(drive.clone());
        store::each_row(root, Tier::Raw, now - KEEP_MS, now, |ts, _, cols, vals| {
            let (f, cores) = figures(
                &drive,
                cols.iter().zip(vals).map(|((k, _), v)| (k.as_str(), *v)),
            );
            r.push(ts, f, cores);
        });
        store::each_app_row(
            root,
            Tier::AppMin,
            now - APPS_MS - apps_fresh_ms(),
            now,
            |end, _, names, rows| {
                let rows = rows
                    .iter()
                    .filter_map(|(id, v)| {
                        let mut a = [0.0; APP_N];
                        a.copy_from_slice(&v[..APP_N]);
                        Some((names.get(*id)?.clone(), a, f64::NAN))
                    })
                    .collect();
                r.push_apps(end, rows);
            },
        );
        r
    }

    /// Time of the newest sample kept.
    #[cfg(test)]
    fn newest(&self) -> Option<i64> {
        self.points.back().map(|p| p.0)
    }

    /// Samples and minutes of apps kept (for the log).
    pub fn sizes(&self) -> (usize, usize) {
        (self.points.len(), self.apps.len())
    }

    /// Forgets the samples, the apps or both (their history was deleted).
    pub fn clear(&mut self, samples: bool, apps: bool) {
        if samples {
            self.points.clear();
        }
        if apps {
            self.apps.clear();
        }
    }

    /// For a sample at `t`: after the clock went back (`far`: by more than two minutes, a new
    /// timeline), drops everything kept, which cannot be placed on the new timeline; after a
    /// smaller step, only what now lies at or after `t`.
    pub fn clock_check(&mut self, t: i64, far: bool) {
        if far {
            self.points.clear();
            self.apps.clear();
            return;
        }
        while self.points.back().is_some_and(|p| p.0 >= t) {
            self.points.pop_back();
        }
        while self.apps.back().is_some_and(|a| a.0 > t + 60_000) {
            self.apps.pop_back();
        }
    }

    fn push(&mut self, t: i64, f: [f64; N], cores: usize) {
        // After the clock went back, the points that now look newer are dropped.
        while self.points.back().is_some_and(|p| p.0 >= t) {
            self.points.pop_back();
        }
        self.points.push_back((t, f));
        while self.points.front().is_some_and(|p| p.0 <= t - KEEP_MS) {
            self.points.pop_front();
        }
        if cores > 0 {
            self.cores = cores;
        }
    }

    fn push_apps(&mut self, end: i64, rows: Vec<(String, [f64; APP_N], f64)>) {
        while self.apps.back().is_some_and(|a| a.0 >= end) {
            self.apps.pop_back();
        }
        self.apps.push_back((end, rows));
        while self.apps.front().is_some_and(|a| a.0 <= end - APPS_MS) {
            self.apps.pop_front();
        }
    }

    /// A sample kept in the history (baselines are not).
    pub fn sample(&mut self, t: i64, samples: &[Sample]) {
        let (f, cores) = figures(
            &self.drive,
            samples.iter().map(|s| (s.key.as_str(), s.value)),
        );
        self.push(t, f, cores);
    }

    /// A minute of apps that ended.
    pub fn apps(&mut self, end: i64, rows: &[AppRow]) {
        self.push_apps(
            end,
            rows.iter()
                .map(|r| (r.name.clone(), r.v, r.faults))
                .collect(),
        );
    }

    /// Whether any sample of the last hour has a figure (a source that answered before).
    fn seen(&self, i: usize) -> bool {
        self.points.iter().any(|p| p.1[i].is_finite())
    }

    /// Logical processors: from the samples, or from Windows before the first one.
    fn cores(&self) -> usize {
        if self.cores > 0 {
            self.cores
        } else {
            win::logical_processors().max(1)
        }
    }

    /// `"sources":{...}`, each "ok" or "failing"; the GPU "none" when there is no NVIDIA GPU, and the
    /// apps "waiting" until a minute of them is due; the GPU and the apps "off" when the settings
    /// do not record them. `latest` is the newest sample's series (None: the newest point kept
    /// stands in); `gpu_failing`, whether the NVIDIA driver is there without readings; `apps_due`,
    /// whether a minute of apps should have ended by `t_ref`.
    #[allow(clippy::too_many_arguments)]
    pub fn write_sources(
        &self,
        o: &mut String,
        t_ref: i64,
        latest: Option<&[(String, Unit, f64)]>,
        gpu_failing: bool,
        apps_due: bool,
        gpu_on: bool,
        apps_on: bool,
    ) {
        let f = match latest {
            Some(series) => figures(&self.drive, series.iter().map(|(k, _, v)| (k.as_str(), *v))).0,
            None => self.points.back().map_or([f64::NAN; N], |p| p.1),
        };
        let has = |figs: &[usize]| figs.iter().all(|&i| f[i].is_finite());
        let apps_fresh = self
            .apps
            .back()
            .is_some_and(|a| a.0 > t_ref - apps_fresh_ms());
        key(o, "sources", false);
        o.push('{');
        for (i, (name, state)) in [
            (
                "counters",
                if has(&[CPU, MEM_USED]) {
                    "ok"
                } else {
                    "failing"
                },
            ),
            (
                "disk",
                if has(&[DISK_ACTIVE, DISK_FREE]) {
                    "ok"
                } else {
                    "failing"
                },
            ),
            (
                "gpu",
                if !gpu_on {
                    "off"
                } else if has(&[GPU]) {
                    "ok"
                } else if gpu_failing || self.seen(GPU) {
                    "failing"
                } else {
                    "none"
                },
            ),
            (
                "apps",
                if !apps_on {
                    "off"
                } else if apps_fresh {
                    "ok"
                } else if apps_due {
                    "failing"
                } else {
                    "waiting"
                },
            ),
        ]
        .into_iter()
        .enumerate()
        {
            key(o, name, i == 0);
            string(o, state);
        }
        o.push('}');
    }

    /// Mean of each figure over the samples in (t_ref - span, t_ref], with the read and write
    /// latency in ms (time waited / operations).
    fn mean(&self, t_ref: i64, span: i64) -> ([f64; N], [f64; 2]) {
        let mut sum = [0.0; N];
        let mut n = [0u32; N];
        let mut lat = [0.0f64; 4]; // read time, reads, write time, writes
        for (t, f) in self.points.iter().rev() {
            if *t > t_ref {
                continue;
            }
            if *t <= t_ref - span {
                break;
            }
            for i in 0..N {
                if f[i].is_finite() {
                    sum[i] += f[i];
                    n[i] += 1;
                }
            }
            for (k, (time, ops)) in [(DISK_READ_TIME, DISK_READS), (DISK_WRITE_TIME, DISK_WRITES)]
                .into_iter()
                .enumerate()
            {
                if f[time].is_finite() && f[ops].is_finite() {
                    lat[2 * k] += f[time];
                    lat[2 * k + 1] += f[ops];
                }
            }
        }
        let mut m = [f64::NAN; N];
        for i in 0..N {
            if n[i] > 0 {
                m[i] = sum[i] / n[i] as f64;
            }
        }
        let per_op = |time: f64, ops: f64| {
            if ops > 0.0 {
                1000.0 * time / ops
            } else {
                f64::NAN
            }
        };
        (m, [per_op(lat[0], lat[1]), per_op(lat[2], lat[3])])
    }

    /// The card's figures after the caller's fields: drive, cores, now, avg5, last, hour, top. The
    /// windows end at `t_ref`: the newest sample's time while it is current, else the present.
    /// Without a `current` sample, now, avg5, last and top are left empty (a stopped recorder's
    /// last figures never pass for current ones); the last hour is still drawn.
    pub fn write(&self, o: &mut String, t_ref: i64, current: bool) {
        key(o, "drive", false);
        string(o, &self.drive);
        key(o, "cores", false);
        number(o, self.cores() as f64);

        let none = ([f64::NAN; N], [f64::NAN; 2]);
        let (now, now_lat) = if current {
            self.mean(t_ref, now_ms())
        } else {
            none
        };
        let (avg5, avg5_lat) = if current {
            self.mean(t_ref, AVG_MS)
        } else {
            none
        };
        let last = match current {
            true => self
                .points
                .iter()
                .rev()
                .find(|p| p.0 <= t_ref)
                .filter(|p| p.0 > t_ref - now_ms())
                .map_or([f64::NAN; N], |p| p.1),
            false => [f64::NAN; N],
        };
        write_figures(o, "now", &now, Some(now_lat));
        write_figures(o, "avg5", &avg5, Some(avg5_lat));
        write_figures(o, "last", &last, None);

        // The last hour per minute: minute i covers (start + i min, start + (i + 1) min].
        let end = minute_end(t_ref);
        let start = end - KEEP_MS;
        let mut sum = [[0.0f64; 60]; HOUR.len()];
        let mut n = [[0u32; 60]; HOUR.len()];
        for (t, f) in &self.points {
            if *t <= start || *t > end {
                continue;
            }
            let b = ((t - start - 1) / 60_000) as usize;
            for (j, &i) in HOUR.iter().enumerate() {
                if f[i].is_finite() {
                    sum[j][b] += f[i];
                    n[j][b] += 1;
                }
            }
        }
        key(o, "hour", false);
        o.push('{');
        key(o, "start", true);
        number(o, start as f64 / 1000.0);
        key(o, "step", false);
        number(o, 60.0);
        for (j, &i) in HOUR.iter().enumerate() {
            key(o, FIGS[i], false);
            let v: Vec<f64> = (0..60)
                .map(|b| {
                    if n[j][b] > 0 {
                        sum[j][b] / n[j][b] as f64
                    } else {
                        f64::NAN
                    }
                })
                .collect();
            numbers(o, &v);
        }
        o.push('}');
        if current {
            self.write_top(o, t_ref);
        } else {
            key(o, "top", false);
            o.push_str("{}");
        }
    }

    /// The busiest app by CPU over the last 5 minutes (% of the whole CPU; time without data counts
    /// as 0, as in /api/apps), the biggest by memory (private working set) and by commit in the
    /// newest minute, and over the last 5 minutes the one reading and writing the most ("io", bytes/s
    /// over the minutes that have apps: files, devices and network; from 1 MB/s) and the one with the most hard page faults ("faults",
    /// per second over the minutes that counted them, which those read back from the history did not,
    /// with "share": its % of the PC's page reads). An app is named for paging only when it makes at
    /// least a fifth of the page reads and one a second: reads through the file cache are charged to
    /// no app, and naming whichever app faulted a little would name the wrong one. Memory Compression
    /// is never named for memory, commit or paging: its working set holds other apps' pages, compressed
    /// when RAM runs short, and its hard faults read those pages back from the page file (Task
    /// Manager's Processes tab leaves it out too). Left out when the newest minute of apps is not
    /// current.
    fn write_top(&self, o: &mut String, t_ref: i64) {
        key(o, "top", false);
        o.push('{');
        if let Some((end, rows)) = self.apps.back().filter(|a| a.0 > t_ref - apps_fresh_ms()) {
            let mut cpu: HashMap<&str, f64> = HashMap::new();
            let mut io: HashMap<&str, f64> = HashMap::new();
            let mut faults: HashMap<&str, f64> = HashMap::new();
            let mut fault_s = 0.0; // seconds of the minutes that counted faults
            let mut io_s = 0.0; // seconds of the minutes with apps at all (none after a start or a sleep)
            for (e, minute) in &self.apps {
                if *e <= end - APPS_MS {
                    continue;
                }
                if let Some((_, v, _)) = minute.iter().find(|(_, _, f)| f.is_finite()) {
                    if v[APP_SECONDS].is_finite() {
                        fault_s += v[APP_SECONDS];
                    }
                }
                if let Some((_, v, _)) = minute.first() {
                    if v[APP_SECONDS].is_finite() {
                        io_s += v[APP_SECONDS];
                    }
                }
                for (name, v, f) in minute {
                    let secs = v[APP_SECONDS];
                    if !secs.is_finite() {
                        continue;
                    }
                    if v[0].is_finite() {
                        *cpu.entry(name.as_str()).or_default() += v[0] * secs;
                    }
                    if v[3].is_finite() && v[4].is_finite() {
                        *io.entry(name.as_str()).or_default() += (v[3] + v[4]) * secs;
                    }
                    if f.is_finite() && name != COMPRESSION {
                        *faults.entry(name.as_str()).or_default() += f * secs;
                    }
                }
            }
            let cores = self.cores() as f64;
            let window_s = APPS_MS as f64 / 1000.0;
            let top_cpu = cpu
                .into_iter()
                .max_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.0.cmp(a.0)))
                .map(|(name, s)| (name, (s / window_s / cores).min(100.0)));
            fn most(m: HashMap<&str, f64>, window_s: f64) -> Option<(&str, f64)> {
                if window_s <= 0.0 {
                    return None;
                }
                m.into_iter()
                    .filter(|(_, s)| *s > 0.0)
                    .max_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.0.cmp(a.0)))
                    .map(|(name, s)| (name, s / window_s))
            }
            let top_io = most(io, io_s).filter(|(_, b)| *b >= 1_048_576.0);
            let page_reads = self.mean(t_ref, AVG_MS).0[PAGE_READS];
            let top_faults = most(faults, fault_s)
                .filter(|(_, f)| *f >= 1.0 && page_reads > 0.0 && *f >= 0.2 * page_reads)
                .map(|(name, f)| (name, f, (100.0 * f / page_reads).min(100.0)));
            let biggest = |field: usize| {
                rows.iter()
                    .filter(|(name, v, _)| v[field].is_finite() && name != COMPRESSION)
                    .max_by(|a, b| {
                        a.1[field]
                            .total_cmp(&b.1[field])
                            .then_with(|| b.0.cmp(&a.0))
                    })
                    .map(|(name, v, _)| (name.as_str(), v[field]))
            };
            let mut first = true;
            for (label, best) in [
                ("cpu", top_cpu),
                ("mem", biggest(5)),
                ("commit", biggest(6)),
                ("io", top_io),
            ] {
                let Some((name, value)) = best else { continue };
                key(o, label, first);
                first = false;
                o.push('{');
                key(o, "name", true);
                string(o, name);
                key(o, "value", false);
                number(o, value);
                o.push('}');
            }
            if let Some((name, value, share)) = top_faults {
                key(o, "faults", first);
                o.push('{');
                key(o, "name", true);
                string(o, name);
                key(o, "value", false);
                number(o, value);
                key(o, "share", false);
                number(o, share.round());
                o.push('}');
            }
        }
        o.push('}');
    }
}

/// `"name":{figure: value, ...}` with only the figures that have a value.
fn write_figures(o: &mut String, name: &str, f: &[f64; N], latency: Option<[f64; 2]>) {
    key(o, name, false);
    o.push('{');
    let mut first = true;
    let mut put = |o: &mut String, k: &str, v: f64| {
        if v.is_finite() {
            key(o, k, first);
            number(o, v);
            first = false;
        }
    };
    for i in 0..N {
        put(o, FIGS[i], f[i]);
    }
    if let Some([read, write]) = latency {
        put(o, "disk_read_latency_ms", read);
        put(o, "disk_write_latency_ms", write);
    }
    o.push('}');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(key: &str, value: f64) -> Sample {
        Sample {
            key: key.to_string(),
            unit: Unit::Count,
            value,
        }
    }

    fn sample_at(r: &mut Recent, t: i64, cpu: f64, reads: f64, read_time: f64) {
        r.sample(
            t,
            &[
                s("cpu.busy", cpu),
                s("cpu.core.00.busy", cpu),
                s("cpu.core.01.busy", cpu),
                s("mem.total", 1000.0),
                s("mem.available", 250.0),
                s("disk.C:.reads", reads),
                s("disk.C:.read_time", read_time),
                s("disk.D:.active", 99.0),
            ],
        );
    }

    fn json(r: &Recent, t_ref: i64) -> String {
        let mut o = String::from("{\"x\":0");
        r.write(&mut o, t_ref, true);
        o.push('}');
        o
    }

    fn sources(
        r: &Recent,
        t_ref: i64,
        latest: Option<&[(String, Unit, f64)]>,
        gpu_failing: bool,
        apps_due: bool,
    ) -> String {
        let mut o = String::from("{\"x\":0");
        r.write_sources(&mut o, t_ref, latest, gpu_failing, apps_due, true, true);
        o.push('}');
        o
    }

    #[test]
    fn indices_match_names() {
        for (i, name) in [
            (CPU, "cpu"),
            (CPU_POWER, "cpu_power"),
            (CPU_LIMIT, "cpu_limit"),
            (MEM_USED, "mem_used_pct"),
            (MEM_TOTAL, "mem_total"),
            (COMMITTED, "committed"),
            (COMMIT_LIMIT, "commit_limit"),
            (PAGES_IN, "pages_in"),
            (PAGEFILE, "pagefile_size"),
            (GPU, "gpu"),
            (GPU_TEMP, "gpu_temp"),
            (VRAM, "vram_pct"),
            (GPU_SLOW, "gpu_slow"),
            (DISK_ACTIVE, "disk_active"),
            (DISK_READS, "disk_reads"),
            (DISK_WRITES, "disk_writes"),
            (DISK_READ_TIME, "disk_read_time"),
            (DISK_WRITE_TIME, "disk_write_time"),
            (DISK_FREE, "disk_free"),
            (DISK_SIZE, "disk_size"),
            (NET_RX, "net_rx"),
            (NET_TX, "net_tx"),
            (NET_ERRORS, "net_errors"),
            (PAGE_READS, "page_reads"),
        ] {
            assert_eq!(FIGS[i], name);
        }
    }

    #[test]
    fn sources_states() {
        let mut r = Recent::new("C:".into());
        let t = 1_792_454_400_000;
        sample_at(&mut r, t, 1.0, 0.0, 0.0);
        let v = |k: &str| (k.to_string(), Unit::Count, 1.0);
        let full = [
            v("cpu.busy"),
            v("mem.total"),
            v("mem.available"),
            v("disk.C:.active"),
            v("disk.C:.free"),
            v("gpu.util"),
        ];
        let ok =
            "\"sources\":{\"counters\":\"ok\",\"disk\":\"ok\",\"gpu\":\"ok\",\"apps\":\"waiting\"}";
        assert!(sources(&r, t, Some(&full), false, false).contains(ok));
        // No disk counters, no GPU reading: the GPU was never seen, unless the driver says it fails.
        let partial = [v("cpu.busy"), v("mem.total"), v("mem.available")];
        let j = sources(&r, t, Some(&partial), false, true);
        assert!(
            j.contains(
                "\"counters\":\"ok\",\"disk\":\"failing\",\"gpu\":\"none\",\"apps\":\"failing\""
            ),
            "{j}"
        );
        assert!(sources(&r, t, Some(&partial), true, false).contains("\"gpu\":\"failing\""));
        // A GPU seen in the last hour that stopped answering is failing too.
        r.sample(t + 10_000, &[s("gpu.util", 5.0)]);
        assert!(
            sources(&r, t + 20_000, Some(&partial), false, false).contains("\"gpu\":\"failing\"")
        );
        // Fresh apps.
        r.apps(t + 60_000, &[row("a", 1.0, 1.0, 1.0)]);
        assert!(sources(&r, t + 70_000, Some(&full), false, true).contains("\"apps\":\"ok\""));
        // Without the newest sample's series, the newest point stands in.
        let j = sources(&r, t + 20_000, None, false, false);
        assert!(
            j.contains("\"counters\":\"failing\",\"disk\":\"failing\",\"gpu\":\"ok\""),
            "{j}"
        );
    }

    #[test]
    fn cores_before_any_sample() {
        let r = Recent::new("C:".into());
        assert!(json(&r, 1_000_000).contains(&format!("\"cores\":{}", win::logical_processors())));
    }

    #[test]
    fn figures_of_a_sample() {
        let series = [
            ("mem.total", 1000.0),
            ("mem.available", 250.0),
            ("gpu.mem_used", 3.0),
            ("gpu.mem_total", 12.0),
            ("gpu.slow_sw_power", 100.0), // the power limit is not a slowdown here
            ("gpu.slow_hw_thermal", 50.0),
            ("gpu.slow_hw", 20.0),
            ("net.A.rx_errors", 1.0),
            ("net.B.tx_errors", 2.0),
            ("net.A.rx_discards", 9.0),
            ("disk.C:.free", 5.0),
            ("disk.CD:.free", 7.0),
            ("disk.D:.free", 8.0),
            ("cpu.core.00.busy", 1.0),
            ("cpu.core.01.busy", 1.0),
            ("cpu.core.01.clock", 1.0),
            ("cpu.busy", f64::NAN),
        ];
        let (f, cores) = figures("C:", series.iter().map(|(k, v)| (*k, *v)));
        assert_eq!(f[MEM_USED], 75.0);
        assert_eq!(f[VRAM], 25.0);
        assert_eq!(f[GPU_SLOW], 50.0);
        assert_eq!(f[NET_ERRORS], 3.0);
        assert_eq!(f[DISK_FREE], 5.0);
        assert_eq!(cores, 2);
        assert!(f[CPU].is_nan());
        assert!(f[GPU].is_nan());
    }

    #[test]
    fn windows_latency_and_hour() {
        let t0 = 1_792_454_400_000; // a whole hour
        let mut r = Recent::new("C:".into());
        // 40 minutes of samples: CPU 10 %, then the last 30 s at 40 %; reads of 2 ms, then 10 ms.
        let mut t = t0;
        while t < t0 + 2_400_000 {
            t += 10_000;
            let last = t > t0 + 2_400_000 - 30_000;
            sample_at(
                &mut r,
                t,
                if last { 40.0 } else { 10.0 },
                100.0,
                if last { 1.0 } else { 0.2 },
            );
        }
        let j = json(&r, t);
        assert!(
            j.contains("\"now\":{\"cpu\":40,\"mem_used_pct\":75,\"mem_total\":1000,"),
            "{j}"
        );
        // 5 minutes = 30 samples: 27 at 10 %, 3 at 40 %.
        assert!(j.contains("\"avg5\":{\"cpu\":13,"), "{j}");
        assert!(j.contains("\"disk_read_latency_ms\":10}"), "{j}");
        assert!(j.contains("\"disk_read_latency_ms\":2.8}"), "{j}"); // (27 x 0.2 + 3 x 1) / (30 x 100) s
        assert!(j.contains("\"cores\":2"), "{j}");
        // The hour ends with the minute of the newest sample; 20 minutes before the first sample are empty.
        assert!(
            j.contains(&format!(
                "\"hour\":{{\"start\":{},\"step\":60,\"cpu\":[null,",
                (t - 3_600_000) / 1000
            )),
            "{j}"
        );
        assert!(j.contains("10,10,10,25]"), "{j}"); // the last minute: three samples at 10 %, three at 40 %
                                                    // Nothing older than an hour is kept.
        for _ in 0..400 {
            t += 10_000;
            sample_at(&mut r, t, 5.0, 0.0, 0.0);
        }
        assert_eq!(r.sizes().0, 360);
        // No operations: no latency.
        assert!(!json(&r, t).contains("disk_read_latency_ms"));
    }

    #[test]
    fn clock_going_back_drops_newer_points() {
        let mut r = Recent::new("C:".into());
        for i in 1..=10 {
            sample_at(&mut r, i * 10_000, 1.0, 0.0, 0.0);
        }
        sample_at(&mut r, 45_000, 2.0, 0.0, 0.0);
        assert_eq!(r.sizes().0, 5);
        assert_eq!(r.newest(), Some(45_000));
        // Live samples check first: a small step drops only what now lies ahead; a long one,
        // everything from the old timeline.
        r.apps(60_000, &[row("a", 1.0, 1.0, 1.0)]);
        r.clock_check(50_000, false);
        assert_eq!(r.sizes(), (5, 1));
        r.clock_check(45_000, false);
        assert_eq!(r.sizes(), (4, 1));
        r.clock_check(40_000 - 200_000, true);
        assert_eq!(r.sizes(), (0, 0));
    }

    #[test]
    fn no_current_figures_without_a_current_sample() {
        let mut r = Recent::new("C:".into());
        let t = 1_792_454_400_000;
        sample_at(&mut r, t, 50.0, 0.0, 0.0);
        r.apps(t, &[row("a", 1.0, 1.0, 1.0)]);
        let mut o = String::from("{\"x\":0");
        r.write(&mut o, t + 20_000, false);
        o.push('}');
        assert!(o.contains("\"now\":{},\"avg5\":{},\"last\":{}"), "{o}");
        assert!(o.contains("\"top\":{}"), "{o}");
        assert!(o.contains(",50,null]"), "{o}"); // the last hour is still drawn
    }

    fn row(name: &str, cpu: f64, ws: f64, private: f64) -> AppRow {
        AppRow {
            name: name.to_string(),
            v: [cpu, cpu, 0.0, 0.0, 0.0, ws, private, 1.0, 60.0],
            faults: 0.0,
        }
    }

    #[test]
    fn top_io_and_faults() {
        let t0 = 1_792_454_400_000;
        // Five minutes: "disk" reads 8 MB/s and writes 2; "pager" faults 300 times a second in
        // four of them (the fifth: none), "quiet" does neither. The PC: `reads` page reads a second.
        // Memory Compression faults the most, reading other apps' pages back: never named.
        let five = |reads: f64| {
            let mut r = Recent::new("C:".into());
            sample_at(&mut r, t0, 1.0, 0.0, 0.0);
            for m in 0..5 {
                let io = AppRow {
                    name: "disk".into(),
                    v: [1.0, 1.0, 0.0, 8e6, 2e6, 1.0, 1.0, 1.0, 60.0],
                    faults: 10.0,
                };
                let pager = AppRow {
                    name: "pager".into(),
                    v: [1.0, 1.0, 0.0, 1e3, 0.0, 1.0, 1.0, 1.0, 60.0],
                    faults: if m < 4 { 300.0 } else { 0.0 },
                };
                let store = AppRow {
                    name: "Memory Compression".into(),
                    v: [1.0, 1.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 60.0],
                    faults: 500.0,
                };
                r.apps(
                    t0 + (m + 1) * 60_000,
                    &[io, pager, store, row("quiet", 0.0, 1.0, 1.0)],
                );
            }
            r.sample(t0 + 5 * 60_000, &[s("mem.page_reads", reads)]);
            json(&r, t0 + 5 * 60_000 + 10_000)
        };
        let j = five(1000.0);
        assert!(
            j.contains("\"io\":{\"name\":\"disk\",\"value\":10000000}"),
            "{j}"
        );
        // 4 of 5 minutes at 300: 240 a second, 24 % of the PC's 1000 page reads.
        assert!(
            j.contains("\"faults\":{\"name\":\"pager\",\"value\":240,\"share\":24}"),
            "{j}"
        );
        // Paging mostly through the file cache (5000 reads, the apps' faults a twentieth): none named.
        let j = five(5000.0);
        assert!(!j.contains("\"faults\""), "{j}");

        // After a start: four minutes from the history (no faults) and one counted: its rate, not a
        // fifth of it; and an app under 1 MB/s of I/O is not named.
        let mut h = Recent::new("C:".into());
        sample_at(&mut h, t0, 1.0, 0.0, 0.0);
        for m in 0..4 {
            h.push_apps(
                t0 + (m + 1) * 60_000,
                vec![(
                    "pager".into(),
                    [0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0, 1.0, 60.0],
                    f64::NAN,
                )],
            );
        }
        h.apps(
            t0 + 5 * 60_000,
            &[AppRow {
                name: "pager".into(),
                v: [0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0, 1.0, 60.0],
                faults: 300.0,
            }],
        );
        h.sample(t0 + 5 * 60_000, &[s("mem.page_reads", 500.0)]);
        let j = json(&h, t0 + 5 * 60_000 + 10_000);
        assert!(
            j.contains("\"faults\":{\"name\":\"pager\",\"value\":300,\"share\":60}"),
            "{j}"
        );
        assert!(!j.contains("\"io\""), "{j}");

        // Two minutes of apps after a start: the I/O rate over those two minutes, not over five.
        let mut fresh = Recent::new("C:".into());
        sample_at(&mut fresh, t0, 1.0, 0.0, 0.0);
        for m in 3..5 {
            fresh.apps(
                t0 + (m + 1) * 60_000,
                &[AppRow {
                    name: "scanner".into(),
                    v: [0.0, 0.0, 0.0, 30e6, 0.0, 1.0, 1.0, 1.0, 60.0],
                    faults: 0.0,
                }],
            );
        }
        let j = json(&fresh, t0 + 5 * 60_000 + 10_000);
        assert!(
            j.contains("\"io\":{\"name\":\"scanner\",\"value\":30000000}"),
            "{j}"
        );
    }

    #[test]
    fn top_apps() {
        let mut r = Recent::new("C:".into());
        let t0 = 1_792_454_400_000;
        sample_at(&mut r, t0, 1.0, 0.0, 0.0); // 2 cores
                                              // Six minutes: "a" uses 100 % of one core in the first (outside the 5 minutes), "b" 60 % in each.
        for m in 0..6 {
            let a = if m == 0 { 100.0 } else { 0.0 };
            r.apps(
                t0 + (m + 1) * 60_000,
                &[
                    row("a", a, 9e9, 1.0),
                    row("b", 60.0, 1.0, 5e9),
                    row("Memory Compression", 1.0, 20e9, 6e9),
                ],
            );
        }
        let end = t0 + 6 * 60_000;
        let j = json(&r, end + 10_000);
        assert!(
            j.contains("\"top\":{\"cpu\":{\"name\":\"b\",\"value\":30}"),
            "{j}"
        ); // 60 % of one core of two
        assert!(
            j.contains("\"mem\":{\"name\":\"a\",\"value\":9000000000}"),
            "{j}"
        );
        assert!(
            j.contains("\"commit\":{\"name\":\"b\",\"value\":5000000000}}"),
            "{j}"
        );
        // Not current any more: no top apps.
        assert!(json(&r, end + 200_000).contains("\"top\":{}"));
        assert_eq!(r.sizes().1, 5);
    }
}
