//! Reading the history back for the API: series over time, per-app tables and per-app series.
//!
//! A query splits [from, to] into buckets of `step`; bucket i covers (start + i*step,
//! start + (i+1)*step] and gets the average and the maximum of the rows in it. Each row weighs
//! what it stands for (a 1-hour summary of 360 samples weighs 360 samples). Long steps read the
//! summaries, and the end of the range, which the summaries do not cover yet, comes from the finer
//! files. A query never reads more than about 200,000 rows of one tier: longer ranges read a
//! coarser tier.
//!
//! Disk latency is an average per operation: over a bucket it is the sum of the time spent waiting
//! divided by the sum of the operations (as rate(seconds) / rate(operations) gives), and its
//! maximum is the highest 10-second latency.

use crate::store::{each_app_row, each_row, raw_len, Pace, Tier, APP_FIELDS, APP_N, APP_SECONDS};
use crate::units::Unit;
use std::collections::HashMap;
use std::path::Path;

const MAX_ROWS: i64 = 200_000;

pub struct Series {
    pub key: String,
    pub unit: Unit,
    pub avg: Vec<f64>,
    pub max: Vec<f64>,
}

pub struct Grid {
    pub start: i64,
    pub step: i64,
    pub n: usize,
}

impl Grid {
    pub fn new(from: i64, to: i64, step: i64) -> Grid {
        let start = from.div_euclid(step) * step;
        let n = ((to - start + step - 1) / step).max(1) as usize;
        Grid { start, step, n }
    }

    pub fn end(&self) -> i64 {
        self.start + self.n as i64 * self.step
    }

    fn bucket(&self, ts: i64) -> Option<usize> {
        let b = (ts - 1 - self.start).div_euclid(self.step);
        (b >= 0 && (b as usize) < self.n).then_some(b as usize)
    }

    /// The buckets a row of length `len` ending at `ts` overlaps: (ts - len, ts].
    fn buckets(&self, ts: i64, len: i64) -> std::ops::Range<usize> {
        let lo = (ts - len - self.start).div_euclid(self.step).max(0);
        let hi = ((ts - 1 - self.start).div_euclid(self.step) + 1).min(self.n as i64);
        if hi <= lo {
            0..0
        } else {
            lo as usize..hi as usize
        }
    }
}

/// `*` matches any run of characters within one dot-separated part of the key. Linear time.
pub fn glob(pattern: &str, key: &str) -> bool {
    let (mut ps, mut ks) = (pattern.split('.'), key.split('.'));
    loop {
        match (ps.next(), ks.next()) {
            (None, None) => return true,
            (Some(p), Some(k)) => {
                if !part(p.as_bytes(), k.as_bytes()) {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// Wildcard match of one part: the classic two-pointer algorithm (backtracks to the last '*').
fn part(p: &[u8], k: &[u8]) -> bool {
    let (mut i, mut j) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while j < k.len() {
        if i < p.len() && p[i] != b'*' && p[i] == k[j] {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == b'*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(s) = star {
            i = s + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == b'*' {
        i += 1;
    }
    i == p.len()
}

const SYSTEM_CHAIN: [Tier; 3] = [Tier::Hour, Tier::Min, Tier::Raw];
const APP_CHAIN: [Tier; 2] = [Tier::AppHour, Tier::AppMin];

/// Length of the rows a query of this step and range reads mostly (10 s, 1 min or 1 h): the API
/// rounds steps up to a whole number of them, so that no bucket falls between two rows.
pub fn row_len(step: i64, range: i64, apps: bool) -> i64 {
    tiers_for(step, range, if apps { &APP_CHAIN } else { &SYSTEM_CHAIN })[0].len_ms()
}

/// The tiers to read, coarsest first: the one that suits the step (or a coarser one when the
/// range would mean too many rows), then the finer ones for the end of the range.
fn tiers_for(step: i64, range: i64, chain: &[Tier]) -> Vec<Tier> {
    // The coarsest tier whose rows are not longer than the step...
    let mut first = chain
        .iter()
        .position(|t| t.len_ms() <= step)
        .unwrap_or(chain.len() - 1);
    // ...unless the range holds too many of its rows.
    while first > 0 && range / chain[first].len_ms() > MAX_ROWS {
        first -= 1;
    }
    chain[first..].to_vec()
}

struct Acc {
    key: String,
    unit: Unit,
    sum: Vec<f64>,
    w: Vec<f64>,
    max: Vec<f64>,
    hidden: bool,
}

impl Acc {
    fn new(key: String, unit: Unit, n: usize, hidden: bool) -> Acc {
        Acc {
            key,
            unit,
            sum: vec![0.0; n],
            w: vec![0.0; n],
            max: vec![f64::NEG_INFINITY; n],
            hidden,
        }
    }

    fn add(&mut self, b: usize, avg: f64, max: f64, weight: f64) {
        if avg.is_finite() {
            self.sum[b] += avg * weight;
            self.w[b] += weight;
        }
        if max.is_finite() && max > self.max[b] {
            self.max[b] = max;
        }
    }

    fn finish(self) -> Series {
        let avg = self
            .sum
            .iter()
            .zip(&self.w)
            .map(|(s, &w)| if w > 0.0 { s / w } else { f64::NAN })
            .collect();
        let max = self
            .max
            .into_iter()
            .map(|m| if m.is_finite() { m } else { f64::NAN })
            .collect();
        Series {
            key: self.key,
            unit: self.unit,
            avg,
            max,
        }
    }
}

/// For "disk.C:.read_latency": the series it is computed from ("disk.C:.read_time", "disk.C:.reads").
fn latency_parts(key: &str) -> Option<(String, String)> {
    let (prefix, last) = key.rsplit_once('.')?;
    let dir = last.strip_suffix("_latency")?;
    Some((format!("{prefix}.{dir}_time"), format!("{prefix}.{dir}s")))
}

/// For "disk.C:.read_time" or "disk.C:.reads": the latency series that needs it.
fn latency_of(key: &str) -> Option<String> {
    let (prefix, last) = key.rsplit_once('.')?;
    let dir = last
        .strip_suffix("_time")
        .or_else(|| last.strip_suffix('s'))
        .filter(|d| *d == "read" || *d == "write")?;
    Some(format!("{prefix}.{dir}_latency"))
}

/// What a series query found.
pub struct Found {
    pub series: Vec<Series>,
    /// Tiers read ("1h", "1m", "10s").
    pub tiers: Vec<&'static str>,
    /// Matching series left out because of `limit`.
    pub left_out: usize,
}

/// System series whose key matches one of `patterns`; at most `limit` of them (the series that
/// latency is computed from are read as well, without counting).
///
/// Where the tier that suits the step has no rows at the start of the range, the coarser tiers fill
/// that start: history imported from the previous stack is only kept per minute and per hour. Such
/// a row counts in every bucket it overlaps.
pub fn series(root: &Path, grid: &Grid, patterns: &[String], limit: usize) -> Found {
    let wanted = |k: &str| patterns.iter().any(|p| glob(p, k));
    let mut accs: Vec<Acc> = Vec::new();
    let mut visible = 0usize;
    let mut left_out = 0usize;
    let mut index: HashMap<String, Option<usize>> = HashMap::new();
    // A row stands for `len` ms before its time and weighs `weight` (10-second units).
    let mut add_row =
        |tier: Tier, ts: i64, weight: f64, len: i64, cols: &[(String, Unit)], vals: &[f64]| {
            let vpc = tier.values();
            let span = if len > grid.step {
                grid.buckets(ts, len)
            } else {
                grid.bucket(ts).map_or(0..0, |b| b..b + 1)
            };
            if span.is_empty() {
                return;
            }
            for (c, (key, unit)) in cols.iter().enumerate() {
                let slot = match index.get(key) {
                    Some(s) => *s,
                    None => {
                        let shown = wanted(key);
                        let helper = !shown && latency_of(key).is_some_and(|l| wanted(&l));
                        let fits = !shown || visible < limit;
                        if shown && !fits {
                            left_out += 1;
                        }
                        let s = ((shown || helper) && fits).then(|| {
                            visible += shown as usize;
                            accs.push(Acc::new(key.clone(), *unit, grid.n, !shown));
                            accs.len() - 1
                        });
                        index.insert(key.clone(), s);
                        s
                    }
                };
                if let Some(s) = slot {
                    let avg = vals[c * vpc];
                    let max = if vpc == 2 { vals[c * vpc + 1] } else { avg };
                    for b in span.clone() {
                        accs[s].add(b, avg, max, weight);
                    }
                }
            }
        };
    let mut used = Vec::new();
    let mut covered = grid.start;
    let tiers = tiers_for(grid.step, grid.end() - grid.start, &SYSTEM_CHAIN);
    // Where the rows read start: the earliest one's interval begins at `until`.
    let mut until = grid.end();
    for &tier in &tiers {
        let mut last = covered;
        // Samples stand for the time since the one before (the interval may have been another then:
        // 10-second rows still fill a 6-second grid after the interval became 1 s); summaries for
        // their period, weighted by their samples' time.
        let mut pace = Pace::new();
        each_row(root, tier, covered, grid.end(), |ts, count, cols, vals| {
            let (weight, len) = if tier == Tier::Raw {
                let dt = pace.next(ts, raw_len(count));
                (dt as f64 / 10_000.0, dt)
            } else {
                (count.max(1) as f64, tier.len_ms())
            };
            if grid.bucket(ts).is_none() {
                return;
            }
            last = last.max(ts);
            until = until.min(ts - len);
            add_row(tier, ts, weight, len, cols, vals);
        });
        if last > covered {
            used.push(tier.label());
        }
        covered = last;
    }
    // The start of the range from the coarser tiers, finer first, up to the rows already read.
    let first = SYSTEM_CHAIN
        .iter()
        .position(|t| *t == tiers[0])
        .unwrap_or(0);
    for (pos, &tier) in SYSTEM_CHAIN[..first].iter().enumerate().rev() {
        if until <= grid.start {
            break;
        }
        // Only a row with no finer row anywhere in its period: the partial hour written before a
        // sleep would otherwise fill the gap after it with the minutes before. (Finer rows exist
        // only before the range here: in it, they would have been read above.)
        let mut finer_last = i64::MIN;
        for &f in &SYSTEM_CHAIN[pos + 1..] {
            each_row(
                root,
                f,
                grid.start - tier.len_ms(),
                grid.start,
                |ts, _, _, _| finer_last = finer_last.max(ts),
            );
        }
        let mut first_row = i64::MAX;
        each_row(root, tier, grid.start, until, |ts, count, cols, vals| {
            if ts - tier.len_ms() < finer_last {
                return;
            }
            first_row = first_row.min(ts);
            add_row(tier, ts, count.max(1) as f64, tier.len_ms(), cols, vals);
        });
        if first_row < i64::MAX {
            used.push(tier.label());
            until = first_row - tier.len_ms();
        }
    }
    // Latency per operation: summed waiting time over summed operations.
    let by_key: HashMap<String, usize> = accs
        .iter()
        .enumerate()
        .map(|(i, a)| (a.key.clone(), i))
        .collect();
    let mut latency: Vec<(usize, Vec<f64>)> = Vec::new();
    for (i, a) in accs.iter().enumerate() {
        if a.hidden {
            continue;
        }
        if let Some((time, ops)) = latency_parts(&a.key) {
            if let (Some(&t), Some(&o)) = (by_key.get(&time), by_key.get(&ops)) {
                let v = (0..grid.n)
                    .map(|b| {
                        if accs[o].sum[b] > 0.0 {
                            accs[t].sum[b] / accs[o].sum[b]
                        } else {
                            f64::NAN
                        }
                    })
                    .collect();
                latency.push((i, v));
            }
        }
    }
    let mut out: Vec<Series> = Vec::new();
    let mut fixed: HashMap<usize, Vec<f64>> = latency.into_iter().collect();
    for (i, a) in accs.into_iter().enumerate() {
        if a.hidden {
            continue;
        }
        let mut s = a.finish();
        if let Some(v) = fixed.remove(&i) {
            s.avg = v;
        }
        out.push(s);
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Found {
        series: out,
        tiers: used,
        left_out,
    }
}

pub struct AppSummary {
    pub name: String,
    pub cpu_avg: f64,
    pub cpu_max: f64,
    pub ws_max: f64,
    pub private_max: f64,
    pub read_bytes: f64,
    pub write_bytes: f64,
    pub instances_max: f64,
}

/// Every app seen in (from, to]: average CPU over the whole range (as the dashboards' "increase
/// over the range"), peak CPU of a minute (or hour, for long ranges), peak memory and commit, data
/// read and written, most instances. Also returns the minutes of data in the range.
pub fn apps(root: &Path, from: i64, to: i64) -> (Vec<AppSummary>, f64) {
    let mut by_name: HashMap<String, AppSummary> = HashMap::new();
    let mut seconds = 0.0;
    let mut covered = from;
    // Long ranges read hourly rows, but only whole hours inside the range: minutes before the
    // first hour boundary, hours, then the minutes after the last hourly row.
    let hour = 3_600_000;
    let first_hour = ((from + hour - 1).div_euclid(hour) * hour).min(to);
    let passes: Vec<(Tier, Option<i64>)> = if to - from > 2 * 86_400_000 {
        vec![
            (Tier::AppMin, Some(first_hour)),
            (Tier::AppHour, None),
            (Tier::AppMin, None),
        ]
    } else {
        vec![(Tier::AppMin, None)]
    };
    for (tier, until) in passes {
        let hourly = tier == Tier::AppHour;
        let mut last = covered;
        let hi = until.unwrap_or(to);
        each_app_row(root, tier, covered, hi, |ts, _, names, rows| {
            last = last.max(ts);
            let secs = rows
                .iter()
                .map(|(_, v)| v[APP_SECONDS])
                .find(|s| s.is_finite())
                .unwrap_or(0.0);
            seconds += secs;
            for (id, v) in rows {
                let max = |i: usize| if hourly { v[APP_N + i] } else { v[i] };
                let e = by_name
                    .entry(names[*id].clone())
                    .or_insert_with(|| AppSummary {
                        name: names[*id].clone(),
                        cpu_avg: 0.0,
                        cpu_max: 0.0,
                        ws_max: 0.0,
                        private_max: 0.0,
                        read_bytes: 0.0,
                        write_bytes: 0.0,
                        instances_max: 0.0,
                    });
                let nz = |x: f64| if x.is_finite() { x } else { 0.0 };
                e.cpu_avg += nz(v[0]) * secs;
                e.cpu_max = e.cpu_max.max(nz(max(0)));
                e.read_bytes += nz(v[3]) * secs;
                e.write_bytes += nz(v[4]) * secs;
                e.ws_max = e.ws_max.max(nz(max(5)));
                e.private_max = e.private_max.max(nz(max(6)));
                e.instances_max = e.instances_max.max(nz(max(7)));
            }
        });
        // The hourly pass starts at the hour boundary even when no minute row came before it.
        covered = if until.is_some() { hi.max(last) } else { last };
    }
    let range_s = (to - from) as f64 / 1000.0;
    let mut out: Vec<AppSummary> = by_name.into_values().collect();
    for a in &mut out {
        a.cpu_avg /= range_s;
    }
    (out, seconds / 60.0)
}

/// Index of an app field by name ("cpu", "ws", ...), or the names "mem" (= ws) and "commit" (= private).
pub fn app_field(name: &str) -> Option<usize> {
    let name = match name {
        "mem" => "ws",
        "commit" => "private",
        n => n,
    };
    APP_FIELDS
        .iter()
        .position(|(n, _)| *n == name)
        .filter(|&i| i != APP_SECONDS)
}

/// Name of the series that sums every app not asked for (`others`).
pub const OTHERS: &str = "all other apps";

/// One field of the named apps over time, weighted by the seconds of data; time without the app
/// counts as 0 in its averages. With `others`, one more series adds up every other app.
pub fn app_series(
    root: &Path,
    grid: &Grid,
    names: &[String],
    field: usize,
    others: bool,
) -> Vec<Series> {
    let unit = APP_FIELDS[field].1;
    let mut accs: Vec<Acc> = names
        .iter()
        .map(|n| Acc::new(n.clone(), unit, grid.n, false))
        .collect();
    if others {
        accs.push(Acc::new(OTHERS.to_string(), unit, grid.n, false));
    }
    let rest = others.then_some(names.len());
    // A name with no row at all in the range (mistyped, or not running then) gets no data, not zeros.
    let mut seen = vec![false; accs.len()];
    let mut bucket_secs = vec![0.0; grid.n];
    let mut covered = grid.start;
    for tier in tiers_for(grid.step, grid.end() - grid.start, &APP_CHAIN) {
        let hourly = tier == Tier::AppHour;
        let mut last = covered;
        // Name ids are per file: map them to the wanted names once per file.
        let (mut file, mut wanted): (usize, Vec<Option<usize>>) = (usize::MAX, Vec::new());
        each_app_row(root, tier, covered, grid.end(), |ts, file_no, all, rows| {
            let Some(b) = grid.bucket(ts) else { return };
            last = last.max(ts);
            if file_no != file || wanted.len() < all.len() {
                file = file_no;
                wanted = all
                    .iter()
                    .map(|n| names.iter().position(|x| x == n).or(rest))
                    .collect();
            }
            let secs = rows
                .iter()
                .map(|(_, v)| v[APP_SECONDS])
                .find(|s| s.is_finite())
                .unwrap_or(0.0);
            bucket_secs[b] += secs;
            let mut rest_max = 0.0f64;
            for (id, v) in rows {
                if let Some(Some(i)) = wanted.get(*id) {
                    seen[*i] = true;
                    if v[field].is_finite() {
                        accs[*i].sum[b] += v[field] * secs;
                    }
                    let max = if hourly { v[APP_N + field] } else { v[field] };
                    if Some(*i) == rest {
                        // The rest's peak: the sum of the other apps' values in this row.
                        rest_max += if v[field].is_finite() { v[field] } else { 0.0 };
                    } else if max.is_finite() && max > accs[*i].max[b] {
                        accs[*i].max[b] = max;
                    }
                }
            }
            if let Some(r) = rest {
                accs[r].max[b] = accs[r].max[b].max(rest_max);
            }
        });
        covered = last;
    }
    for (k, a) in accs.iter_mut().enumerate() {
        if !seen[k] {
            continue; // weight stays 0: no data
        }
        for (b, &secs) in bucket_secs.iter().enumerate().take(grid.n) {
            a.w[b] = secs;
            if secs > 0.0 && !a.max[b].is_finite() {
                a.max[b] = 0.0;
            }
        }
    }
    accs.into_iter().map(Acc::finish).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob("cpu.busy", "cpu.busy"));
        assert!(glob("cpu.core.*.busy", "cpu.core.07.busy"));
        assert!(!glob("cpu.core.*.busy", "cpu.core.07.utility"));
        assert!(!glob("cpu.*", "cpu.core.07.busy"));
        assert!(glob("disk.*.active", "disk.C:.active"));
        assert!(glob("*", "self"));
        assert!(glob(
            "net.Intel*I225-V.rx_bps",
            "net.Intel[R] Ethernet Controller [3] I225-V.rx_bps"
        ));
        assert!(glob("c*u.b*y", "cpu.busy"));
        assert!(!glob("cpu", "cpu.busy"));
        // Many stars stay fast (no backtracking blow-up).
        let start = std::time::Instant::now();
        assert!(!glob(
            &format!("net.{}Q", "*".repeat(5000)),
            &format!("net.{}", "x".repeat(5000))
        ));
        assert!(start.elapsed().as_millis() < 2000);
    }

    #[test]
    fn grid() {
        let g = Grid::new(60_000, 120_000, 10_000);
        assert_eq!(g.n, 6);
        assert_eq!(g.bucket(60_001), Some(0));
        assert_eq!(g.bucket(70_000), Some(0));
        assert_eq!(g.bucket(70_001), Some(1));
        assert_eq!(g.bucket(60_000), None);
    }

    #[test]
    fn tiers() {
        let chain = [Tier::Hour, Tier::Min, Tier::Raw];
        assert_eq!(tiers_for(10_000, 3_600_000, &chain), vec![Tier::Raw]);
        assert_eq!(
            tiers_for(300_000, 86_400_000, &chain),
            vec![Tier::Min, Tier::Raw]
        );
        assert_eq!(
            tiers_for(3_600_000, 86_400_000, &chain),
            vec![Tier::Hour, Tier::Min, Tier::Raw]
        );
        // 300 days at a step just under an hour: too many minute rows, so hours.
        assert_eq!(
            tiers_for(3_599_000, 300 * 86_400_000, &chain),
            vec![Tier::Hour, Tier::Min, Tier::Raw]
        );
        // Steps are rounded to whole rows: app rows are per minute; long ranges read hours.
        assert_eq!(row_len(12_000, 3_600_000, true), 60_000);
        assert_eq!(row_len(12_000, 3_600_000, false), 10_000);
        assert_eq!(row_len(1_800_000, 200 * 86_400_000, false), 3_600_000);
        assert_eq!(row_len(840_000, 3 * 86_400_000, true), 60_000);
    }

    #[test]
    fn coarser_tiers_fill_the_start() {
        use crate::store::{Point, RowWriter};
        let root = std::env::temp_dir().join(format!("kymo-test-fallback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let t0 = 1_792_454_400_000; // 2026-10-20 00:00 UTC
        let row = |w: &mut RowWriter, ts: i64, count: u32, v: f64| {
            w.write(
                ts,
                count,
                &[Point {
                    key: "cpu.busy",
                    unit: Unit::Percent,
                    avg: v,
                    max: v + 1.0,
                }],
            )
            .unwrap();
        };
        {
            // Minutes 1-30 only per minute (imported), then 10-second samples from minute 30 on, and
            // their minutes as the recorder writes them too (at 99 %, which must not count twice).
            let mut min = RowWriter::open(&root, Tier::Min, "2026-10-20").unwrap();
            for m in 1..=30 {
                row(&mut min, t0 + m * 60_000, 6, m as f64);
            }
            for m in 31..=40 {
                row(&mut min, t0 + m * 60_000, 6, 99.0);
            }
            let mut raw = RowWriter::open(&root, Tier::Raw, "2026-10-20").unwrap();
            for i in 1..=60 {
                row(&mut raw, t0 + 30 * 60_000 + i * 10_000, 1, 50.0);
            }
        }
        // 20 to 35 minutes, per 10 s: the imported minutes spread over their six buckets, then the samples.
        let g = Grid::new(t0 + 20 * 60_000, t0 + 35 * 60_000, 10_000);
        let found = series(&root, &g, &["cpu.busy".to_string()], 10);
        let avg = &found.series[0].avg;
        assert_eq!(avg.len(), 90);
        assert_eq!(&avg[..6], &[21.0; 6]); // minute 21
        assert_eq!(avg[59], 30.0); // the last imported minute
        assert!(avg[60..].iter().all(|&v| v == 50.0), "{:?}", &avg[60..]);
        assert_eq!(found.tiers, vec!["10s", "1m"]);
        // A range with samples from its start reads no minute rows.
        let g = Grid::new(t0 + 31 * 60_000, t0 + 35 * 60_000, 10_000);
        assert_eq!(
            series(&root, &g, &["cpu.busy".to_string()], 10).tiers,
            vec!["10s"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn samples_at_a_slower_pace_than_the_step_fill_every_bucket() {
        use crate::store::{Point, RowWriter};
        let root = std::env::temp_dir().join(format!("kymo-test-pace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let t0 = 1_792_454_400_000; // 2026-10-20 00:00 UTC
        {
            // Samples every 20 s (an interval of 20 s at the time), then a 2-minute gap, then more.
            let mut raw = RowWriter::open(&root, Tier::Raw, "2026-10-20").unwrap();
            for i in 1..=30i64 {
                raw.write(
                    t0 + i * 20_000,
                    1,
                    &[Point {
                        key: "cpu.busy",
                        unit: Unit::Percent,
                        avg: 40.0,
                        max: 40.0,
                    }],
                )
                .unwrap();
            }
            for i in 1..=6i64 {
                raw.write(
                    t0 + 720_000 + i * 20_000,
                    1,
                    &[Point {
                        key: "cpu.busy",
                        unit: Unit::Percent,
                        avg: 40.0,
                        max: 40.0,
                    }],
                )
                .unwrap();
            }
        }
        // Per 10 s: the 20-second samples fill both buckets of their time (all but the first, which
        // has no sample before it to tell its pace), the gap stays empty.
        let g = Grid::new(t0, t0 + 840_000, 10_000);
        let avg = &series(&root, &g, &["cpu.busy".to_string()], 10).series[0].avg;
        assert!(avg[1..60].iter().all(|&v| v == 40.0), "{:?}", &avg[..60]);
        assert!(
            avg[60..72].iter().all(|v| v.is_nan()),
            "the gap: {:?}",
            &avg[60..72]
        );
        assert!(avg[72..].iter().filter(|v| !v.is_nan()).count() >= 10);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_partial_hour_before_a_sleep_leaves_the_gap_empty() {
        use crate::store::{Point, RowWriter};
        let root = std::env::temp_dir().join(format!("kymo-test-sleep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let t0 = 1_792_454_400_000; // 2026-10-20 00:00 UTC
        let point = |v: f64| {
            [Point {
                key: "cpu.busy",
                unit: Unit::Percent,
                avg: v,
                max: v,
            }]
        };
        {
            // Recording 12:00-12:10, asleep until 15:20, then recording again: the recorder's rows.
            let (mut raw, mut min, mut hour) = (
                RowWriter::open(&root, Tier::Raw, "2026-10-20").unwrap(),
                RowWriter::open(&root, Tier::Min, "2026-10-20").unwrap(),
                RowWriter::open(&root, Tier::Hour, "2026-10").unwrap(),
            );
            let h12 = t0 + 12 * 3_600_000;
            for i in 1..=60 {
                raw.write(h12 + i * 10_000, 1, &point(30.0)).unwrap();
            }
            for m in 1..=10 {
                min.write(h12 + m * 60_000, 6, &point(30.0)).unwrap();
            }
            hour.write(h12 + 3_600_000, 60, &point(30.0)).unwrap(); // 12:00-13:00, only its first 10 minutes
            let back = t0 + 15 * 3_600_000 + 20 * 60_000;
            for i in 1..=60 {
                raw.write(back + i * 10_000, 1, &point(70.0)).unwrap();
            }
        }
        // 12:30-15:30 per 30 s: nothing until 15:20.
        let g = Grid::new(
            t0 + 12 * 3_600_000 + 30 * 60_000,
            t0 + 15 * 3_600_000 + 30 * 60_000,
            30_000,
        );
        let found = series(&root, &g, &["cpu.busy".to_string()], 10);
        let avg = &found.series[0].avg;
        let gap = (15 * 60 + 20 - (12 * 60 + 30)) * 2;
        assert!(
            avg[..gap].iter().all(|v| v.is_nan()),
            "the sleep shows no values"
        );
        assert!(avg[gap..].iter().all(|&v| v == 70.0));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn latency_names() {
        assert_eq!(
            latency_parts("disk.C:.read_latency"),
            Some(("disk.C:.read_time".into(), "disk.C:.reads".into()))
        );
        assert_eq!(
            latency_of("disk.C:.writes").as_deref(),
            Some("disk.C:.write_latency")
        );
        assert_eq!(
            latency_of("disk.C:.read_time").as_deref(),
            Some("disk.C:.read_latency")
        );
        assert_eq!(latency_of("disk.C:.queue"), None);
    }
}
