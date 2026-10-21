//! History on disk: append-only files, one per period, of delta-encoded rows.
//!
//! A file starts with a 12-byte header: "PHR1", kind, version (3; 2 is still read), 2 reserved
//! bytes, interval in seconds. Records follow: u32 length (type + payload), u8 type, payload, u32
//! CRC-32 of type + payload. When a file is reopened, a record cut short by a power cut is dropped; a
//! damaged record with data after it is copied aside (".tail-<time>") before the file is cut back to
//! its last whole record. Readers of version 3 files pass over a record that fails its CRC and go
//! on from the next RESET record (the rows in between cannot be decoded); in version 2 files, which
//! have none, they stop there.
//!
//! System files (kinds 1, 2 and 3: 10-second samples, 1-minute and 1-hour summaries):
//!   SCHEMA (1): column count, then per column: key (varint length + UTF-8), unit code (u8).
//!   ROW    (2): time (zigzag varint, ms after the previous row), number of 10-second samples the
//!               row stands for (1 in sample files), then one varint per value: 0 = missing, else
//!               zigzag(q - previous q of that value) + 1, q = the value in its unit's fixed
//!               resolution (see `Unit`). Summary files hold two values per column: the average and
//!               the maximum over the period.
//! App files (kinds 4 and 5: per app name and minute, per app name and hour):
//!   NAME   (3): id (varint, 0, 1, 2... in order), name.
//!   APPS   (4): time, entry count, then per entry: id, then one varint per app value as in ROW,
//!               delta-coded against the previous entry of the same name in the file. The last
//!               field is the seconds of data the row covers, which weights it.
//! A SCHEMA record resets the previous values; times run on across it.
//!   RESET  (5): seek point (version 3 files), written before every hour's worth of rows (a day's
//!               in hourly files): the time the next row's time counts from, and the latest row
//!               time so far in the file (zigzag varints). The previous values go back to zero, so
//!               a reader can start there: it reads the SCHEMA and NAME records before it and
//!               skips the rows. A query starts at the last RESET whose latest time is not after
//!               its range, which holds even after the clock went back.
//!
//! A row's time is the END of the interval it describes: a 10-second sample at 12:00:10 covers
//! 12:00:00-12:00:10, the 1-minute summary at 12:01:00 covers 12:00:00-12:01:00. A row goes into the
//! file of the period its interval lies in (the period of time - 1 ms). Files are named after their
//! UTC period: raw/2026-10-20.phr, 1m/2026-10-20.phr, 1h/2026-10.phr, apps/2026-10-20.phr,
//! apps-1h/2026-10.phr. A reader decodes from the seek point before its range to the end.
//!
//! Summaries are derived data: at start-up, the buckets the previous run did not finish are rebuilt
//! from the finer files, and a summary that could not be written is kept and written again later.

use crate::procs::AppRow;
use crate::system::Sample;
use crate::units::{self, Unit};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};

/// The sample interval (a setting): the length of a row in sample files.
static SAMPLE_MS: AtomicI64 = AtomicI64::new(10_000);

/// Sets the sample interval (1 to 60 s).
pub fn set_sample_ms(ms: i64) {
    SAMPLE_MS.store(ms.clamp(1000, 60_000), Ordering::Relaxed);
}

const MAGIC: &[u8; 4] = b"PHR1";
/// Version of new files: 3 adds RESET records. Version 2 files are read, and appended to without them.
const VERSION: u8 = 3;
const OLDEST_VERSION: u8 = 2;
const HEADER: u64 = 12;
const MAX_RECORD: u32 = 64 << 20;
pub const T_SCHEMA: u8 = 1;
pub const T_ROW: u8 = 2;
pub const T_NAME: u8 = 3;
pub const T_APPS: u8 = 4;
pub const T_RESET: u8 = 5;
pub const RETENTION_DAYS: i64 = 730;
/// Summaries waiting to be written again are dropped beyond this (they can still be rebuilt).
const MAX_PENDING: usize = 2000;

/// Values of one app in app files: averages over the seconds covered, then (hourly files) maxima.
pub const APP_FIELDS: [(&str, Unit); 9] = [
    ("cpu", Unit::Percent),
    ("user", Unit::Percent),
    ("kernel", Unit::Percent),
    ("read", Unit::BytesPerSec),
    ("write", Unit::BytesPerSec),
    ("ws", Unit::Bytes),
    ("private", Unit::Bytes),
    ("instances", Unit::Count),
    ("seconds", Unit::Count),
];
pub const APP_N: usize = APP_FIELDS.len();
/// Index of the covered seconds among the app fields.
pub const APP_SECONDS: usize = APP_N - 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    Raw,
    Min,
    Hour,
    AppMin,
    AppHour,
}

impl Tier {
    pub const ALL: [Tier; 5] = [
        Tier::Raw,
        Tier::Min,
        Tier::Hour,
        Tier::AppMin,
        Tier::AppHour,
    ];

    pub fn kind(self) -> u8 {
        match self {
            Tier::Raw => 1,
            Tier::Min => 2,
            Tier::Hour => 3,
            Tier::AppMin => 4,
            Tier::AppHour => 5,
        }
    }
    pub fn dir(self) -> &'static str {
        match self {
            Tier::Raw => "raw",
            Tier::Min => "1m",
            Tier::Hour => "1h",
            Tier::AppMin => "apps",
            Tier::AppHour => "apps-1h",
        }
    }
    pub fn len_ms(self) -> i64 {
        match self {
            Tier::Raw => SAMPLE_MS.load(Ordering::Relaxed),
            Tier::Min | Tier::AppMin => 60_000,
            Tier::Hour | Tier::AppHour => 3_600_000,
        }
    }
    /// Values per column: 1 for samples and per-minute app rows, 2 (average, maximum) for summaries.
    pub fn values(self) -> usize {
        match self {
            Tier::Raw | Tier::AppMin => 1,
            _ => 2,
        }
    }
    pub fn period(self, ts: i64) -> String {
        let t = ts - 1;
        match self {
            Tier::Raw | Tier::Min | Tier::AppMin => units::day_name(t),
            Tier::Hour | Tier::AppHour => units::month_name(t),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Tier::Raw => "10s",
            Tier::Min | Tier::AppMin => "1m",
            Tier::Hour | Tier::AppHour => "1h",
        }
    }
    fn is_app(self) -> bool {
        matches!(self, Tier::AppMin | Tier::AppHour)
    }
    /// Rows between two RESET records: an hour's worth, a day's in hourly files.
    fn reset_every(self) -> u32 {
        match self {
            Tier::Raw => 360,
            Tier::Min | Tier::AppMin => 60,
            Tier::Hour | Tier::AppHour => 24,
        }
    }
}

fn reset_payload(base: i64, latest: i64) -> Vec<u8> {
    let mut p = Vec::with_capacity(20);
    put_varint(&mut p, zz(base));
    put_varint(&mut p, zz(latest));
    p
}

/// A RESET record's times: the base of the next row's time, the latest row time before it.
fn reset_times(p: &[u8]) -> Option<(i64, i64)> {
    let mut pos = 0;
    Some((
        unzz(get_varint(p, &mut pos)?),
        unzz(get_varint(p, &mut pos)?),
    ))
}

// ---- encoding ---------------------------------------------------------------------------------

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}
const CRC: [u32; 256] = crc_table();

pub fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c = CRC[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn get_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = *buf.get(*pos)?;
        *pos += 1;
        v |= ((b & 0x7F) as u64) << shift;
        if b < 0x80 {
            return Some(v);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

fn zz(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzz(u: u64) -> i64 {
    (u >> 1) as i64 ^ -((u & 1) as i64)
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn get_str(buf: &[u8], pos: &mut usize) -> Option<String> {
    let n = get_varint(buf, pos)? as usize;
    let s = buf.get(*pos..pos.checked_add(n)?)?;
    *pos += n;
    String::from_utf8(s.to_vec()).ok()
}

fn record(kind: u8, payload: &[u8], out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(&((payload.len() + 1) as u32).to_le_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
    let crc = crc32(&out[start + 4..]);
    out.extend_from_slice(&crc.to_le_bytes());
}

/// Encodes one value against its previous value.
fn put_value(out: &mut Vec<u8>, v: f64, unit: Unit, prev: &mut i64) {
    if !v.is_finite() {
        out.push(0);
        return;
    }
    let q = unit.quantize(v);
    put_varint(out, zz(q.wrapping_sub(*prev)).wrapping_add(1));
    *prev = q;
}

fn get_value(buf: &[u8], pos: &mut usize, unit: Unit, prev: &mut i64) -> Option<f64> {
    let v = get_varint(buf, pos)?;
    if v == 0 {
        return Some(f64::NAN);
    }
    let q = prev.wrapping_add(unzz(v - 1));
    *prev = q;
    Some(unit.value(q))
}

// ---- reading ----------------------------------------------------------------------------------

/// What reading a record's payload gave.
enum Body<'a> {
    Ok(&'a [u8]),
    /// Whole but failing its CRC.
    Bad,
    /// Cut short: the end of the readable file.
    End,
}

/// Reads the whole records of a file, in order; stops at the end or at the first bad record.
pub struct Records {
    r: BufReader<File>,
    buf: Vec<u8>,
    /// Offset just after the last record returned (or passed over).
    pub end: u64,
    pub kind: u8,
    pub version: u8,
}

impl Records {
    pub fn open(path: &Path) -> io::Result<Records> {
        let mut f = File::open(path)?;
        let mut h = [0u8; HEADER as usize];
        f.read_exact(&mut h)?;
        if &h[0..4] != MAGIC || !(OLDEST_VERSION..=VERSION).contains(&h[5]) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a history file of a known version",
            ));
        }
        Ok(Records {
            r: BufReader::with_capacity(128 * 1024, f),
            buf: Vec::new(),
            end: HEADER,
            kind: h[4],
            version: h[5],
        })
    }

    /// The next record's type and length (type + payload); `body` or `skip` must follow.
    fn head(&mut self) -> Option<(u8, u32)> {
        let mut h = [0u8; 5];
        self.r.read_exact(&mut h).ok()?;
        let len = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
        (1..=MAX_RECORD).contains(&len).then_some((h[4], len))
    }

    /// The payload of the record whose head was just read, checked against its CRC; a record that
    /// fails it is passed over (Bad), one cut short ends the file (End).
    fn read(&mut self, kind: u8, len: u32) -> Body<'_> {
        self.buf.resize(len as usize + 4, 0);
        self.buf[0] = kind;
        if self.r.read_exact(&mut self.buf[1..]).is_err() {
            return Body::End;
        }
        self.end += 8 + len as u64;
        let (body, crc) = self.buf.split_at(len as usize);
        if crc32(body) != u32::from_le_bytes([crc[0], crc[1], crc[2], crc[3]]) {
            return Body::Bad;
        }
        Body::Ok(&self.buf[1..len as usize])
    }

    /// The payload of the record whose head was just read, if whole and checked.
    fn body(&mut self, kind: u8, len: u32) -> Option<&[u8]> {
        match self.read(kind, len) {
            Body::Ok(p) => Some(p),
            _ => None,
        }
    }

    /// Passes over the record whose head was just read, unchecked (a reader that later reads the
    /// records after it still checks those).
    fn skip(&mut self, len: u32) -> Option<()> {
        self.r.seek_relative(len as i64 + 3).ok()?;
        self.end += 8 + len as u64;
        Some(())
    }

    pub fn next(&mut self) -> Option<(u8, &[u8])> {
        let (kind, len) = self.head()?;
        let payload = self.body(kind, len)?;
        Some((kind, payload))
    }

    /// Where to read rows after `lo` from: the offset of the last RESET record that no earlier row
    /// is later than (0: from the start), and of the last SCHEMA record before it. Only record
    /// heads are read, up to the first RESET past `lo` (their latest times only grow). Leaves the
    /// reader back at the first record.
    fn seek_point(&mut self, lo: i64) -> (u64, u64) {
        let (mut best, mut schema, mut best_schema) = (0, 0, 0);
        loop {
            let at = self.end;
            let Some((kind, len)) = self.head() else {
                break;
            };
            if kind == T_RESET {
                match self.body(kind, len).and_then(reset_times) {
                    Some((_, latest)) if latest <= lo => (best, best_schema) = (at, schema),
                    _ => break,
                }
            } else {
                if kind == T_SCHEMA {
                    schema = at;
                }
                if self.skip(len).is_none() {
                    break;
                }
            }
        }
        self.seek(HEADER);
        (best, best_schema)
    }

    /// Continues at `offset`, the start of a record.
    fn seek(&mut self, offset: u64) {
        let _ = self.r.seek(SeekFrom::Start(offset));
        self.end = offset;
    }
}

/// Decoding state of a system file.
pub struct RowState {
    pub cols: Vec<(String, Unit)>,
    pub values: usize,
    prev: Vec<i64>,
    pub ts: i64,
}

impl RowState {
    pub fn new(values: usize) -> RowState {
        RowState {
            cols: Vec::new(),
            values,
            prev: Vec::new(),
            ts: 0,
        }
    }

    pub fn schema(&mut self, p: &[u8]) -> Option<()> {
        let mut pos = 0;
        let n = get_varint(p, &mut pos)? as usize;
        let mut cols = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            let key = get_str(p, &mut pos)?;
            let unit = Unit::from_code(*p.get(pos)?)?;
            pos += 1;
            cols.push((key, unit));
        }
        self.prev = vec![0; cols.len() * self.values];
        self.cols = cols;
        Some(())
    }

    /// A RESET record: previous values back to zero, the next time counts from the one it gives.
    pub fn reset(&mut self, p: &[u8]) -> Option<()> {
        self.ts = reset_times(p)?.0;
        self.prev.fill(0);
        Some(())
    }

    /// Decodes a ROW into `out` (NaN = missing; `values` per column); returns its time and the
    /// number of 10-second samples it stands for.
    pub fn row(&mut self, p: &[u8], out: &mut Vec<f64>) -> Option<(i64, u32)> {
        let mut pos = 0;
        let ts = self.ts.wrapping_add(unzz(get_varint(p, &mut pos)?));
        let count = get_varint(p, &mut pos)?.min(u32::MAX as u64) as u32;
        out.clear();
        for i in 0..self.prev.len() {
            let unit = self.cols[i / self.values].1;
            out.push(get_value(p, &mut pos, unit, &mut self.prev[i])?);
        }
        self.ts = ts;
        Some((ts, count))
    }

    fn encode_schema(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_varint(&mut out, self.cols.len() as u64);
        for (k, u) in &self.cols {
            put_str(&mut out, k);
            out.push(u.code());
        }
        out
    }

    fn encode_row(&mut self, ts: i64, count: u32, vals: &[f64]) -> Vec<u8> {
        let mut out = Vec::with_capacity(vals.len() * 2 + 8);
        put_varint(&mut out, zz(ts.wrapping_sub(self.ts)));
        put_varint(&mut out, count as u64);
        for (i, &v) in vals.iter().enumerate() {
            let unit = self.cols[i / self.values].1;
            put_value(&mut out, v, unit, &mut self.prev[i]);
        }
        self.ts = ts;
        out
    }
}

pub type AppVals = [f64; 2 * APP_N];

/// Decoding state of an app file.
pub struct AppState {
    pub names: Vec<String>,
    pub values: usize,
    prev: Vec<[i64; 2 * APP_N]>,
    pub ts: i64,
}

impl AppState {
    pub fn new(values: usize) -> AppState {
        AppState {
            names: Vec::new(),
            values,
            prev: Vec::new(),
            ts: 0,
        }
    }

    pub fn name(&mut self, p: &[u8]) -> Option<()> {
        let mut pos = 0;
        let id = get_varint(p, &mut pos)? as usize;
        let name = get_str(p, &mut pos)?;
        if id != self.names.len() {
            return None;
        }
        self.names.push(name);
        self.prev.push([0; 2 * APP_N]);
        Some(())
    }

    /// A RESET record: every app's previous values back to zero, the next time counts from the one
    /// it gives.
    pub fn reset(&mut self, p: &[u8]) -> Option<()> {
        self.ts = reset_times(p)?.0;
        self.prev.iter_mut().for_each(|v| *v = [0; 2 * APP_N]);
        Some(())
    }

    /// Decodes an APPS record into `out` (name id, values); returns its time.
    pub fn row(&mut self, p: &[u8], out: &mut Vec<(usize, AppVals)>) -> Option<i64> {
        let mut pos = 0;
        let ts = self.ts.wrapping_add(unzz(get_varint(p, &mut pos)?));
        let n = get_varint(p, &mut pos)? as usize;
        out.clear();
        for _ in 0..n {
            let id = get_varint(p, &mut pos)? as usize;
            let prev = self.prev.get_mut(id)?;
            let mut v = [f64::NAN; 2 * APP_N];
            for (i, slot) in v.iter_mut().enumerate().take(APP_N * self.values) {
                *slot = get_value(p, &mut pos, APP_FIELDS[i % APP_N].1, &mut prev[i])?;
            }
            out.push((id, v));
        }
        self.ts = ts;
        Some(ts)
    }
}

// ---- writing ----------------------------------------------------------------------------------

fn file_path(root: &Path, tier: Tier, period: &str) -> PathBuf {
    root.join(tier.dir()).join(format!("{period}.phr"))
}

/// Is what follows the last whole record a record cut short: a plausible length that runs past the
/// end of the file, by less than one record? (A damaged length field is not.)
fn torn_tail(path: &Path, valid: u64, len: u64) -> bool {
    let rest = len - valid;
    if rest < 8 {
        return true;
    }
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut b = [0u8; 4];
    if f.seek(SeekFrom::Start(valid)).is_err() || f.read_exact(&mut b).is_err() {
        return false;
    }
    let declared = u32::from_le_bytes(b) as u64;
    (1..=MAX_RECORD as u64).contains(&declared) && rest < 8 + declared && rest <= 1 << 20
}

/// Opens a data file for appending. A new file gets its header; an existing one is read to the end
/// (`replay` sees every record). A record cut short at the end is dropped; a damaged record followed
/// by more data is copied aside first, so nothing readable is destroyed. An unreadable file (bad
/// header, unknown version) is renamed aside and a new one started. Returns the file, positioned at
/// its end, and its version.
fn open_append(
    root: &Path,
    tier: Tier,
    period: &str,
    mut replay: impl FnMut(u8, &[u8]) -> bool,
) -> io::Result<(File, u8)> {
    let path = file_path(root, tier, period);
    fs::create_dir_all(path.parent().unwrap_or(root))?;
    let mut valid = 0u64;
    let mut version = VERSION;
    if path.exists() {
        if let Ok(mut rec) = Records::open(&path) {
            if rec.kind == tier.kind() {
                valid = HEADER;
                version = rec.version;
                while let Some((t, p)) = rec.next() {
                    if !replay(t, p) {
                        break;
                    }
                    valid = rec.end;
                }
            }
        }
        if valid == 0 {
            let aside = path.with_extension(format!("bad-{}", units::now_ms()));
            log!(
                "{}: unreadable; renamed to {}",
                path.display(),
                aside.display()
            );
            fs::rename(&path, &aside)?;
        }
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    if valid == 0 {
        version = VERSION;
        file.set_len(0)?;
        let mut h = Vec::with_capacity(HEADER as usize);
        h.extend_from_slice(MAGIC);
        h.push(tier.kind());
        h.push(VERSION);
        h.extend_from_slice(&[0, 0]);
        h.extend_from_slice(&((tier.len_ms() / 1000) as u32).to_le_bytes());
        file.write_all(&h)?;
        valid = HEADER;
    } else {
        let len = file.metadata()?.len();
        if len > valid {
            if torn_tail(&path, valid, len) {
                log!(
                    "{}: dropped a record cut short ({} bytes)",
                    path.display(),
                    len - valid
                );
            } else {
                let aside = path.with_extension(format!("tail-{}", units::now_ms()));
                let mut tail = vec![0u8; (len - valid) as usize];
                file.seek(SeekFrom::Start(valid))?;
                file.read_exact(&mut tail)?;
                fs::write(&aside, &tail)?;
                log!(
                    "{}: damaged record at byte {valid}; the {} bytes after it are in {}",
                    path.display(),
                    tail.len(),
                    aside.display()
                );
            }
            file.set_len(valid)?;
        }
    }
    file.seek(SeekFrom::Start(valid))?;
    Ok((file, version))
}

/// One value of a column: (key, unit, average, maximum).
pub struct Point<'a> {
    pub key: &'a str,
    pub unit: Unit,
    pub avg: f64,
    pub max: f64,
}

/// Where the RESET records of a file being written stand.
#[derive(Default)]
struct Seeks {
    /// Whether the file has them (version 3).
    on: bool,
    /// Rows since the last one.
    rows: u32,
    /// Latest row time in the file.
    latest: i64,
}

impl Seeks {
    fn replayed(&mut self, kind: u8, ts: i64) {
        if kind == T_RESET {
            self.rows = 0;
        } else {
            self.rows += 1;
            self.latest = self.latest.max(ts);
        }
    }

    /// The RESET record due before the next row, if one is; `base` is the time it counts from.
    fn due(&mut self, tier: Tier, base: i64, buf: &mut Vec<u8>) -> bool {
        if !self.on || self.rows < tier.reset_every() {
            return false;
        }
        record(T_RESET, &reset_payload(base, self.latest), buf);
        self.rows = 0;
        true
    }

    fn wrote(&mut self, ts: i64) {
        self.rows += 1;
        self.latest = self.latest.max(ts);
    }
}

pub struct RowWriter {
    tier: Tier,
    pub period: String,
    file: File,
    state: RowState,
    index: HashMap<String, usize>,
    seeks: Seeks,
}

impl RowWriter {
    pub fn open(root: &Path, tier: Tier, period: &str) -> io::Result<RowWriter> {
        let mut state = RowState::new(tier.values());
        let mut scratch = Vec::new();
        let mut seeks = Seeks {
            latest: i64::MIN,
            ..Seeks::default()
        };
        let (file, version) = open_append(root, tier, period, |t, p| match t {
            T_SCHEMA => state.schema(p).is_some(),
            T_ROW => match state.row(p, &mut scratch) {
                Some((ts, _)) => {
                    seeks.replayed(T_ROW, ts);
                    true
                }
                None => false,
            },
            T_RESET => {
                seeks.replayed(T_RESET, 0);
                state.reset(p).is_some()
            }
            _ => true,
        })?;
        seeks.on = version >= 3;
        let index = state
            .cols
            .iter()
            .enumerate()
            .map(|(i, c)| (c.0.clone(), i))
            .collect();
        Ok(RowWriter {
            tier,
            period: period.to_string(),
            file,
            state,
            index,
            seeks,
        })
    }

    /// Appends one row standing for `count` samples; returns the bytes written. A new key adds a
    /// column (and a SCHEMA record).
    pub fn write(&mut self, ts: i64, count: u32, points: &[Point]) -> io::Result<u64> {
        let mut grew = self.state.cols.is_empty();
        for p in points {
            if !self.index.contains_key(p.key) {
                self.index.insert(p.key.to_string(), self.state.cols.len());
                self.state.cols.push((p.key.to_string(), p.unit));
                grew = true;
            }
        }
        let mut buf = Vec::new();
        if grew {
            self.state.prev = vec![0; self.state.cols.len() * self.state.values];
            record(T_SCHEMA, &self.state.encode_schema(), &mut buf);
        }
        if self.seeks.due(self.tier, self.state.ts, &mut buf) {
            self.state.prev.fill(0);
        }
        let vpc = self.tier.values();
        let mut vals = vec![f64::NAN; self.state.cols.len() * vpc];
        for p in points {
            let i = self.index[p.key] * vpc;
            vals[i] = p.avg;
            if vpc == 2 {
                vals[i + 1] = p.max;
            }
        }
        let payload = self.state.encode_row(ts, count, &vals);
        record(T_ROW, &payload, &mut buf);
        self.file.write_all(&buf)?;
        self.seeks.wrote(ts);
        Ok(buf.len() as u64)
    }
}

/// One app over one period: averages in `v[..APP_N]`, maxima in `v[APP_N..]` (summaries only).
#[derive(Clone)]
pub struct AppValues {
    pub name: String,
    pub v: AppVals,
}

pub struct AppWriter {
    tier: Tier,
    pub period: String,
    file: File,
    state: AppState,
    ids: HashMap<String, usize>,
    seeks: Seeks,
}

impl AppWriter {
    pub fn open(root: &Path, tier: Tier, period: &str) -> io::Result<AppWriter> {
        let mut state = AppState::new(tier.values());
        let mut scratch = Vec::new();
        let mut seeks = Seeks {
            latest: i64::MIN,
            ..Seeks::default()
        };
        let (file, version) = open_append(root, tier, period, |t, p| match t {
            T_NAME => state.name(p).is_some(),
            T_APPS => match state.row(p, &mut scratch) {
                Some(ts) => {
                    seeks.replayed(T_APPS, ts);
                    true
                }
                None => false,
            },
            T_RESET => {
                seeks.replayed(T_RESET, 0);
                state.reset(p).is_some()
            }
            _ => true,
        })?;
        seeks.on = version >= 3;
        let ids = state
            .names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), i))
            .collect();
        Ok(AppWriter {
            tier,
            period: period.to_string(),
            file,
            state,
            ids,
            seeks,
        })
    }

    pub fn write(&mut self, ts: i64, rows: &[AppValues]) -> io::Result<u64> {
        let mut buf = Vec::new();
        let mut added = Vec::new();
        for r in rows {
            if !self.ids.contains_key(&r.name) {
                let id = self.state.names.len() + added.len();
                let mut p = Vec::new();
                put_varint(&mut p, id as u64);
                put_str(&mut p, &r.name);
                record(T_NAME, &p, &mut buf);
                self.ids.insert(r.name.clone(), id);
                added.push(r.name.clone());
            }
        }
        for name in added {
            self.state.names.push(name);
            self.state.prev.push([0; 2 * APP_N]);
        }
        if self.seeks.due(self.tier, self.state.ts, &mut buf) {
            self.state.prev.iter_mut().for_each(|v| *v = [0; 2 * APP_N]);
        }
        let n = APP_N * self.tier.values();
        let mut p = Vec::with_capacity(rows.len() * (n + 2) + 8);
        put_varint(&mut p, zz(ts.wrapping_sub(self.state.ts)));
        put_varint(&mut p, rows.len() as u64);
        for r in rows {
            let id = self.ids[&r.name];
            put_varint(&mut p, id as u64);
            let prev = &mut self.state.prev[id];
            for i in 0..n {
                put_value(&mut p, r.v[i], APP_FIELDS[i % APP_N].1, &mut prev[i]);
            }
        }
        self.state.ts = ts;
        record(T_APPS, &p, &mut buf);
        self.file.write_all(&buf)?;
        self.seeks.wrote(ts);
        Ok(buf.len() as u64)
    }
}

// ---- summaries --------------------------------------------------------------------------------

fn bucket_end(ts: i64, len: i64) -> i64 {
    ((ts - 1).div_euclid(len) + 1) * len
}

/// The time each sample stands for, from the spacing of the samples (the interval can change at
/// any time, and the files do not say what it was): the time since the sample before, when that is
/// a plausible step at the pace so far (within 2.5 times it, or 25 s); else, after a gap, the pace.
#[derive(Clone, Copy)]
pub struct Pace {
    prev: i64,
    pace: i64,
}

impl Pace {
    /// Rows without a stored length were all written at the 10-second interval of that time.
    pub fn new() -> Pace {
        Pace {
            prev: i64::MIN,
            pace: 10_000,
        }
    }

    /// The milliseconds the sample at `ts` stands for (samples come in time order): `known` when
    /// the row says it (rows written since the interval became a setting), else from the spacing.
    pub fn next(&mut self, ts: i64, known: Option<i64>) -> i64 {
        let d = if self.prev == i64::MIN {
            i64::MAX
        } else {
            ts.saturating_sub(self.prev)
        };
        self.prev = ts;
        if let Some(len) = known {
            self.pace = len.min(60_000);
            return len;
        }
        if d <= 0 {
            return self.pace;
        }
        // A plausible step at the pace so far is the sample's time, and the new pace (no interval is
        // longer than a minute); a longer step is a gap, never a pace.
        if d <= (self.pace * 5 / 2).max(25_000) {
            self.pace = d.min(60_000);
            d
        } else {
            self.pace
        }
    }
}

/// A sample row's count: the time it stands for, in tenths of a second (1 to 150 s: one interval,
/// or more after the recorder skipped a slot). Rows written before the interval could change hold
/// 1, and their time comes from their spacing (`Pace`).
pub fn raw_count(len_ms: i64) -> u32 {
    (len_ms / 100).clamp(10, 1500) as u32
}

/// The time a sample row stands for, when the row says it.
pub fn raw_len(count: u32) -> Option<i64> {
    (count >= 10).then_some(count as i64 * 100)
}

/// The weight of a summary row: the time its samples stand for, in 10-second units (the number of
/// 10-second samples, as rows were counted before the interval could change).
pub fn weight_units(ms: f64) -> u32 {
    ((ms / 10_000.0).round() as u32).max(1)
}

/// Average and maximum per column over fixed periods, each sample weighted by the time it stands
/// for (so a stretch at 1 s does not outweigh one at 10 s).
pub struct Rollup {
    len: i64,
    end: i64,
    pace: Pace,
    covered: f64,
    cols: Vec<(String, Unit)>,
    index: HashMap<String, usize>,
    sum: Vec<f64>,
    w: Vec<f64>,
    max: Vec<f64>,
}

pub struct Bucket {
    pub end: i64,
    /// The time its samples stand for, in 10-second units (`weight_units`).
    pub count: u32,
    pub points: Vec<(String, Unit, f64, f64)>,
}

impl Rollup {
    pub fn new(len: i64) -> Rollup {
        Rollup {
            len,
            end: 0,
            pace: Pace::new(),
            covered: 0.0,
            cols: Vec::new(),
            index: HashMap::new(),
            sum: Vec::new(),
            w: Vec::new(),
            max: Vec::new(),
        }
    }

    /// Adds a sample standing for `len` ms (when known); returns the period that ended before it,
    /// if any.
    pub fn add<'a>(
        &mut self,
        ts: i64,
        len: Option<i64>,
        row: impl Iterator<Item = (&'a str, Unit, f64)>,
    ) -> Option<Bucket> {
        let end = bucket_end(ts, self.len);
        let done = if self.end != 0 && end != self.end {
            self.take()
        } else {
            None
        };
        self.end = end;
        let dt = self.pace.next(ts, len) as f64;
        self.covered += dt;
        for (k, u, v) in row {
            if !v.is_finite() {
                continue;
            }
            let i = match self.index.get(k) {
                Some(&i) => i,
                None => {
                    self.index.insert(k.to_string(), self.cols.len());
                    self.cols.push((k.to_string(), u));
                    self.sum.push(0.0);
                    self.w.push(0.0);
                    self.max.push(f64::NEG_INFINITY);
                    self.cols.len() - 1
                }
            };
            self.sum[i] += v * dt;
            self.w[i] += dt;
            if v > self.max[i] {
                self.max[i] = v;
            }
        }
        done
    }

    pub fn take(&mut self) -> Option<Bucket> {
        if self.end == 0 {
            return None;
        }
        let mut points = Vec::new();
        for i in 0..self.cols.len() {
            if self.w[i] > 0.0 {
                let (k, u) = &self.cols[i];
                points.push((k.clone(), *u, self.sum[i] / self.w[i], self.max[i]));
            }
            self.sum[i] = 0.0;
            self.w[i] = 0.0;
            self.max[i] = f64::NEG_INFINITY;
        }
        let (end, count) = (self.end, weight_units(self.covered));
        self.end = 0;
        self.covered = 0.0;
        (!points.is_empty()).then_some(Bucket { end, count, points })
    }
}

/// Hourly app summaries from per-minute app rows, weighted by the seconds each minute covers.
/// A minute without the app counts as 0 in its averages.
pub struct AppRollup {
    end: i64,
    seconds: f64,
    apps: HashMap<String, ([f64; APP_N], [f64; APP_N])>,
}

impl AppRollup {
    pub fn new() -> AppRollup {
        AppRollup {
            end: 0,
            seconds: 0.0,
            apps: HashMap::new(),
        }
    }

    /// Adds one minute (all its apps); returns the hour that ended before it, if any.
    pub fn add(&mut self, ts: i64, rows: &[(&str, &[f64])]) -> Option<(i64, Vec<AppValues>)> {
        let end = bucket_end(ts, 3_600_000);
        let done = if self.end != 0 && end != self.end {
            self.take()
        } else {
            None
        };
        self.end = end;
        let secs = rows
            .iter()
            .map(|(_, v)| v[APP_SECONDS])
            .find(|s| s.is_finite())
            .unwrap_or(0.0);
        self.seconds += secs;
        for (name, v) in rows {
            let e = self
                .apps
                .entry(name.to_string())
                .or_insert(([0.0; APP_N], [f64::NEG_INFINITY; APP_N]));
            for i in 0..APP_SECONDS {
                if v[i].is_finite() {
                    e.0[i] += v[i] * secs;
                    e.1[i] = e.1[i].max(v[i]);
                }
            }
        }
        done
    }

    pub fn take(&mut self) -> Option<(i64, Vec<AppValues>)> {
        let (end, secs) = (self.end, self.seconds);
        self.end = 0;
        self.seconds = 0.0;
        if end == 0 || secs <= 0.0 {
            self.apps.clear();
            return None;
        }
        let mut rows: Vec<AppValues> = self
            .apps
            .drain()
            .map(|(name, (sum, max))| {
                let mut v = [f64::NAN; 2 * APP_N];
                for i in 0..APP_SECONDS {
                    v[i] = sum[i] / secs;
                    v[APP_N + i] = if max[i].is_finite() { max[i] } else { f64::NAN };
                }
                v[APP_SECONDS] = secs;
                AppValues { name, v }
            })
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        Some((end, rows))
    }
}

// ---- the store --------------------------------------------------------------------------------

/// Data files of a tier whose rows may fall in (lo, hi], oldest first.
pub fn files(root: &Path, tier: Tier, lo: i64, hi: i64) -> Vec<PathBuf> {
    let mut out: Vec<(i64, PathBuf)> = Vec::new();
    let Ok(entries) = fs::read_dir(root.join(tier.dir())) else {
        return Vec::new();
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("phr") {
            continue;
        }
        let Some((start, end)) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(units::period_range)
        else {
            continue;
        };
        if start < hi && end > lo {
            out.push((start, path));
        }
    }
    out.sort();
    out.into_iter().map(|(_, p)| p).collect()
}

/// Calls `f` with each system row of a tier with a time in (lo, hi]: time, samples it stands
/// for, columns, values.
pub fn each_row(
    root: &Path,
    tier: Tier,
    lo: i64,
    hi: i64,
    mut f: impl FnMut(i64, u32, &[(String, Unit)], &[f64]),
) {
    for path in files(root, tier, lo, hi) {
        file_rows(&path, tier, lo, hi, &mut f);
    }
}

/// Where a reader of rows after `lo` in this file starts, and the SCHEMA record that applies there
/// (see Records::seek_point); a range that starts before the file's period reads it all, without
/// looking.
fn start_of(rec: &mut Records, path: &Path, lo: i64) -> (u64, u64) {
    let period_start = path
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(units::period_range)
        .map_or(i64::MIN, |p| p.0);
    if rec.version >= 3 && lo > period_start {
        rec.seek_point(lo)
    } else {
        (0, 0)
    }
}

fn file_rows(
    path: &Path,
    tier: Tier,
    lo: i64,
    hi: i64,
    f: &mut impl FnMut(i64, u32, &[(String, Unit)], &[f64]),
) {
    let mut vals = Vec::new();
    let Ok(mut rec) = Records::open(path) else {
        return;
    };
    if rec.kind != tier.kind() {
        return;
    }
    let mut st = RowState::new(tier.values());
    // From a seek point: its SCHEMA record, then the RESET record itself.
    let (start, schema) = start_of(&mut rec, path, lo);
    if start > 0 {
        rec.seek(schema);
        match rec.next() {
            Some((T_SCHEMA, p)) if st.schema(p).is_some() => rec.seek(start),
            _ => rec.seek(HEADER),
        }
    }
    // After a damaged record, rows are passed over until the next RESET record (version 3), so a
    // read from a seek point and a read from the start return the same rows.
    let mut lost = false;
    let resyncs = rec.version >= 3;
    loop {
        let Some((t, len)) = rec.head() else { break };
        if t == T_ROW && lost {
            if rec.skip(len).is_none() {
                break;
            }
            continue;
        }
        let p = match rec.read(t, len) {
            Body::Ok(p) => p,
            Body::Bad if resyncs => {
                lost = true;
                continue;
            }
            _ => break,
        };
        match t {
            T_SCHEMA => {
                if st.schema(p).is_none() {
                    break;
                }
            }
            T_RESET => {
                if st.reset(p).is_none() {
                    break;
                }
                lost = false;
            }
            T_ROW => {
                let Some((ts, count)) = st.row(p, &mut vals) else {
                    break;
                };
                if ts > lo && ts <= hi {
                    f(ts, count, &st.cols, &vals);
                }
            }
            _ => {}
        }
    }
}

/// Calls `f` with each app row of a tier with a time in (lo, hi]. `f` also gets the names of the
/// file and a counter that changes whenever a new file starts (name ids are per file).
pub fn each_app_row(
    root: &Path,
    tier: Tier,
    lo: i64,
    hi: i64,
    mut f: impl FnMut(i64, usize, &[String], &[(usize, AppVals)]),
) {
    for (file_no, path) in files(root, tier, lo, hi).into_iter().enumerate() {
        file_app_rows(
            &path,
            tier,
            lo,
            hi,
            &mut |ts, names: &[String], rows: &[(usize, AppVals)]| f(ts, file_no, names, rows),
        );
    }
}

fn file_app_rows(
    path: &Path,
    tier: Tier,
    lo: i64,
    hi: i64,
    f: &mut impl FnMut(i64, &[String], &[(usize, AppVals)]),
) {
    let mut rows = Vec::new();
    let Ok(mut rec) = Records::open(path) else {
        return;
    };
    if rec.kind != tier.kind() {
        return;
    }
    let start = start_of(&mut rec, path, lo).0;
    let mut st = AppState::new(tier.values());
    let mut lost = false; // after a damaged record, until the next RESET (as file_rows)
    let resyncs = rec.version >= 3;
    loop {
        let at = rec.end;
        let Some((t, len)) = rec.head() else { break };
        // Rows before the seek point are passed over; the NAME and RESET records are read.
        if t == T_APPS && (at < start || lost) {
            if rec.skip(len).is_none() {
                break;
            }
            continue;
        }
        let p = match rec.read(t, len) {
            Body::Ok(p) => p,
            Body::Bad if resyncs => {
                lost = true;
                continue;
            }
            _ => break,
        };
        match t {
            T_NAME => {
                if st.name(p).is_none() {
                    break;
                }
            }
            T_RESET => {
                if st.reset(p).is_none() {
                    break;
                }
                lost = false;
            }
            T_APPS => {
                let Some(ts) = st.row(p, &mut rows) else {
                    break;
                };
                if ts > lo && ts <= hi {
                    f(ts, &st.names, &rows);
                }
            }
            _ => {}
        }
    }
}

/// Latest row time of a tier in (lo, hi], if any: the newest file that has a row there decides,
/// so this reads one file in the usual case.
fn max_ts(root: &Path, tier: Tier, lo: i64, hi: i64) -> Option<i64> {
    for path in files(root, tier, lo, hi).into_iter().rev() {
        let mut last: Option<i64> = None;
        let mut keep = |ts: i64| last = Some(last.map_or(ts, |l| l.max(ts)));
        if tier.is_app() {
            file_app_rows(
                &path,
                tier,
                lo,
                hi,
                &mut |ts, _: &[String], _: &[(usize, AppVals)]| keep(ts),
            );
        } else {
            file_rows(
                &path,
                tier,
                lo,
                hi,
                &mut |ts, _, _: &[(String, Unit)], _: &[f64]| keep(ts),
            );
        }
        if last.is_some() {
            return last;
        }
    }
    None
}

enum Pending {
    Rows(Tier, Bucket),
    Apps(Tier, i64, Vec<AppValues>),
}

impl Pending {
    fn describe(&self) -> String {
        match self {
            Pending::Rows(tier, b) => {
                format!("{} summary at {}", tier.dir(), units::fmt_time(b.end))
            }
            Pending::Apps(tier, end, _) => {
                format!("{} rows at {}", tier.dir(), units::fmt_time(*end))
            }
        }
    }
}

/// First wait before writing failed summaries again; it doubles up to RETRY_MAX.
const RETRY_FIRST: i64 = 60_000;
const RETRY_MAX: i64 = 300_000;

/// How long each tier is kept, in days (in Tier::ALL order), and the most the whole history may
/// use in bytes (0: no limit).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Keep {
    pub days: [i64; 5],
    pub budget: u64,
}

impl Default for Keep {
    fn default() -> Keep {
        Keep {
            days: [RETENTION_DAYS; 5],
            budget: 0,
        }
    }
}

/// One file of a tier: the period it covers and its size.
pub struct FileInfo {
    pub start: i64,
    pub end: i64,
    pub bytes: u64,
}

/// The data files of a tier, oldest first.
pub fn tier_files(root: &Path, tier: Tier) -> Vec<FileInfo> {
    files(root, tier, i64::MIN, i64::MAX)
        .into_iter()
        .filter_map(|p| {
            let (start, end) = p
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(units::period_range)?;
            Some(FileInfo {
                start,
                end,
                bytes: fs::metadata(&p).map_or(0, |m| m.len()),
            })
        })
        .collect()
}

/// The files set aside after damage in a tier's folder (`<period>.bad-<time>`, `<period>.tail-<time>`),
/// oldest period first.
pub fn aside_files(root: &Path, tier: Tier) -> Vec<(PathBuf, FileInfo)> {
    let mut out: Vec<(PathBuf, FileInfo)> = Vec::new();
    let Ok(entries) = fs::read_dir(root.join(tier.dir())) else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        let ext = path.extension().and_then(|x| x.to_str()).unwrap_or("");
        if !(ext.starts_with("bad-") || ext.starts_with("tail-")) {
            continue;
        }
        let Some((start, end)) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(units::period_range)
        else {
            continue;
        };
        let bytes = fs::metadata(&path).map_or(0, |m| m.len());
        out.push((path, FileInfo { start, end, bytes }));
    }
    out.sort_by_key(|(_, f)| f.start);
    out
}

fn remove(path: &Path, why: &str) -> Option<u64> {
    let bytes = fs::metadata(path).map_or(0, |m| m.len());
    match fs::remove_file(path) {
        Ok(()) => {
            log!("{why}: deleted {}", path.display());
            Some(bytes)
        }
        Err(e) => {
            log!("{why}: {}: {e}", path.display());
            None
        }
    }
}

pub struct Store {
    root: PathBuf,
    keep: Keep,
    raw: Option<RowWriter>,
    min: Option<RowWriter>,
    hour: Option<RowWriter>,
    apps: Option<AppWriter>,
    apps_hour: Option<AppWriter>,
    roll_min: Rollup,
    roll_hour: Rollup,
    roll_apps: AppRollup,
    /// Summaries that could not be written, oldest first, tried again with a growing wait.
    pending: Vec<Pending>,
    retry_at: i64,
    retry_wait: i64,
    pub bytes: u64,
}

impl Store {
    /// Opens the history and finishes the summaries the previous run left open. Summary write
    /// errors are logged and retried later; they never stop recording.
    pub fn open(root: &Path, now: i64) -> io::Result<Store> {
        fs::create_dir_all(root)?;
        let mut s = Store {
            root: root.to_path_buf(),
            keep: Keep::default(),
            raw: None,
            min: None,
            hour: None,
            apps: None,
            apps_hour: None,
            roll_min: Rollup::new(Tier::Min.len_ms()),
            roll_hour: Rollup::new(Tier::Hour.len_ms()),
            roll_apps: AppRollup::new(),
            pending: Vec::new(),
            retry_at: 0,
            retry_wait: RETRY_FIRST,
            bytes: 0,
        };
        s.rebuild(now);
        Ok(s)
    }

    fn rebuild(&mut self, now: i64) {
        let root = self.root.clone();
        let since = now - RETENTION_DAYS * units::DAY_MS;
        let last_min = max_ts(&root, Tier::Min, since, now).unwrap_or(since);
        let last_hour = max_ts(&root, Tier::Hour, since, now).unwrap_or(since);
        let mut rows = 0u64;
        each_row(
            &root,
            Tier::Raw,
            last_min.min(last_hour),
            now,
            |ts, count, cols, vals| {
                rows += 1;
                let row = || {
                    cols.iter()
                        .zip(vals)
                        .map(|((k, u), &v)| (k.as_str(), *u, v))
                };
                if ts > last_min {
                    if let Some(b) = self.roll_min.add(ts, raw_len(count), row()) {
                        self.write_bucket(Tier::Min, b);
                    }
                }
                if ts > last_hour {
                    if let Some(b) = self.roll_hour.add(ts, raw_len(count), row()) {
                        self.write_bucket(Tier::Hour, b);
                    }
                }
            },
        );
        let last_app_hour = max_ts(&root, Tier::AppHour, since, now).unwrap_or(since);
        each_app_row(
            &root,
            Tier::AppMin,
            last_app_hour,
            now,
            |ts, _, names, entries| {
                let minute: Vec<(&str, &[f64])> = entries
                    .iter()
                    .map(|(id, v)| (names[*id].as_str(), &v[..]))
                    .collect();
                if let Some((end, done)) = self.roll_apps.add(ts, &minute) {
                    self.write_apps_bucket(Tier::AppHour, end, done);
                }
            },
        );
        if rows > 0 {
            log!("summaries brought up to date from {rows} samples");
        }
    }

    fn row_slot(&mut self, tier: Tier) -> &mut Option<RowWriter> {
        match tier {
            Tier::Raw => &mut self.raw,
            Tier::Min => &mut self.min,
            _ => &mut self.hour,
        }
    }

    fn write_rows(&mut self, tier: Tier, ts: i64, count: u32, points: &[Point]) -> io::Result<()> {
        let period = tier.period(ts);
        let root = self.root.clone();
        let slot = self.row_slot(tier);
        if slot.as_ref().map_or(true, |w| w.period != period) {
            *slot = None;
            *slot = Some(RowWriter::open(&root, tier, &period)?);
        }
        let res = slot.as_mut().map(|w| w.write(ts, count, points));
        match res {
            Some(Ok(n)) => {
                self.bytes += n;
                Ok(())
            }
            Some(Err(e)) => {
                // Reopened (and cut back to its last whole record) on the next write.
                *self.row_slot(tier) = None;
                Err(e)
            }
            None => Ok(()),
        }
    }

    fn write_app_rows(&mut self, tier: Tier, ts: i64, rows: &[AppValues]) -> io::Result<()> {
        let period = tier.period(ts);
        let root = self.root.clone();
        let slot = if tier == Tier::AppMin {
            &mut self.apps
        } else {
            &mut self.apps_hour
        };
        if slot.as_ref().map_or(true, |w| w.period != period) {
            *slot = None;
            *slot = Some(AppWriter::open(&root, tier, &period)?);
        }
        let res = slot.as_mut().map(|w| w.write(ts, rows));
        match res {
            Some(Ok(n)) => {
                self.bytes += n;
                Ok(())
            }
            Some(Err(e)) => {
                if tier == Tier::AppMin {
                    self.apps = None;
                } else {
                    self.apps_hour = None;
                }
                Err(e)
            }
            None => Ok(()),
        }
    }

    fn try_write(&mut self, p: &Pending) -> io::Result<()> {
        match p {
            Pending::Rows(tier, b) => {
                let points: Vec<Point> = b
                    .points
                    .iter()
                    .map(|(k, u, a, m)| Point {
                        key: k,
                        unit: *u,
                        avg: *a,
                        max: *m,
                    })
                    .collect();
                self.write_rows(*tier, b.end, b.count, &points)
            }
            Pending::Apps(tier, end, rows) => self.write_app_rows(*tier, *end, rows),
        }
    }

    /// Writes a finished summary. While earlier ones wait to be written again, it waits behind them.
    fn put(&mut self, p: Pending, now: i64) {
        if self.pending.is_empty() {
            match self.try_write(&p) {
                Ok(()) => return,
                Err(e) => {
                    log!(
                        "{}: {e}; kept, and tried again in 1 to 5 minutes",
                        p.describe()
                    );
                    self.retry_wait = RETRY_FIRST;
                    self.retry_at = now + RETRY_FIRST;
                }
            }
        }
        if self.pending.len() < MAX_PENDING {
            self.pending.push(p);
        }
    }

    fn write_bucket(&mut self, tier: Tier, b: Bucket) {
        let now = b.end;
        self.put(Pending::Rows(tier, b), now);
    }

    fn write_apps_bucket(&mut self, tier: Tier, end: i64, rows: Vec<AppValues>) {
        self.put(Pending::Apps(tier, end, rows), end);
    }

    /// Writes the waiting summaries in order; stops at the first that still fails.
    fn retry_pending(&mut self, now: i64) {
        if self.pending.is_empty() || now < self.retry_at {
            return;
        }
        let mut list = std::mem::take(&mut self.pending).into_iter();
        while let Some(p) = list.next() {
            if self.try_write(&p).is_err() {
                self.pending.push(p);
                self.pending.extend(list);
                self.retry_wait = (self.retry_wait * 2).min(RETRY_MAX);
                self.retry_at = now + self.retry_wait;
                return;
            }
        }
        log!("the waiting summaries are written");
    }

    /// Stores one 10-second sample and feeds the summaries. Only the sample's own write error is
    /// returned; summaries are retried on their own.
    /// Stores one sample standing for `len` ms (its rates cover that: one interval, or more when
    /// the recorder skipped a slot; the first sample after a start, a gap or a new interval is a
    /// baseline, never written).
    pub fn write_sample(&mut self, ts: i64, len: i64, samples: &[Sample]) -> io::Result<()> {
        self.retry_pending(ts);
        let points: Vec<Point> = samples
            .iter()
            .map(|s| Point {
                key: &s.key,
                unit: s.unit,
                avg: s.value,
                max: s.value,
            })
            .collect();
        let result = self.write_rows(Tier::Raw, ts, raw_count(len), &points);
        let row = || samples.iter().map(|s| (s.key.as_str(), s.unit, s.value));
        if let Some(b) = self.roll_min.add(ts, Some(len), row()) {
            self.write_bucket(Tier::Min, b);
        }
        if let Some(b) = self.roll_hour.add(ts, Some(len), row()) {
            self.write_bucket(Tier::Hour, b);
        }
        result
    }

    /// Stores one minute of app rows and feeds the hourly app summary.
    pub fn write_apps(&mut self, end: i64, rows: &[AppRow]) -> io::Result<()> {
        let mut values: Vec<AppValues> = rows
            .iter()
            .map(|r| {
                let mut v = [f64::NAN; 2 * APP_N];
                v[..APP_N].copy_from_slice(&r.v);
                AppValues {
                    name: r.name.clone(),
                    v,
                }
            })
            .collect();
        values.sort_by(|a, b| a.name.cmp(&b.name));
        let result = self.write_app_rows(Tier::AppMin, end, &values);
        let minute: Vec<(&str, &[f64])> =
            rows.iter().map(|r| (r.name.as_str(), &r.v[..])).collect();
        if let Some((hour_end, done)) = self.roll_apps.add(end, &minute) {
            self.write_apps_bucket(Tier::AppHour, hour_end, done);
        }
        result
    }

    /// The period of the file a writer of this tier has open, if any.
    fn open_period(&self, tier: Tier) -> Option<&str> {
        match tier {
            Tier::Raw => self.raw.as_ref().map(|w| w.period.as_str()),
            Tier::Min => self.min.as_ref().map(|w| w.period.as_str()),
            Tier::Hour => self.hour.as_ref().map(|w| w.period.as_str()),
            Tier::AppMin => self.apps.as_ref().map(|w| w.period.as_str()),
            Tier::AppHour => self.apps_hour.as_ref().map(|w| w.period.as_str()),
        }
    }

    pub fn set_keep(&mut self, keep: Keep) {
        self.keep = keep;
    }

    /// Deletes the files of each tier whose whole period ended more than its retention ago. On
    /// the hourly run, at most one per tier (normally one a day expires), so that a clock set far
    /// ahead by mistake cannot wipe the history at once; right after the retention was shortened on
    /// the Settings page (`catch_up`), all of them. Then the history is brought under its budget.
    /// Returns the files and bytes deleted.
    pub fn retention(&self, now: i64, catch_up: bool) -> (usize, u64) {
        let (mut n, mut bytes) = (0usize, 0u64);
        for (i, tier) in Tier::ALL.into_iter().enumerate() {
            let cutoff = now - self.keep.days[i] * units::DAY_MS;
            let expired = files(&self.root, tier, i64::MIN, cutoff)
                .into_iter()
                .filter(|p| {
                    p.file_stem()
                        .and_then(|s| s.to_str())
                        .and_then(units::period_range)
                        .is_some_and(|(_, e)| e <= cutoff)
                });
            for path in expired.take(if catch_up { usize::MAX } else { 1 }) {
                if let Some(b) = remove(&path, "retention") {
                    n += 1;
                    bytes += b;
                }
            }
            // Files set aside after damage go with the period they belong to.
            for (path, _) in aside_files(&self.root, tier)
                .into_iter()
                .filter(|(_, f)| f.end <= cutoff)
            {
                if let Some(b) = remove(&path, "retention") {
                    n += 1;
                    bytes += b;
                }
            }
        }
        let (bn, bb) = self.trim_to_budget();
        (n + bn, bytes + bb)
    }

    /// While the history uses more than its budget, deletes the files set aside after damage, oldest
    /// first, then the oldest file of the first tier in this order that has more than one: samples,
    /// apps per minute, minutes, apps per hour, hours.
    fn trim_to_budget(&self) -> (usize, u64) {
        let (mut n, mut bytes) = (0usize, 0u64);
        if self.keep.budget == 0 {
            return (0, 0);
        }
        let mut total: u64 = disk_usage(&self.root).iter().map(|u| u.1).sum();
        while total > self.keep.budget {
            let aside = Tier::ALL
                .into_iter()
                .flat_map(|t| aside_files(&self.root, t))
                .min_by_key(|(_, f)| f.start)
                .map(|(p, _)| p);
            // Never the newest file of a tier, nor one a writer has open (after the clock went back,
            // that can be an old period).
            let victim = aside.or_else(|| {
                [
                    Tier::Raw,
                    Tier::AppMin,
                    Tier::Min,
                    Tier::AppHour,
                    Tier::Hour,
                ]
                .into_iter()
                .find_map(|t| {
                    let list = files(&self.root, t, i64::MIN, i64::MAX);
                    let open = self.open_period(t);
                    let older = &list[..list.len().saturating_sub(1)];
                    older
                        .iter()
                        .find(|p| p.file_stem().and_then(|x| x.to_str()) != open)
                        .cloned()
                })
            });
            let Some(path) = victim else {
                log!("the history uses {} MB, over its budget, and only the files being written are left", total / 1_000_000);
                break;
            };
            match remove(&path, "budget") {
                Some(b) => {
                    n += 1;
                    bytes += b;
                    total = total.saturating_sub(b);
                }
                None => break,
            }
        }
        (n, bytes)
    }

    /// Deletes the history of `tiers`: the files whose whole period ended by `before`, or, without
    /// it, everything, the files being written included (their writers close first, and the
    /// summaries in progress start afresh). Returns the files and bytes deleted.
    pub fn delete(&mut self, tiers: &[Tier], before: Option<i64>) -> (usize, u64) {
        let (mut n, mut bytes) = (0usize, 0u64);
        for &tier in tiers {
            let targets: Vec<PathBuf> = match before {
                Some(b) => files(&self.root, tier, i64::MIN, b)
                    .into_iter()
                    .filter(|p| {
                        p.file_stem()
                            .and_then(|s| s.to_str())
                            .and_then(units::period_range)
                            .is_some_and(|(_, e)| e <= b)
                    })
                    .chain(
                        aside_files(&self.root, tier)
                            .into_iter()
                            .filter(|(_, f)| f.end <= b)
                            .map(|(p, _)| p),
                    )
                    .collect(),
                None => {
                    match tier {
                        Tier::Raw | Tier::Min | Tier::Hour => *self.row_slot(tier) = None,
                        Tier::AppMin => self.apps = None,
                        Tier::AppHour => self.apps_hour = None,
                    }
                    // Everything in the folder, the files set aside after damage included.
                    fs::read_dir(self.root.join(tier.dir()))
                        .map(|d| {
                            d.flatten()
                                .map(|e| e.path())
                                .filter(|p| p.is_file())
                                .collect()
                        })
                        .unwrap_or_default()
                }
            };
            for path in targets {
                if let Some(b) = remove(&path, "deleted on request") {
                    n += 1;
                    bytes += b;
                }
            }
        }
        if before.is_none() {
            if tiers.iter().any(|t| matches!(t, Tier::Raw | Tier::Min)) {
                self.roll_min = Rollup::new(Tier::Min.len_ms());
            }
            if tiers.iter().any(|t| matches!(t, Tier::Raw | Tier::Hour)) {
                self.roll_hour = Rollup::new(Tier::Hour.len_ms());
            }
            if tiers
                .iter()
                .any(|t| matches!(t, Tier::AppMin | Tier::AppHour))
            {
                self.roll_apps = AppRollup::new();
            }
            self.pending.retain(|p| match p {
                Pending::Rows(t, _) | Pending::Apps(t, _, _) => !tiers.contains(t),
            });
        }
        (n, bytes)
    }

    /// Time of the newest sample on disk, however old (for start-up checks).
    pub fn newest_sample(&self) -> Option<i64> {
        max_ts(&self.root, Tier::Raw, i64::MIN, i64::MAX)
            .or_else(|| max_ts(&self.root, Tier::Hour, i64::MIN, i64::MAX))
    }
}

/// Time of the newest 10-second sample on disk (reads the newest day's file).
pub fn newest_raw(root: &Path) -> Option<i64> {
    max_ts(root, Tier::Raw, i64::MIN, i64::MAX)
}

/// Bytes on disk per tier (each file's real size, also for files being written).
pub fn disk_usage(root: &Path) -> Vec<(&'static str, u64)> {
    Tier::ALL
        .iter()
        .map(|t| {
            let total = fs::read_dir(root.join(t.dir()))
                .map(|d| {
                    d.flatten()
                        .filter_map(|e| fs::metadata(e.path()).ok())
                        .map(|m| m.len())
                        .sum()
                })
                .unwrap_or(0);
            (t.dir(), total)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("kymo-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn sample(key: &str, unit: Unit, value: f64) -> Sample {
        Sample {
            key: key.to_string(),
            unit,
            value,
        }
    }

    const T0: i64 = 1_792_454_400_000; // 2026-10-20 00:00 UTC

    #[test]
    fn varints() {
        for v in [0i64, 1, -1, 63, -64, 1 << 40, i64::MAX / 4, i64::MIN / 4] {
            let mut b = Vec::new();
            put_varint(&mut b, zz(v));
            let mut pos = 0;
            assert_eq!(unzz(get_varint(&b, &mut pos).unwrap()), v);
        }
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn rows_round_trip_and_recover() {
        let root = tmp("rows");
        {
            let mut s = Store::open(&root, T0).unwrap();
            for i in 1..=400i64 {
                let mut v = vec![
                    sample("cpu.busy", Unit::Percent, (i % 100) as f64 + 0.25),
                    sample("mem.available", Unit::Bytes, 1e10 + i as f64 * 4096.0),
                ];
                if i > 200 {
                    v.push(sample("net.rx_bps", Unit::BytesPerSec, 1234.0));
                }
                if i == 300 {
                    v.remove(0);
                }
                s.write_sample(T0 + i * 10_000, 10_000, &v).unwrap();
            }
        }
        // A record cut short at the end is dropped on reopening.
        let raw = file_path(&root, Tier::Raw, "2026-10-20");
        OpenOptions::new()
            .append(true)
            .open(&raw)
            .unwrap()
            .write_all(&[9, 0, 0, 0, 2, 1, 2])
            .unwrap();
        let mut n = 0;
        let mut seen_missing = false;
        each_row(&root, Tier::Raw, 0, i64::MAX, |ts, count, cols, vals| {
            n += 1;
            assert_eq!(
                raw_len(count),
                Some(10_000),
                "each sample row says it stands for the 10 s interval"
            );
            let i = (ts - T0) / 10_000;
            let busy = cols.iter().position(|c| c.0 == "cpu.busy").unwrap();
            if i == 300 {
                assert!(vals[busy].is_nan());
                seen_missing = true;
            } else {
                assert!((vals[busy] - ((i % 100) as f64 + 0.25)).abs() < 1e-9);
            }
            let mem = cols.iter().position(|c| c.0 == "mem.available").unwrap();
            assert!((vals[mem] - (1e10 + i as f64 * 4096.0)).abs() <= 500.0);
        });
        assert_eq!(n, 400);
        assert!(seen_missing);
        {
            let mut s = Store::open(&root, T0 + 401 * 10_000).unwrap();
            s.write_sample(
                T0 + 401 * 10_000,
                10_000,
                &[sample("cpu.busy", Unit::Percent, 7.0)],
            )
            .unwrap();
        }
        // Readable only if the cut-short record was dropped before appending.
        let mut last = 0.0;
        each_row(
            &root,
            Tier::Raw,
            T0 + 400 * 10_000,
            i64::MAX,
            |_, _, cols, vals| {
                last = vals[cols.iter().position(|c| c.0 == "cpu.busy").unwrap()];
            },
        );
        assert_eq!(last, 7.0);
        // 400 samples = 66 whole minutes, each standing for 6 samples.
        let mut minutes = 0;
        each_row(&root, Tier::Min, 0, i64::MAX, |ts, count, cols, vals| {
            minutes += 1;
            assert_eq!(ts % 60_000, 0);
            assert_eq!(count, 6);
            let i = cols.iter().position(|c| c.0 == "cpu.busy").unwrap();
            assert!(vals[2 * i + 1] >= vals[2 * i]);
        });
        assert_eq!(minutes, 66);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn damaged_record_is_kept_aside() {
        let root = tmp("damaged");
        {
            let mut s = Store::open(&root, T0).unwrap();
            for i in 1..=50i64 {
                s.write_sample(
                    T0 + i * 10_000,
                    10_000,
                    &[sample("cpu.busy", Unit::Percent, i as f64)],
                )
                .unwrap();
            }
        }
        let raw = file_path(&root, Tier::Raw, "2026-10-20");
        let mut bytes = fs::read(&raw).unwrap();
        // Damage the CRC of the 25th record: a whole record that fails its check, with more after it.
        let mut off = HEADER as usize;
        for _ in 0..24 {
            off += 8 + u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
        }
        let len = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
        bytes[off + 4 + len] ^= 0xFF;
        fs::write(&raw, &bytes).unwrap();
        {
            let mut s = Store::open(&root, T0 + 51 * 10_000).unwrap();
            s.write_sample(
                T0 + 51 * 10_000,
                10_000,
                &[sample("cpu.busy", Unit::Percent, 51.0)],
            )
            .unwrap();
        }
        let aside = fs::read_dir(root.join("raw"))
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains(".tail-"));
        assert!(aside, "the damaged tail is copied aside");
        let mut last = 0.0;
        each_row(&root, Tier::Raw, 0, i64::MAX, |_, _, _, vals| {
            last = vals[0]
        });
        assert_eq!(last, 51.0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn damaged_length_is_not_taken_for_a_cut_short_record() {
        let root = tmp("length");
        {
            let mut s = Store::open(&root, T0).unwrap();
            for i in 1..=50i64 {
                s.write_sample(
                    T0 + i * 10_000,
                    10_000,
                    &[sample("cpu.busy", Unit::Percent, i as f64)],
                )
                .unwrap();
            }
        }
        let raw = file_path(&root, Tier::Raw, "2026-10-20");
        let mut bytes = fs::read(&raw).unwrap();
        let mut off = HEADER as usize;
        for _ in 0..10 {
            off += 8 + u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
        }
        bytes[off..off + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        fs::write(&raw, &bytes).unwrap();
        Store::open(&root, T0)
            .unwrap()
            .write_sample(
                T0 + 51 * 10_000,
                10_000,
                &[sample("cpu.busy", Unit::Percent, 51.0)],
            )
            .unwrap();
        let aside = fs::read_dir(root.join("raw"))
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains(".tail-"));
        assert!(aside, "everything after a damaged length is copied aside");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn apps_round_trip() {
        let root = tmp("apps");
        {
            let mut s = Store::open(&root, T0).unwrap();
            for m in 1..=130i64 {
                // The first minute of each hour covers only 30 s.
                let secs = if m % 60 == 1 { 30.0 } else { 60.0 };
                let rows = vec![
                    AppRow {
                        name: "chrome".into(),
                        v: [10.0, 8.0, 2.0, 100.0, 50.0, 2e9, 3e9, 12.0, secs],
                    },
                    AppRow {
                        name: format!("app{}", m % 3),
                        v: [m as f64, 0.0, 0.0, 0.0, 0.0, 1e6, 2e6, 1.0, secs],
                    },
                ];
                s.write_apps(T0 + m * 60_000, &rows).unwrap();
            }
        }
        let mut n = 0;
        each_app_row(&root, Tier::AppMin, 0, i64::MAX, |_, _, names, rows| {
            n += 1;
            let chrome = rows.iter().find(|(id, _)| names[*id] == "chrome").unwrap();
            assert_eq!(chrome.1[0], 10.0);
            assert_eq!(chrome.1[5], 2e9);
        });
        assert_eq!(n, 130);
        let mut hours = Vec::new();
        each_app_row(&root, Tier::AppHour, 0, i64::MAX, |_, _, names, rows| {
            let chrome = rows.iter().find(|(id, _)| names[*id] == "chrome").unwrap();
            assert_eq!(chrome.1[0], 10.0);
            assert_eq!(chrome.1[APP_N + 7], 12.0);
            hours.push(chrome.1[APP_SECONDS]);
        });
        assert_eq!(hours, vec![30.0 + 59.0 * 60.0, 30.0 + 59.0 * 60.0]);
        let _ = fs::remove_dir_all(&root);
    }

    /// Every row of a tier in (lo, hi] as (time, count, key = value text) lines.
    fn rows_text(root: &Path, tier: Tier, lo: i64, hi: i64) -> Vec<String> {
        let mut out = Vec::new();
        each_row(root, tier, lo, hi, |ts, count, cols, vals| {
            let vpc = tier.values();
            let mut line = format!("{ts} {count}");
            for (c, (k, _)) in cols.iter().enumerate() {
                for j in 0..vpc {
                    let v = vals[c * vpc + j];
                    if v.is_finite() {
                        line.push_str(&format!(" {k}={v}"));
                    }
                }
            }
            out.push(line);
        });
        out
    }

    fn points(i: i64) -> Vec<(String, Unit, f64)> {
        let mut v = vec![
            ("cpu.busy".to_string(), Unit::Percent, (i % 97) as f64),
            (
                "mem.available".to_string(),
                Unit::Bytes,
                1e10 + (i * 7919 % 1000) as f64 * 4096.0,
            ),
        ];
        if (700..1300).contains(&i) {
            v.push(("net.rx_bps".to_string(), Unit::BytesPerSec, i as f64));
        }
        v
    }

    fn write_raw(w: &mut RowWriter, i: i64, ts: i64) {
        let p = points(i);
        let pts: Vec<Point> = p
            .iter()
            .map(|(k, u, v)| Point {
                key: k,
                unit: *u,
                avg: *v,
                max: *v,
            })
            .collect();
        w.write(ts, 1, &pts).unwrap();
    }

    #[test]
    fn seek_points_give_the_same_rows() {
        let root = tmp("seek");
        {
            let mut w = RowWriter::open(&root, Tier::Raw, "2026-10-20").unwrap();
            for i in 1..=2000i64 {
                write_raw(&mut w, i, T0 + i * 10_000);
            }
        }
        // Reopened and appended to: the RESET records carry on where they were.
        {
            let mut w = RowWriter::open(&root, Tier::Raw, "2026-10-20").unwrap();
            for i in 2001..=2500i64 {
                write_raw(&mut w, i, T0 + i * 10_000);
            }
        }
        let bytes = fs::read(file_path(&root, Tier::Raw, "2026-10-20")).unwrap();
        let mut resets = 0;
        let mut off = HEADER as usize;
        while off + 5 <= bytes.len() {
            let len = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
            resets += (bytes[off + 4] == T_RESET) as usize;
            off += 8 + len;
        }
        assert_eq!(resets, 2500 / 360);
        let all = rows_text(&root, Tier::Raw, 0, i64::MAX);
        assert_eq!(all.len(), 2500);
        for i in [
            1i64, 359, 360, 361, 700, 1299, 1300, 2000, 2400, 2499, 2500, 3000,
        ] {
            let lo = T0 + i * 10_000 - 5_000;
            let hi = lo + 600_000;
            let want: Vec<String> = all
                .iter()
                .filter(|l| l.split(' ').next().unwrap().parse::<i64>().unwrap() > lo)
                .filter(|l| l.split(' ').next().unwrap().parse::<i64>().unwrap() <= hi)
                .cloned()
                .collect();
            assert_eq!(rows_text(&root, Tier::Raw, lo, hi), want, "from sample {i}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn seek_points_after_the_clock_went_back() {
        let root = tmp("seekback");
        {
            let mut w = RowWriter::open(&root, Tier::Raw, "2026-10-20").unwrap();
            for i in 1..=1000i64 {
                write_raw(&mut w, i, T0 + i * 10_000);
            }
            // The clock goes back 600 samples: the same day's times again.
            for i in 1001..=2000i64 {
                write_raw(&mut w, i, T0 + (i - 600) * 10_000);
            }
        }
        let all = rows_text(&root, Tier::Raw, 0, i64::MAX);
        assert_eq!(all.len(), 2000);
        for i in [300i64, 450, 900, 1300] {
            let lo = T0 + i * 10_000;
            let want: Vec<String> = all
                .iter()
                .filter(|l| l.split(' ').next().unwrap().parse::<i64>().unwrap() > lo)
                .cloned()
                .collect();
            assert_eq!(rows_text(&root, Tier::Raw, lo, i64::MAX), want, "after {i}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_damaged_row_costs_the_rows_up_to_the_next_seek_point() {
        let root = tmp("damagedseek");
        {
            let mut w = RowWriter::open(&root, Tier::Raw, "2026-10-20").unwrap();
            for i in 1..=1000i64 {
                write_raw(&mut w, i, T0 + i * 10_000);
            }
        }
        // Spoil the CRC of the 500th row (between the RESET records before rows 361 and 721).
        let path = file_path(&root, Tier::Raw, "2026-10-20");
        let mut bytes = fs::read(&path).unwrap();
        let (mut off, mut rows) = (HEADER as usize, 0);
        loop {
            let len = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
            if bytes[off + 4] == T_ROW {
                rows += 1;
                if rows == 500 {
                    bytes[off + 4 + len] ^= 0xFF;
                    break;
                }
            }
            off += 8 + len;
        }
        fs::write(&path, &bytes).unwrap();
        let all = rows_text(&root, Tier::Raw, 0, i64::MAX);
        // Rows 1-499 and 721-1000.
        assert_eq!(all.len(), 499 + 280);
        assert!(all[499].starts_with(&format!("{} ", T0 + 721 * 10_000)));
        for i in [100i64, 450, 600, 800] {
            let lo = T0 + i * 10_000;
            let want: Vec<String> = all
                .iter()
                .filter(|l| l.split(' ').next().unwrap().parse::<i64>().unwrap() > lo)
                .cloned()
                .collect();
            assert_eq!(rows_text(&root, Tier::Raw, lo, i64::MAX), want, "after {i}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn version_2_files_are_read_and_get_no_seek_points() {
        let root = tmp("v2");
        {
            let mut w = RowWriter::open(&root, Tier::Raw, "2026-10-20").unwrap();
            for i in 1..=100i64 {
                write_raw(&mut w, i, T0 + i * 10_000);
            }
        }
        let path = file_path(&root, Tier::Raw, "2026-10-20");
        let mut bytes = fs::read(&path).unwrap();
        bytes[5] = 2;
        fs::write(&path, &bytes).unwrap();
        {
            let mut w = RowWriter::open(&root, Tier::Raw, "2026-10-20").unwrap();
            for i in 101..=1000i64 {
                write_raw(&mut w, i, T0 + i * 10_000);
            }
        }
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes[5], 2);
        let mut off = HEADER as usize;
        while off + 5 <= bytes.len() {
            assert_ne!(bytes[off + 4], T_RESET);
            off += 8 + u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
        }
        assert_eq!(
            rows_text(&root, Tier::Raw, T0 + 500 * 10_000, i64::MAX).len(),
            500
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn app_seek_points_give_the_same_rows() {
        let root = tmp("appseek");
        {
            let mut w = AppWriter::open(&root, Tier::AppMin, "2026-10-20").unwrap();
            for m in 1..=300i64 {
                let mut rows = vec![AppValues {
                    name: "chrome".into(),
                    v: [f64::NAN; 2 * APP_N],
                }];
                rows[0].v[..APP_N].copy_from_slice(&[
                    m as f64,
                    1.0,
                    2.0,
                    3.0,
                    4.0,
                    5e8 + m as f64 * 4096.0,
                    6e8,
                    3.0,
                    60.0,
                ]);
                // A new app every 7 minutes, so names keep coming after the seek points.
                let mut extra = AppValues {
                    name: format!("app{}", m / 7),
                    v: [f64::NAN; 2 * APP_N],
                };
                extra.v[..APP_N].copy_from_slice(&[
                    (m % 13) as f64,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                    1e6,
                    2e6,
                    1.0,
                    60.0,
                ]);
                rows.push(extra);
                w.write(T0 + m * 60_000, &rows).unwrap();
            }
        }
        let text = |lo: i64| {
            let mut out = Vec::new();
            each_app_row(&root, Tier::AppMin, lo, i64::MAX, |ts, _, names, rows| {
                for (id, v) in rows {
                    out.push(format!("{ts} {} {:?}", names[*id], &v[..APP_N]));
                }
            });
            out
        };
        let all = text(0);
        assert_eq!(all.len(), 600);
        for m in [30i64, 61, 119, 120, 121, 250] {
            let lo = T0 + m * 60_000;
            let want: Vec<String> = all
                .iter()
                .filter(|l| l.split(' ').next().unwrap().parse::<i64>().unwrap() > lo)
                .cloned()
                .collect();
            assert_eq!(text(lo), want, "after minute {m}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    fn day_files(root: &Path, tier: Tier, days: &[&str], size: usize) {
        fs::create_dir_all(root.join(tier.dir())).unwrap();
        for d in days {
            fs::write(
                root.join(tier.dir()).join(format!("{d}.phr")),
                vec![0u8; size],
            )
            .unwrap();
        }
    }

    fn names(root: &Path, tier: Tier) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(root.join(tier.dir()))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn retention_per_tier_catch_up_and_budget() {
        let root = tmp("keep");
        let days = [
            "2026-10-14",
            "2026-10-15",
            "2026-10-16",
            "2026-10-17",
            "2026-10-18",
            "2026-10-19",
            "2026-10-20",
        ];
        day_files(&root, Tier::Raw, &days, 1000);
        day_files(&root, Tier::AppMin, &days, 1000);
        let mut s = Store::open(&root, T0).unwrap();
        // Samples 3 days, apps 30: the hourly run deletes one expired file, a catch-up all of them.
        s.set_keep(Keep {
            days: [3, 730, 730, 30, 730],
            budget: 0,
        });
        let now = T0 + 86_400_000; // 2026-10-21 00:00
        assert_eq!(s.retention(now, false).0, 1);
        assert_eq!(s.retention(now, true).0, 3);
        assert_eq!(
            names(&root, Tier::Raw),
            ["2026-10-18.phr", "2026-10-19.phr", "2026-10-20.phr"]
        );
        assert_eq!(names(&root, Tier::AppMin).len(), 7);
        // A 4 KB budget: samples go first, down to the one being written, then apps.
        s.set_keep(Keep {
            days: [3, 730, 730, 30, 730],
            budget: 4000,
        });
        s.retention(now, false);
        assert_eq!(names(&root, Tier::Raw), ["2026-10-20.phr"]);
        assert_eq!(
            names(&root, Tier::AppMin),
            ["2026-10-18.phr", "2026-10-19.phr", "2026-10-20.phr"]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn files_set_aside_go_with_their_period_and_first_for_the_budget() {
        let root = tmp("aside");
        day_files(
            &root,
            Tier::Raw,
            &["2026-10-16", "2026-10-19", "2026-10-20"],
            1000,
        );
        let dir = root.join(Tier::Raw.dir());
        fs::write(dir.join("2026-10-16.tail-1"), vec![0u8; 3000]).unwrap();
        fs::write(dir.join("2026-10-19.bad-2"), vec![0u8; 3000]).unwrap();
        let mut s = Store::open(&root, T0).unwrap();
        let now = T0 + 86_400_000; // 2026-10-21 00:00
                                   // Samples kept 3 days: 2026-10-16 goes, with its set-aside tail.
        s.set_keep(Keep {
            days: [3, 730, 730, 730, 730],
            budget: 0,
        });
        s.retention(now, true);
        assert_eq!(aside_files(&root, Tier::Raw).len(), 1);
        // Over budget: the set-aside file goes before any history.
        let total: u64 = disk_usage(&root).iter().map(|u| u.1).sum();
        s.set_keep(Keep {
            days: [3, 730, 730, 730, 730],
            budget: total - 100,
        });
        s.retention(now, false);
        assert!(aside_files(&root, Tier::Raw).is_empty());
        assert_eq!(
            names(&root, Tier::Raw),
            ["2026-10-19.phr", "2026-10-20.phr"]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn pace_follows_the_spacing_but_never_takes_a_gap_for_a_pace() {
        let mut p = Pace::new();
        let mut t = 1_792_454_400_000;
        for _ in 0..5 {
            t += 10_000;
            p.next(t, None);
        }
        assert_eq!(
            p.next(t + 120_000, None),
            10_000,
            "a 2-minute stop is a gap"
        );
        assert_eq!(
            p.next(t + 240_000, None),
            10_000,
            "and the pace stays: the next 2-minute stop is one too"
        );
        // A row that says its length is taken at its word, and sets the pace.
        assert_eq!(p.next(t + 470_000, Some(60_000)), 60_000);
        assert_eq!(p.next(t + 530_000, None), 60_000);
    }

    #[test]
    fn summaries_weigh_samples_by_the_time_they_stand_for() {
        // Half a minute at 10 s (10 %), then half a minute at 1 s (90 %): 50 %, not 82 %.
        let mut r = Rollup::new(60_000);
        let row = |v: f64| std::iter::once(("cpu.busy", Unit::Percent, v));
        let t0 = 1_792_454_400_000;
        r.add(t0, None, row(0.0)); // the minute before
        let mut t = t0;
        for _ in 0..3 {
            t += 10_000;
            assert!(r.add(t, None, row(10.0)).is_none() || t == t0 + 10_000);
        }
        for _ in 0..30 {
            t += 1_000;
            r.add(t, None, row(90.0));
        }
        let b = r.add(t + 1_000, None, row(0.0)).unwrap();
        assert_eq!(b.end, t0 + 60_000);
        assert!((b.points[0].2 - 50.0).abs() < 1e-9, "{}", b.points[0].2);
        assert_eq!(b.count, 6);
    }

    #[test]
    fn delete_before_or_everything() {
        let root = tmp("delete");
        {
            let mut s = Store::open(&root, T0).unwrap();
            for i in 1..=10i64 {
                s.write_sample(
                    T0 + i * 10_000,
                    10_000,
                    &[sample("cpu.busy", Unit::Percent, 1.0)],
                )
                .unwrap();
            }
        }
        day_files(&root, Tier::Raw, &["2026-10-18", "2026-10-19"], 10);
        day_files(&root, Tier::AppMin, &["2026-10-18"], 10);
        let mut s = Store::open(&root, T0 + 11 * 10_000).unwrap();
        // Before 2026-10-20: the two older sample files only.
        let (n, _) = s.delete(&[Tier::Raw, Tier::Min, Tier::Hour], Some(T0));
        assert_eq!(n, 2);
        assert_eq!(names(&root, Tier::Raw), ["2026-10-20.phr"]);
        assert_eq!(names(&root, Tier::AppMin), ["2026-10-18.phr"]);
        // Everything of the system tiers, the file being written included; writing goes on.
        s.write_sample(
            T0 + 12 * 10_000,
            10_000,
            &[sample("cpu.busy", Unit::Percent, 2.0)],
        )
        .unwrap();
        let (n, _) = s.delete(&[Tier::Raw, Tier::Min, Tier::Hour], None);
        assert!(n >= 1);
        assert!(names(&root, Tier::Raw).is_empty());
        s.write_sample(
            T0 + 13 * 10_000,
            10_000,
            &[sample("cpu.busy", Unit::Percent, 3.0)],
        )
        .unwrap();
        let mut rows = 0;
        each_row(&root, Tier::Raw, 0, i64::MAX, |_, _, _, vals| {
            rows += 1;
            assert_eq!(vals[0], 3.0);
        });
        assert_eq!(rows, 1);
        let _ = fs::remove_dir_all(&root);
    }

    /// cargo test --release -- --ignored --nocapture bench_seek: a full day of 600-value samples, read
    /// for its last 30 s and last 3 h with and without the seek points.
    #[test]
    #[ignore]
    fn bench_seek() {
        let root = tmp("bench");
        let keys: Vec<String> = (0..600).map(|c| format!("k.{c:03}")).collect();
        {
            let mut w = RowWriter::open(&root, Tier::Raw, "2026-10-20").unwrap();
            for i in 1..=8640i64 {
                let pts: Vec<Point> = keys
                    .iter()
                    .enumerate()
                    .map(|(c, k)| {
                        let v = if c % 3 == 0 {
                            ((i * 31 + c as i64) % 1000) as f64 / 10.0
                        } else {
                            1e9 + (c as f64) * 1e6
                        };
                        Point {
                            key: k,
                            unit: Unit::Percent,
                            avg: v,
                            max: v,
                        }
                    })
                    .collect();
                w.write(T0 + i * 10_000, 1, &pts).unwrap();
            }
        }
        let path = file_path(&root, Tier::Raw, "2026-10-20");
        let size = fs::metadata(&path).unwrap().len();
        let end = T0 + 8640 * 10_000;
        let time = |lo: i64| {
            let t = std::time::Instant::now();
            let mut n = 0;
            for _ in 0..20 {
                each_row(&root, Tier::Raw, lo, end, |_, _, _, _| n += 1);
            }
            (t.elapsed().as_secs_f64() * 1000.0 / 20.0, n / 20)
        };
        let with = [time(end - 30_000), time(end - 3 * 3_600_000)];
        let mut bytes = fs::read(&path).unwrap();
        bytes[5] = 2; // no seeking
        fs::write(&path, &bytes).unwrap();
        let without = [time(end - 30_000), time(end - 3 * 3_600_000)];
        println!("day file {:.1} MB", size as f64 / 1e6);
        for (i, label) in ["last 30 s", "last 3 h"].iter().enumerate() {
            println!(
                "{label}: {:.2} ms with seek points, {:.2} ms without ({} rows)",
                with[i].0, without[i].0, with[i].1
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn retention_deletes_one_file_per_tier_per_call() {
        let root = tmp("retention");
        fs::create_dir_all(root.join("raw")).unwrap();
        for d in ["2024-01-01", "2024-01-02", "2026-10-20"] {
            fs::write(root.join("raw").join(format!("{d}.phr")), b"x").unwrap();
        }
        let s = Store::open(&root, T0).unwrap();
        s.retention(T0, false);
        let left: Vec<String> = fs::read_dir(root.join("raw"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left.len(), 2);
        assert!(!left.contains(&"2024-01-01.phr".to_string()));
        let _ = fs::remove_dir_all(&root);
    }
}
