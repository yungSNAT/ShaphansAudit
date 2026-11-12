//! Report sections and their text and JSON renderings.

use std::fmt::Write as _;

use chrono::{DateTime, Local, Utc};
use serde_json::{Map, Value, json};

use crate::scan::RootStats;

/// Options that shape a detector's report.
#[derive(Debug, Clone)]
pub struct Opts {
    pub limit: usize,
    pub paths: bool,
    /// The home folder with forward slashes, shown as `~`; compared case-insensitively.
    pub home: Option<String>,
}

/// One table in a section. Column names are `snake_case`; the text view spaces them.
#[derive(Debug, Clone, Default)]
pub struct Table {
    pub title: String,
    pub columns: Vec<&'static str>,
    pub rows: Vec<Vec<Value>>,
    /// Rows left out by `--limit`.
    pub omitted: usize,
}

impl Table {
    #[must_use]
    pub fn new(title: &str, columns: Vec<&'static str>) -> Self {
        Table {
            title: title.to_owned(),
            columns,
            rows: Vec::new(),
            omitted: 0,
        }
    }

    /// Keeps the first `limit` rows and records how many were dropped.
    pub fn truncate(&mut self, limit: usize) {
        self.omitted = self.rows.len().saturating_sub(limit);
        self.rows.truncate(limit);
    }
}

/// One detector's report.
#[derive(Debug, Clone, Default)]
pub struct Section {
    pub name: &'static str,
    pub title: &'static str,
    pub summary: Vec<(String, Value)>,
    pub tables: Vec<Table>,
    pub notes: Vec<String>,
}

/// Run-level facts printed above the sections.
pub struct Meta<'a> {
    pub since: DateTime<Utc>,
    pub roots: &'a [RootStats],
    pub skipped_roots: &'a [String],
    pub home: Option<&'a str>,
}

impl Meta<'_> {
    fn root(&self, r: &RootStats) -> String {
        display_path(&r.root.display().to_string(), self.home)
    }
}

/// A ratio rounded to 2 decimals, or null when the denominator is zero.
#[must_use]
pub fn ratio(num: u64, den: u64) -> Value {
    if den == 0 {
        return Value::Null;
    }
    #[expect(clippy::cast_precision_loss, reason = "counts stay far below 2^52")]
    let r = num as f64 / den as f64;
    json!((r * 100.0).round() / 100.0)
}

/// The median of `v`, or 0 when empty.
#[must_use]
pub fn median(v: &[u64]) -> u64 {
    percentile(v, 50)
}

/// The nearest-rank percentile `p` (0 to 100) of `v`, or 0 when empty.
#[must_use]
pub fn percentile(v: &[u64], p: usize) -> u64 {
    if v.is_empty() {
        return 0;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    let rank = (p * s.len()).div_ceil(100).max(1);
    s[rank - 1]
}

/// Shows a path relative to the home folder, with forward slashes.
#[must_use]
pub fn display_path(path: &str, home: Option<&str>) -> String {
    let p = path.replace('\\', "/");
    if let Some(h) = home
        && p.get(..h.len())
            .is_some_and(|pre| pre.to_lowercase() == h.to_lowercase())
        && p[h.len()..].starts_with('/')
    {
        return format!("~{}", &p[h.len()..]);
    }
    p
}

fn local(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M %:z")
        .to_string()
}

fn cell(v: &Value) -> String {
    match v {
        Value::Null => "-".to_owned(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Renders sections as plain text tables.
#[must_use]
pub fn text(meta: &Meta<'_>, sections: &[Section]) -> String {
    let mut out = format!(
        "shaphan {}: records since {}\n",
        env!("CARGO_PKG_VERSION"),
        local(meta.since)
    );
    for r in meta.roots {
        let _ = writeln!(
            out,
            "{}: {} files under {} (unreadable files or folders: {}, lines skipped: {}, compressed files skipped: {})",
            r.agent.name(),
            r.files,
            meta.root(r),
            r.unreadable,
            r.bad_lines,
            r.compressed
        );
    }
    for s in meta.skipped_roots {
        let _ = writeln!(out, "{s}");
    }
    for s in sections {
        let _ = write!(out, "\n== {}: {} ==\n", s.name, s.title);
        for (k, v) in &s.summary {
            let _ = writeln!(out, "{}: {}", k.replace('_', " "), cell(v));
        }
        for t in &s.tables {
            render_table(&mut out, t);
        }
        for n in &s.notes {
            let _ = writeln!(out, "Note: {n}");
        }
    }
    out
}

fn render_table(out: &mut String, t: &Table) {
    let head: Vec<String> = t.columns.iter().map(|c| c.replace('_', " ")).collect();
    let cells: Vec<Vec<String>> = t
        .rows
        .iter()
        .map(|r| r.iter().map(cell).collect())
        .collect();
    let mut widths: Vec<usize> = head.iter().map(|h| h.chars().count()).collect();
    for row in &cells {
        for (w, c) in widths.iter_mut().zip(row) {
            *w = (*w).max(c.chars().count());
        }
    }
    let _ = writeln!(out, "\n{}", t.title);
    let line = |out: &mut String, row: &[String], numeric: &[bool]| {
        let parts: Vec<String> = row
            .iter()
            .zip(&widths)
            .zip(numeric)
            .map(|((c, w), n)| {
                if *n {
                    format!("{c:>w$}")
                } else {
                    format!("{c:<w$}")
                }
            })
            .collect();
        let _ = writeln!(out, "  {}", parts.join("  ").trim_end());
    };
    let numeric: Vec<bool> = (0..head.len())
        .map(|i| {
            t.rows
                .iter()
                .any(|r| r.get(i).is_some_and(Value::is_number))
        })
        .collect();
    line(out, &head, &numeric);
    for row in &cells {
        line(out, row, &numeric);
    }
    if t.omitted > 0 {
        let _ = writeln!(
            out,
            "  ({} more rows; raise --limit to see them)",
            t.omitted
        );
    }
}

/// Renders sections as one JSON object.
#[must_use]
pub fn json(meta: &Meta<'_>, sections: &[Section]) -> String {
    let roots: Vec<Value> = meta
        .roots
        .iter()
        .map(|r| {
            json!({"agent": r.agent.name(), "root": meta.root(r), "files": r.files,
                   "unreadable": r.unreadable, "lines_skipped": r.bad_lines,
                   "compressed_skipped": r.compressed})
        })
        .collect();
    let mut detectors = Map::new();
    for s in sections {
        detectors.insert(s.name.to_owned(), section_json(s));
    }
    let doc = json!({
        "schema_version": 1,
        "tool": "shaphan",
        "version": env!("CARGO_PKG_VERSION"),
        "since": meta.since.to_rfc3339(),
        "roots": roots,
        "skipped_roots": meta.skipped_roots,
        "detectors": detectors,
    });
    let mut s = serde_json::to_string_pretty(&doc).unwrap_or_default();
    s.push('\n');
    s
}

fn section_json(s: &Section) -> Value {
    let summary: Map<String, Value> = s.summary.iter().cloned().collect();
    let tables: Vec<Value> = s
        .tables
        .iter()
        .map(|t| {
            let rows: Vec<Value> = t
                .rows
                .iter()
                .map(|r| {
                    Value::Object(
                        t.columns
                            .iter()
                            .map(|c| (*c).to_owned())
                            .zip(r.iter().cloned())
                            .collect(),
                    )
                })
                .collect();
            json!({"title": t.title, "rows": rows, "omitted": t.omitted})
        })
        .collect();
    json!({"title": s.title, "summary": summary, "tables": tables, "notes": s.notes})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_nearest_rank() {
        let v = [5, 1, 4, 2, 3];
        assert_eq!(median(&v), 3);
        assert_eq!(percentile(&v, 90), 5);
        assert_eq!(percentile(&[], 90), 0);
        assert_eq!(median(&[7, 9]), 7);
    }

    #[test]
    fn ratio_rounds_and_handles_zero() {
        assert_eq!(ratio(1, 3), json!(0.33));
        assert_eq!(ratio(5, 0), Value::Null);
    }

    #[test]
    fn display_path_uses_tilde_only_at_a_boundary() {
        let home = Some("c:/users/tester");
        assert_eq!(
            display_path(r"C:\Users\Tester\.claude\CLAUDE.md", home),
            "~/.claude/CLAUDE.md"
        );
        assert_eq!(
            display_path("c:/users/tester2/x.md", home),
            "c:/users/tester2/x.md"
        );
        assert_eq!(display_path("/tmp/x", None), "/tmp/x");
    }
}
