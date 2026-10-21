//! The HTTP API on 127.0.0.1 only: JSON, no cookies, no CORS. Requests must name this machine in
//! their Host header (127.0.0.1 or localhost), which blocks DNS-rebinding pages. Changes (the
//! settings, deletions) are POSTs that must carry the header `X-Kymo: 1` and, when the
//! browser sends an Origin, this origin: another web page can send neither without a CORS
//! preflight, which this server never answers. The recording loop carries them out.
//!
//! GET  /api/settings                  every setting: kind, choices or bounds, default, value, help,
//!                                     and the rules between settings (settings.rs)
//! POST /api/settings                  form body key=value&...: checked whole, then applied and saved
//! POST /api/settings/defaults         back to every default
//! GET  /api/history                   the history's files per tier, sizes, retention, budget, free space
//! POST /api/history/delete            form body scope=all|system|apps&before=all|<unix s, a day ago or
//!                                     earlier>: deletes whole files (day or month) ended by then
//!
//! GET /api/health                     {"status":"ok"|"stale", ...}; 503 when no sample for 60 s
//! GET /api/now                        the latest sample: every series with its value
//! GET /api/series                     the keys and units of the latest sample
//! GET /api/query?keys=a,b*&from=&to=&step=
//!                                     average and maximum per step; `*` matches within one key part
//! GET /api/apps?from=&to=&sort=cpu|cpu_max|mem|commit|read|write|instances&limit=
//! GET /api/app?names=a,b&field=cpu|user|kernel|read|write|mem|commit|instances&from=&to=&step=[&others=1]
//!                                     per-app series; `others=1` adds "all other apps" (their sum)
//! GET /api/stats                      this program's own cost and the size of the history
//! GET /api/card                       the desktop card's figures in one answer, from memory (card.rs)
//! GET /, /app.js, /app.css, /favicon.svg   the dashboards (built into the program; `--ui DIR` serves
//!                                     them from a folder instead, for editing)
//!
//! Times are Unix seconds (decimals allowed); `from` defaults to an hour before `to`, `to` to now,
//! `step` to about 300 points (at least 10 s). Bucket i covers (start + i*step, start + (i+1)*step].

use crate::card;
use crate::json::{key, number, numbers, string};
use crate::procs::AppRow;
use crate::query::{self, Grid};
use crate::settings::{self, Settings};
use crate::store::{self, Tier};
use crate::system::Sample;
use crate::units::{self, Unit};
use crate::win;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Settings to change: some fields (checked again by the loop against the settings in force when
/// it applies them, so two saves close together both count), or all back to their defaults.
pub enum Change {
    Set(Vec<(String, String)>),
    Defaults,
}

/// A change for the recording loop, with its ticket (see `claim`) and the channel it answers on.
pub enum Command {
    /// Apply and save these settings; answers the field at fault and why when they do not hold
    /// (an empty field: they could not be saved).
    Settings(
        Change,
        Arc<AtomicBool>,
        mpsc::Sender<Result<(), (String, String)>>,
    ),
    /// Delete the history of these tiers: the files ended by this time, or everything (None).
    Delete(
        Vec<Tier>,
        Option<i64>,
        Arc<AtomicBool>,
        mpsc::Sender<Result<(usize, u64), String>>,
    ),
}

/// Takes a command's ticket: true for whoever takes it first. The loop carries out a command only
/// with its ticket; the API takes it when it stops waiting, so a change it answered "not done" for
/// is never done later.
pub fn claim(ticket: &AtomicBool) -> bool {
    !ticket.swap(true, Ordering::SeqCst)
}

enum Asked<T> {
    Done(T),
    NotRunning,
    /// Withdrawn before the loop took it up: nothing was done.
    TimedOut,
    /// Taken up, but no answer came.
    Unknown,
}

/// Sends a command made with a fresh ticket and answer channel, and waits `wait` for its answer.
/// When the loop has not taken it up by then, it is withdrawn; once taken up, its answer is waited
/// for a while longer (a loop stuck for 3 minutes is ended by the hang watchdog).
fn ask<T>(
    tx: &mpsc::Sender<Command>,
    make: impl FnOnce(Arc<AtomicBool>, mpsc::Sender<T>) -> Command,
    wait: Duration,
) -> Asked<T> {
    let (reply, answer) = mpsc::channel();
    let ticket = Arc::new(AtomicBool::new(false));
    if tx.send(make(ticket.clone(), reply)).is_err() {
        return Asked::NotRunning;
    }
    match answer.recv_timeout(wait) {
        Ok(v) => Asked::Done(v),
        Err(_) if claim(&ticket) => Asked::TimedOut,
        Err(_) => answer
            .recv_timeout(Duration::from_secs(240))
            .map_or(Asked::Unknown, Asked::Done),
    }
}

fn field_error(field: &str, why: &str) -> String {
    let mut o = String::from("{");
    key(&mut o, "error", true);
    string(&mut o, why);
    key(&mut o, "field", false);
    string(&mut o, field);
    o.push('}');
    o
}

/// The largest request body (a settings form is about 1 KB).
const MAX_BODY: usize = 16 * 1024;

const MAX_CONNECTIONS: usize = 8;
const MAX_POINTS: usize = 11_000;
/// Points times series in one answer (about 10 MB of JSON at most).
const MAX_VALUES: usize = 500_000;
/// Whole time allowed to send a request.
const REQUEST_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct State {
    pub ts: i64,
    pub values: Vec<(String, Unit, f64)>,
    pub samples: u64,
    /// Last and summed milliseconds: counters, processes, GPU, storage.
    pub ms: [f64; 4],
    pub ms_sum: [f64; 4],
    pub errors: u64,
    pub last_error: String,
    pub bytes: u64,
    /// The desktop card's figures.
    pub recent: card::Recent,
    /// The newest sample kept in the history (not a baseline), and the first of its unbroken run.
    pub kept_ts: i64,
    pub kept_since: i64,
    /// Whether the newest sample was kept (a baseline at the same slot as the newest kept one,
    /// after the clock stepped back a little, is not).
    pub newest_kept: bool,
    /// When apps were last switched on or their history cleared: their first minute is only due
    /// a while after.
    pub apps_since: i64,
    /// win::awake_ms() at the newest sample: tells a PC that slept from a recorder that stopped.
    pub awake_at: i64,
    /// Why the newest write of samples [0] and of apps [1] to the history failed; each cleared by
    /// the next good write of its kind.
    pub save_errors: [Option<String>; 2],
}

pub struct Shared {
    pub started: i64,
    /// win::awake_ms() at start-up: "starting" lasts at most a minute awake.
    pub started_awake: i64,
    pub version: &'static str,
    /// Whether this process records (false in the read-only `serve` mode).
    pub recording: bool,
    /// Serve the dashboard files from this folder instead of the built-in copies.
    pub ui_dir: Option<PathBuf>,
    /// Whether each source answers now.
    pub gpu: AtomicBool,
    pub counters: AtomicBool,
    /// Whether an installed NVIDIA driver gives no readings.
    pub gpu_failing: AtomicBool,
    /// The settings in force.
    pub settings: Mutex<Settings>,
    /// To the recording loop; None in the read-only `serve` mode.
    pub commands: Mutex<Option<mpsc::Sender<Command>>>,
    pub state: Mutex<State>,
}

impl Shared {
    /// A sample taken (`kept`: written to the history, not a baseline) and the minute of apps
    /// that ended with it, if any.
    /// `back`: how far the clock went back just before this sample (ms), as the loop measured it.
    #[allow(clippy::too_many_arguments)] // the recording loop's outputs for one sample
    pub fn update(
        &self,
        ts: i64,
        samples: &[Sample],
        ms: [f64; 4],
        bytes: u64,
        kept: bool,
        apps: Option<&(i64, Vec<AppRow>)>,
        back: i64,
    ) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // A sample no newer than the figures kept: the clock went back, and they cannot be placed
        // on the new timeline.
        // A step back of more than two minutes (the step itself, not the distance between the
        // slots) is a new timeline.
        let far = back > 120_000;
        s.recent.clock_check(ts, far);
        // A new timeline breaks the run, and the times of the old one mean nothing on it; a
        // smaller step only drops what now lies ahead (Recent::clock_check).
        if far {
            s.kept_since = ts;
            s.apps_since = 0;
        }
        s.newest_kept = kept;
        if kept {
            // A gap, as the loop counts one (two and a half intervals), breaks the run too.
            if ts - s.kept_ts > crate::gap_ms() {
                s.kept_since = ts;
            }
            s.kept_ts = ts;
            s.recent.sample(ts, samples);
        }
        // A minute of apps that ends more than a minute after this sample is the old timeline's
        // minute in progress, handed in as the clock went back: not current.
        if let Some((end, rows)) = apps.filter(|(end, _)| *end <= ts + 60_000) {
            s.recent.apps(*end, rows);
        }
        s.awake_at = win::awake_ms();
        s.ts = ts;
        s.values = samples
            .iter()
            .map(|x| (x.key.clone(), x.unit, x.value))
            .collect();
        s.samples += 1;
        s.ms = ms;
        for (sum, v) in s.ms_sum.iter_mut().zip(ms) {
            *sum += v;
        }
        s.bytes = bytes;
    }

    pub fn error(&self, msg: String) {
        log!("{msg}");
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.errors += 1;
        s.last_error = msg;
    }

    /// The outcome of a write to the history: of samples (`which` 0) or of apps (1).
    pub fn saving(&self, which: usize, error: Option<String>) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .save_errors[which] = error;
    }
}

/// Listens on 127.0.0.1:`port` for good; while the port is taken, tries again every minute, unless
/// `retry` is false (the read-only serve mode), which returns the error instead.
pub fn serve(port: u16, root: PathBuf, shared: Arc<Shared>, retry: bool) -> io::Result<()> {
    let mut logged = false;
    loop {
        match TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))) {
            Ok(listener) => {
                if logged {
                    log!("the API listens on 127.0.0.1:{port} again");
                }
                accept(listener, port, &root, &shared);
            }
            Err(e) if !retry => return Err(e),
            Err(e) => {
                if !logged {
                    shared.error(format!(
                        "the API cannot listen on 127.0.0.1:{port}: {e}; trying again every minute"
                    ));
                    logged = true;
                }
                std::thread::sleep(Duration::from_secs(60));
            }
        }
    }
}

fn accept(listener: TcpListener, port: u16, root: &Path, shared: &Arc<Shared>) {
    let active = Arc::new(AtomicUsize::new(0));
    for conn in listener.incoming() {
        let Ok(mut stream) = conn else { continue };
        if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
            // Answer, then let the client finish sending (briefly: this thread accepts the others).
            let _ = respond(
                &mut stream,
                &Reply::json(503, "{\"error\":\"busy\"}".into()),
            );
            drain(&mut stream, Duration::from_millis(100));
            continue;
        }
        let root = root.to_path_buf();
        active.fetch_add(1, Ordering::SeqCst);
        let (shared, done) = (shared.clone(), active.clone());
        let spawned = std::thread::Builder::new()
            .name("http".into())
            .spawn(move || {
                let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    handle(stream, port, &root, &shared)
                }));
                if r.is_err() {
                    log!("http: a request handler panicked");
                }
                done.fetch_sub(1, Ordering::SeqCst);
            });
        if spawned.is_err() {
            active.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// After an early answer: stops sending and reads what the client still sends (dropped, for at most
/// `wait` and 4 MB), so that it reads the answer instead of a connection reset.
fn drain(s: &mut TcpStream, wait: Duration) {
    let _ = s.shutdown(std::net::Shutdown::Write);
    let end = Instant::now() + wait;
    let mut buf = [0u8; 16 * 1024];
    let mut total = 0usize;
    while total < 4 << 20 {
        let Some(left) = end
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
        else {
            break;
        };
        let _ = s.set_read_timeout(Some(left));
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n,
        }
    }
}

/// Reads the request head, and the body when it declares one (at most MAX_BODY bytes); the whole
/// request must arrive within REQUEST_DEADLINE.
fn read_request(s: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let deadline = Instant::now() + REQUEST_DEADLINE;
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let mut read_more = |s: &mut TcpStream, buf: &mut Vec<u8>| -> Option<()> {
        let left = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())?;
        s.set_read_timeout(Some(left)).ok()?;
        let n = s.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        Some(())
    };
    let end = loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
        if buf.len() > 16 * 1024 {
            return None;
        }
        read_more(s, &mut buf)?;
    };
    let head = String::from_utf8(buf[..end].to_vec()).ok()?;
    let length = header(&head, "content-length").map_or(Some(0), |v| v.parse::<usize>().ok())?;
    if length > MAX_BODY {
        return None;
    }
    let mut body = buf[end + 4..].to_vec();
    while body.len() < length {
        read_more(s, &mut body)?;
    }
    body.truncate(length);
    Some((head, body))
}

/// A request header's value (the name in any case).
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.split("\r\n")
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim())
}

const JSON: &str = "application/json; charset=utf-8";

/// The dashboard files: path, file name, content type.
const UI_FILES: [(&str, &str, &str); 4] = [
    ("/", "index.html", "text/html; charset=utf-8"),
    ("/app.js", "app.js", "text/javascript; charset=utf-8"),
    ("/app.css", "app.css", "text/css; charset=utf-8"),
    ("/favicon.svg", "favicon.svg", "image/svg+xml"),
];

fn built_in(name: &str) -> &'static str {
    match name {
        "index.html" => include_str!("../ui/index.html"),
        "app.js" => include_str!("../ui/app.js"),
        "app.css" => include_str!("../ui/app.css"),
        _ => include_str!("../ui/favicon.svg"),
    }
}

/// Only this origin's own files and API; no framing, no inline code.
const UI_HEADERS: &str = "Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
    connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'\r\nX-Frame-Options: DENY\r\nReferrer-Policy: no-referrer\r\n";

struct Reply {
    status: u16,
    ctype: &'static str,
    body: Vec<u8>,
    headers: &'static str,
}

impl Reply {
    fn json(status: u16, body: String) -> Reply {
        Reply {
            status,
            ctype: JSON,
            body: body.into_bytes(),
            headers: "",
        }
    }
}

fn respond(s: &mut TcpStream, r: &Reply) -> io::Result<()> {
    let reason = match r.status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n{}Connection: close\r\n\r\n",
        r.status,
        r.ctype,
        r.body.len(),
        r.headers
    );
    s.write_all(head.as_bytes())?;
    s.write_all(&r.body)?;
    s.flush()
}

fn error(msg: &str) -> String {
    let mut o = String::from("{\"error\":");
    string(&mut o, msg);
    o.push('}');
    o
}

fn handle(mut s: TcpStream, port: u16, root: &Path, shared: &Shared) {
    let _ = s.set_write_timeout(Some(Duration::from_secs(30)));
    let Some((head, body)) = read_request(&mut s) else {
        let _ = respond(
            &mut s,
            &Reply::json(400, error("request too large, cut short or too slow")),
        );
        drain(&mut s, Duration::from_secs(1));
        return;
    };
    let mut first = head.split("\r\n").next().unwrap_or("").split(' ');
    let (method, target) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    let host = header(&head, "host").map(|v| v.to_ascii_lowercase());
    let allowed = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    let (path, q) = target.split_once('?').unwrap_or((target, ""));
    let reply = if !host.as_ref().is_some_and(|h| allowed.contains(h)) {
        Reply::json(
            403,
            error("the Host header must be 127.0.0.1 or localhost with this port"),
        )
    } else if method == "POST" {
        // A change: only from this origin's own page (see the module comment).
        let origin_ok = header(&head, "origin").map_or(true, |o| {
            allowed
                .iter()
                .any(|a| o.eq_ignore_ascii_case(&format!("http://{a}")))
        });
        if header(&head, "x-kymo") != Some("1") || !origin_ok {
            Reply::json(
                403,
                error("changes come only from the dashboards' own page"),
            )
        } else {
            let (status, out) = change(path, &String::from_utf8_lossy(&body), shared);
            Reply::json(status, out)
        }
    } else if method != "GET" {
        Reply::json(405, error("only GET, and POST for changes"))
    } else {
        match UI_FILES
            .iter()
            .find(|f| f.0 == path || (f.0 == "/" && path == "/index.html"))
        {
            Some(&(_, name, ctype)) => ui_file(name, ctype, shared),
            None => {
                let params = parse_query(q);
                let (status, body) = route(path, &params, root, shared);
                Reply::json(status, body)
            }
        }
    };
    let _ = respond(&mut s, &reply);
}

fn ui_file(name: &str, ctype: &'static str, shared: &Shared) -> Reply {
    let body = match &shared.ui_dir {
        Some(dir) => match std::fs::read(dir.join(name)) {
            Ok(b) => b,
            Err(e) => return Reply::json(404, error(&format!("{name}: {e}"))),
        },
        None => built_in(name).as_bytes().to_vec(),
    };
    Reply {
        status: 200,
        ctype,
        body,
        headers: UI_HEADERS,
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
                {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn param<'a>(q: &'a [(String, String)], k: &str) -> Option<&'a str> {
    q.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str())
}

fn seconds(q: &[(String, String)], k: &str) -> Result<Option<i64>, String> {
    match param(q, k) {
        None | Some("") => Ok(None),
        Some(v) => v
            .parse::<f64>()
            .ok()
            .filter(|x| x.is_finite() && x.abs() < 1e11)
            .map(|x| Some((x * 1000.0).round() as i64))
            .ok_or_else(|| format!("{k} must be a number of seconds")),
    }
}

/// from and to in ms, checked.
fn from_to(q: &[(String, String)]) -> Result<(i64, i64), String> {
    let to = seconds(q, "to")?.unwrap_or_else(units::now_ms);
    let from = seconds(q, "from")?.unwrap_or(to - 3_600_000);
    if from >= to {
        return Err("from must be before to".into());
    }
    Ok((from, to))
}

/// from, to and step in ms, checked. The step (by default about 300 points) is rounded up to a
/// whole number of the rows the query reads: 10 s, 1 min (app rows are per minute) or 1 h; the
/// answer gives the step used.
fn range(q: &[(String, String)], apps: bool) -> Result<Grid, String> {
    let (from, to) = from_to(q)?;
    let step = match seconds(q, "step")? {
        Some(s) if s >= 1000 => s,
        Some(_) => return Err("step must be at least 1 second".into()),
        None => ((to - from) / 300).max(1000),
    };
    let row = query::row_len(step, to - from, apps);
    let step = (step + row - 1) / row * row;
    let grid = Grid::new(from, to, step);
    if grid.n > MAX_POINTS {
        return Err(format!("too many points ({}); use a longer step", grid.n));
    }
    Ok(grid)
}

/// A comma-separated list of at most 64 entries of at most 200 bytes.
fn list(q: &[(String, String)], k: &str) -> Result<Vec<String>, String> {
    let v: Vec<String> = param(q, k)
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if v.len() > 64 {
        return Err(format!("{k}: at most 64 entries"));
    }
    if v.iter().any(|s| s.len() > 200) {
        return Err(format!("{k}: entries of at most 200 characters"));
    }
    Ok(v)
}

fn route(path: &str, q: &[(String, String)], root: &Path, shared: &Shared) -> (u16, String) {
    let r = match path {
        "/api/health" => return health(shared, root),
        "/api/now" => Ok(now(shared, true)),
        "/api/series" => Ok(now(shared, false)),
        "/api/query" => series(q, root),
        "/api/apps" => apps(q, root),
        "/api/app" => app(q, root),
        "/api/stats" => Ok(stats(shared, root)),
        "/api/card" => Ok(card(shared, root)),
        "/api/settings" => Ok(settings_json(shared)),
        "/api/history" => Ok(history_json(shared, root)),
        _ => return (404, error("unknown path")),
    };
    match r {
        Ok(body) => (200, body),
        Err(e) => (400, error(&e)),
    }
}

/// The drives the recorder sees, from the newest sample.
fn drives(shared: &Shared) -> Vec<String> {
    let s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    let mut d: Vec<String> = s
        .values
        .iter()
        .filter_map(|v| {
            v.0.strip_prefix("disk.")
                .and_then(|r| r.strip_suffix(".active"))
                .map(str::to_string)
        })
        .collect();
    d.sort();
    d.dedup();
    d
}

fn settings_json(shared: &Shared) -> String {
    let current = shared
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    current.schema_json(&drives(shared), &card::system_drive())
}

/// POSTs: the settings and deletions, carried out by the recording loop.
fn change(path: &str, body: &str, shared: &Shared) -> (u16, String) {
    let Some(tx) = shared
        .commands
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    else {
        return (
            409,
            error("this is a read-only copy: change the settings on the recorder's own page"),
        );
    };
    match path {
        "/api/settings" | "/api/settings/defaults" => {
            let base = shared
                .settings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            // Checked here for a quick answer; the loop checks again against the settings in force.
            let change = if path == "/api/settings" {
                let list = settings::form(body, percent_decode);
                match base.with(&list) {
                    Ok(n) if n == base => return (200, settings_json(shared)),
                    Ok(_) => Change::Set(list),
                    Err((field, why)) => return (400, field_error(&field, &why)),
                }
            } else if base == Settings::default() {
                return (200, settings_json(shared));
            } else {
                Change::Defaults
            };
            match ask(&tx, |t, r| Command::Settings(change, t, r), Duration::from_secs(30)) {
                Asked::Done(Ok(())) => (200, settings_json(shared)),
                Asked::Done(Err((field, why))) if field.is_empty() => (500, error(&why)),
                Asked::Done(Err((field, why))) => (400, field_error(&field, &why)),
                Asked::NotRunning => (503, error("the recorder is not running")),
                Asked::TimedOut => (503, error("the recorder did not answer in time; nothing was changed")),
                Asked::Unknown => (503, error("the recorder did not finish in time; reload the page to see the settings in force")),
            }
        }
        "/api/history/delete" => {
            let f = settings::form(body, percent_decode);
            let get = |k: &str| f.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str());
            let tiers: Vec<Tier> = match get("scope") {
                Some("all") => Tier::ALL.to_vec(),
                Some("system") => vec![Tier::Raw, Tier::Min, Tier::Hour],
                Some("apps") => vec![Tier::AppMin, Tier::AppHour],
                _ => return (400, error("scope must be all, system or apps")),
            };
            // Whole days in the past only: the files being written go only with "everything".
            let before = match get("before") {
                Some("all") => None,
                Some(v) => match v.parse::<i64>() {
                    Ok(s) if s > 0 && s * 1000 <= units::now_ms() - units::DAY_MS => Some(s * 1000),
                    _ => {
                        return (
                            400,
                            error(
                                "before must be all, or a time a day ago or earlier (Unix seconds)",
                            ),
                        )
                    }
                },
                None => return (400, error("before is required")),
            };
            match ask(
                &tx,
                |t, r| Command::Delete(tiers, before, t, r),
                Duration::from_secs(60),
            ) {
                Asked::Done(Ok((n, bytes))) => {
                    let mut o = String::from("{");
                    key(&mut o, "files", true);
                    number(&mut o, n as f64);
                    key(&mut o, "bytes", false);
                    number(&mut o, bytes as f64);
                    o.push('}');
                    (200, o)
                }
                Asked::Done(Err(e)) => (500, error(&e)),
                Asked::NotRunning => (503, error("the recorder is not running")),
                Asked::TimedOut => (
                    503,
                    error("the recorder did not answer in time; nothing was deleted"),
                ),
                Asked::Unknown => (
                    503,
                    error(
                        "the recorder did not finish in time; reload the page to see what is left",
                    ),
                ),
            }
        }
        _ => (404, error("unknown path")),
    }
}

/// The volume ("C:") a folder is on.
fn volume_of(p: &Path) -> Option<String> {
    let full = std::fs::canonicalize(p).ok()?;
    let s = full.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    let v: String = s.chars().take(2).collect();
    (v.len() == 2 && v.ends_with(':')).then(|| v.to_ascii_uppercase())
}

/// The history per tier (its files with their periods and sizes, and how long it is kept), the
/// budget and the free space on its disk.
fn history_json(shared: &Shared, root: &Path) -> String {
    let cfg = shared
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let tiers = [
        (Tier::Raw, "Every sample", "keep_raw"),
        (Tier::Min, "1-minute summaries", "keep_min"),
        (Tier::Hour, "Hourly summaries", "keep_hour"),
        (Tier::AppMin, "Apps per minute", "keep_apps"),
        (Tier::AppHour, "Apps per hour", "keep_apps_hour"),
    ];
    let mut o = String::from("{");
    key(&mut o, "tiers", true);
    o.push('[');
    let mut total = 0u64;
    for (i, (tier, label, keep)) in tiers.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        let files = store::tier_files(root, *tier);
        let bytes: u64 = files.iter().map(|f| f.bytes).sum();
        total += bytes;
        o.push('{');
        key(&mut o, "tier", true);
        string(&mut o, tier.dir());
        key(&mut o, "label", false);
        string(&mut o, label);
        key(&mut o, "scope", false);
        string(
            &mut o,
            if matches!(tier, Tier::AppMin | Tier::AppHour) {
                "apps"
            } else {
                "system"
            },
        );
        key(&mut o, "keep", false);
        string(&mut o, keep);
        key(&mut o, "keep_days", false);
        number(&mut o, cfg.int(keep) as f64);
        key(&mut o, "bytes", false);
        number(&mut o, bytes as f64);
        key(&mut o, "files", false);
        o.push('[');
        for (j, f) in files.iter().enumerate() {
            if j > 0 {
                o.push(',');
            }
            o.push('[');
            number(&mut o, f.start as f64 / 1000.0);
            o.push(',');
            number(&mut o, f.end as f64 / 1000.0);
            o.push(',');
            number(&mut o, f.bytes as f64);
            o.push(']');
        }
        o.push(']');
        // Files set aside after damage: go with their period (retention, deletion), first for the budget.
        key(&mut o, "aside", false);
        o.push('[');
        for (j, (_, f)) in store::aside_files(root, *tier).iter().enumerate() {
            if j > 0 {
                o.push(',');
            }
            o.push('[');
            number(&mut o, f.start as f64 / 1000.0);
            o.push(',');
            number(&mut o, f.end as f64 / 1000.0);
            o.push(',');
            number(&mut o, f.bytes as f64);
            o.push(']');
        }
        o.push(']');
        o.push('}');
    }
    o.push(']');
    let all: u64 = store::disk_usage(root).iter().map(|u| u.1).sum();
    key(&mut o, "total", false);
    number(&mut o, all.max(total) as f64);
    key(&mut o, "budget", false);
    number(&mut o, ((cfg.int("budget_gb") as u64) << 30) as f64);
    key(&mut o, "folder", false);
    string(
        &mut o,
        &std::fs::canonicalize(root).map_or_else(
            |_| root.display().to_string(),
            |p| {
                p.display()
                    .to_string()
                    .trim_start_matches(r"\\?\")
                    .to_string()
            },
        ),
    );
    key(&mut o, "free", false);
    number(
        &mut o,
        volume_of(root)
            .and_then(|v| win::disk_space(&v))
            .map_or(f64::NAN, |d| d.1 as f64),
    );
    key(&mut o, "interval_s", false);
    number(&mut o, crate::interval_ms() as f64 / 1000.0);
    key(&mut o, "read_only", false);
    o.push_str(
        if shared
            .commands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            "false"
        } else {
            "true"
        },
    );
    o.push('}');
    o
}

/// Whether the newest sample, taken at `ts`, is current: at most a minute old, or two intervals
/// and 10 s at long intervals. A sample from the future means the clock went back and sampling
/// waits: not current either.
fn fresh(ts: i64, now: i64) -> bool {
    let limit = (2 * crate::interval_ms() + 10_000).max(60_000);
    ts > 0 && (-15_000..limit).contains(&(now - ts))
}

/// Time of the newest sample: the recorder's own, or in the read-only `serve` mode the newest on disk.
fn newest_sample(shared: &Shared, root: &Path) -> i64 {
    if shared.recording {
        shared.state.lock().unwrap_or_else(|e| e.into_inner()).ts
    } else {
        store::newest_raw(root).unwrap_or(0)
    }
}

fn health(shared: &Shared, root: &Path) -> (u16, String) {
    let ts = newest_sample(shared, root);
    let now = units::now_ms();
    let age = (now - ts) as f64 / 1000.0;
    let ok = fresh(ts, now);
    let mut o = String::from("{");
    key(&mut o, "status", true);
    string(&mut o, if ok { "ok" } else { "stale" });
    key(&mut o, "version", false);
    string(&mut o, shared.version);
    key(&mut o, "last_sample", false);
    number(&mut o, ts as f64 / 1000.0);
    key(&mut o, "age_s", false);
    number(&mut o, if ts > 0 { age } else { f64::NAN });
    o.push('}');
    (if ok { 200 } else { 503 }, o)
}

/// The desktop card's figures in one answer (see card.rs): from memory while recording, without
/// reading the history; from the history in the read-only `serve` mode.
///
/// status: "ok"; "starting" before the first sample (for at most a minute awake); "resuming"
/// while the newest sample only starts afresh (after start-up or sleep: no figures yet) or when the
/// PC slept since it (the next one is on its way); "stale" when the PC has been awake for a minute
/// without a sample. The current figures (now, avg5, last, top) and the sources are only given
/// while the newest sample is current; older figures are never passed off as current.
fn card(shared: &Shared, root: &Path) -> String {
    let now = units::now_ms();
    let cfg = shared
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    // How long "starting" and "resuming" may last: a minute, or two intervals and 10 s.
    let grace = (2 * crate::interval_ms() + 10_000).max(60_000);
    let (gpu_on, apps_on) = (cfg.flag("gpu"), cfg.flag("apps"));
    let mut o = String::from("{");
    key(&mut o, "version", true);
    string(&mut o, shared.version);
    key(&mut o, "interval_s", false);
    number(&mut o, crate::interval_ms() as f64 / 1000.0);
    cfg.write_limits(&mut o);
    cfg.write_card(&mut o);
    let head = |o: &mut String, status: &str, ts: i64| {
        key(o, "status", false);
        string(o, status);
        key(o, "ts", false);
        number(o, if ts > 0 { ts as f64 / 1000.0 } else { f64::NAN });
        key(o, "age_s", false);
        number(
            o,
            if ts > 0 {
                (now - ts) as f64 / 1000.0
            } else {
                f64::NAN
            },
        );
    };
    if !shared.recording {
        let r = card::Recent::load(root, now, crate::drive_of(&cfg));
        let ts = store::newest_raw(root).unwrap_or(0);
        let t_ref = if fresh(ts, now) { ts } else { now };
        head(&mut o, if fresh(ts, now) { "ok" } else { "stale" }, ts);
        key(&mut o, "error", false);
        o.push_str("null");
        if fresh(ts, now) {
            r.write_sources(&mut o, t_ref, None, false, false, gpu_on, apps_on);
        }
        r.write(&mut o, t_ref, fresh(ts, now));
        o.push('}');
        return o;
    }
    let s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    let status = if s.ts == 0 {
        if win::awake_ms() - shared.started_awake < grace {
            "starting"
        } else {
            "stale"
        }
    } else if fresh(s.ts, now) {
        if s.newest_kept {
            "ok"
        } else {
            "resuming"
        }
    } else if win::awake_ms() - s.awake_at < grace {
        "resuming"
    } else {
        "stale"
    };
    let t_ref = if fresh(s.ts, now) { s.ts } else { now };
    head(&mut o, status, s.ts);
    // A write to the history failing now.
    key(&mut o, "error", false);
    match s.save_errors[0].as_ref().or(s.save_errors[1].as_ref()) {
        Some(e) => {
            o.push('{');
            key(&mut o, "text", true);
            string(&mut o, e);
            o.push('}');
        }
        None => o.push_str("null"),
    }
    // The sources' state only from a current sample (a stopped recorder is reported as such).
    if fresh(s.ts, now) {
        // A minute of apps is due once samples have been kept in a row for 2.5 minutes (or a
        // minute and three intervals).
        let apps_due = status == "ok"
            && s.kept_ts - s.kept_since.max(s.apps_since)
                >= (60_000 + 3 * crate::interval_ms()).max(150_000);
        s.recent.write_sources(
            &mut o,
            t_ref,
            Some(&s.values),
            shared.gpu_failing.load(Ordering::Relaxed),
            apps_due,
            gpu_on,
            apps_on,
        );
    }
    // A newest sample from the future: the clock went back and the figures kept are from the old
    // timeline (they are dropped with the next sample).
    if s.ts > now + 15_000 {
        card::Recent::default().write(&mut o, t_ref, false);
    } else {
        s.recent.write(&mut o, t_ref, fresh(s.ts, now));
    }
    o.push('}');
    o
}

fn now(shared: &Shared, with_values: bool) -> String {
    let s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    let mut vals: Vec<&(String, Unit, f64)> = s.values.iter().collect();
    vals.sort_by(|a, b| a.0.cmp(&b.0));
    let mut o = String::from("{");
    key(&mut o, "ts", true);
    number(&mut o, s.ts as f64 / 1000.0);
    key(&mut o, "series", false);
    o.push('[');
    for (i, (k, u, v)) in vals.into_iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        o.push('{');
        key(&mut o, "key", true);
        string(&mut o, k);
        key(&mut o, "unit", false);
        string(&mut o, u.name());
        if with_values {
            key(&mut o, "value", false);
            number(&mut o, *v);
        }
        o.push('}');
    }
    o.push_str("]}");
    o
}

fn grid_head(o: &mut String, g: &Grid) {
    key(o, "start", true);
    number(o, g.start as f64 / 1000.0);
    key(o, "step", false);
    number(o, g.step as f64 / 1000.0);
    key(o, "points", false);
    number(o, g.n as f64);
}

fn series_json(o: &mut String, list: &[query::Series]) {
    key(o, "series", false);
    o.push('[');
    for (i, s) in list.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        o.push('{');
        key(o, "key", true);
        string(o, &s.key);
        key(o, "unit", false);
        string(o, s.unit.name());
        key(o, "avg", false);
        numbers(o, &s.avg);
        key(o, "max", false);
        numbers(o, &s.max);
        o.push('}');
    }
    o.push(']');
}

fn series(q: &[(String, String)], root: &Path) -> Result<String, String> {
    let keys = list(q, "keys")?;
    if keys.is_empty() {
        return Err("keys is required, for example keys=cpu.busy,mem.available".into());
    }
    let grid = range(q, false)?;
    // Series beyond the limit are left out ("left_out" says how many): split such a query.
    let found = query::series(root, &grid, &keys, (MAX_VALUES / grid.n).clamp(1, 500));
    let mut o = String::from("{");
    grid_head(&mut o, &grid);
    key(&mut o, "tiers", false);
    o.push('[');
    for (i, t) in found.tiers.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        string(&mut o, t);
    }
    o.push(']');
    key(&mut o, "left_out", false);
    number(&mut o, found.left_out as f64);
    series_json(&mut o, &found.series);
    o.push('}');
    Ok(o)
}

fn apps(q: &[(String, String)], root: &Path) -> Result<String, String> {
    let (from, to) = from_to(q)?;
    let limit = param(q, "limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(20)
        .min(1000);
    let (mut list, minutes) = query::apps(root, from, to);
    let sort = param(q, "sort").unwrap_or("cpu");
    let by: fn(&query::AppSummary) -> f64 = match sort {
        "cpu" => |a| a.cpu_avg,
        "cpu_max" => |a| a.cpu_max,
        "mem" => |a| a.ws_max,
        "commit" => |a| a.private_max,
        "read" => |a| a.read_bytes,
        "write" => |a| a.write_bytes,
        "instances" => |a| a.instances_max,
        _ => return Err("sort must be cpu, cpu_max, mem, commit, read, write or instances".into()),
    };
    list.sort_by(|a, b| {
        by(b)
            .partial_cmp(&by(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });
    list.truncate(limit);
    let mut o = String::from("{");
    key(&mut o, "from", true);
    number(&mut o, from as f64 / 1000.0);
    key(&mut o, "to", false);
    number(&mut o, to as f64 / 1000.0);
    // cpu_avg is over the whole range (time without data counts as 0); minutes is the data in it.
    key(&mut o, "minutes", false);
    number(&mut o, minutes);
    key(&mut o, "apps", false);
    o.push('[');
    for (i, a) in list.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        o.push('{');
        key(&mut o, "name", true);
        string(&mut o, &a.name);
        for (k, v) in [
            ("cpu_avg", a.cpu_avg),
            ("cpu_max", a.cpu_max),
            ("mem_max", a.ws_max),
            ("commit_max", a.private_max),
            ("read_bytes", a.read_bytes),
            ("write_bytes", a.write_bytes),
            ("instances_max", a.instances_max),
        ] {
            key(&mut o, k, false);
            number(&mut o, v);
        }
        o.push('}');
    }
    o.push_str("]}");
    Ok(o)
}

fn app(q: &[(String, String)], root: &Path) -> Result<String, String> {
    let names = list(q, "names")?;
    if names.is_empty() {
        return Err("names is required, for example names=chrome,Code".into());
    }
    let field = query::app_field(param(q, "field").unwrap_or("cpu"))
        .ok_or("field must be cpu, user, kernel, read, write, mem, commit or instances")?;
    let grid = range(q, true)?;
    if names.len() * grid.n > MAX_VALUES {
        return Err(format!(
            "{} names x {} points is too much; use fewer names or a longer step",
            names.len(),
            grid.n
        ));
    }
    let others = param(q, "others") == Some("1");
    let found = query::app_series(root, &grid, &names, field, others);
    let mut o = String::from("{");
    grid_head(&mut o, &grid);
    series_json(&mut o, &found);
    o.push('}');
    Ok(o)
}

fn stats(shared: &Shared, root: &Path) -> String {
    let s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    let now = units::now_ms();
    let up_s = (now - shared.started).max(1) as f64 / 1000.0;
    let value = |k: &str| s.values.iter().find(|v| v.0 == k).map_or(f64::NAN, |v| v.2);
    let mut o = String::from("{");
    key(&mut o, "version", true);
    string(&mut o, shared.version);
    key(&mut o, "started", false);
    number(&mut o, shared.started as f64 / 1000.0);
    key(&mut o, "uptime_s", false);
    number(&mut o, up_s);
    key(&mut o, "samples", false);
    number(&mut o, s.samples as f64);
    key(&mut o, "sources", false);
    o.push('{');
    key(&mut o, "counters", true);
    o.push_str(if shared.counters.load(Ordering::Relaxed) {
        "true"
    } else {
        "false"
    });
    key(&mut o, "gpu", false);
    o.push_str(if shared.gpu.load(Ordering::Relaxed) {
        "true"
    } else {
        "false"
    });
    o.push('}');
    key(&mut o, "cpu_percent_of_one_core", false);
    o.push('{');
    key(&mut o, "since_start", true);
    number(&mut o, win::own_cpu_100ns() as f64 / 1e5 / up_s);
    key(&mut o, "last_10s", false);
    number(&mut o, value("self.cpu"));
    o.push('}');
    key(&mut o, "memory_bytes", false);
    o.push('{');
    key(&mut o, "private", true);
    number(&mut o, value("self.private"));
    key(&mut o, "working_set_private", false);
    number(&mut o, value("self.ws"));
    o.push('}');
    let names = ["counters", "processes", "gpu", "storage"];
    for (label, arr, div) in [
        ("collect_ms_last", s.ms, 1.0),
        ("collect_ms_avg", s.ms_sum, s.samples.max(1) as f64),
    ] {
        key(&mut o, label, false);
        o.push('{');
        for (i, n) in names.iter().enumerate() {
            key(&mut o, n, i == 0);
            number(&mut o, arr[i] / div);
        }
        o.push('}');
    }
    key(&mut o, "written_bytes", false);
    number(&mut o, s.bytes as f64);
    key(&mut o, "data_bytes", false);
    o.push('{');
    let usage = store::disk_usage(root);
    let total: u64 = usage.iter().map(|u| u.1).sum();
    for (i, (dir, bytes)) in usage.iter().enumerate() {
        key(&mut o, dir, i == 0);
        number(&mut o, *bytes as f64);
    }
    key(&mut o, "total", false);
    number(&mut o, total as f64);
    o.push('}');
    key(&mut o, "errors", false);
    number(&mut o, s.errors as f64);
    key(&mut o, "last_error", false);
    string(&mut o, &s.last_error);
    o.push('}');
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding() {
        assert_eq!(percent_decode("Intel%5BR%5D+Wi-Fi"), "Intel[R] Wi-Fi");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("a%2"), "a%2");
        let q = parse_query("keys=cpu.busy,disk.C%3A.active&step=60");
        assert_eq!(
            list(&q, "keys").unwrap(),
            vec!["cpu.busy", "disk.C:.active"]
        );
        let many = parse_query(&format!("keys={}", vec!["a"; 65].join(",")));
        assert!(list(&many, "keys").is_err());
        assert_eq!(seconds(&q, "step").unwrap(), Some(60_000));
    }
}
