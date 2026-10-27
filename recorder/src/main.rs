//! kymo: the performance history of this PC in one small process. Every 10 seconds it reads
//! the performance counters, every process and the GPU, appends one row to the history, and serves
//! it on http://127.0.0.1:21223 (see api.rs). No dependencies, no admin rights needed.
//!
//! kymo [run] [--data DIR] [--port N] [--ui DIR]
//!                                             collect, store and serve (default data: next to the
//!                                             exe; --ui serves the dashboard files from a folder)
//! kymo service --data DIR [--port N]   the same, as the Windows service "Kymo"
//!                                             (started by the service control manager only)
//! kymo import --data DIR --file F [--replace]  add history exported from the previous stack
//!                                             (import.rs; the recorder must be stopped)
//! kymo serve --data DIR --port N [--ui DIR]
//!                                             only serve an existing history (no recording; for
//!                                             working on the dashboards next to the recorder)
//! kymo snapshot                        print one sample as JSON and exit (for checks)
//! kymo probe                           memory used by each start-up step
#![windows_subsystem = "windows"]

macro_rules! log {
    ($($arg:tt)*) => { $crate::logfile::write(&format!($($arg)*)) };
}

mod api;
mod card;
mod gpu;
mod import;
mod json;
mod logfile;
mod pdh;
mod ping;
mod procs;
mod query;
mod service;
mod settings;
mod store;
mod system;
mod units;
mod win;

use settings::Settings;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The sample interval in ms (a setting, 1 to 60 s).
static INTERVAL: AtomicI64 = AtomicI64::new(10_000);

pub fn interval_ms() -> i64 {
    INTERVAL.load(Ordering::Relaxed)
}

fn set_interval(ms: i64) {
    INTERVAL.store(ms, Ordering::Relaxed);
    store::set_sample_ms(ms);
}

/// A sample more than this after the previous one starts afresh (start-up, sleep, hibernation):
/// two and a half intervals, at least 25 s.
fn gap_ms() -> i64 {
    (interval_ms() * 5 / 2).max(25_000)
}

/// What the settings say to record, keep and show.
fn sources_of(s: &Settings) -> system::Sources {
    system::Sources {
        per_core: s.flag("per_core"),
        per_adapter: s.flag("per_adapter"),
        gpu: s.flag("gpu"),
        power: s.flag("power"),
        apps: s.flag("apps"),
    }
}

fn keep_of(s: &Settings) -> store::Keep {
    // Tier::ALL order: samples, minutes, hours, apps per minute, apps per hour.
    store::Keep {
        days: [
            s.int("keep_raw"),
            s.int("keep_min"),
            s.int("keep_hour"),
            s.int("keep_apps"),
            s.int("keep_apps_hour"),
        ],
        budget: (s.int("budget_gb") as u64) << 30,
    }
}

fn drive_of(s: &Settings) -> String {
    match s.text("card_drive") {
        "" => card::system_drive(),
        d => d.to_string(),
    }
}

/// What to ping, from the settings: the target, and the router too unless it is the target.
fn ping_of(s: &Settings) {
    let target = ping::target_of(s.int("ping_target"), s.text("ping_host"));
    let router = s.flag("ping_router") && s.int("ping_target") != 4;
    ping::configure(target, router);
}

fn load_settings(data: &Path) -> Settings {
    let (s, problems) = Settings::load(data);
    for p in problems {
        log!("settings: {p}");
    }
    s
}

/// The recording loop not moving for this long while the PC is awake means it hangs (a driver
/// call that never returns): the process ends, and the service control manager (or the task)
/// starts it again.
const HANG_MS: i64 = 180_000;
/// One recorder per PC, whatever the session or account it runs in.
const INSTANCE: &str = "Global\\KymoRecorder";
/// Exit code when another recorder runs already.
const ALREADY_RUNNING: i32 = 5;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut data = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("data")))
        .unwrap_or_else(|| PathBuf::from("data"));
    let mut port: Option<u16> = None;
    let mut ui: Option<PathBuf> = None;
    let mut file: Option<PathBuf> = None;
    let mut replace = false;
    let mut cmd = String::from("run");
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--data" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    data = PathBuf::from(v);
                }
            }
            "--file" => {
                i += 1;
                file = args.get(i).map(PathBuf::from);
            }
            "--ui" => {
                i += 1;
                ui = args.get(i).map(PathBuf::from);
            }
            "--replace" => replace = true,
            "--port" => {
                i += 1;
                port = args.get(i).and_then(|v| v.parse().ok());
            }
            other => cmd = other.to_string(),
        }
        i += 1;
    }
    let code = match cmd.as_str() {
        "run" => {
            static NEVER: AtomicBool = AtomicBool::new(false);
            run(data, port.unwrap_or(21223), ui, &NEVER)
        }
        "service" => service::run(data, port.unwrap_or(21223)),
        "import" => import::run(&data, file.as_deref(), replace),
        "serve" => serve_only(data, port, ui),
        "snapshot" => snapshot(),
        "probe" => {
            for (step, bytes) in system::probe() {
                println!("{:>8.1} MB  {step}", bytes as f64 / 1e6);
            }
            0
        }
        _ => 2,
    };
    std::process::exit(code);
}

fn align_up(ms: i64) -> i64 {
    let i = interval_ms();
    (ms / i + 1) * i
}

/// Records into `data` and serves the API until `stop` is set (by the service control manager).
fn run(data: PathBuf, port: u16, ui_dir: Option<PathBuf>, stop: &AtomicBool) -> i32 {
    if std::fs::create_dir_all(&data).is_err() {
        return 1;
    }
    logfile::init(data.join("kymo.log"));
    let Some(_instance) = win::single_instance(INSTANCE) else {
        log!("another recorder runs already");
        return ALREADY_RUNNING;
    };
    win::normal_memory_priority();
    std::panic::set_hook(Box::new(|info| log!("panic: {info}")));
    let started = units::now_ms();

    // Hang watchdog, first of all: start-up opens the same drivers and providers (PDH, NVML) that
    // can hang. The loop beats at every turn (at least once a second), start-up once; awake time
    // leaves sleep out. It ends the process with TerminateProcess, which runs no DLL detach code
    // that could wait on a lock the hung thread holds, after a log line that never waits.
    let beat = Arc::new(AtomicI64::new(win::awake_ms()));
    let api_down = Arc::new(AtomicBool::new(false));
    {
        let (beat, api_down) = (beat.clone(), api_down.clone());
        let spawned = std::thread::Builder::new()
            .name("watchdog".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_secs(30));
                let quiet = win::awake_ms() - beat.load(Ordering::Relaxed);
                if quiet > HANG_MS {
                    logfile::try_write(&format!(
                        "the recording loop has not moved for {} s: ending so that it starts again",
                        quiet / 1000
                    ));
                    win::terminate(3);
                }
                if api_down.load(Ordering::SeqCst) {
                    logfile::try_write("ending so that the API starts again");
                    win::terminate(3);
                }
            });
        if spawned.is_err() {
            log!("cannot start the watchdog thread");
        }
    }

    let mut cfg = load_settings(&data);
    set_interval(cfg.int("interval") * 1000);
    let mut store = match store::Store::open(&data, started) {
        Ok(s) => s,
        Err(e) => {
            log!("cannot open the history in {}: {e}", data.display());
            return 1;
        }
    };
    store.set_keep(keep_of(&cfg));
    // The desktop card's figures start from the last hour of history, then follow each sample.
    let w = Instant::now();
    let recent = card::Recent::load(&data, started, drive_of(&cfg));
    let (points, minutes) = recent.sizes();
    log!(
        "card figures: {points} samples and {minutes} minutes of apps loaded in {:.0} ms",
        w.elapsed().as_secs_f64() * 1000.0
    );
    // Settings changes and deletions come from the API and are carried out by this loop, which owns
    // the history files.
    let (commands, inbox) = mpsc::channel::<api::Command>();
    let shared = Arc::new(api::Shared {
        started,
        started_awake: win::awake_ms(),
        version: VERSION,
        recording: true,
        ui_dir,
        gpu: AtomicBool::new(false),
        counters: AtomicBool::new(false),
        gpu_failing: AtomicBool::new(false),
        settings: Mutex::new(cfg.clone()),
        commands: Mutex::new(Some(commands)),
        state: Mutex::new(api::State {
            recent,
            ..Default::default()
        }),
    });
    // The API, before the sources are opened: while they hang, it answers "starting", then "stale".
    // It keeps trying to listen if the port is taken; recording goes on regardless. If its thread
    // ever ends (a panic outside a request), the watchdog restarts the whole process.
    {
        let (shared, data, down) = (shared.clone(), data.clone(), api_down.clone());
        let spawned = std::thread::Builder::new()
            .name("api".into())
            .spawn(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    api::serve(port, data, shared, true)
                }));
                log!(
                    "the API stopped: {}",
                    if r.is_err() { "panic" } else { "returned" }
                );
                down.store(true, Ordering::SeqCst);
            });
        if spawned.is_err() {
            log!("cannot start the API thread");
            api_down.store(true, Ordering::SeqCst);
        }
    }
    let mut sampler = system::Sampler::new(sources_of(&cfg));
    ping_of(&cfg);
    shared.gpu.store(sampler.has_gpu(), Ordering::Relaxed);
    shared
        .counters
        .store(sampler.has_counters(), Ordering::Relaxed);
    shared
        .gpu_failing
        .store(sampler.gpu_failing(), Ordering::Relaxed);
    beat.store(win::awake_ms(), Ordering::Relaxed);
    log!(
        "started {VERSION}: data {}, http://127.0.0.1:{port}, counters {}, GPU {}",
        data.display(),
        if sampler.has_counters() {
            "ok"
        } else {
            "unavailable"
        },
        if sampler.has_gpu() { "ok" } else { "none" }
    );

    // Retention deletes history by the wall clock, so it is held back when the clock looks wrong:
    // for this whole run after the clock jumped ahead, and for the first week of a run that finds
    // its newest data more than 30 days old (a clock set ahead before start-up looks like that).
    let newest = store.newest_sample();
    let stale_start = newest.is_some_and(|n| started - n > 30 * units::DAY_MS);
    if stale_start {
        log!(
            "newest history is from {}: deletions held back for a week",
            units::fmt_time(newest.unwrap_or(0))
        );
    }
    let mut clock_jumped = false;
    let start_tick = win::tick_ms();

    let mut last: Option<(i64, i64)> = None; // wall time and tick at the previous read
    let mut prev_slot: Option<i64> = None; // the slot of the previous read
    let mut back = 0i64; // how far the clock went back just before this read (ms)
    let mut next = align_up(units::now_ms());
    let mut retention_at = 0;
    // Size of the history on disk, kept as a series; measured hourly (with retention).
    let mut data_bytes = store::disk_usage(&data).iter().map(|u| u.1).sum::<u64>();
    loop {
        beat.store(win::awake_ms(), Ordering::Relaxed);
        if stop.load(Ordering::SeqCst) {
            log!("stopped");
            return 0;
        }
        // Settings changes and deletions asked for on the dashboards.
        while let Ok(command) = inbox.try_recv() {
            let retention_hold =
                clock_jumped || (stale_start && win::tick_ms() - start_tick < 7 * units::DAY_MS);
            match command {
                api::Command::Settings(change, ticket, reply) => {
                    if !api::claim(&ticket) {
                        continue; // the page stopped waiting and was told nothing changed
                    }
                    // Applied to the settings in force now, which another save may have changed.
                    let checked = match change {
                        api::Change::Set(list) => cfg.with(&list),
                        api::Change::Defaults => Ok(Settings::default()),
                    };
                    let result = checked.and_then(|new| {
                        new.save(&data)
                            .map(|()| new)
                            .map_err(|e| (String::new(), format!("cannot save the settings: {e}")))
                    });
                    if let Ok(new) = &result {
                        let new = new.clone();
                        log!("settings changed: {}", cfg.diff(&new).join(", "));
                        if new.flag("apps") && !cfg.flag("apps") {
                            shared
                                .state
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .apps_since = units::now_ms();
                        }
                        if new.int("interval") != cfg.int("interval") {
                            set_interval(new.int("interval") * 1000);
                            next = align_up(units::now_ms());
                            last = None; // rates start afresh at the new pace
                        }
                        sampler.configure(sources_of(&new));
                        ping_of(&new);
                        if keep_of(&new) != keep_of(&cfg) {
                            store.set_keep(keep_of(&new));
                            if retention_hold {
                                log!("retention changed; deletions held back (the clock looked wrong)");
                            } else {
                                let (n, bytes) = store.retention(units::now_ms(), true);
                                log!(
                                    "retention applied: {n} files, {} MB deleted",
                                    bytes / 1_000_000
                                );
                            }
                            data_bytes = store::disk_usage(&data).iter().map(|u| u.1).sum();
                        }
                        if drive_of(&new) != drive_of(&cfg) {
                            let recent = card::Recent::load(&data, units::now_ms(), drive_of(&new));
                            shared
                                .state
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .recent = recent;
                        }
                        *shared.settings.lock().unwrap_or_else(|e| e.into_inner()) = new.clone();
                        cfg = new;
                    }
                    let _ = reply.send(result.map(|_| ()));
                }
                api::Command::Delete(tiers, before, ticket, reply) => {
                    if !api::claim(&ticket) {
                        continue; // the page stopped waiting and was told nothing was deleted
                    }
                    let (n, bytes) = store.delete(&tiers, before);
                    log!("deleted on request: {n} files, {} MB", bytes / 1_000_000);
                    if before.is_none() {
                        let system = tiers.contains(&store::Tier::Raw);
                        let apps = tiers.contains(&store::Tier::AppMin);
                        let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                        s.recent.clear(system, apps);
                        if apps {
                            s.apps_since = units::now_ms();
                        }
                    }
                    data_bytes = store::disk_usage(&data).iter().map(|u| u.1).sum();
                    let _ = reply.send(Ok((n, bytes)));
                }
            }
        }
        let now = units::now_ms();
        if now < next - interval_ms() {
            back = next - interval_ms() - now;
            log!("the clock went back by {} s", back / 1000);
            next = align_up(now);
            last = None;
            retention_at = 0;
            continue;
        }
        if now < next {
            // At most a second at a time: a stop request is answered within a second.
            std::thread::sleep(Duration::from_millis((next - now).min(1000) as u64));
            continue;
        }
        if now - next > 5_000 {
            // Woke late (sleep, hibernation, a busy PC): wait for the next slot.
            next = align_up(now);
            continue;
        }
        let t = next;
        next += interval_ms();
        let tick = win::tick_ms();
        // Rates use the time that really passed between two reads (a monotonic clock that counts
        // sleep); a sample that comes too long after the previous one, or across a clock change
        // (the wall clock and the monotonic one disagree), only starts afresh.
        let (baseline, dt) = match last {
            None => (true, interval_ms() as f64 / 1000.0),
            Some((lnow, ltick)) => {
                let (wall, real) = (now - lnow, tick - ltick);
                let moved = wall - real;
                if moved > units::DAY_MS && !clock_jumped {
                    log!(
                        "the clock jumped {} h ahead: deletions held back until the next start",
                        moved / 3_600_000
                    );
                    clock_jumped = true;
                }
                (
                    real > gap_ms() || moved.abs() > 2_000,
                    real.max(1) as f64 / 1000.0,
                )
            }
        };
        last = Some((now, tick));
        // The time a kept sample stands for: from the slot of the sample before (one interval, or
        // more when a slot was skipped); the slots, not the jittery real time, so that a sample
        // fills exactly its own slots in the charts.
        let len = prev_slot.map_or(interval_ms(), |p| t - p).max(1000);
        prev_slot = Some(t);
        let retention_hold = clock_jumped || (stale_start && tick - start_tick < 7 * units::DAY_MS);

        let mut out = sampler.sample(t, dt, baseline);
        ping::push_samples(&mut out.samples);
        system::push(
            &mut out.samples,
            "self.data",
            units::Unit::Bytes,
            data_bytes as f64,
        );
        let w = Instant::now();
        // Each kind of write reports its own outcome: a failing one stays reported until it succeeds.
        if !baseline {
            let error = store
                .write_sample(t, len, &out.samples)
                .err()
                .map(|e| format!("writing the sample: {e}"));
            if let Some(msg) = &error {
                shared.error(msg.clone());
            }
            shared.saving(0, error);
        }
        if let Some((end, rows)) = &out.apps {
            let error = store
                .write_apps(*end, rows)
                .err()
                .map(|e| format!("writing the apps: {e}"));
            if let Some(msg) = &error {
                shared.error(msg.clone());
            }
            shared.saving(1, error);
        }
        let storage_ms = w.elapsed().as_secs_f64() * 1000.0;
        shared.update(
            t,
            &out.samples,
            [out.ms[0], out.ms[1], out.ms[2], storage_ms],
            store.bytes,
            !baseline,
            out.apps.as_ref(),
            back,
        );
        back = 0;
        shared.gpu.store(sampler.has_gpu(), Ordering::Relaxed);
        shared
            .counters
            .store(sampler.has_counters(), Ordering::Relaxed);
        shared
            .gpu_failing
            .store(sampler.gpu_failing(), Ordering::Relaxed);
        if t >= retention_at {
            retention_at = t + 3_600_000;
            if !retention_hold {
                store.retention(t, false);
            }
            data_bytes = store::disk_usage(&data).iter().map(|u| u.1).sum();
        }
    }
}

/// Serves the history in `data` read-only: the recorder writes it, this only reads it. Port 21224
/// unless given; it never waits for the recorder's port, it stops (exit code 3) if it cannot listen.
fn serve_only(data: PathBuf, port: Option<u16>, ui_dir: Option<PathBuf>) -> i32 {
    set_interval(load_settings(&data).int("interval") * 1000); // queries round steps to it
    let port = port.unwrap_or(21224);
    let shared = Arc::new(api::Shared {
        started: units::now_ms(),
        started_awake: win::awake_ms(),
        version: VERSION,
        recording: false,
        ui_dir,
        gpu: AtomicBool::new(false),
        counters: AtomicBool::new(false),
        gpu_failing: AtomicBool::new(false),
        settings: Mutex::new(load_settings(&data)),
        commands: Mutex::new(None), // read-only: no changes, no deletions
        state: Mutex::new(api::State::default()),
    });
    match api::serve(port, data, shared, false) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("cannot listen on 127.0.0.1:{port}: {e}");
            3
        }
    }
}

/// Two samples 2 s apart, printed as JSON with the busiest apps (for checks: no files written).
fn snapshot() -> i32 {
    let mut sampler = system::Sampler::new(system::Sources::default());
    let (t0, tick0) = (units::now_ms(), win::tick_ms());
    sampler.sample(t0, 2.0, true);
    std::thread::sleep(Duration::from_secs(2));
    let out = sampler.sample(t0 + 2000, (win::tick_ms() - tick0) as f64 / 1000.0, false);
    let mut apps = sampler.procs.peek();
    apps.sort_by(|a, b| {
        b.v[0]
            .partial_cmp(&a.v[0])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut o = String::from("{");
    json::key(&mut o, "ms", true);
    json::numbers(&mut o, &out.ms);
    json::key(&mut o, "counters", false);
    o.push_str(if sampler.has_counters() {
        "true"
    } else {
        "false"
    });
    json::key(&mut o, "gpu", false);
    o.push_str(if sampler.has_gpu() { "true" } else { "false" });
    json::key(&mut o, "values", false);
    o.push('{');
    let mut samples: Vec<&system::Sample> = out.samples.iter().collect();
    samples.sort_by(|a, b| a.key.cmp(&b.key));
    for (i, s) in samples.iter().enumerate() {
        json::key(&mut o, &s.key, i == 0);
        json::number(&mut o, s.value);
    }
    o.push('}');
    json::key(&mut o, "apps", false);
    o.push('[');
    for (i, a) in apps.iter().take(15).enumerate() {
        if i > 0 {
            o.push(',');
        }
        o.push('{');
        json::key(&mut o, "name", true);
        json::string(&mut o, &a.name);
        for (k, f) in store::APP_FIELDS.iter().enumerate() {
            json::key(&mut o, f.0, false);
            json::number(&mut o, a.v[k]);
        }
        o.push('}');
    }
    o.push_str("]}");
    println!("{o}");
    0
}
