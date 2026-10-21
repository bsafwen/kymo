//! Units of the stored series, and UTC calendar arithmetic. Files, rows and the API use UTC only,
//! so summer time and time-zone changes do not affect them.

use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Percent,
    Bytes,
    BytesPerSec,
    PerSec,
    Seconds,
    Mhz,
    Celsius,
    Watts,
    Count,
    BitsPerSec,
    Timestamp,
    /// Per second at 0.001 resolution: disk operations, whose averages over an hour can be tiny.
    PerSecFine,
    /// Seconds at 1 ns resolution: disk waiting time per second, for the same reason.
    SecondsFine,
}

impl Unit {
    const ALL: [Unit; 13] = [
        Unit::Percent,
        Unit::Bytes,
        Unit::BytesPerSec,
        Unit::PerSec,
        Unit::Seconds,
        Unit::Mhz,
        Unit::Celsius,
        Unit::Watts,
        Unit::Count,
        Unit::BitsPerSec,
        Unit::Timestamp,
        Unit::PerSecFine,
        Unit::SecondsFine,
    ];

    pub fn code(self) -> u8 {
        self as u8
    }

    pub fn from_code(c: u8) -> Option<Unit> {
        Self::ALL.get(c as usize).copied()
    }

    pub fn name(self) -> &'static str {
        match self {
            Unit::Percent => "percent",
            Unit::Bytes => "bytes",
            Unit::BytesPerSec => "bytes/s",
            Unit::PerSec => "per second",
            Unit::Seconds => "seconds",
            Unit::Mhz => "MHz",
            Unit::Celsius => "celsius",
            Unit::Watts => "watts",
            Unit::Count => "count",
            Unit::BitsPerSec => "bits/s",
            Unit::Timestamp => "unix seconds",
            Unit::PerSecFine => "per second",
            Unit::SecondsFine => "seconds",
        }
    }

    /// Values are stored as round(value * factor): 0.01 %, 1 kB, 100 B/s, 0.1 per second, 1 µs,
    /// 1 MHz, 0.1 °C, 0.01 W, 0.1, 1 kbit/s, 1 s.
    fn factor(self) -> f64 {
        match self {
            Unit::Percent => 100.0,
            Unit::Bytes => 1e-3,
            Unit::BytesPerSec => 1e-2,
            Unit::PerSec => 10.0,
            Unit::Seconds => 1e6,
            Unit::Mhz => 1.0,
            Unit::Celsius => 10.0,
            Unit::Watts => 100.0,
            Unit::Count => 10.0,
            Unit::BitsPerSec => 1e-3,
            Unit::Timestamp => 1.0,
            Unit::PerSecFine => 1e3,
            Unit::SecondsFine => 1e9,
        }
    }

    pub fn quantize(self, v: f64) -> i64 {
        (v * self.factor()).round() as i64
    }

    pub fn value(self, q: i64) -> f64 {
        q as f64 / self.factor()
    }
}

pub const DAY_MS: i64 = 86_400_000;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn ymd(ms: i64) -> (i64, i64, i64) {
    civil_from_days(ms.div_euclid(DAY_MS))
}

pub fn day_name(ms: i64) -> String {
    let (y, m, d) = ymd(ms);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn month_name(ms: i64) -> String {
    let (y, m, _) = ymd(ms);
    format!("{y:04}-{m:02}")
}

fn num(s: &str) -> Option<i64> {
    s.parse::<i64>().ok()
}

/// Start and end (exclusive) of a period named like a data file: "2026-10-20", "2026-10" or "2026".
pub fn period_range(name: &str) -> Option<(i64, i64)> {
    let parts: Vec<&str> = name.split('-').collect();
    match parts.as_slice() {
        [y] => {
            let y = num(y)?;
            Some((
                days_from_civil(y, 1, 1) * DAY_MS,
                days_from_civil(y + 1, 1, 1) * DAY_MS,
            ))
        }
        [y, m] => {
            let (y, m) = (num(y)?, num(m)?);
            if !(1..=12).contains(&m) {
                return None;
            }
            let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            Some((
                days_from_civil(y, m, 1) * DAY_MS,
                days_from_civil(ny, nm, 1) * DAY_MS,
            ))
        }
        [y, m, d] => {
            let (y, m, d) = (num(y)?, num(m)?, num(d)?);
            if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
                return None;
            }
            let s = days_from_civil(y, m, d) * DAY_MS;
            Some((s, s + DAY_MS))
        }
        _ => None,
    }
}

/// "2026-10-20 06:00:00" (UTC).
pub fn fmt_time(ms: i64) -> String {
    let (y, m, d) = ymd(ms);
    let s = ms.div_euclid(1000).rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_round_trip() {
        for z in -800_000..800_000 {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
        assert_eq!(day_name(0), "1970-01-01");
        assert_eq!(fmt_time(1_792_454_400_000), "2026-10-20 00:00:00");
    }

    #[test]
    fn periods() {
        let (s, e) = period_range("2026-10-20").unwrap();
        assert_eq!(day_name(s), "2026-10-20");
        assert_eq!(e - s, DAY_MS);
        let (s, e) = period_range("2026-12").unwrap();
        assert_eq!(day_name(s), "2026-12-01");
        assert_eq!(day_name(e), "2027-01-01");
        let (s, e) = period_range("2028").unwrap();
        assert_eq!((e - s) / DAY_MS, 366);
        assert!(period_range("2026-13").is_none());
        assert!(period_range("x").is_none());
    }

    #[test]
    fn quantize_round_trip() {
        for u in Unit::ALL {
            assert_eq!(Unit::from_code(u.code()), Some(u));
        }
        assert_eq!(Unit::Percent.value(Unit::Percent.quantize(12.345)), 12.35);
        assert_eq!(
            Unit::Bytes.value(Unit::Bytes.quantize(68_000_123_456.0)),
            68_000_123_000.0
        );
    }
}
