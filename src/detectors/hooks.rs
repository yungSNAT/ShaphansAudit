//! Hook durations, cancellations and timeouts (Claude).

use std::collections::BTreeMap;
use std::rc::Rc;

use serde_json::{Value, json};

use super::chars;
use crate::programs::Programs;
use crate::report::{Opts, Section, Table, median, percentile};
use crate::scan::{Agent, Detector, Record, Session};
use crate::shell::hook_label;

#[derive(Default)]
struct Stats {
    runs: u64,
    cancelled: u64,
    timed_out: u64,
    errors: u64,
    durations: Vec<u64>,
    output_chars: Vec<u64>,
}

/// Per hook name (event and matcher) and label.
#[derive(Default)]
pub struct Hooks {
    hooks: BTreeMap<(String, String), Stats>,
    programs: Rc<Programs>,
}

impl Hooks {
    pub(super) fn new(programs: Rc<Programs>) -> Self {
        Self {
            programs,
            ..Self::default()
        }
    }
}

impl Detector for Hooks {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude]
    }

    fn wants(&self, _s: &Session, line: &str) -> bool {
        line.contains("\"type\":\"hook_")
    }

    fn record(&mut self, _s: &Session, r: &Record<'_>) {
        if !r.in_window || r.kind() != "attachment" {
            return;
        }
        let Some(a) = r.value.get("attachment") else {
            return;
        };
        let kind = a.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(
            kind,
            "hook_success" | "hook_cancelled" | "hook_non_blocking_error" | "hook_blocking_error"
        ) {
            return;
        }
        let field = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or("");
        let name = match (field("hookName").trim(), field("hookEvent").trim()) {
            ("", "") => "(unknown)",
            ("", event) => event,
            (n, _) => n,
        };
        let label = hook_label(field("command"), &mut |w| self.programs.has(w));
        let key = (name.to_owned(), label);
        let st = self.hooks.entry(key).or_default();
        st.runs += 1;
        let ms = a.get("durationMs").and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f.round() as u64))
        });
        if let Some(ms) = ms {
            st.durations.push(ms);
        }
        match kind {
            "hook_success" => st.output_chars.push(chars(field("content"))),
            "hook_cancelled" => {
                st.cancelled += 1;
                if a.get("timedOut").and_then(Value::as_bool) == Some(true) {
                    st.timed_out += 1;
                }
            }
            _ => st.errors += 1,
        }
    }

    fn section(&self, opts: &Opts) -> Section {
        let mut rows: Vec<((u64, u64), Vec<Value>)> = self
            .hooks
            .iter()
            .map(|((name, label), s)| {
                let p90 = percentile(&s.durations, 90);
                let or_null = |v: &[u64], x: u64| if v.is_empty() { Value::Null } else { json!(x) };
                (
                    (s.cancelled, p90),
                    vec![
                        json!(name),
                        json!(label),
                        json!(s.runs),
                        json!(s.cancelled),
                        json!(s.timed_out),
                        json!(s.errors),
                        or_null(&s.durations, median(&s.durations)),
                        or_null(&s.durations, p90),
                        json!(s.durations.iter().max()),
                        or_null(&s.output_chars, median(&s.output_chars)),
                    ],
                )
            })
            .collect();
        rows.sort_by_key(|(key, _)| std::cmp::Reverse(*key));
        let mut table = Table::new(
            "By hook name and command, most cancellations first",
            vec![
                "hook_name",
                "hook",
                "runs",
                "cancelled",
                "timed_out",
                "errors",
                "median_ms",
                "p90_ms",
                "max_ms",
                "median_output_chars",
            ],
        );
        table.rows = rows.into_iter().map(|(_, r)| r).collect();
        table.truncate(opts.limit);
        Section {
            name: "hooks",
            title: "hook runs, cancellations and durations (Claude)",
            summary: Vec::new(),
            tables: vec![table],
            notes: Vec::new(),
        }
    }
}
