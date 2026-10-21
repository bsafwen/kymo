//! `kymo import --data DIR --file F [--replace]`: adds history exported from the previous
//! stack (import_vm.py) as 1-minute and 1-hour rows, system and apps. The recorder must be stopped.
//!
//! Every file it changes is written again whole, rows in time order, and renamed into place; the
//! files as they were go to a folder of each run's own next to the history
//! (`<history>.before-import-<time>`). Rows at a time the history already has are kept and the
//! file's row skipped, so running it again adds nothing; with --replace, the history's rows inside
//! the file's time span (per tier) give way to the file's instead (to redo an import). An hour that
//! holds minutes of both, the file's and the history's own (the hour the recorder started in), gets
//! its hourly row made again from all its minutes, so no minute is left out or counted twice. Hours
//! not yet over are left until the import is run again after them.
//!
//! The file is UTF-8 text, one row per line, fields separated by tabs; "nan" for a missing value:
//!   S <tier 1m|1h> <time ms> <samples> <key> <unit code> <average> <maximum>
//!   A <tier apps|apps-1h> <time ms> <name> <9 averages> [<9 maxima>]   (store::APP_FIELDS order)
//! Lines starting with # are comments.

use crate::store::{
    each_app_row, each_row, AppRollup, AppValues, AppWriter, Point, RowWriter, Tier, APP_N,
    APP_SECONDS,
};
use crate::units::{self, Unit};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

const HOUR: i64 = 3_600_000;

fn tier(name: &str) -> Option<Tier> {
    [Tier::Min, Tier::Hour, Tier::AppMin, Tier::AppHour]
        .into_iter()
        .find(|t| t.dir() == name)
}

fn num(s: &str) -> Option<f64> {
    if s.eq_ignore_ascii_case("nan") {
        Some(f64::NAN)
    } else {
        s.parse::<f64>().ok()
    }
}

/// A system row: samples it stands for, then (key, unit, average, maximum).
type SysRow = (u32, Vec<(String, Unit, f64, f64)>);
/// Rows by time and, for rows at the same time (written after the clock went back), file order: all
/// of them are kept.
type Rows<R> = BTreeMap<(i64, u32), R>;

/// The rows at time `ts`.
fn at<R>(rows: &Rows<R>, ts: i64) -> impl DoubleEndedIterator<Item = &R> {
    rows.range((ts, 0)..=(ts, u32::MAX)).map(|(_, r)| r)
}

/// Adds a row after any at the same time.
fn put<R>(rows: &mut Rows<R>, ts: i64, r: R) {
    let n = at(rows, ts).count() as u32;
    rows.insert((ts, n), r);
}

/// The rows timed lo..=hi, with their times.
fn within<R>(rows: &Rows<R>, lo: i64, hi: i64) -> Vec<(&i64, &R)> {
    rows.range((lo, 0)..=(hi, u32::MAX))
        .map(|((t, _), r)| (t, r))
        .collect()
}

fn hour_end(ts: i64) -> i64 {
    (ts - 1).div_euclid(HOUR) * HOUR + HOUR
}

/// The rows of one file of the history: its own period (lo, hi] only, so the row at lo, which
/// belongs to the file before, is not read into it.
fn read_sys(root: &Path, tier: Tier, period: &str) -> Rows<SysRow> {
    let mut out = BTreeMap::new();
    let Some((lo, hi)) = units::period_range(period) else {
        return out;
    };
    let vpc = tier.values();
    each_row(root, tier, lo, hi, |ts, count, cols, vals| {
        let pts = cols
            .iter()
            .enumerate()
            .map(|(c, (k, u))| {
                (
                    k.clone(),
                    *u,
                    vals[c * vpc],
                    if vpc == 2 {
                        vals[c * vpc + 1]
                    } else {
                        vals[c * vpc]
                    },
                )
            })
            .filter(|p| p.2.is_finite() || p.3.is_finite())
            .collect();
        put(&mut out, ts, (count, pts));
    });
    out
}

fn read_apps(root: &Path, tier: Tier, period: &str) -> Rows<Vec<AppValues>> {
    let mut out = BTreeMap::new();
    let Some((lo, hi)) = units::period_range(period) else {
        return out;
    };
    each_app_row(root, tier, lo, hi, |ts, _, names, rows| {
        let list = rows
            .iter()
            .map(|(id, v)| AppValues {
                name: names[*id].clone(),
                v: *v,
            })
            .collect();
        put(&mut out, ts, list);
    });
    out
}

/// One hourly row from minute rows: averages weighted by samples, maxima, samples added up.
fn roll_sys<'a>(minutes: impl Iterator<Item = &'a SysRow>) -> SysRow {
    let mut count = 0u32;
    let mut acc: Vec<(String, Unit, f64, f64, f64)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for (n, pts) in minutes {
        count += n;
        let w = (*n).max(1) as f64;
        for (k, u, avg, max) in pts {
            let i = *index.entry(k.clone()).or_insert_with(|| {
                acc.push((k.clone(), *u, 0.0, 0.0, f64::NEG_INFINITY));
                acc.len() - 1
            });
            if avg.is_finite() {
                acc[i].2 += avg * w;
                acc[i].3 += w;
            }
            let m = if max.is_finite() { *max } else { *avg };
            if m.is_finite() && m > acc[i].4 {
                acc[i].4 = m;
            }
        }
    }
    let pts = acc
        .into_iter()
        .map(|(k, u, sum, w, max)| {
            (
                k,
                u,
                if w > 0.0 { sum / w } else { f64::NAN },
                if max.is_finite() { max } else { f64::NAN },
            )
        })
        .collect();
    (count, pts)
}

fn roll_apps<'a>(
    minutes: impl Iterator<Item = (&'a i64, &'a Vec<AppValues>)>,
) -> Option<Vec<AppValues>> {
    let mut r = AppRollup::new();
    for (ts, rows) in minutes {
        let list: Vec<(&str, &[f64])> = rows.iter().map(|a| (a.name.as_str(), &a.v[..])).collect();
        r.add(*ts, &list);
    }
    r.take().map(|(_, rows)| rows)
}

/// Where the files as they were go (a folder of this run's own), and where new ones are written first.
struct Swap {
    root: PathBuf,
    staging: PathBuf,
    backup: PathBuf,
    files: usize,
    backed_up: usize,
}

impl Swap {
    fn new(root: &Path) -> Swap {
        let name = root
            .file_name()
            .map_or_else(|| "history".into(), |n| n.to_string_lossy().into_owned());
        let now = units::now_ms();
        let s = now.div_euclid(1000).rem_euclid(86_400);
        let stamp = format!(
            "{}-{:02}{:02}{:02}",
            units::day_name(now),
            s / 3600,
            s / 60 % 60,
            s % 60
        );
        // A folder no earlier run uses (two runs can start within the same second).
        let mut backup = root.with_file_name(format!("{name}.before-import-{stamp}"));
        for n in 2.. {
            if !backup.exists() {
                break;
            }
            backup = root.with_file_name(format!("{name}.before-import-{stamp}-{n}"));
        }
        Swap {
            root: root.to_path_buf(),
            staging: root.join(".import-staging"),
            backup,
            files: 0,
            backed_up: 0,
        }
    }

    /// Puts the staged file of (tier, period) in place. The file as it was is linked (or copied)
    /// into the backup folder first, and the staged one then replaces it in one step, so the history
    /// never lacks the file, whatever fails. A file this run already replaced once is not kept
    /// twice: the first copy there is the original.
    fn commit(&mut self, tier: Tier, period: &str) -> std::io::Result<()> {
        let rel = Path::new(tier.dir()).join(format!("{period}.phr"));
        let (new, cur, old) = (
            self.staging.join(&rel),
            self.root.join(&rel),
            self.backup.join(&rel),
        );
        if cur.exists() && !old.exists() {
            fs::create_dir_all(old.parent().unwrap_or(&self.backup))?;
            if fs::hard_link(&cur, &old).is_err() {
                fs::copy(&cur, &old)?;
            }
            self.backed_up += 1;
        }
        fs::create_dir_all(cur.parent().unwrap_or(&self.root))?;
        fs::rename(&new, &cur)?; // replaces the file as it was
        self.files += 1;
        Ok(())
    }

    fn write_sys(&mut self, tier: Tier, period: &str, rows: &Rows<SysRow>) -> std::io::Result<()> {
        let _ = fs::remove_file(self.staging.join(tier.dir()).join(format!("{period}.phr")));
        {
            let mut w = RowWriter::open(&self.staging, tier, period)?;
            for ((ts, _), (count, pts)) in rows {
                let points: Vec<Point> = pts
                    .iter()
                    .map(|(k, u, a, m)| Point {
                        key: k,
                        unit: *u,
                        avg: *a,
                        max: *m,
                    })
                    .collect();
                w.write(*ts, *count, &points)?;
            }
        }
        self.commit(tier, period)
    }

    fn write_apps(
        &mut self,
        tier: Tier,
        period: &str,
        rows: &Rows<Vec<AppValues>>,
    ) -> std::io::Result<()> {
        let _ = fs::remove_file(self.staging.join(tier.dir()).join(format!("{period}.phr")));
        {
            let mut w = AppWriter::open(&self.staging, tier, period)?;
            for ((ts, _), list) in rows {
                let mut list = list.clone();
                list.sort_by(|a, b| a.name.cmp(&b.name));
                w.write(*ts, &list)?;
            }
        }
        self.commit(tier, period)
    }
}

/// What one tier pair (minutes and hours) of the import did.
#[derive(Default)]
struct Counts {
    /// Minutes the history did not have.
    added: usize,
    /// Minutes of the file in place of the history's (--replace).
    replaced: usize,
    /// The history's minutes inside the file's span with none of the file's in their place.
    dropped: usize,
    /// Minutes the history has already (kept).
    skipped: usize,
    hours_made: usize,
    /// Hourly rows inside the file's span given way and not made again (no minutes left).
    hours_dropped: usize,
    /// Hours not over yet (their ends), left to the recorder.
    hours_left: BTreeSet<i64>,
}

/// Leaves out the rows of `tier` whose period ended `days` or more ago (as the recorder's retention
/// deletes whole periods); returns how many.
fn keep_recent<R>(rows: &mut BTreeMap<i64, R>, tier: Tier, days: i64, now: i64) -> usize {
    let cutoff = now - days * units::DAY_MS;
    let before = rows.len();
    rows.retain(|ts, _| {
        units::period_range(&tier.period(*ts)).map_or(true, |(_, end)| end > cutoff)
    });
    before - rows.len()
}

/// The periods of `tier` that rows timed a..=b fall in, in order.
fn periods(tier: Tier, a: i64, b: i64) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut t = a;
    while t <= b {
        let p = tier.period(t);
        let Some((_, end)) = units::period_range(&p) else {
            break;
        };
        out.insert(p);
        t = end + 1;
    }
    out
}

/// Merges the file's rows of a minute tier and its hour tier into the history, generically over
/// system and app rows. `weight` is what a row stands for (time in 10-second units, or seconds
/// covered), stored to `resolution`.
///
/// An hour is made again from its minutes as merged when this run added or replaced any of them,
/// or when its row does not stand for all of them (a run after one that stopped halfway): when the
/// weights differ by more than the minutes' own rounding can explain. An hour whose minutes this
/// run removed, with no row of the file in their place, goes too. With --replace, both tiers give
/// way inside the file's spans, every file they touch included.
#[allow(clippy::too_many_arguments)]
fn merge<R: Clone>(
    swap: &mut Swap,
    (min_tier, hour_tier): (Tier, Tier),
    file_min: &BTreeMap<i64, R>,
    file_hour: &BTreeMap<i64, R>,
    replace: bool,
    now: i64,
    read: impl Fn(Tier, &str) -> Rows<R>,
    roll: impl Fn(&[(&i64, &R)]) -> Option<R>,
    weight: impl Fn(&R) -> f64,
    resolution: f64,
    write: impl Fn(&mut Swap, Tier, &str, &Rows<R>) -> std::io::Result<()>,
) -> Result<Counts, String> {
    let mut c = Counts::default();
    let span = |m: &BTreeMap<i64, R>| m.keys().next().copied().zip(m.keys().next_back().copied());
    let (min_span, hour_span) = (span(file_min), span(file_hour));

    // Minutes, a day file at a time; the merged days are kept for the hours.
    let mut day_files: BTreeSet<String> = file_min.keys().map(|ts| min_tier.period(*ts)).collect();
    if let (true, Some((a, b))) = (replace, min_span) {
        day_files.extend(periods(min_tier, a, b));
    }
    let mut days: BTreeMap<String, Rows<R>> = BTreeMap::new();
    // The hours whose minutes this run added or replaced.
    let mut touched: BTreeSet<i64> = BTreeSet::new();
    for p in day_files {
        let mut rows = read(min_tier, &p);
        let before = rows.len();
        let mut removed: BTreeSet<i64> = BTreeSet::new();
        if let (true, Some((a, b))) = (replace, min_span) {
            rows.retain(|(ts, _), _| {
                let keep = *ts < a || *ts > b;
                if !keep {
                    touched.insert(hour_end(*ts));
                    removed.insert(*ts);
                }
                keep
            });
        }
        let (lo, hi) = units::period_range(&p).unwrap_or((i64::MIN, i64::MAX));
        let mut changed = before != rows.len();
        for (ts, r) in file_min.range(lo + 1..=hi) {
            if at(&rows, *ts).next().is_some() {
                c.skipped += 1;
                continue;
            }
            rows.insert((*ts, 0), r.clone());
            touched.insert(hour_end(*ts));
            changed = true;
            if removed.contains(ts) {
                c.replaced += 1;
            } else {
                c.added += 1;
            }
        }
        // Times the history had inside the span and the file has none for.
        c.dropped += removed.iter().filter(|t| !file_min.contains_key(t)).count();
        if changed {
            write(swap, min_tier, &p, &rows).map_err(|e| format!("{} {p}: {e}", min_tier.dir()))?;
        }
        days.insert(p, rows);
    }

    // Hours: of the file's hourly rows, of its minutes, of the minutes replaced, and (with
    // --replace) the hourly rows that give way.
    let mut hours: BTreeSet<i64> = file_hour.keys().copied().collect();
    hours.extend(file_min.keys().map(|ts| hour_end(*ts)));
    hours.extend(touched.iter().copied());
    let mut month_files: BTreeSet<String> = hours.iter().map(|h| hour_tier.period(*h)).collect();
    if let (true, Some((a, b))) = (replace, hour_span) {
        month_files.extend(periods(hour_tier, a, b));
    }
    for p in month_files {
        let mut rows = read(hour_tier, &p);
        let before = rows.len();
        let (lo, hi) = units::period_range(&p).unwrap_or((i64::MIN, i64::MAX));
        let mut todo: BTreeSet<i64> = hours.range(lo + 1..=hi).copied().collect();
        let mut given_way: BTreeSet<i64> = BTreeSet::new();
        if let (true, Some((a, b))) = (replace, hour_span) {
            rows.retain(|(ts, _), _| {
                let keep = *ts < a || *ts > b;
                if !keep {
                    todo.insert(*ts);
                    given_way.insert(*ts);
                }
                keep
            });
        }
        let mut changed = before != rows.len();
        for h in todo {
            if h > now {
                c.hours_left.insert(h);
                continue;
            }
            // The hour's minutes as merged above, or as the history has them.
            let day = min_tier.period(h);
            let read_day;
            let minutes = match days.get(&day) {
                Some(d) => d,
                None => {
                    read_day = read(min_tier, &day);
                    &read_day
                }
            };
            let in_hour = within(minutes, h - HOUR + 1, h);
            let have = at(&rows, h).next().is_some();
            let row = if in_hour.is_empty() {
                if touched.contains(&h) {
                    // This run removed its minutes: the file's row in its place, or none at all.
                    if file_hour.get(&h).is_none() && have {
                        rows.retain(|(t, _), _| *t != h);
                        c.hours_dropped += 1;
                        changed = true;
                    }
                    file_hour.get(&h).cloned()
                } else if have {
                    None // no minutes left (older than their retention): the row stays
                } else {
                    file_hour.get(&h).cloned()
                }
            } else if touched.contains(&h) || !have {
                roll(&in_hour)
            } else {
                // Its row (or rows, after the clock went back) must stand for all its minutes, up
                // to the rounding of each minute's weight and of the hour's.
                let stands: f64 = at(&rows, h).map(&weight).sum();
                let slack = (in_hour.len() as f64 / 2.0 + 1.0) * resolution;
                roll(&in_hour).filter(|r| (stands - weight(r)).abs() > slack)
            };
            match row {
                Some(r) => {
                    rows.retain(|(t, _), _| *t != h);
                    rows.insert((h, 0), r);
                    c.hours_made += 1;
                    changed = true;
                }
                None if given_way.contains(&h) && !have => c.hours_dropped += 1,
                None => {}
            }
        }
        if changed {
            write(swap, hour_tier, &p, &rows)
                .map_err(|e| format!("{} {p}: {e}", hour_tier.dir()))?;
        }
    }
    Ok(c)
}

pub fn run(root: &Path, file: Option<&Path>, replace: bool) -> i32 {
    let Some(file) = file else {
        eprintln!("usage: kymo import --data DIR --file EXPORT [--replace]");
        return 2;
    };
    let Some(_instance) = crate::win::single_instance(crate::INSTANCE) else {
        eprintln!("a recorder is running: stop it first");
        return crate::ALREADY_RUNNING;
    };
    match fs::read_to_string(file) {
        Ok(text) => apply(root, &text, replace),
        Err(e) => {
            eprintln!("{}: {e}", file.display());
            1
        }
    }
}

/// The import itself, on the text of an export (the caller makes sure no recorder runs).
fn apply(root: &Path, text: &str, replace: bool) -> i32 {
    let mut sys: [BTreeMap<i64, SysRow>; 2] = Default::default();
    let mut apps: [BTreeMap<i64, Vec<AppValues>>; 2] = Default::default();
    for (n, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        let bad = || {
            eprintln!("line {}: not a row: {line}", n + 1);
            1
        };
        let (Some(t), Some(ts)) = (
            f.get(1).and_then(|s| tier(s)),
            f.get(2).and_then(|s| s.parse::<i64>().ok()),
        ) else {
            return bad();
        };
        match (f[0], f.len()) {
            ("S", 8) if matches!(t, Tier::Min | Tier::Hour) => {
                let (Some(count), Some(unit), Some(avg), Some(max)) = (
                    f[3].parse::<u32>().ok(),
                    f[5].parse::<u8>().ok().and_then(Unit::from_code),
                    num(f[6]),
                    num(f[7]),
                ) else {
                    return bad();
                };
                let e = sys[(t == Tier::Hour) as usize]
                    .entry(ts)
                    .or_insert((0, Vec::new()));
                e.0 = e.0.max(count);
                e.1.push((f[4].to_string(), unit, avg, max));
            }
            ("A", len)
                if matches!(t, Tier::AppMin | Tier::AppHour)
                    && (len == 4 + APP_N || len == 4 + 2 * APP_N) =>
            {
                let mut v = [f64::NAN; 2 * APP_N];
                for (i, s) in f[4..].iter().enumerate() {
                    let Some(x) = num(s) else { return bad() };
                    v[i] = x;
                }
                apps[(t == Tier::AppHour) as usize]
                    .entry(ts)
                    .or_default()
                    .push(AppValues {
                        name: f[3].to_string(),
                        v,
                    });
            }
            _ => return bad(),
        }
    }

    // Rows the history's retention no longer keeps are left out: the recorder would delete their
    // files again (as it does, whole periods that ended longer ago than the retention).
    let now = units::now_ms();
    let (cfg, _) = crate::settings::Settings::load(root);
    let too_old = keep_recent(&mut sys[0], Tier::Min, cfg.int("keep_min"), now)
        + keep_recent(&mut sys[1], Tier::Hour, cfg.int("keep_hour"), now)
        + keep_recent(&mut apps[0], Tier::AppMin, cfg.int("keep_apps"), now)
        + keep_recent(&mut apps[1], Tier::AppHour, cfg.int("keep_apps_hour"), now);
    let mut swap = Swap::new(root);
    let s = merge(
        &mut swap,
        (Tier::Min, Tier::Hour),
        &sys[0],
        &sys[1],
        replace,
        now,
        |t, p| read_sys(root, t, p),
        |m| Some(roll_sys(m.iter().map(|(_, r)| *r))),
        |r| r.0 as f64,
        1.0,
        |sw, t, p, rows| sw.write_sys(t, p, rows),
    );
    let a = s.and_then(|s| {
        merge(
            &mut swap,
            (Tier::AppMin, Tier::AppHour),
            &apps[0],
            &apps[1],
            replace,
            now,
            |t, p| read_apps(root, t, p),
            |m| roll_apps(m.iter().map(|(t, r)| (*t, *r))),
            |r| {
                r.iter()
                    .map(|a| a.v[APP_SECONDS])
                    .find(|s| s.is_finite())
                    .unwrap_or(0.0)
            },
            0.1,
            |sw, t, p, rows| sw.write_apps(t, p, rows),
        )
        .map(|a| (s, a))
    });
    let _ = fs::remove_dir_all(&swap.staging);
    match a {
        Ok((s, a)) => {
            println!(
                "system minutes: {} added, {} replaced, {} already there; app minutes: {} added, {} replaced, {} already there",
                s.added, s.replaced, s.skipped, a.added, a.replaced, a.skipped
            );
            println!(
                "hourly rows written: {} system, {} apps",
                s.hours_made, a.hours_made
            );
            if s.dropped + a.dropped + s.hours_dropped + a.hours_dropped > 0 {
                println!(
                    "given way inside the file's span, with nothing from the file in their place: {} minutes, {} hourly rows",
                    s.dropped + a.dropped,
                    s.hours_dropped + a.hours_dropped
                );
            }
            if too_old > 0 {
                println!(
                    "left out, older than the history keeps (Settings, Retention): {too_old} rows"
                );
            }
            // The recorder rolls only its own samples into the system's hourly row: once the hour
            // is over, the same import run again makes it from all its minutes.
            if let Some(h) = s.hours_left.iter().chain(&a.hours_left).max() {
                println!("the hour ending {} UTC is not over: run the same import again after it, to complete its hourly rows", units::fmt_time(*h));
            }
            if swap.backed_up > 0 {
                println!(
                    "{} files written; the ones they replaced are in {}",
                    swap.files,
                    swap.backup.display()
                );
            } else if swap.files > 0 {
                println!("{} files written, all new", swap.files);
            }
            0
        }
        Err(e) => {
            eprintln!("{e}");
            if swap.files > 0 && swap.backed_up > 0 {
                eprintln!("{} files were already written (the history stays whole); the ones they replaced are in {}", swap.files, swap.backup.display());
            } else if swap.files > 0 {
                eprintln!(
                    "{} files were already written, all new (the history stays whole)",
                    swap.files
                );
            } else if swap.backed_up > 0 {
                eprintln!(
                    "nothing was replaced; copies made on the way are in {}",
                    swap.backup.display()
                );
            }
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("kymo-import-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn min_row(root: &Path, ts: i64, v: f64) {
        let mut w = RowWriter::open(root, Tier::Min, &Tier::Min.period(ts)).unwrap();
        w.write(
            ts,
            6,
            &[Point {
                key: "cpu.busy",
                unit: Unit::Percent,
                avg: v,
                max: v,
            }],
        )
        .unwrap();
    }

    fn hour_row(root: &Path, ts: i64, v: f64) {
        let mut w = RowWriter::open(root, Tier::Hour, &Tier::Hour.period(ts)).unwrap();
        w.write(
            ts,
            6,
            &[Point {
                key: "cpu.busy",
                unit: Unit::Percent,
                avg: v,
                max: v,
            }],
        )
        .unwrap();
    }

    fn rows(root: &Path, tier: Tier) -> Vec<(i64, f64)> {
        let mut out = Vec::new();
        each_row(root, tier, 0, i64::MAX, |ts, _, _, vals| {
            out.push((ts, vals[0]))
        });
        out
    }

    #[test]
    fn boundary_hour_is_made_from_all_its_minutes_and_files_stay_in_order() {
        let root = tmp("boundary");
        let data = root.join("history");
        let h0 = 1_790_000_000_000 / HOUR * HOUR; // an hour boundary
                                                  // The recorder's own: minutes 31-60 of the hour at 40 %, and its partial hourly row.
        for m in 31..=60 {
            min_row(&data, h0 + m * 60_000, 40.0);
        }
        hour_row(&data, h0 + HOUR, 40.0);
        // The export: minutes 1-30 at 10 %, its partial hourly row (10 %), and the hour before.
        let mut text = String::new();
        for m in 1..=30 {
            text += &format!("S\t1m\t{}\t6\tcpu.busy\t0\t10\t10\n", h0 + m * 60_000);
        }
        text += &format!("S\t1h\t{}\t180\tcpu.busy\t0\t10\t10\n", h0 + HOUR);
        text += &format!("S\t1h\t{h0}\t360\tcpu.busy\t0\t20\t30\n");
        assert_eq!(apply(&data, &text, false), 0);
        let m = rows(&data, Tier::Min);
        assert_eq!(m.len(), 60);
        assert!(
            m.windows(2).all(|w| w[0].0 < w[1].0),
            "minutes in time order"
        );
        let h = rows(&data, Tier::Hour);
        assert_eq!(
            h,
            vec![(h0, 20.0), (h0 + HOUR, 25.0)],
            "the boundary hour holds both halves"
        );
        // Again: nothing changes. With --replace: the same result.
        assert_eq!(apply(&data, &text, false), 0);
        assert_eq!(rows(&data, Tier::Hour), h);
        assert_eq!(apply(&data, &text, true), 0);
        assert_eq!(rows(&data, Tier::Min).len(), 60);
        assert_eq!(rows(&data, Tier::Hour), h);
        let backups: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("history.before-import-")
            })
            .collect();
        assert!(!backups.is_empty() && backups[0].path().join("1h").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rows_at_period_ends_stay_in_their_own_file_once() {
        let data = tmp("ends").join("history");
        let day = 1_792_454_400_000; // 2026-10-20 00:00 UTC: the end of 2026-10-19's file
        let month = 1_790_812_800_000; // 2026-10-01 00:00 UTC: the end of September's hourly file
        let mut text = String::new();
        for ts in [day - 60_000, day, day + 60_000] {
            text += &format!("S\t1m\t{ts}\t6\tcpu.busy\t0\t10\t10\n");
            text += &format!("A\tapps\t{ts}\tcode\t1\t1\t0\t0\t0\t1\t1\t1\t60\n");
        }
        for ts in [month - HOUR, month, month + HOUR] {
            text += &format!("S\t1h\t{ts}\t360\tcpu.busy\t0\t10\t10\n");
        }
        assert_eq!(apply(&data, &text, false), 0);
        assert_eq!(apply(&data, &text, true), 0);
        let minutes: Vec<i64> = rows(&data, Tier::Min).into_iter().map(|r| r.0).collect();
        assert_eq!(minutes, vec![day - 60_000, day, day + 60_000]);
        let mut in_day = Vec::new();
        each_row(&data, Tier::Min, day - 86_400_000, day, |ts, _, _, _| {
            in_day.push(ts)
        });
        assert_eq!(
            in_day,
            vec![day - 60_000, day],
            "00:00 belongs to the day before"
        );
        let hours: Vec<i64> = rows(&data, Tier::Hour)
            .into_iter()
            .map(|r| r.0)
            .filter(|t| (month - HOUR..=month + HOUR).contains(t))
            .collect();
        assert_eq!(hours, vec![month - HOUR, month, month + HOUR]);
        let mut apps = Vec::new();
        each_app_row(&data, Tier::AppMin, 0, i64::MAX, |ts, _, _, _| {
            apps.push(ts)
        });
        assert_eq!(apps, vec![day - 60_000, day, day + 60_000]);
        let _ = fs::remove_dir_all(data.parent().unwrap());
    }

    #[test]
    fn an_import_that_stopped_halfway_still_completes_the_boundary_hour() {
        let data = tmp("halfway").join("history");
        let h0 = 1_790_000_000_000 / HOUR * HOUR;
        // An earlier run wrote the file's minutes 1-30, then stopped before the hourly file; the
        // recorder's own minutes 31-60 and its partial hourly row are there too.
        for m in 1..=30 {
            min_row(&data, h0 + m * 60_000, 10.0);
        }
        for m in 31..=60 {
            min_row(&data, h0 + m * 60_000, 40.0);
        }
        hour_row(&data, h0 + HOUR, 40.0);
        let mut text = String::new();
        for m in 1..=30 {
            text += &format!("S\t1m\t{}\t6\tcpu.busy\t0\t10\t10\n", h0 + m * 60_000);
        }
        assert_eq!(apply(&data, &text, false), 0);
        assert_eq!(rows(&data, Tier::Hour), vec![(h0 + HOUR, 25.0)]);
        let _ = fs::remove_dir_all(data.parent().unwrap());
    }

    fn backups(root: &Path) -> usize {
        fs::read_dir(root).map_or(0, |d| {
            d.flatten()
                .filter(|e| e.file_name().to_string_lossy().contains(".before-import-"))
                .count()
        })
    }

    #[test]
    fn hours_follow_their_minutes_in_every_case() {
        let root = tmp("cases");
        let data = root.join("history");
        let h0 = 1_790_000_000_000 / HOUR * HOUR;
        let line =
            |m: i64, v: f64| format!("S\t1m\t{}\t6\tcpu.busy\t0\t{v}\t{v}\n", h0 + m * 60_000);
        // An export overlapping the recorder's own minutes: those stay, and the hour is made from all.
        for m in 31..=60 {
            min_row(&data, h0 + m * 60_000, 40.0);
        }
        hour_row(&data, h0 + HOUR, 40.0);
        let text: String = (1..=60).map(|m| line(m, 10.0)).collect();
        assert_eq!(apply(&data, &text, false), 0);
        assert_eq!(rows(&data, Tier::Hour), vec![(h0 + HOUR, 25.0)]);
        // Redone with --replace and other values but the same samples: the hour follows them.
        let text: String = (1..=30).map(|m| line(m, 20.0)).collect();
        assert_eq!(apply(&data, &text, true), 0);
        assert_eq!(rows(&data, Tier::Hour), vec![(h0 + HOUR, 30.0)]);
        // Run again: nothing to write, no new backup.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let n = backups(&root);
        assert_eq!(apply(&data, &text, false), 0);
        assert_eq!(backups(&root), n);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn app_hours_with_measured_seconds_are_not_remade_on_every_run() {
        let root = tmp("appsecs");
        let data = root.join("history");
        let h0 = 1_790_000_000_000 / HOUR * HOUR;
        {
            // The recorder's own minutes 31-60, with the seconds it measured (not exactly 60).
            let mut w =
                AppWriter::open(&data, Tier::AppMin, &Tier::AppMin.period(h0 + HOUR)).unwrap();
            for m in 31..=60 {
                let mut v = [f64::NAN; 2 * APP_N];
                v[0] = 5.0;
                v[APP_SECONDS] = [60.1, 59.9, 60.3][m as usize % 3];
                w.write(
                    h0 + m * 60_000,
                    &[AppValues {
                        name: "code".into(),
                        v,
                    }],
                )
                .unwrap();
            }
        }
        let text = (1..=30).fold(String::new(), |mut s, m| {
            use std::fmt::Write;
            let _ = writeln!(
                s,
                "A\tapps\t{}\tcode\t1\t1\t0\t0\t0\t1\t1\t1\t60",
                h0 + m * 60_000
            );
            s
        });
        assert_eq!(apply(&data, &text, false), 0);
        let n = backups(&root);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(apply(&data, &text, false), 0);
        assert_eq!(backups(&root), n, "a second run rewrites nothing");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn replace_clears_the_whole_span_in_both_tiers() {
        let data = tmp("span").join("history");
        let day = 86_400_000;
        let t1 = 1_788_264_000_000; // 2026-09-01 12:00 UTC
                                    // An earlier import left a minute and its hour on the day between.
        min_row(&data, t1 + day, 77.0);
        hour_row(&data, t1 + day, 77.0);
        let mut text = String::new();
        for t in [t1, t1 + 2 * day] {
            text += &format!("S\t1m\t{t}\t6\tcpu.busy\t0\t10\t10\n");
            text += &format!("S\t1h\t{t}\t6\tcpu.busy\t0\t10\t10\n");
        }
        assert_eq!(apply(&data, &text, true), 0);
        assert_eq!(
            rows(&data, Tier::Min),
            vec![(t1, 10.0), (t1 + 2 * day, 10.0)]
        );
        assert_eq!(
            rows(&data, Tier::Hour),
            vec![(t1, 10.0), (t1 + 2 * day, 10.0)]
        );
        let _ = fs::remove_dir_all(data.parent().unwrap());
    }

    #[test]
    fn rows_at_the_same_time_survive_a_rewrite_and_old_rows_are_left_out() {
        let data = tmp("same").join("history");
        let h0 = units::now_ms() / HOUR * HOUR - 24 * HOUR;
        // The clock went back: the recorder wrote minute 3 twice.
        for m in [1, 2, 3, 3] {
            min_row(&data, h0 + m * 60_000, m as f64);
        }
        // An export adding minute 4, and a minute 30 days old while minutes are kept 7 days.
        fs::write(data.join("settings.conf"), "keep_raw = 7\nkeep_min = 7\n").unwrap();
        let old = h0 - 30 * 86_400_000;
        let text = format!(
            "S\t1m\t{}\t6\tcpu.busy\t0\t4\t4\nS\t1m\t{old}\t6\tcpu.busy\t0\t9\t9\n",
            h0 + 4 * 60_000
        );
        assert_eq!(apply(&data, &text, false), 0);
        let m: Vec<f64> = rows(&data, Tier::Min).into_iter().map(|r| r.1).collect();
        assert_eq!(m, vec![1.0, 2.0, 3.0, 3.0, 4.0]);
        let _ = fs::remove_dir_all(data.parent().unwrap());
    }

    #[test]
    fn replace_with_minutes_only_takes_away_the_hours_of_the_minutes_it_removes() {
        let data = tmp("minutesonly").join("history");
        let day = 86_400_000;
        let t1 = 1_788_264_000_000; // 2026-09-01 12:00 UTC
        min_row(&data, t1 + day, 77.0);
        hour_row(&data, t1 + day, 77.0);
        let text = format!(
            "S\t1m\t{t1}\t6\tcpu.busy\t0\t10\t10\nS\t1m\t{}\t6\tcpu.busy\t0\t10\t10\n",
            t1 + 2 * day
        );
        assert_eq!(apply(&data, &text, true), 0);
        assert_eq!(
            rows(&data, Tier::Min),
            vec![(t1, 10.0), (t1 + 2 * day, 10.0)]
        );
        assert_eq!(
            rows(&data, Tier::Hour),
            vec![(t1, 10.0), (t1 + 2 * day, 10.0)]
        );
        let _ = fs::remove_dir_all(data.parent().unwrap());
    }

    #[test]
    fn replace_swaps_the_imported_rows_and_keeps_the_rest() {
        let root = tmp("replace").join("history");
        let h0 = 1_790_000_000_000 / HOUR * HOUR;
        for m in 1..=10 {
            min_row(&root, h0 + m * 60_000, 99.0); // an earlier, wrong import
        }
        for m in 11..=20 {
            min_row(&root, h0 + m * 60_000, 50.0); // the recorder's own
        }
        let mut text = String::new();
        for m in 1..=10 {
            text += &format!("S\t1m\t{}\t6\tcpu.busy\t0\t5\t5\n", h0 + m * 60_000);
        }
        assert_eq!(apply(&root, &text, true), 0);
        let m = rows(&root, Tier::Min);
        assert_eq!(m.len(), 20);
        assert!(m[..10].iter().all(|r| r.1 == 5.0) && m[10..].iter().all(|r| r.1 == 50.0));
        let _ = fs::remove_dir_all(root.parent().unwrap());
    }
}
