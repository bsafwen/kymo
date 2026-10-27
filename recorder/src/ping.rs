//! Ping: one small ICMP echo a second (as the ping command) to the address the settings name, and
//! another to the router (the default gateway), each on a thread of its own so that one waiting
//! for an answer never holds the other back. Windows' ICMP API needs no administrator rights.
//!
//! Each sample takes what the threads counted since the one before: the mean round trip of the
//! answers, the share of echoes that never came back within a second, and the jitter (the mean
//! change between one answer's round trip and the next). The card also gets the address as
//! resolved, and how long it has been silent.

use crate::json::{key, number, string};
use crate::system::{self, Sample};
use crate::units::{self, Unit};
use std::ffi::c_void;
use std::net::{Ipv4Addr, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[repr(C)]
struct MibIpForwardRow {
    dest: u32,
    mask: u32,
    policy: u32,
    next_hop: u32,
    if_index: u32,
    kind: u32,
    proto: u32,
    age: u32,
    next_hop_as: u32,
    metrics: [u32; 5],
}

#[link(name = "iphlpapi")]
extern "system" {
    fn IcmpCreateFile() -> *mut c_void;
    fn IcmpCloseHandle(handle: *mut c_void) -> i32;
    fn IcmpSendEcho(
        handle: *mut c_void,
        dest: u32,
        request: *const c_void,
        request_size: u16,
        options: *const c_void,
        reply: *mut c_void,
        reply_size: u32,
        timeout_ms: u32,
    ) -> u32;
    fn GetBestRoute(dest: u32, source: u32, row: *mut MibIpForwardRow) -> u32;
}

/// What a target is pinged by: the address, or the router (the default gateway, found anew).
pub const ROUTER: &str = "router";
/// An echo not back within this is lost.
const TIMEOUT_MS: u32 = 1000;
/// A name is looked up again this often (its address may change).
const RESOLVE_MS: i64 = 300_000;

/// The target a setting names: its label and what to ping (an address, a name, or ROUTER); None
/// when ping is off, or set to another address that is still empty.
pub fn target_of(choice: i64, host: &str) -> Option<(String, String)> {
    let (label, what) = match choice {
        1 => ("Cloudflare", "1.1.1.1"),
        2 => ("Google", "8.8.8.8"),
        3 => ("Quad9", "9.9.9.9"),
        4 => ("Your router", ROUTER),
        5 if !host.is_empty() => (host, host),
        _ => return None,
    };
    Some((label.to_string(), what.to_string()))
}

/// What one target's pings came to since the last sample.
#[derive(Default, Clone)]
struct Count {
    sent: u32,
    answered: u32,
    sum_ms: f64,
    jitter_sum: f64,
    jitter_n: u32,
    last_ms: Option<f64>,
}

impl Count {
    fn add(&mut self, rtt: Option<f64>) {
        self.sent += 1;
        if let Some(ms) = rtt {
            self.answered += 1;
            self.sum_ms += ms;
            if let Some(last) = self.last_ms {
                self.jitter_sum += (ms - last).abs();
                self.jitter_n += 1;
            }
            self.last_ms = Some(ms);
        }
    }

    /// Mean round trip (s), loss (%), jitter (s); NaN where there is nothing to tell. Then empty.
    fn take(&mut self) -> Option<(f64, f64, f64)> {
        if self.sent == 0 {
            return None;
        }
        let rtt = if self.answered > 0 {
            self.sum_ms / self.answered as f64 / 1000.0
        } else {
            f64::NAN
        };
        let loss = 100.0 * (self.sent - self.answered) as f64 / self.sent as f64;
        let jitter = if self.jitter_n > 0 {
            self.jitter_sum / self.jitter_n as f64 / 1000.0
        } else {
            f64::NAN
        };
        let last = self.last_ms;
        *self = Count {
            last_ms: last,
            ..Count::default()
        };
        Some((rtt, loss, jitter))
    }
}

#[derive(Default)]
struct State {
    /// The target's label and what to ping; None: off.
    target: Option<(String, String)>,
    router_on: bool,
    /// Bumped at each change of target: a ping in flight for the one before is not counted.
    generation: u64,
    /// The target's address as last resolved, and when (win::tick_ms, a clock set back never
    /// delays a lookup); a lookup that failed is tried again 10 s later.
    address: Option<Ipv4Addr>,
    resolved_at: i64,
    retry_at: i64,
    /// Why the target cannot be pinged now ("no_name": the name is not found; "no_v4": it has
    /// only IPv6 addresses; "no_route": no router), or None.
    problem: Option<&'static str>,
    /// When the target was set, and when it last answered (wall ms).
    since: i64,
    answered_at: i64,
    /// When the target last changed (wall ms; 0 before any change): the card leaves out the pings
    /// of the address before.
    changed_at: i64,
    gateway: Option<Ipv4Addr>,
    /// The router answered at least once (one that ignores ping is no baseline).
    router_answered: bool,
    main: Count,
    router: Count,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn lock() -> std::sync::MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// What to ping, from the settings: started the first time. The router is pinged only beside a
/// target (never with ping off).
pub fn configure(target: Option<(String, String)>, router: bool) {
    let router = router && target.is_some();
    {
        let mut s = lock();
        if s.target != target {
            if s.target.is_some() {
                s.changed_at = units::now_ms();
            }
            s.target = target;
            s.generation += 1;
            s.address = None;
            s.resolved_at = 0;
            s.retry_at = 0;
            s.problem = None;
            s.since = units::now_ms();
            s.answered_at = 0;
            s.main = Count::default();
        }
        if s.router_on != router {
            s.router_on = router;
            s.router = Count::default();
        }
    }
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        for main in [true, false] {
            let spawned = std::thread::Builder::new()
                .name(if main { "ping" } else { "ping router" }.into())
                .spawn(move || pinger(main));
            if spawned.is_err() {
                log!("cannot start a ping thread");
            }
        }
    });
}

/// When the target last changed (wall ms; 0 before any change in this run).
pub fn changed_at() -> i64 {
    lock().changed_at
}

/// The router: the next hop toward the internet (1.1.1.1), None when there is none. Only a route
/// through a gateway counts: a VPN's direct route names this PC's own tunnel address.
fn gateway() -> Option<Ipv4Addr> {
    let mut row: MibIpForwardRow = unsafe { std::mem::zeroed() };
    let dest = u32::from_ne_bytes([1, 1, 1, 1]);
    if unsafe { GetBestRoute(dest, 0, &mut row) } != 0 || row.next_hop == 0 || row.kind != 4 {
        return None; // 4: MIB_IPROUTE_TYPE_INDIRECT
    }
    Some(Ipv4Addr::from(row.next_hop.to_ne_bytes()))
}

/// A name's IPv4 address, or why there is none ("no_name", "no_v4").
fn resolve(host: &str) -> Result<Ipv4Addr, &'static str> {
    if let Ok(a) = host.parse::<Ipv4Addr>() {
        return Ok(a);
    }
    let all: Vec<_> = (host, 0)
        .to_socket_addrs()
        .map_err(|_| "no_name")?
        .collect();
    all.iter()
        .find_map(|a| match a.ip() {
            std::net::IpAddr::V4(v4) => Some(v4),
            _ => None,
        })
        .ok_or(if all.is_empty() { "no_name" } else { "no_v4" })
}

/// One echo: its round trip in ms, or None when no answer came within the timeout.
fn echo(handle: *mut c_void, to: Ipv4Addr) -> Option<f64> {
    let request = *b"kymo ping 32 bytes, as Windows'.";
    let mut reply = [0u64; 32]; // 256 bytes, aligned: the reply and a copy of the request
    let n = unsafe {
        IcmpSendEcho(
            handle,
            u32::from_ne_bytes(to.octets()),
            request.as_ptr().cast(),
            request.len() as u16,
            std::ptr::null(),
            reply.as_mut_ptr().cast(),
            std::mem::size_of_val(&reply) as u32,
            TIMEOUT_MS,
        )
    };
    // ICMP_ECHO_REPLY: address, status (0: success), round trip (ms).
    let words: &[u32] = unsafe { std::slice::from_raw_parts(reply.as_ptr().cast(), 3) };
    if n == 0 || words[1] != 0 {
        return None;
    }
    Some(words[2] as f64)
}

/// The target's address now: the router's, the address as resolved (looked up again every 5
/// minutes; a lookup that fails keeps the address it had, and is tried again 10 s later), or None.
fn address_of(what: &str, generation: u64) -> Option<Ipv4Addr> {
    let tick = crate::win::tick_ms();
    if what == ROUTER {
        let g = gateway();
        let mut s = lock();
        if s.generation == generation {
            s.problem = if g.is_none() { Some("no_route") } else { None };
            s.address = g;
        }
        return g;
    }
    let (known, due) = {
        let s = lock();
        let fresh = s.address.is_some() && tick - s.resolved_at < RESOLVE_MS;
        (s.address, !fresh && tick >= s.retry_at)
    };
    if !due {
        return known;
    }
    let found = resolve(what);
    let mut s = lock();
    if s.generation != generation {
        return None;
    }
    match found {
        Ok(a) => {
            s.address = Some(a);
            s.resolved_at = tick;
            s.problem = None;
        }
        Err(why) => {
            s.retry_at = tick + 10_000;
            if s.address.is_none() {
                s.problem = Some(why);
            }
        }
    }
    s.address
}

/// The target's thread (`main`) or the router's: an echo a second while it has something to ping.
fn pinger(main: bool) {
    let handle = unsafe { IcmpCreateFile() };
    if handle.is_null() || handle as isize == -1 {
        log!("ping: ICMP is not available");
        return;
    }
    let mut gateway_at = i64::MIN;
    loop {
        let started = Instant::now();
        // The router, looked up again each minute (another network, a VPN).
        if !main && crate::win::tick_ms() - gateway_at > 60_000 {
            gateway_at = crate::win::tick_ms();
            let g = gateway();
            let mut s = lock();
            if s.gateway != g {
                s.gateway = g;
                s.router_answered = false;
                s.router = Count::default();
            }
        }
        let (what, generation, gw, router_on) = {
            let s = lock();
            (
                s.target.as_ref().map(|t| t.1.clone()),
                s.generation,
                s.gateway,
                s.router_on,
            )
        };
        let to = if main {
            what.as_deref().and_then(|w| address_of(w, generation))
        } else if router_on && what.as_deref() != Some(ROUTER) {
            gw
        } else {
            None
        };
        match to {
            Some(to) => {
                let rtt = echo(handle, to);
                let mut s = lock();
                if main {
                    if s.generation == generation {
                        s.main.add(rtt);
                        if rtt.is_some() {
                            s.answered_at = units::now_ms();
                        }
                    }
                } else if s.router_on {
                    s.router.add(rtt);
                    s.router_answered |= rtt.is_some();
                }
            }
            // An address that answered before and cannot be found now (offline, no DNS): an outage,
            // counted as lost.
            None if main && what.is_some() => {
                let mut s = lock();
                if s.generation == generation && s.answered_at > 0 {
                    s.main.add(None);
                }
            }
            None => {}
        }
        let spent = started.elapsed();
        std::thread::sleep(
            Duration::from_secs(1)
                .saturating_sub(spent)
                .max(Duration::from_millis(50)),
        );
    }
    #[allow(unreachable_code)]
    unsafe {
        IcmpCloseHandle(handle);
    }
}

/// What the pings came to since the last sample, as its series.
pub fn push_samples(out: &mut Vec<Sample>) {
    let mut s = lock();
    let target = s.target.is_some();
    if let Some((rtt, loss, jitter)) = s.main.take().filter(|_| target) {
        system::push(out, "ping.rtt", Unit::SecondsFine, rtt);
        system::push(out, "ping.loss", Unit::Percent, loss);
        system::push(out, "ping.jitter", Unit::SecondsFine, jitter);
    }
    let router = s.router_answered;
    if let Some((rtt, loss, _)) = s.router.take().filter(|_| router) {
        system::push(out, "ping.router_rtt", Unit::SecondsFine, rtt);
        system::push(out, "ping.router_loss", Unit::Percent, loss);
    }
}

/// `"ping":{...}` after the caller's fields, for the card: the target's label and what is pinged
/// (the address, or the name and the address it resolves to), the router, how long the target has
/// been silent and whether it answered at all since it was set, the setting's `choice` and the
/// address typed last (`typed`), and a problem when there is one.
pub fn write_card(o: &mut String, choice: i64, typed: &str) {
    let s = lock();
    let now = units::now_ms();
    key(o, "ping", false);
    o.push('{');
    key(o, "on", true);
    o.push_str(if s.target.is_some() { "true" } else { "false" });
    key(o, "choice", false);
    number(o, choice as f64);
    key(o, "answered", false);
    o.push_str(if s.answered_at > 0 { "true" } else { "false" });
    if let Some((label, what)) = &s.target {
        key(o, "label", false);
        string(o, label);
        key(o, "host", false);
        string(o, if what == ROUTER { "" } else { what });
        if let Some(a) = s.address {
            key(o, "address", false);
            string(o, &a.to_string());
        }
        // Silent: no answer for 5 s or more (or since it was set).
        let last = if s.answered_at > 0 {
            s.answered_at
        } else {
            s.since
        };
        if now - last >= 5_000 {
            key(o, "silent_s", false);
            number(o, ((now - last) / 1000) as f64);
        }
        if let Some(p) = s.problem {
            key(o, "problem", false);
            string(o, p);
        }
    }
    if let Some(g) = s.gateway {
        key(o, "router", false);
        string(o, &g.to_string());
    }
    key(o, "router_on", false);
    o.push_str(if s.router_on { "true" } else { "false" });
    key(o, "typed", false);
    string(o, typed);
    o.push('}');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_loss_rtt_and_jitter() {
        let mut c = Count::default();
        for r in [Some(10.0), Some(14.0), None, Some(12.0)] {
            c.add(r);
        }
        let (rtt, loss, jitter) = c.take().unwrap();
        assert!((rtt - 0.012).abs() < 1e-9, "{rtt}");
        assert_eq!(loss, 25.0);
        assert!((jitter - 0.003).abs() < 1e-9, "{jitter}"); // |14-10| and |12-14|
        assert!(c.take().is_none());
        // The jitter goes on from the last answer of the sample before.
        c.add(Some(20.0));
        assert!((c.take().unwrap().2 - 0.008).abs() < 1e-9);
        // Nothing answered: the loss only.
        c.add(None);
        let (rtt, loss, jitter) = c.take().unwrap();
        assert!(rtt.is_nan() && loss == 100.0 && jitter.is_nan());
    }

    #[test]
    fn targets_from_the_settings() {
        assert_eq!(
            target_of(1, ""),
            Some(("Cloudflare".into(), "1.1.1.1".into()))
        );
        assert_eq!(
            target_of(4, ""),
            Some(("Your router".into(), ROUTER.into()))
        );
        assert_eq!(
            target_of(5, "example.com"),
            Some(("example.com".into(), "example.com".into()))
        );
        assert_eq!(target_of(5, ""), None);
        assert_eq!(target_of(0, "example.com"), None);
        assert_eq!(resolve("192.0.2.7"), Ok(Ipv4Addr::new(192, 0, 2, 7)));
    }

    #[test]
    #[ignore] // sends real pings (to this PC): cargo test -- --ignored
    fn the_threads_ping_and_report() {
        configure(Some(("here".into(), "127.0.0.1".into())), false);
        std::thread::sleep(Duration::from_millis(3500));
        let mut out = Vec::new();
        push_samples(&mut out);
        let get = |k: &str| out.iter().find(|s| s.key == k).map(|s| s.value);
        assert!(
            get("ping.rtt").is_some_and(|v| (0.0..0.05).contains(&v)),
            "{:?}",
            get("ping.rtt")
        );
        assert_eq!(get("ping.loss"), Some(0.0));
        let mut o = String::from("{\"x\":0");
        write_card(&mut o, 5, "example.com");
        assert!(o.contains("\"ping\":{\"on\":true,\"choice\":5,\"answered\":true,\"label\":\"here\",\"host\":\"127.0.0.1\",\"address\":\"127.0.0.1\""), "{o}");
        assert!(
            o.contains("\"typed\":\"example.com\"") && !o.contains("silent_s"),
            "{o}"
        );
        configure(None, false);
    }

    #[test]
    #[ignore] // sends real pings: cargo test -- --ignored
    fn pings_this_pc() {
        let handle = unsafe { IcmpCreateFile() };
        assert!(echo(handle, Ipv4Addr::LOCALHOST).is_some());
        unsafe { IcmpCloseHandle(handle) };
    }
}
