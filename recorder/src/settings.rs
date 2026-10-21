//! The recorder's settings: one table of fields, each with its kind (a choice, a range or a switch),
//! its default and its help. The dashboards' Settings page is drawn from this table (GET
//! /api/settings), so it offers only valid values; every change is checked here again, field by
//! field and against the rules between fields, before it is applied and saved.
//!
//! Saved as `settings.conf` in the history folder, one `key = value` per line, written whole and
//! renamed into place. A missing file, or a missing or invalid line, means the default.

use crate::json::{key, number, string};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Int(i64),
    Bool(bool),
    Text(String),
}

pub enum Kind {
    /// One of these integers (value, label).
    Choice(&'static [(i64, &'static str)]),
    /// An integer from min to max by step.
    Range {
        min: i64,
        max: i64,
        step: i64,
        unit: &'static str,
    },
    Bool,
    /// "" (the system drive) or a drive letter the recorder sees ("D:").
    Drive,
}

pub struct Field {
    pub key: &'static str,
    pub group: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: Kind,
    pub default: fn() -> Value,
}

/// Between two fields: `a` must stay below (Lt), not above (Le) or above (Gt) `b`.
#[derive(Clone, Copy)]
pub enum Rel {
    Le,
    Lt,
    Gt,
}

pub struct Rule {
    pub a: &'static str,
    pub rel: Rel,
    pub b: &'static str,
    pub text: &'static str,
}

const DAYS_SHORT: &[(i64, &str)] = &[
    (1, "1 day"),
    (2, "2 days"),
    (3, "3 days"),
    (7, "1 week"),
    (14, "2 weeks"),
    (30, "30 days"),
    (60, "60 days"),
    (90, "90 days"),
    (180, "6 months"),
    (365, "1 year"),
    (730, "2 years"),
];
const DAYS_LONG: &[(i64, &str)] = &[
    (7, "1 week"),
    (14, "2 weeks"),
    (30, "30 days"),
    (60, "60 days"),
    (90, "90 days"),
    (180, "6 months"),
    (365, "1 year"),
    (730, "2 years"),
    (1095, "3 years"),
    (1825, "5 years"),
    (3650, "10 years"),
];
const DAYS_HOURS: &[(i64, &str)] = &[
    (30, "30 days"),
    (90, "90 days"),
    (180, "6 months"),
    (365, "1 year"),
    (730, "2 years"),
    (1095, "3 years"),
    (1825, "5 years"),
    (3650, "10 years"),
    (7300, "20 years"),
];

pub static FIELDS: &[Field] = &[
    // Recording.
    Field {
        key: "interval",
        group: "Recording",
        label: "Sample every",
        help: "How often the whole PC is read. Shorter gives finer charts and a livelier card, but costs more: at 10 s about 0.2 % of one CPU core and 2 MB of 10-second history a day; at 1 s about ten times that. Every value divides a minute, so the 1-minute and hourly summaries stay aligned.",
        kind: Kind::Choice(&[(1, "1 s"), (2, "2 s"), (5, "5 s"), (10, "10 s"), (15, "15 s"), (20, "20 s"), (30, "30 s"), (60, "1 min")]),
        default: || Value::Int(10),
    },
    Field {
        key: "per_core",
        group: "Recording",
        label: "Each logical processor",
        help: "Busy %, utility and clock of every logical processor (the heat map). About 60 of the 600 series; off saves a tenth of the history.",
        kind: Kind::Bool,
        default: || Value::Bool(true),
    },
    Field {
        key: "per_adapter",
        group: "Recording",
        label: "Each network adapter",
        help: "Traffic, bandwidth, errors and drops per adapter. The totals are always kept.",
        kind: Kind::Bool,
        default: || Value::Bool(true),
    },
    Field {
        key: "gpu",
        group: "Recording",
        label: "NVIDIA GPU",
        help: "Load, memory, temperature, power, clocks and slowdowns through NVML. Off unloads NVML, which saves about 20 MB of memory.",
        kind: Kind::Bool,
        default: || Value::Bool(true),
    },
    Field {
        key: "power",
        group: "Recording",
        label: "CPU power",
        help: "Package and core power from the RAPL energy counters.",
        kind: Kind::Bool,
        default: || Value::Bool(true),
    },
    Field {
        key: "apps",
        group: "Recording",
        label: "Apps",
        help: "CPU, memory, commit, I/O and instances of every app, per minute: the Apps and Process explorer pages and the card's top apps. The largest part of the history (about 2.5 MB a day).",
        kind: Kind::Bool,
        default: || Value::Bool(true),
    },
    // Retention.
    Field {
        key: "keep_raw",
        group: "Retention",
        label: "Every sample",
        help: "How long each sample is kept at its full resolution. Older periods are still shown from the 1-minute and hourly summaries.",
        kind: Kind::Choice(DAYS_SHORT),
        default: || Value::Int(90),
    },
    Field {
        key: "keep_min",
        group: "Retention",
        label: "1-minute summaries",
        help: "Average and maximum of every series per minute.",
        kind: Kind::Choice(DAYS_LONG),
        default: || Value::Int(730),
    },
    Field {
        key: "keep_hour",
        group: "Retention",
        label: "Hourly summaries",
        help: "Average and maximum of every series per hour: very small.",
        kind: Kind::Choice(DAYS_HOURS),
        default: || Value::Int(3650),
    },
    Field {
        key: "keep_apps",
        group: "Retention",
        label: "Apps per minute",
        help: "Every app's figures per minute.",
        kind: Kind::Choice(DAYS_SHORT),
        default: || Value::Int(180),
    },
    Field {
        key: "keep_apps_hour",
        group: "Retention",
        label: "Apps per hour",
        help: "Every app's figures per hour.",
        kind: Kind::Choice(DAYS_HOURS),
        default: || Value::Int(3650),
    },
    Field {
        key: "budget_gb",
        group: "Retention",
        label: "Never use more than",
        help: "A ceiling for the whole history. When it is reached, the oldest files go first: samples, then apps per minute, then 1-minute summaries, then apps per hour, then hourly summaries. The files being written now are never deleted.",
        kind: Kind::Choice(&[(0, "no limit"), (1, "1 GB"), (2, "2 GB"), (5, "5 GB"), (10, "10 GB"), (20, "20 GB"), (50, "50 GB"), (100, "100 GB")]),
        default: || Value::Int(10),
    },
    // Alerts: the desktop card's issues and the dashboards' tile colours.
    Field {
        key: "cpu_warn",
        group: "Alerts",
        label: "CPU busy, 5-minute average: orange from",
        help: "",
        kind: Kind::Range { min: 10, max: 100, step: 5, unit: "%" },
        default: || Value::Int(75),
    },
    Field {
        key: "cpu_crit",
        group: "Alerts",
        label: "CPU busy: red from",
        help: "",
        kind: Kind::Range { min: 10, max: 100, step: 5, unit: "%" },
        default: || Value::Int(90),
    },
    Field {
        key: "mem_warn",
        group: "Alerts",
        label: "Memory in use: orange from",
        help: "",
        kind: Kind::Range { min: 50, max: 100, step: 1, unit: "%" },
        default: || Value::Int(85),
    },
    Field {
        key: "mem_crit",
        group: "Alerts",
        label: "Memory in use: red from",
        help: "",
        kind: Kind::Range { min: 50, max: 100, step: 1, unit: "%" },
        default: || Value::Int(92),
    },
    Field {
        key: "commit_warn",
        group: "Alerts",
        label: "Commit, of the most it can reach: orange from",
        help: "The most it can reach is RAM plus the most the page file can grow to.",
        kind: Kind::Range { min: 50, max: 100, step: 1, unit: "%" },
        default: || Value::Int(85),
    },
    Field {
        key: "commit_crit",
        group: "Alerts",
        label: "Commit: red from",
        help: "",
        kind: Kind::Range { min: 50, max: 100, step: 1, unit: "%" },
        default: || Value::Int(95),
    },
    Field {
        key: "paging_warn",
        group: "Alerts",
        label: "Paging, pages read per second over 5 minutes: orange from",
        help: "",
        kind: Kind::Choice(&[(100, "100"), (200, "200"), (500, "500"), (1000, "1,000"), (1500, "1,500"), (2000, "2,000"), (5000, "5,000"), (10000, "10,000"), (20000, "20,000")]),
        default: || Value::Int(1500),
    },
    Field {
        key: "paging_crit",
        group: "Alerts",
        label: "Paging: red from",
        help: "",
        kind: Kind::Choice(&[(100, "100"), (200, "200"), (500, "500"), (1000, "1,000"), (1500, "1,500"), (2000, "2,000"), (5000, "5,000"), (10000, "10,000"), (20000, "20,000")]),
        default: || Value::Int(5000),
    },
    Field {
        key: "free_warn",
        group: "Alerts",
        label: "Free space on the card's disk: orange under",
        help: "",
        kind: Kind::Choice(&[(1, "1 GB"), (2, "2 GB"), (5, "5 GB"), (10, "10 GB"), (15, "15 GB"), (20, "20 GB"), (25, "25 GB"), (50, "50 GB"), (100, "100 GB"), (200, "200 GB")]),
        default: || Value::Int(25),
    },
    Field {
        key: "free_crit",
        group: "Alerts",
        label: "Free space: red under",
        help: "",
        kind: Kind::Choice(&[(1, "1 GB"), (2, "2 GB"), (5, "5 GB"), (10, "10 GB"), (15, "15 GB"), (20, "20 GB"), (25, "25 GB"), (50, "50 GB"), (100, "100 GB"), (200, "200 GB")]),
        default: || Value::Int(10),
    },
    Field {
        key: "latency_warn",
        group: "Alerts",
        label: "Disk latency over 5 minutes: orange from",
        help: "",
        kind: Kind::Choice(&[(5, "5 ms"), (10, "10 ms"), (20, "20 ms"), (50, "50 ms"), (100, "100 ms"), (200, "200 ms"), (500, "500 ms"), (1000, "1 s")]),
        default: || Value::Int(50),
    },
    Field {
        key: "latency_crit",
        group: "Alerts",
        label: "Disk latency: red from",
        help: "",
        kind: Kind::Choice(&[(5, "5 ms"), (10, "10 ms"), (20, "20 ms"), (50, "50 ms"), (100, "100 ms"), (200, "200 ms"), (500, "500 ms"), (1000, "1 s")]),
        default: || Value::Int(200),
    },
    Field {
        key: "gpu_temp_warn",
        group: "Alerts",
        label: "GPU temperature: orange from",
        help: "",
        kind: Kind::Range { min: 50, max: 100, step: 1, unit: "°C" },
        default: || Value::Int(78),
    },
    Field {
        key: "gpu_temp_crit",
        group: "Alerts",
        label: "GPU temperature: red from",
        help: "",
        kind: Kind::Range { min: 50, max: 100, step: 1, unit: "°C" },
        default: || Value::Int(83),
    },
    Field {
        key: "disk_busy",
        group: "Alerts",
        label: "Disk busy over 5 minutes: orange from",
        help: "",
        kind: Kind::Range { min: 50, max: 100, step: 5, unit: "%" },
        default: || Value::Int(90),
    },
    Field {
        key: "vram_full",
        group: "Alerts",
        label: "GPU memory used: orange from",
        help: "",
        kind: Kind::Range { min: 50, max: 100, step: 1, unit: "%" },
        default: || Value::Int(95),
    },
    Field {
        key: "perf_limit",
        group: "Alerts",
        label: "CPU performance limit over 5 minutes: orange under",
        help: "100 % means the CPU is not held back; below, power, heat or the platform limit it.",
        kind: Kind::Range { min: 50, max: 100, step: 5, unit: "%" },
        default: || Value::Int(90),
    },
    // Desktop card: what it shows and how (sent with /api/card as "card"). Its place and size are
    // the card's own: it is dragged and resized on the desktop.
    Field {
        key: "card_cpu",
        group: "Desktop card",
        label: "CPU tile",
        help: "Each tile shows the last 30 seconds and, with the charts on, the last hour. A hidden tile's alerts still show as issue lines.",
        kind: Kind::Bool,
        default: || Value::Bool(true),
    },
    Field {
        key: "card_memory",
        group: "Desktop card",
        label: "Memory tile",
        help: "",
        kind: Kind::Bool,
        default: || Value::Bool(true),
    },
    Field {
        key: "card_commit",
        group: "Desktop card",
        label: "Commit tile",
        help: "",
        kind: Kind::Bool,
        default: || Value::Bool(true),
    },
    Field {
        key: "card_gpu",
        group: "Desktop card",
        label: "GPU tile",
        help: "",
        kind: Kind::Bool,
        default: || Value::Bool(true),
    },
    Field {
        key: "card_disk",
        group: "Desktop card",
        label: "Disk tile",
        help: "",
        kind: Kind::Bool,
        default: || Value::Bool(true),
    },
    Field {
        key: "card_drive",
        group: "Desktop card",
        label: "Disk on the card",
        help: "The disk whose activity, latency and free space the card shows and checks.",
        kind: Kind::Drive,
        default: || Value::Text(String::new()),
    },
    Field {
        key: "card_network",
        group: "Desktop card",
        label: "Network tile",
        help: "",
        kind: Kind::Bool,
        default: || Value::Bool(true),
    },
    Field {
        key: "card_columns",
        group: "Desktop card",
        label: "Tiles per row",
        help: "1 makes a narrow card, 3 a wide and short one.",
        kind: Kind::Choice(&[(1, "1"), (2, "2"), (3, "3")]),
        default: || Value::Int(2),
    },
    Field {
        key: "card_charts",
        group: "Desktop card",
        label: "Last-hour charts",
        help: "A line of the last hour at the foot of each tile. Off makes the tiles shorter.",
        kind: Kind::Bool,
        default: || Value::Bool(true),
    },
    Field {
        key: "card_issue_lines",
        group: "Desktop card",
        label: "Issue lines, at most",
        help: "One line per issue under the coloured bar, red ones first. The bar always counts them all.",
        kind: Kind::Choice(&[(0, "none, the count only"), (1, "1"), (2, "2"), (3, "3"), (4, "4"), (5, "5"), (6, "6")]),
        default: || Value::Int(6),
    },
    Field {
        key: "card_top_apps",
        group: "Desktop card",
        label: "Top apps line",
        help: "The app using the most CPU and the one using the most memory.",
        kind: Kind::Bool,
        default: || Value::Bool(true),
    },
    Field {
        key: "card_status",
        group: "Desktop card",
        label: "Status line",
        help: "Whether recording is OK, and the time of the sample shown.",
        kind: Kind::Bool,
        default: || Value::Bool(true),
    },
    Field {
        key: "card_clock",
        group: "Desktop card",
        label: "Clock",
        help: "The time, at the top right of the card.",
        kind: Kind::Bool,
        default: || Value::Bool(true),
    },
    Field {
        key: "card_opacity",
        group: "Desktop card",
        label: "Background opacity",
        help: "Below 100 % the wallpaper shows through the card's background (the text stays solid).",
        kind: Kind::Range { min: 40, max: 100, step: 5, unit: "%" },
        default: || Value::Int(85),
    },
    Field {
        key: "card_locked",
        group: "Desktop card",
        label: "Lock its place and size",
        help: "Dragging and resizing are off; a click still opens the dashboards.",
        kind: Kind::Bool,
        default: || Value::Bool(false),
    },
    // Display.
    Field {
        key: "refresh",
        group: "Display",
        label: "Dashboards refresh",
        help: "Automatic: every 10 s for ranges up to 3 hours, then less often. Pages never refresh while their tab is hidden.",
        kind: Kind::Choice(&[(0, "automatic"), (5, "every 5 s"), (10, "every 10 s"), (30, "every 30 s"), (60, "every minute"), (300, "every 5 minutes"), (-1, "never")]),
        default: || Value::Int(0),
    },
];

pub static RULES: &[Rule] = &[
    Rule {
        a: "keep_raw",
        rel: Rel::Le,
        b: "keep_min",
        text: "Samples cannot be kept longer than the 1-minute summaries.",
    },
    Rule {
        a: "keep_min",
        rel: Rel::Le,
        b: "keep_hour",
        text: "1-minute summaries cannot be kept longer than the hourly ones.",
    },
    Rule {
        a: "keep_apps",
        rel: Rel::Le,
        b: "keep_apps_hour",
        text: "Apps per minute cannot be kept longer than apps per hour.",
    },
    Rule {
        a: "cpu_warn",
        rel: Rel::Lt,
        b: "cpu_crit",
        text: "Orange must start below red.",
    },
    Rule {
        a: "mem_warn",
        rel: Rel::Lt,
        b: "mem_crit",
        text: "Orange must start below red.",
    },
    Rule {
        a: "commit_warn",
        rel: Rel::Lt,
        b: "commit_crit",
        text: "Orange must start below red.",
    },
    Rule {
        a: "paging_warn",
        rel: Rel::Lt,
        b: "paging_crit",
        text: "Orange must start below red.",
    },
    Rule {
        a: "free_warn",
        rel: Rel::Gt,
        b: "free_crit",
        text: "Orange must start at more free space than red.",
    },
    Rule {
        a: "latency_warn",
        rel: Rel::Lt,
        b: "latency_crit",
        text: "Orange must start below red.",
    },
    Rule {
        a: "gpu_temp_warn",
        rel: Rel::Lt,
        b: "gpu_temp_crit",
        text: "Orange must start below red.",
    },
];

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    values: Vec<Value>,
}

fn index(key: &str) -> Option<usize> {
    FIELDS.iter().position(|f| f.key == key)
}

/// A drive letter as the recorder names volumes ("C:").
fn is_drive(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 2 && b[0].is_ascii_uppercase() && b[1] == b':'
}

/// The value `text` gives for field `f`, if valid on its own.
fn parse(f: &Field, text: &str) -> Result<Value, String> {
    let text = text.trim();
    match &f.kind {
        Kind::Bool => match text {
            "true" | "1" | "on" => Ok(Value::Bool(true)),
            "false" | "0" | "off" => Ok(Value::Bool(false)),
            _ => Err("must be on or off".into()),
        },
        Kind::Choice(choices) => {
            let v: i64 = text
                .parse()
                .map_err(|_| "must be one of the listed values".to_string())?;
            if choices.iter().any(|c| c.0 == v) {
                Ok(Value::Int(v))
            } else {
                Err("must be one of the listed values".into())
            }
        }
        Kind::Range { min, max, step, .. } => {
            let v: i64 = text
                .parse()
                .map_err(|_| format!("must be a whole number from {min} to {max}"))?;
            if v < *min || v > *max || (v - min) % step != 0 {
                Err(format!("must be from {min} to {max}, by {step}"))
            } else {
                Ok(Value::Int(v))
            }
        }
        Kind::Drive => {
            let t = text.to_ascii_uppercase();
            if t.is_empty() || is_drive(&t) {
                Ok(Value::Text(t))
            } else {
                Err("must be a drive such as C:".into())
            }
        }
    }
}

fn text_of(v: &Value) -> String {
    match v {
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Text(t) => t.clone(),
    }
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            values: FIELDS.iter().map(|f| (f.default)()).collect(),
        }
    }
}

impl Settings {
    pub fn int(&self, key: &str) -> i64 {
        match index(key).map(|i| &self.values[i]) {
            Some(Value::Int(v)) => *v,
            _ => panic!("no integer setting {key}"),
        }
    }

    pub fn flag(&self, key: &str) -> bool {
        match index(key).map(|i| &self.values[i]) {
            Some(Value::Bool(v)) => *v,
            _ => panic!("no on/off setting {key}"),
        }
    }

    pub fn text(&self, key: &str) -> &str {
        match index(key).map(|i| &self.values[i]) {
            Some(Value::Text(v)) => v,
            _ => panic!("no text setting {key}"),
        }
    }

    /// The rules between fields that these values break: (rule, its first field).
    fn broken(&self) -> Option<&'static Rule> {
        RULES.iter().find(|r| {
            let (a, b) = (self.int(r.a), self.int(r.b));
            match r.rel {
                Rel::Le => a > b,
                Rel::Lt => a >= b,
                Rel::Gt => a <= b,
            }
        })
    }

    /// These settings with `changes` (key, text) applied, if every field and rule holds; else the
    /// first field at fault and why. Unknown keys are refused.
    pub fn with(&self, changes: &[(String, String)]) -> Result<Settings, (String, String)> {
        let mut next = self.clone();
        for (k, v) in changes {
            let i = index(k).ok_or_else(|| (k.clone(), "unknown setting".to_string()))?;
            next.values[i] = parse(&FIELDS[i], v)
                .map_err(|e| (k.clone(), format!("{}: {e}", FIELDS[i].label)))?;
        }
        if let Some(r) = next.broken() {
            return Err((r.b.to_string(), r.text.to_string()));
        }
        Ok(next)
    }

    /// Settings from the file's text: each valid line counts, the rest stays at the default; values
    /// that break a rule together fall back to the defaults of the fields involved.
    pub fn from_text(text: &str) -> (Settings, Vec<String>) {
        let mut s = Settings::default();
        let mut problems = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                problems.push(format!("not a setting: {line}"));
                continue;
            };
            let k = k.trim();
            match index(k) {
                Some(i) => match parse(&FIELDS[i], v) {
                    Ok(val) => s.values[i] = val,
                    Err(e) => problems.push(format!("{k}: {e}; default kept")),
                },
                None => problems.push(format!("unknown setting {k}")),
            }
        }
        while let Some(r) = s.broken() {
            problems.push(format!("{} / {}: {} Defaults kept.", r.a, r.b, r.text));
            for k in [r.a, r.b] {
                let i = index(k).unwrap_or(0);
                s.values[i] = (FIELDS[i].default)();
            }
        }
        (s, problems)
    }

    pub fn to_text(&self) -> String {
        let mut o = String::from("# Kymo settings: change them on the dashboards' Settings page (http://127.0.0.1:21223/#/settings).\n");
        for (f, v) in FIELDS.iter().zip(&self.values) {
            o.push_str(&format!("{} = {}\n", f.key, text_of(v)));
        }
        o
    }

    /// The settings file of a history folder.
    pub fn path(root: &Path) -> PathBuf {
        root.join("settings.conf")
    }

    /// Reads the settings of a history folder (defaults when there is no file).
    pub fn load(root: &Path) -> (Settings, Vec<String>) {
        match std::fs::read_to_string(Settings::path(root)) {
            Ok(t) => Settings::from_text(&t),
            Err(_) => (Settings::default(), Vec::new()),
        }
    }

    /// Writes the file whole, then renames it into place.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let path = Settings::path(root);
        let tmp = path.with_extension("conf.new");
        std::fs::write(&tmp, self.to_text())?;
        std::fs::rename(&tmp, &path)
    }

    /// The values as `"key":value` pairs (after the caller's first field).
    fn write_values(&self, o: &mut String, name: &str) {
        key(o, name, false);
        o.push('{');
        for (i, (f, v)) in FIELDS.iter().zip(&self.values).enumerate() {
            key(o, f.key, i == 0);
            write_value(o, v);
        }
        o.push('}');
    }

    /// The whole table for the Settings page: groups of fields with their kind, choices, bounds,
    /// default and value, the rules between fields, and the drives the recorder sees.
    pub fn schema_json(&self, drives: &[String], system_drive: &str) -> String {
        let mut o = String::from("{");
        key(&mut o, "groups", true);
        o.push('[');
        let mut groups: Vec<&str> = Vec::new();
        for f in FIELDS {
            if !groups.contains(&f.group) {
                groups.push(f.group);
            }
        }
        for (gi, g) in groups.iter().enumerate() {
            if gi > 0 {
                o.push(',');
            }
            o.push('{');
            key(&mut o, "name", true);
            string(&mut o, g);
            key(&mut o, "fields", false);
            o.push('[');
            for (fi, (i, f)) in FIELDS
                .iter()
                .enumerate()
                .filter(|(_, f)| f.group == *g)
                .enumerate()
            {
                if fi > 0 {
                    o.push(',');
                }
                o.push('{');
                key(&mut o, "key", true);
                string(&mut o, f.key);
                key(&mut o, "label", false);
                string(&mut o, f.label);
                key(&mut o, "help", false);
                string(&mut o, f.help);
                key(&mut o, "kind", false);
                match &f.kind {
                    Kind::Bool => string(&mut o, "bool"),
                    Kind::Choice(choices) => {
                        string(&mut o, "choice");
                        key(&mut o, "choices", false);
                        o.push('[');
                        for (ci, (v, label)) in choices.iter().enumerate() {
                            if ci > 0 {
                                o.push(',');
                            }
                            o.push('{');
                            key(&mut o, "value", true);
                            number(&mut o, *v as f64);
                            key(&mut o, "label", false);
                            string(&mut o, label);
                            o.push('}');
                        }
                        o.push(']');
                    }
                    Kind::Range {
                        min,
                        max,
                        step,
                        unit,
                    } => {
                        string(&mut o, "range");
                        for (k, v) in [("min", min), ("max", max), ("step", step)] {
                            key(&mut o, k, false);
                            number(&mut o, *v as f64);
                        }
                        key(&mut o, "unit", false);
                        string(&mut o, unit);
                    }
                    Kind::Drive => {
                        string(&mut o, "choice");
                        key(&mut o, "choices", false);
                        o.push('[');
                        o.push('{');
                        key(&mut o, "value", true);
                        string(&mut o, "");
                        key(&mut o, "label", false);
                        string(&mut o, &format!("the system drive ({system_drive})"));
                        o.push('}');
                        let mut list: Vec<&String> = drives.iter().collect();
                        let current = self.values[i].clone();
                        let kept;
                        if let Value::Text(t) = &current {
                            if !t.is_empty() && !drives.contains(t) {
                                kept = t.clone();
                                list.push(&kept);
                            }
                        }
                        for d in list {
                            o.push_str(",{");
                            key(&mut o, "value", true);
                            string(&mut o, d);
                            key(&mut o, "label", false);
                            string(&mut o, d);
                            o.push('}');
                        }
                        o.push(']');
                    }
                }
                key(&mut o, "default", false);
                write_value(&mut o, &(f.default)());
                key(&mut o, "value", false);
                write_value(&mut o, &self.values[i]);
                o.push('}');
            }
            o.push(']');
            o.push('}');
        }
        o.push(']');
        key(&mut o, "rules", false);
        o.push('[');
        for (ri, r) in RULES.iter().enumerate() {
            if ri > 0 {
                o.push(',');
            }
            o.push('{');
            key(&mut o, "a", true);
            string(&mut o, r.a);
            key(&mut o, "rel", false);
            string(
                &mut o,
                match r.rel {
                    Rel::Le => "le",
                    Rel::Lt => "lt",
                    Rel::Gt => "gt",
                },
            );
            key(&mut o, "b", false);
            string(&mut o, r.b);
            key(&mut o, "text", false);
            string(&mut o, r.text);
            o.push('}');
        }
        o.push(']');
        self.write_values(&mut o, "values");
        key(&mut o, "system_drive", false);
        string(&mut o, system_drive);
        o.push('}');
        o
    }

    /// The alert thresholds as `"limits":{...}` after the caller's fields (for the card).
    pub fn write_limits(&self, o: &mut String) {
        key(o, "limits", false);
        o.push('{');
        let mut first = true;
        for f in FIELDS.iter().filter(|f| f.group == "Alerts") {
            key(o, f.key, first);
            first = false;
            number(o, self.int(f.key) as f64);
        }
        o.push('}');
    }

    /// What the desktop card shows and how, as `"card":{...}` after the caller's fields: the "Desktop
    /// card" group without its "card_" prefix (the disk goes as "drive", already resolved).
    pub fn write_card(&self, o: &mut String) {
        key(o, "card", false);
        o.push('{');
        let mut first = true;
        for (f, v) in FIELDS
            .iter()
            .zip(&self.values)
            .filter(|(f, _)| f.group == "Desktop card" && f.key != "card_drive")
        {
            key(o, f.key.trim_start_matches("card_"), first);
            first = false;
            write_value(o, v);
        }
        o.push('}');
    }

    /// All values by key, for logging what changed.
    pub fn diff(&self, other: &Settings) -> Vec<String> {
        FIELDS
            .iter()
            .zip(self.values.iter().zip(&other.values))
            .filter(|(_, (a, b))| a != b)
            .map(|(f, (a, b))| format!("{} {} -> {}", f.key, text_of(a), text_of(b)))
            .collect()
    }
}

fn write_value(o: &mut String, v: &Value) {
    match v {
        Value::Int(i) => number(o, *i as f64),
        Value::Bool(b) => o.push_str(if *b { "true" } else { "false" }),
        Value::Text(t) => string(o, t),
    }
}

/// Changes from a form body (`a=1&b=on`), as the API receives them.
pub fn form(body: &str, decode: impl Fn(&str) -> String) -> Vec<(String, String)> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<(String, String)> = Vec::new();
    for pair in body.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let (k, v) = (decode(k), decode(v));
        match seen.get(&k) {
            Some(&i) => out[i].1 = v, // the last one counts
            None => {
                seen.insert(k.clone(), out.len());
                out.push((k, v));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn defaults_are_valid_and_every_key_is_typed() {
        let s = Settings::default();
        assert!(s.broken().is_none());
        for f in FIELDS {
            assert!(parse(f, &text_of(&(f.default)())).is_ok(), "{}", f.key);
            match f.kind {
                Kind::Bool => {
                    s.flag(f.key);
                }
                Kind::Drive => {
                    s.text(f.key);
                }
                _ => {
                    s.int(f.key);
                }
            }
        }
        for r in RULES {
            assert!(index(r.a).is_some() && index(r.b).is_some());
        }
    }

    #[test]
    fn only_valid_values_pass() {
        let s = Settings::default();
        assert_eq!(
            s.with(&ch(&[("interval", "5")])).unwrap().int("interval"),
            5
        );
        assert!(s.with(&ch(&[("interval", "7")])).is_err());
        assert!(s.with(&ch(&[("interval", "ten")])).is_err());
        assert!(s.with(&ch(&[("cpu_warn", "77")])).is_err()); // step 5
        assert!(s.with(&ch(&[("cpu_warn", "105")])).is_err());
        assert!(s.with(&ch(&[("gpu", "maybe")])).is_err());
        assert!(s.with(&ch(&[("nonsense", "1")])).is_err());
        assert!(s.with(&ch(&[("card_drive", "C:\\")])).is_err());
        assert_eq!(
            s.with(&ch(&[("card_drive", "d:")]))
                .unwrap()
                .text("card_drive"),
            "D:"
        );
        // Rules between fields.
        let e = s
            .with(&ch(&[("keep_raw", "730"), ("keep_min", "365")]))
            .unwrap_err();
        assert_eq!(e.0, "keep_min");
        assert!(s
            .with(&ch(&[("cpu_warn", "90"), ("cpu_crit", "90")]))
            .is_err());
        assert!(s
            .with(&ch(&[("free_warn", "5"), ("free_crit", "10")]))
            .is_err());
        assert!(s
            .with(&ch(&[("free_warn", "50"), ("free_crit", "10")]))
            .is_ok());
    }

    #[test]
    fn file_round_trip_and_bad_lines() {
        let s = Settings::default()
            .with(&ch(&[
                ("interval", "30"),
                ("gpu", "false"),
                ("card_drive", "E:"),
            ]))
            .unwrap();
        let (back, problems) = Settings::from_text(&s.to_text());
        assert_eq!(back, s);
        assert!(problems.is_empty());
        let (t, problems) =
            Settings::from_text("interval = 7\ngpu=off\nkeep_raw = 730\nkeep_min = 30\nwhat\n");
        assert_eq!(t.int("interval"), 10);
        assert!(!t.flag("gpu"));
        assert_eq!((t.int("keep_raw"), t.int("keep_min")), (90, 730)); // both back to defaults
        assert_eq!(problems.len(), 3);
    }

    #[test]
    fn schema_is_json_shaped() {
        let j = Settings::default().schema_json(&["C:".to_string(), "E:".to_string()], "C:");
        assert!(j.starts_with("{\"groups\":[{\"name\":\"Recording\""));
        assert!(j.contains("\"key\":\"card_drive\""));
        assert!(j.contains("{\"value\":\"E:\",\"label\":\"E:\"}"));
        assert!(j.contains("\"rules\":[{\"a\":\"keep_raw\",\"rel\":\"le\",\"b\":\"keep_min\""));
        assert!(j.ends_with("},\"system_drive\":\"C:\"}"));
        let mut o = String::from("{\"x\":0");
        Settings::default().write_limits(&mut o);
        assert!(o.contains("\"limits\":{\"cpu_warn\":75,\"cpu_crit\":90"));
        assert!(j.contains("{\"name\":\"Desktop card\",\"fields\":[{\"key\":\"card_cpu\""));
    }

    #[test]
    fn card_options_for_the_card() {
        let s = Settings::default()
            .with(&ch(&[
                ("card_columns", "3"),
                ("card_gpu", "off"),
                ("card_opacity", "60"),
            ]))
            .unwrap();
        let mut o = String::from("{\"x\":0");
        s.write_card(&mut o);
        o.push('}');
        assert_eq!(o, "{\"x\":0,\"card\":{\"cpu\":true,\"memory\":true,\"commit\":true,\"gpu\":false,\"disk\":true,\"network\":true,\"columns\":3,\"charts\":true,\"issue_lines\":6,\"top_apps\":true,\"status\":true,\"clock\":true,\"opacity\":60,\"locked\":false}}");
        assert!(Settings::default()
            .with(&ch(&[("card_columns", "4")]))
            .is_err());
        assert!(Settings::default()
            .with(&ch(&[("card_opacity", "35")]))
            .is_err());
        assert!(Settings::default()
            .with(&ch(&[("card_issue_lines", "0")]))
            .is_ok());
    }

    #[test]
    fn form_bodies() {
        let f = form("interval=5&gpu=off&interval=10", |s| s.to_string());
        assert_eq!(f, ch(&[("interval", "10"), ("gpu", "off")]));
    }
}
