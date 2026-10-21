//! Just enough JSON writing for the API.

use std::fmt::Write;

pub fn string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A number with at most 6 decimals (the finest stored resolution is 1 µs); NaN and infinities
/// as null.
pub fn number(out: &mut String, v: f64) {
    if !v.is_finite() {
        out.push_str("null");
    } else if v.abs() >= 1e11 {
        let _ = write!(out, "{}", v.round() as i64);
    } else {
        let s = format!("{v:.6}");
        let s = s.trim_end_matches('0').trim_end_matches('.');
        out.push_str(if s == "-0" { "0" } else { s });
    }
}

pub fn numbers(out: &mut String, vs: &[f64]) {
    out.push('[');
    for (i, v) in vs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        number(out, *v);
    }
    out.push(']');
}

/// `"key":` (with a comma first unless `first`).
pub fn key(out: &mut String, k: &str, first: bool) {
    if !first {
        out.push(',');
    }
    string(out, k);
    out.push(':');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        let mut s = String::new();
        numbers(
            &mut s,
            &[
                1.0,
                2.5,
                f64::NAN,
                1.23456789,
                68_000_000_000_123.4,
                -0.00001,
                0.000034,
                -0.0000001,
            ],
        );
        assert_eq!(
            s,
            "[1,2.5,null,1.234568,68000000000123,-0.00001,0.000034,0]"
        );
        let mut s = String::new();
        string(&mut s, "a\"b\\c\u{1}");
        assert_eq!(s, "\"a\\\"b\\\\c\\u0001\"");
    }
}
