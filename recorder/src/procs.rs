//! Every process, read in one call (NtQuerySystemInformation, SystemProcessInformation): CPU time,
//! private working set, private bytes (commit) and I/O bytes. No admin rights are needed.
//!
//! History is kept per app name and minute, never per process ID (IDs churn by thousands a day).
//! CPU and I/O are counted per process between samples and added up per name, so a process that
//! starts during the minute counts from its start; memory is each process's last value in the minute,
//! summed per name. These are the rules of the current stack's stream aggregation.

use crate::win::{self, read};
use std::collections::HashMap;
use std::ffi::c_void;

/// One app (all processes of one name) over one minute.
pub struct AppRow {
    pub name: String,
    /// cpu, user, kernel (% of one core), read, write (bytes/s), ws (private working set, bytes),
    /// private (private bytes = commit, bytes), instances, seconds of the minute with data.
    pub v: [f64; 9],
}

#[derive(Default)]
pub struct Totals {
    pub processes: u32,
    pub threads: u64,
    pub handles: u64,
    pub own_ws: u64,
    pub own_private: u64,
}

struct Prev {
    counters: [u64; 4], // user, kernel (100 ns), read, write (bytes)
    seen: u64,
}

struct Minute {
    end: i64,
    covered_s: f64,
    counters: HashMap<String, [u64; 4]>,
    memory: HashMap<(u32, i64), (String, u64, u64)>, // (pid, creation) -> name, private ws, private bytes
}

impl Minute {
    fn new(end: i64) -> Minute {
        Minute {
            end,
            covered_s: 0.0,
            counters: HashMap::new(),
            memory: HashMap::new(),
        }
    }

    fn rows(&self) -> Vec<AppRow> {
        if self.covered_s <= 0.0 {
            return Vec::new();
        }
        let mut mem: HashMap<&str, (u64, u64, u32)> = HashMap::new();
        for (name, ws, private) in self.memory.values() {
            let e = mem.entry(name.as_str()).or_insert((0, 0, 0));
            e.0 += ws;
            e.1 += private;
            e.2 += 1;
        }
        let mut names: Vec<&str> = self.counters.keys().map(|s| s.as_str()).collect();
        names.extend(
            mem.keys()
                .copied()
                .filter(|n| !self.counters.contains_key(*n)),
        );
        let pct = 100.0 / 1e7 / self.covered_s;
        names
            .into_iter()
            .map(|name| {
                let c = self.counters.get(name).copied().unwrap_or_default();
                let m = mem.get(name).copied().unwrap_or_default();
                AppRow {
                    name: name.to_string(),
                    v: [
                        (c[0] + c[1]) as f64 * pct,
                        c[0] as f64 * pct,
                        c[1] as f64 * pct,
                        c[2] as f64 / self.covered_s,
                        c[3] as f64 / self.covered_s,
                        m.0 as f64,
                        m.1 as f64,
                        m.2 as f64,
                        self.covered_s,
                    ],
                }
            })
            .collect()
    }
}

pub struct ProcSampler {
    buf: Vec<u64>,
    prev: HashMap<(u32, i64), Prev>,
    generation: u64,
    prev_t: Option<i64>,
    minute: Option<Minute>,
    own_pid: u32,
    pub totals: Totals,
}

/// End of the minute a sample at `t` belongs to: a sample describes the interval that ends at `t`.
fn minute_end(t: i64) -> i64 {
    ((t - 1).div_euclid(60_000) + 1) * 60_000
}

fn app_name(raw: &str) -> String {
    let n = raw.len();
    if n > 4 && raw.is_char_boundary(n - 4) && raw[n - 4..].eq_ignore_ascii_case(".exe") {
        raw[..n - 4].to_string()
    } else {
        raw.to_string()
    }
}

impl ProcSampler {
    pub fn new() -> ProcSampler {
        ProcSampler {
            buf: vec![0; 256 * 1024],
            prev: HashMap::new(),
            generation: 0,
            prev_t: None,
            minute: None,
            own_pid: unsafe { win::GetCurrentProcessId() },
            totals: Totals::default(),
        }
    }

    fn query(&mut self) -> Option<usize> {
        for _ in 0..8 {
            let mut ret = 0u32;
            let rc = unsafe {
                win::NtQuerySystemInformation(
                    5,
                    self.buf.as_mut_ptr() as *mut c_void,
                    (self.buf.len() * 8) as u32,
                    &mut ret,
                )
            };
            if rc == win::STATUS_INFO_LENGTH_MISMATCH {
                self.buf
                    .resize((ret as usize).max(self.buf.len() * 8) / 8 + 32 * 1024, 0);
                continue;
            }
            return (rc >= 0).then_some(ret as usize);
        }
        None
    }

    /// Reads every process at slot time `t`, `dt_s` seconds (measured) after the previous read. With
    /// `baseline` (first sample, or after a sleep or a clock change) counters are only remembered.
    /// Returns the minute that ended, if any.
    pub fn sample(&mut self, t: i64, dt_s: f64, baseline: bool) -> Option<(i64, Vec<AppRow>)> {
        let end = minute_end(t);
        let mut done = None;
        if self.minute.as_ref().is_some_and(|m| m.end != end) {
            done = self
                .minute
                .take()
                .map(|m| (m.end, m.rows()))
                .filter(|(_, rows)| !rows.is_empty());
        }
        let Some(len) = self.query() else {
            // Counters skipped a read: the next one starts afresh rather than count two intervals.
            self.prev_t = None;
            return done;
        };
        self.generation += 1;
        let gen = self.generation;
        let since = self.prev_t.map(win::unix_ms_to_filetime);
        let counting = !baseline && self.prev_t.is_some();
        let minute = self.minute.get_or_insert_with(|| Minute::new(end));
        let mut totals = Totals::default();

        let base = self.buf.as_ptr() as *const u8;
        let mut off = 0usize;
        while off + 0x100 <= len {
            let next: u32 = read(base, off);
            let threads: u32 = read(base, off + 0x04);
            let ws: i64 = read(base, off + 0x08);
            let created: i64 = read(base, off + 0x20);
            let user: i64 = read(base, off + 0x28);
            let kernel: i64 = read(base, off + 0x30);
            let name_len: u16 = read(base, off + 0x38);
            let name_ptr: usize = read(base, off + 0x40);
            let pid: usize = read(base, off + 0x50);
            let handles: u32 = read(base, off + 0x60);
            let private: usize = read(base, off + 0xC8);
            let read_bytes: i64 = read(base, off + 0xE8);
            let write_bytes: i64 = read(base, off + 0xF0);
            let pid = pid as u32;

            // The Idle process counts in the totals (as in \System\Processes and \System\Threads),
            // not among the apps.
            totals.processes += 1;
            totals.threads += threads as u64;
            if pid != 0 {
                totals.handles += handles as u64;
                if pid == self.own_pid {
                    totals.own_ws = ws as u64;
                    totals.own_private = private as u64;
                }
                let raw = if name_ptr == 0 || name_len == 0 {
                    String::new()
                } else {
                    // The name lies inside the same buffer.
                    let s = unsafe {
                        std::slice::from_raw_parts(name_ptr as *const u16, name_len as usize / 2)
                    };
                    String::from_utf16_lossy(s)
                };
                let name = app_name(&raw);
                let cur = [
                    user as u64,
                    kernel as u64,
                    read_bytes as u64,
                    write_bytes as u64,
                ];
                let key = (pid, created);
                let delta = match self.prev.get_mut(&key) {
                    Some(p) => {
                        let d = [
                            cur[0].saturating_sub(p.counters[0]),
                            cur[1].saturating_sub(p.counters[1]),
                            cur[2].saturating_sub(p.counters[2]),
                            cur[3].saturating_sub(p.counters[3]),
                        ];
                        p.counters = cur;
                        p.seen = gen;
                        Some(d)
                    }
                    None => {
                        self.prev.insert(
                            key,
                            Prev {
                                counters: cur,
                                seen: gen,
                            },
                        );
                        // Started since the previous sample: all of its counters belong to this interval.
                        since.filter(|&s| created >= s).map(|_| cur)
                    }
                };
                if counting && !name.is_empty() {
                    if let Some(d) = delta {
                        let c = minute.counters.entry(name.clone()).or_default();
                        for k in 0..4 {
                            c[k] += d[k];
                        }
                    }
                    minute
                        .memory
                        .insert(key, (name, ws.max(0) as u64, private as u64));
                }
            }
            if next == 0 {
                break;
            }
            off += next as usize;
        }
        self.prev.retain(|_, p| p.seen == gen);
        if counting {
            minute.covered_s += dt_s;
        }
        self.prev_t = Some(t);
        self.totals = totals;
        done
    }

    /// The minute in progress so far (for `snapshot`).
    pub fn peek(&self) -> Vec<AppRow> {
        self.minute.as_ref().map(|m| m.rows()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(app_name("chrome.exe"), "chrome");
        assert_eq!(app_name("MsMpEng.EXE"), "MsMpEng");
        assert_eq!(app_name("Registry"), "Registry");
        assert_eq!(app_name(".exe"), ".exe");
    }

    #[test]
    fn minutes() {
        assert_eq!(minute_end(60_000), 60_000);
        assert_eq!(minute_end(60_001), 120_000);
        assert_eq!(minute_end(10_000), 60_000);
    }

    #[test]
    fn reads_this_process() {
        // All in the minute (1 020 000, 1 080 000].
        let mut p = ProcSampler::new();
        p.sample(1_030_000, 10.0, true);
        assert!(p.totals.processes > 10);
        assert!(p.totals.own_private > 0);
        // Busy for 100 ms: CPU time is counted in clock ticks of about 15.6 ms.
        let start = std::time::Instant::now();
        let mut x = 0u64;
        while start.elapsed().as_millis() < 100 {
            x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(1));
        }
        assert!(p.sample(1_040_000, 10.0, false).is_none());
        let rows = p.peek();
        assert!(rows.iter().any(|r| r.v[0] > 0.0));
        assert!(rows.iter().all(|r| r.v[8] == 10.0));
        // A baseline inside the same minute keeps the minute (no second row for it).
        assert!(p.sample(1_050_000, 10.0, true).is_none());
        assert!(p.sample(1_060_000, 10.0, false).is_none());
        assert!(p.peek().iter().all(|r| r.v[8] == 20.0));
        let (end, rows) = p.sample(1_090_000, 10.0, false).expect("the minute ended");
        assert_eq!(end, 1_080_000);
        assert!(rows.iter().all(|r| r.v[8] == 20.0));
    }
}
