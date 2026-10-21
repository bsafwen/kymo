//! One sample of the whole PC: performance counters (CPU, memory, disks, network, power), the
//! process table and the GPU, plus this program's own cost.
//!
//! Series keys are dotted: `cpu.busy`, `cpu.core.07.utility`, `mem.available`, `disk.C:.active`,
//! `net.<adapter>.rx_bps`, `gpu.temp`, `power.package`, `self.cpu`.

use crate::gpu::{Gpu, NoGpu};
use crate::pdh::Query;
use crate::procs::{AppRow, ProcSampler};
use crate::units::Unit;
use crate::win;
use std::collections::HashMap;
use std::time::Instant;

pub struct Sample {
    pub key: String,
    pub unit: Unit,
    pub value: f64,
}

pub fn push(out: &mut Vec<Sample>, key: impl Into<String>, unit: Unit, value: f64) {
    if value.is_finite() {
        out.push(Sample {
            key: key.into(),
            unit,
            value,
        });
    }
}

/// Processor Information counters; the field position is used below.
const CPU: [&str; 9] = [
    "% Idle Time",
    "% User Time",
    "% Privileged Time",
    "% Interrupt Time",
    "% DPC Time",
    "% Processor Utility",
    "% Processor Performance",
    "Processor Frequency",
    "% Performance Limit",
];

const SINGLE: [(&str, &str, Unit); 16] = [
    (r"\Memory\Available Bytes", "mem.available", Unit::Bytes),
    (r"\Memory\Committed Bytes", "mem.committed", Unit::Bytes),
    (r"\Memory\Commit Limit", "mem.commit_limit", Unit::Bytes),
    (
        r"\Memory\Free & Zero Page List Bytes",
        "mem.free",
        Unit::Bytes,
    ),
    (
        r"\Memory\Modified Page List Bytes",
        "mem.modified",
        Unit::Bytes,
    ),
    (
        r"\Memory\Standby Cache Core Bytes",
        "mem.standby_core",
        Unit::Bytes,
    ),
    (
        r"\Memory\Standby Cache Normal Priority Bytes",
        "mem.standby_normal",
        Unit::Bytes,
    ),
    (
        r"\Memory\Standby Cache Reserve Bytes",
        "mem.standby_reserve",
        Unit::Bytes,
    ),
    (r"\Memory\Pool Paged Bytes", "mem.pool_paged", Unit::Bytes),
    (
        r"\Memory\Pool Nonpaged Bytes",
        "mem.pool_nonpaged",
        Unit::Bytes,
    ),
    (r"\Memory\Cache Bytes", "mem.cache", Unit::Bytes),
    (r"\Memory\Pages Input/sec", "mem.pages_in", Unit::PerSec),
    (r"\Memory\Pages Output/sec", "mem.pages_out", Unit::PerSec),
    (r"\Memory\Page Faults/sec", "mem.faults", Unit::PerSec),
    (r"\System\Processor Queue Length", "sys.queue", Unit::Count),
    (
        r"\System\Context Switches/sec",
        "sys.switches",
        Unit::PerSec,
    ),
];

/// LogicalDisk counters, for volumes with a drive letter; the position is used below.
const DISK: [&str; 8] = [
    "% Idle Time",
    "Disk Read Bytes/sec",
    "Disk Write Bytes/sec",
    "Disk Reads/sec",
    "Disk Writes/sec",
    "Avg. Disk sec/Read",
    "Avg. Disk sec/Write",
    "Current Disk Queue Length",
];

/// How often a source that is missing or failing is tried again.
const RETRY_MS: i64 = 300_000;

/// Network Interface rates.
const NET: [(&str, &str, Unit); 3] = [
    ("Bytes Received/sec", "rx_bps", Unit::BytesPerSec),
    ("Bytes Sent/sec", "tx_bps", Unit::BytesPerSec),
    ("Current Bandwidth", "bandwidth", Unit::BitsPerSec),
];

/// Network Interface cumulative counts, stored per second.
const NET_COUNTS: [(&str, &str); 4] = [
    ("Packets Received Errors", "rx_errors"),
    ("Packets Outbound Errors", "tx_errors"),
    ("Packets Received Discarded", "rx_discards"),
    ("Packets Outbound Discarded", "tx_discards"),
];

/// "0,7" -> "cpu.core.07"; "_Total" -> "cpu"; group totals are left out.
fn cpu_key(instance: &str) -> Option<String> {
    if instance == "_Total" {
        return Some("cpu".to_string());
    }
    let (group, core) = instance.split_once(',')?;
    let core: u32 = core.parse().ok()?;
    if group == "0" {
        Some(format!("cpu.core.{core:02}"))
    } else {
        Some(format!("cpu.core.{group}_{core:02}"))
    }
}

fn is_volume(instance: &str) -> bool {
    let b = instance.as_bytes();
    b.len() == 2 && b[0].is_ascii_uppercase() && b[1] == b':'
}

fn power_key(instance: &str) -> String {
    match instance.to_ascii_lowercase().as_str() {
        "rapl_package0_pkg" => "power.package".to_string(),
        "rapl_package0_pp0" => "power.cores".to_string(),
        "rapl_package0_dram" => "power.dram".to_string(),
        other => format!("power.{}", other.replace('.', "_")),
    }
}

struct Counters {
    q: Query,
    cpu: Vec<(usize, usize)>, // counter, field in CPU
    single: Vec<(usize, &'static str, Unit)>,
    disk: Vec<(usize, usize)>, // counter, field in DISK
    net: Vec<(usize, &'static str, Unit)>,
    net_counts: Vec<(usize, &'static str)>,
    /// The RAPL energy counters; registered late on some boots, so tried again while missing.
    energy: Option<usize>,
    energy_retry_at: i64,
    /// When the other counters missing at open (a provider not ready yet) are tried again.
    sets_retry_at: i64,
    prev_counts: HashMap<String, f64>,
    vals: Vec<(String, f64)>,
    raw: Vec<(String, i64)>,
}

const ENERGY: &str = r"\Energy Meter(*)\Energy";

impl Counters {
    fn open() -> Option<Counters> {
        let mut q = Query::open()?;
        let cpu = CPU
            .iter()
            .enumerate()
            .filter_map(|(f, name)| {
                q.add(&format!(r"\Processor Information(*)\{name}"))
                    .map(|i| (i, f))
            })
            .collect();
        let single = SINGLE
            .iter()
            .filter_map(|&(path, key, unit)| q.add(path).map(|i| (i, key, unit)))
            .collect();
        let disk = DISK
            .iter()
            .enumerate()
            .filter_map(|(f, name)| q.add(&format!(r"\LogicalDisk(*)\{name}")).map(|i| (i, f)))
            .collect();
        let net = NET
            .iter()
            .filter_map(|&(name, field, unit)| {
                q.add(&format!(r"\Network Interface(*)\{name}"))
                    .map(|i| (i, field, unit))
            })
            .collect();
        let net_counts = NET_COUNTS
            .iter()
            .filter_map(|&(name, field)| {
                q.add(&format!(r"\Network Interface(*)\{name}"))
                    .map(|i| (i, field))
            })
            .collect();
        let energy = q.add(ENERGY);
        if energy.is_none() {
            log!("CPU power counters (RAPL) not available yet; trying again every 5 minutes");
        }
        let mut c = Counters {
            q,
            cpu,
            single,
            disk,
            net,
            net_counts,
            energy,
            energy_retry_at: 0,
            sets_retry_at: win::tick_ms() + RETRY_MS,
            prev_counts: HashMap::new(),
            vals: Vec::new(),
            raw: Vec::new(),
        };
        c.q.collect();
        Some(c)
    }

    /// Adds the counters that were missing, such as a provider that was not ready at boot (its
    /// figures then start with the next collection). Counters this PC does not have stay missing.
    fn add_missing(&mut self) {
        let mut found: Vec<&str> = Vec::new();
        for (f, name) in CPU.iter().enumerate() {
            if !self.cpu.iter().any(|c| c.1 == f) {
                if let Some(i) = self.q.add(&format!(r"\Processor Information(*)\{name}")) {
                    self.cpu.push((i, f));
                    found.push(*name);
                }
            }
        }
        for &(path, key, unit) in &SINGLE {
            if !self.single.iter().any(|s| s.1 == key) {
                if let Some(i) = self.q.add(path) {
                    self.single.push((i, key, unit));
                    found.push(key);
                }
            }
        }
        for (f, name) in DISK.iter().enumerate() {
            if !self.disk.iter().any(|d| d.1 == f) {
                if let Some(i) = self.q.add(&format!(r"\LogicalDisk(*)\{name}")) {
                    self.disk.push((i, f));
                    found.push(*name);
                }
            }
        }
        for &(name, field, unit) in &NET {
            if !self.net.iter().any(|n| n.1 == field) {
                if let Some(i) = self.q.add(&format!(r"\Network Interface(*)\{name}")) {
                    self.net.push((i, field, unit));
                    found.push(name);
                }
            }
        }
        for &(name, field) in &NET_COUNTS {
            if !self.net_counts.iter().any(|n| n.1 == field) {
                if let Some(i) = self.q.add(&format!(r"\Network Interface(*)\{name}")) {
                    self.net_counts.push((i, field));
                    found.push(name);
                }
            }
        }
        if !found.is_empty() {
            log!("performance counters found: {}", found.join(", "));
        }
    }

    /// Per-second rate of a cumulative count since the previous sample.
    fn rate(&mut self, key: &str, v: f64, dt: f64) -> f64 {
        match self.prev_counts.insert(key.to_string(), v) {
            Some(p) if dt > 0.0 && v >= p => (v - p) / dt,
            _ => f64::NAN,
        }
    }

    /// Reads every counter; `dt` is the measured time since the previous read, in seconds (rates of
    /// raw counts use it; PDH times its own rates). Retries run on the monotonic clock, so a clock
    /// change cannot put them off.
    fn sample(&mut self, dt: f64, out: &mut Vec<Sample>, src: &Sources) {
        let tick = win::tick_ms();
        if src.power && self.energy.is_none() && tick >= self.energy_retry_at {
            self.energy_retry_at = tick + RETRY_MS;
            self.energy = self.q.add(ENERGY);
            if self.energy.is_some() {
                log!("CPU power counters (RAPL) found");
            }
        }
        if tick >= self.sets_retry_at {
            self.sets_retry_at = tick + RETRY_MS;
            self.add_missing();
        }
        if !self.q.collect() {
            return;
        }

        // CPU: per logical processor and in total.
        let mut cpu: HashMap<String, [f64; 9]> = HashMap::new();
        for &(i, field) in &self.cpu {
            self.q.values(i, &mut self.vals);
            for (inst, v) in self.vals.drain(..) {
                cpu.entry(inst).or_insert([f64::NAN; 9])[field] = v;
            }
        }
        for (inst, f) in &cpu {
            let Some(p) = cpu_key(inst) else { continue };
            if p != "cpu" && !src.per_core {
                continue;
            }
            push(
                out,
                format!("{p}.busy"),
                Unit::Percent,
                (100.0 - f[0]).clamp(0.0, 100.0),
            );
            push(out, format!("{p}.utility"), Unit::Percent, f[5]);
            // Effective clock: nominal frequency times % Processor Performance (turbo above 100 %).
            push(out, format!("{p}.clock"), Unit::Mhz, f[7] * f[6] / 100.0);
            if p == "cpu" {
                push(out, "cpu.user", Unit::Percent, f[1]);
                push(out, "cpu.privileged", Unit::Percent, f[2]);
                push(out, "cpu.interrupt", Unit::Percent, f[3]);
                push(out, "cpu.dpc", Unit::Percent, f[4]);
                push(out, "cpu.frequency", Unit::Mhz, f[7]);
                push(out, "cpu.limit", Unit::Percent, f[8]);
            }
        }

        for &(i, key, unit) in &self.single {
            if let Some(v) = self.q.value(i) {
                push(out, key, unit, v);
            }
        }

        let mut disks: HashMap<String, [f64; 8]> = HashMap::new();
        for &(i, field) in &self.disk {
            self.q.values(i, &mut self.vals);
            for (inst, v) in self.vals.drain(..) {
                if is_volume(&inst) {
                    disks.entry(inst).or_insert([f64::NAN; 8])[field] = v;
                }
            }
        }
        for (vol, f) in &disks {
            let key = |field: &str| format!("disk.{vol}.{field}");
            push(
                out,
                key("active"),
                Unit::Percent,
                (100.0 - f[0]).clamp(0.0, 100.0),
            );
            push(out, key("read_bps"), Unit::BytesPerSec, f[1]);
            push(out, key("write_bps"), Unit::BytesPerSec, f[2]);
            push(out, key("reads"), Unit::PerSecFine, f[3]);
            push(out, key("writes"), Unit::PerSecFine, f[4]);
            push(out, key("queue"), Unit::Count, f[7]);
            // Latency means nothing without operations. The time spent waiting per second (latency x
            // operations per second) is kept as well: the API divides its sum by the operations', so
            // longer periods get the latency per operation, as rate(seconds) / rate(operations) does.
            push(
                out,
                key("read_latency"),
                Unit::Seconds,
                if f[3] > 0.0 { f[5] } else { f64::NAN },
            );
            push(
                out,
                key("write_latency"),
                Unit::Seconds,
                if f[4] > 0.0 { f[6] } else { f64::NAN },
            );
            push(out, key("read_time"), Unit::SecondsFine, f[5] * f[3]);
            push(out, key("write_time"), Unit::SecondsFine, f[6] * f[4]);
            if let Some((size, free)) = win::disk_space(vol) {
                push(out, key("size"), Unit::Bytes, size as f64);
                push(out, key("free"), Unit::Bytes, free as f64);
            }
        }

        // Per adapter when asked (a setting); the totals always.
        let (mut rx, mut tx, mut adapters) = (0.0, 0.0, 0);
        for &(i, field, unit) in &self.net {
            self.q.values(i, &mut self.vals);
            for (inst, v) in self.vals.drain(..) {
                if src.per_adapter {
                    push(
                        out,
                        format!("net.{}.{field}", inst.replace('.', "_")),
                        unit,
                        v,
                    );
                }
                match field {
                    "rx_bps" => {
                        rx += v;
                        adapters += 1;
                    }
                    "tx_bps" => tx += v,
                    _ => {}
                }
            }
        }
        if adapters > 0 {
            push(out, "net.rx_bps", Unit::BytesPerSec, rx);
            push(out, "net.tx_bps", Unit::BytesPerSec, tx);
        }
        let net_counts = std::mem::take(&mut self.net_counts);
        let mut errors = f64::NAN; // received and sent errors of every adapter, per second
        for &(i, field) in &net_counts {
            self.q.values(i, &mut self.vals);
            let vals = std::mem::take(&mut self.vals);
            for (inst, v) in &vals {
                let key = format!("net.{}.{field}", inst.replace('.', "_"));
                let r = self.rate(&key, *v, dt);
                if field.ends_with("_errors") && r.is_finite() {
                    errors = if errors.is_nan() { r } else { errors + r };
                }
                if src.per_adapter {
                    push(out, key, Unit::PerSec, r);
                }
            }
            self.vals = vals;
        }
        self.net_counts = net_counts;
        push(out, "net.errors", Unit::PerSec, errors);

        // CPU power from the RAPL energy counters: cumulative picowatt-hours (1 pWh = 3.6e-9 J).
        if let (Some(i), true) = (self.energy, src.power) {
            self.q.raw_values(i, &mut self.raw);
            let raw = std::mem::take(&mut self.raw);
            for (inst, v) in &raw {
                if inst.eq_ignore_ascii_case("_Total") {
                    continue; // the sum of the domains below, which overlap (cores are inside the package)
                }
                let key = power_key(inst);
                let joules_per_s = self.rate(&key, *v as f64, dt) * 3.6e-9;
                push(out, key, Unit::Watts, joules_per_s);
            }
            self.raw = raw;
        } else {
            // Off: no rate may later span the time it was off.
            self.prev_counts.retain(|k, _| !k.starts_with("power."));
        }
    }
}

/// What to record (settings): the rest is always recorded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sources {
    pub per_core: bool,
    pub per_adapter: bool,
    pub gpu: bool,
    pub power: bool,
    pub apps: bool,
}

impl Default for Sources {
    fn default() -> Sources {
        Sources {
            per_core: true,
            per_adapter: true,
            gpu: true,
            power: true,
            apps: true,
        }
    }
}

pub struct Output {
    pub samples: Vec<Sample>,
    pub apps: Option<(i64, Vec<AppRow>)>,
    /// Milliseconds spent on counters, processes and the GPU.
    pub ms: [f64; 3],
}

pub struct Sampler {
    counters: Option<Counters>,
    counters_retry_at: i64,
    pub procs: ProcSampler,
    /// NVML, reopened when it keeps failing (driver update or reset) and retried while missing.
    gpu: Option<Gpu>,
    /// The NVIDIA driver is installed but gives no readings (NVML does not open, or a sample failed).
    gpu_failing: bool,
    gpu_failures: u32,
    gpu_retry_at: i64,
    gpu_reopened: bool,
    own_cpu: Option<u64>,
    last_collect_s: f64,
    src: Sources,
}

impl Sampler {
    pub fn new(src: Sources) -> Sampler {
        let now = win::tick_ms(); // retries run on the monotonic clock
        let counters = Counters::open();
        // NVML only when the GPU is recorded (it costs about 20 MB).
        let gpu = if src.gpu {
            Gpu::open()
        } else {
            Err(NoGpu::Absent)
        };
        Sampler {
            counters_retry_at: now + RETRY_MS,
            counters,
            procs: ProcSampler::new(),
            gpu_retry_at: now + RETRY_MS,
            gpu_failing: gpu.as_ref().err() == Some(&NoGpu::Failing),
            gpu: gpu.ok(),
            gpu_failures: 0,
            gpu_reopened: false,
            own_cpu: None,
            last_collect_s: f64::NAN,
            src,
        }
    }

    /// What to record from now on: the GPU off unloads NVML, back on opens it at the next sample.
    pub fn configure(&mut self, src: Sources) {
        if !src.gpu {
            self.gpu = None;
            self.gpu_failing = false;
            self.gpu_failures = 0;
        } else if !self.src.gpu {
            self.gpu_retry_at = 0;
        }
        self.src = src;
    }

    pub fn has_gpu(&self) -> bool {
        self.gpu.is_some()
    }

    pub fn has_counters(&self) -> bool {
        self.counters.is_some()
    }

    /// Whether an installed NVIDIA driver gives no readings (as opposed to no NVIDIA GPU at all).
    pub fn gpu_failing(&self) -> bool {
        self.gpu_failing
    }

    /// One sample at slot time `t`, `dt` seconds (measured on a monotonic clock) after the previous
    /// read. With `baseline`, rates are not meaningful (first sample, after a sleep or a clock
    /// change): everything is read, so the next sample has a fresh base, but the caller does not
    /// keep it.
    pub fn sample(&mut self, t: i64, dt: f64, baseline: bool) -> Output {
        let mut out = Vec::with_capacity(256);
        let t0 = Instant::now();
        let tick = win::tick_ms();
        let mut reopened = false;
        if self.counters.is_none() && tick >= self.counters_retry_at {
            self.counters_retry_at = tick + RETRY_MS;
            self.counters = Counters::open();
            reopened = self.counters.is_some(); // rates come from the next collection
        }
        if let (Some(c), false) = (self.counters.as_mut(), reopened) {
            c.sample(dt, &mut out, &self.src);
        }
        if let Some(total) = win::memory_total() {
            push(&mut out, "mem.total", Unit::Bytes, total as f64);
        }
        if let Some((size, used)) = win::pagefiles() {
            push(&mut out, "pagefile.size", Unit::Bytes, size as f64);
            push(&mut out, "pagefile.used", Unit::Bytes, used as f64);
        }
        if let Some(boot) = win::boot_time_ms() {
            push(&mut out, "sys.boot", Unit::Timestamp, (boot / 1000) as f64);
        }
        let t1 = Instant::now();
        // The process table is always read (process, thread and handle counts); its minutes of
        // apps are kept only when apps are recorded.
        let apps = self.procs.sample(t, dt, baseline).filter(|_| self.src.apps);
        let tot = &self.procs.totals;
        push(&mut out, "sys.processes", Unit::Count, tot.processes as f64);
        push(&mut out, "sys.threads", Unit::Count, tot.threads as f64);
        push(&mut out, "sys.handles", Unit::Count, tot.handles as f64);
        let t2 = Instant::now();
        if self.src.gpu && self.gpu.is_none() && tick >= self.gpu_retry_at {
            self.gpu_retry_at = tick + RETRY_MS;
            match Gpu::open() {
                Ok(g) => {
                    self.gpu = Some(g);
                    self.gpu_failing = false;
                    log!("GPU readings (NVML) available");
                }
                Err(why) => self.gpu_failing = why == NoGpu::Failing,
            }
        }
        if let Some(g) = &self.gpu {
            if g.sample(&mut out) {
                self.gpu_failures = 0;
                self.gpu_reopened = false;
                self.gpu_failing = false;
            } else {
                self.gpu_failing = true;
                self.gpu_failures += 1;
                if self.gpu_failures >= 6 {
                    // Reopened at once the first time (driver update or reset), then every 5 minutes.
                    log!("GPU readings failing for a minute: reopening NVML");
                    self.gpu = None; // closes NVML
                    self.gpu_failures = 0;
                    self.gpu_retry_at = if self.gpu_reopened {
                        tick + RETRY_MS
                    } else {
                        tick
                    };
                    self.gpu_reopened = true;
                }
            }
        }
        let t3 = Instant::now();

        // This program's own cost, kept in the history like everything else.
        let cpu = win::own_cpu_100ns();
        if let Some(pc) = self.own_cpu {
            if dt > 0.0 {
                push(
                    &mut out,
                    "self.cpu",
                    Unit::Percent,
                    cpu.saturating_sub(pc) as f64 / 1e5 / dt,
                );
            }
        }
        self.own_cpu = Some(cpu);
        let tot = &self.procs.totals;
        push(&mut out, "self.ws", Unit::Bytes, tot.own_ws as f64);
        push(
            &mut out,
            "self.private",
            Unit::Bytes,
            tot.own_private as f64,
        );
        push(&mut out, "self.collect", Unit::Seconds, self.last_collect_s);
        self.last_collect_s = (t3 - t0).as_secs_f64();

        let ms = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1000.0;
        Output {
            samples: out,
            apps,
            ms: [ms(t0, t1), ms(t1, t2), ms(t2, t3)],
        }
    }
}

/// Private bytes of this process after each start-up step (`kymo probe`).
pub fn probe() -> Vec<(&'static str, u64)> {
    let mut procs = ProcSampler::new();
    let mut steps = Vec::new();
    let mut mark = |name, p: &mut ProcSampler| {
        p.sample(units_now(), 10.0, true);
        steps.push((name, p.totals.own_private));
    };
    mark("start (process table buffer)", &mut procs);
    let mut c = Counters::open();
    mark("performance counters opened", &mut procs);
    let mut out = Vec::new();
    if let Some(c) = c.as_mut() {
        c.sample(1.0, &mut out, &Sources::default());
        c.sample(1.0, &mut out, &Sources::default());
    }
    mark("counters read twice", &mut procs);
    let g = Gpu::open().ok();
    mark("NVML loaded", &mut procs);
    if let Some(g) = &g {
        g.sample(&mut out);
    }
    mark("GPU read", &mut procs);
    steps
}

fn units_now() -> i64 {
    crate::units::now_ms()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(cpu_key("_Total").as_deref(), Some("cpu"));
        assert_eq!(cpu_key("0,_Total"), None);
        assert_eq!(cpu_key("0,7").as_deref(), Some("cpu.core.07"));
        assert_eq!(cpu_key("1,3").as_deref(), Some("cpu.core.1_03"));
        assert!(is_volume("C:"));
        assert!(!is_volume("HarddiskVolume1"));
        assert_eq!(power_key("RAPL_Package0_PKG"), "power.package");
    }
}
