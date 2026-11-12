//! What loads before the first reply of each main Claude session.

use std::collections::BTreeMap;
use std::rc::Rc;

use serde_json::{Value, json};

use super::{chars, json_chars};
use crate::programs::Programs;
use crate::report::{Opts, Section, Table, display_path, median};
use crate::scan::{Agent, Detector, Record, Session};
use crate::shell::hook_label;

const FILE_PREFIX: &str = "file: ";

#[derive(Default)]
struct Current {
    pieces: BTreeMap<String, u64>,
    started_in_window: Option<bool>,
    replied: bool,
    /// Input tokens of the first reply's request, as Claude Code recorded them.
    first_tokens: Option<u64>,
    shape_seen: bool,
    entrypoint: Option<String>,
}

/// Sizes of what a main session loads before its first reply.
#[derive(Default)]
pub struct Startup {
    cur: Current,
    totals: Vec<u64>,
    first_tokens: Vec<u64>,
    pieces: BTreeMap<String, Vec<u64>>,
    entrypoints: BTreeMap<String, u64>,
    programs: Rc<Programs>,
}

impl Startup {
    pub(super) fn new(programs: Rc<Programs>) -> Self {
        Self {
            programs,
            ..Self::default()
        }
    }
}

impl Detector for Startup {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude]
    }

    fn wants(&self, s: &Session, line: &str) -> bool {
        if s.subagent || self.cur.replied || self.cur.started_in_window == Some(false) {
            return false;
        }
        let shape = !self.cur.shape_seen && line.contains("\"type\":\"api-request-shape\"");
        shape || line.contains("\"type\":\"attachment\"") || line.contains("\"type\":\"assistant\"")
    }

    fn begin(&mut self, _s: &Session) {
        self.cur = Current::default();
    }

    fn record(&mut self, _s: &Session, r: &Record<'_>) {
        let cur = &mut self.cur;
        if cur.started_in_window.is_none() {
            cur.started_in_window = Some(r.in_window);
        }
        if cur.entrypoint.is_none() {
            cur.entrypoint = r
                .value
                .get("entrypoint")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        match r.kind() {
            "assistant" if !cur.replied => {
                cur.replied = true;
                cur.first_tokens = input_tokens(r.value);
            }
            "attachment" if !cur.replied => {
                if let Some(a) = r.value.get("attachment") {
                    add_attachment(&mut cur.pieces, a, &self.programs);
                }
            }
            "api-request-shape" if !cur.shape_seen => {
                cur.shape_seen = true;
                let loaded = loaded_schema_chars(r.value);
                *cur.pieces
                    .entry("tool schemas (loaded)".to_owned())
                    .or_default() += loaded;
            }
            _ => {}
        }
    }

    fn end(&mut self, s: &Session) {
        let cur = std::mem::take(&mut self.cur);
        if s.subagent || cur.started_in_window != Some(true) || !cur.replied {
            return;
        }
        self.totals.push(cur.pieces.values().sum());
        self.first_tokens.extend(cur.first_tokens);
        for (k, v) in cur.pieces {
            self.pieces.entry(k).or_default().push(v);
        }
        *self
            .entrypoints
            .entry(cur.entrypoint.unwrap_or_else(|| "unknown".to_owned()))
            .or_default() += 1;
    }

    fn section(&self, opts: &Opts) -> Section {
        let mut rows: Vec<(u64, Vec<Value>)> = self
            .pieces
            .iter()
            .map(|(k, v)| {
                let name = k.strip_prefix(FILE_PREFIX).map_or_else(
                    || k.clone(),
                    |p| format!("{FILE_PREFIX}{}", display_path(p, opts.home.as_deref())),
                );
                let med = median(v);
                (
                    med * v.len() as u64,
                    vec![
                        json!(name),
                        json!(v.len()),
                        json!(med),
                        json!(v.iter().max()),
                    ],
                )
            })
            .collect();
        rows.sort_by_key(|(key, _)| std::cmp::Reverse(*key));
        let mut table = Table::new(
            "By source",
            vec!["piece", "sessions", "median_chars", "max_chars"],
        );
        table.rows = rows.into_iter().map(|(_, r)| r).collect();
        table.truncate(opts.limit);
        let entry: Vec<String> = self
            .entrypoints
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect();
        Section {
            name: "startup",
            title: "loaded before the first reply of each main session (Claude)",
            summary: vec![
                ("sessions".to_owned(), json!(self.totals.len())),
                ("entry_points".to_owned(), json!(entry.join(", "))),
                ("median_total_chars".to_owned(), json!(median(&self.totals))),
                (
                    "median_first_request_tokens".to_owned(),
                    if self.first_tokens.is_empty() {
                        Value::Null
                    } else {
                        json!(median(&self.first_tokens))
                    },
                ),
            ],
            tables: vec![table],
            notes: vec![
                "Only sessions whose first attachment, request or reply falls in the window are measured.".to_owned(),
                "First request tokens are the input tokens Claude Code recorded for the first reply, cached or not; characters by source are not tokens.".to_owned(),
            ],
        }
    }
}

fn add_attachment(pieces: &mut BTreeMap<String, u64>, a: &Value, programs: &Programs) {
    let kind = a.get("type").and_then(Value::as_str).unwrap_or("unknown");
    let str_field = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or("");
    let field = |k: &str| a.get(k).unwrap_or(&Value::Null);
    let mut add = |k: String, n: u64| *pieces.entry(k).or_default() += n;
    match kind {
        "hook_success" => add(
            format!(
                "hook: {}",
                hook_label(str_field("command"), &mut |w| programs.has(w))
            ),
            chars(str_field("content")),
        ),
        "instructions" => {
            for f in items(field("files")) {
                let path = f.get("path").and_then(Value::as_str).unwrap_or("(unknown)");
                let content = f.get("content").and_then(Value::as_str).unwrap_or("");
                add(format!("{FILE_PREFIX}{path}"), chars(content));
            }
        }
        "mcp_instructions_delta" => {
            for (n, b) in items(field("addedNames")).zip(items(field("addedBlocks"))) {
                let name = n.as_str().unwrap_or("?");
                add(format!("mcp instructions: {name}"), json_chars(b));
            }
        }
        "deferred_tools_delta" => add(
            "deferred tool names".to_owned(),
            line_chars(field("addedLines")),
        ),
        "agent_listing_delta" => add("agent listing".to_owned(), line_chars(field("addedLines"))),
        "skill_listing" => add("skill listing".to_owned(), chars(str_field("content"))),
        "prompt_snapshot" => add(
            "system prompt".to_owned(),
            text_chars(field("systemPrompt")),
        ),
        "session_context" => add("session context".to_owned(), json_chars(field("context"))),
        "environment" => add("environment".to_owned(), json_chars(field("snapshot"))),
        other => add(format!("attachment: {other}"), json_chars(a)),
    }
}

/// A reply's recorded input tokens: new, cache writes and cache reads.
fn input_tokens(reply: &Value) -> Option<u64> {
    let usage = reply.pointer("/message/usage")?;
    let field = |k: &str| usage.get(k).and_then(Value::as_u64);
    Some(
        field("input_tokens")?
            + field("cache_creation_input_tokens").unwrap_or(0)
            + field("cache_read_input_tokens").unwrap_or(0),
    )
}

/// The elements of a JSON array, or none for any other value.
fn items(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

fn line_chars(lines: &Value) -> u64 {
    items(lines).map(|l| json_chars(l) + 1).sum()
}

fn text_chars(v: &Value) -> u64 {
    match v {
        Value::Array(items) => items.iter().map(json_chars).sum(),
        Value::Null => 0,
        other => json_chars(other),
    }
}

/// Characters of the tool definitions sent without `defer_loading`.
pub fn loaded_schema_chars(shape_record: &Value) -> u64 {
    loaded_tools(shape_record).map(|(_, n)| n).sum()
}

/// Each loaded tool's name and compact JSON size in an `api-request-shape` record.
pub fn loaded_tools(shape_record: &Value) -> impl Iterator<Item = (&str, u64)> {
    shape_record
        .pointer("/shape/tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|t| t.get("defer_loading").and_then(Value::as_bool) != Some(true))
        .map(|t| {
            (
                t.get("name").and_then(Value::as_str).unwrap_or("?"),
                json_chars(t),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shape_after_the_first_reply_is_not_startup() {
        let s = Session {
            agent: Agent::Claude,
            subagent: false,
        };
        let mut d = Startup::default();
        d.begin(&s);
        for line in [
            r#"{"type":"assistant","timestamp":"2026-11-08T01:00:00Z"}"#,
            r#"{"type":"api-request-shape","timestamp":"2026-11-08T01:00:01Z","shape":{"tools":[{"name":"Read"}]}}"#,
        ] {
            if d.wants(&s, line) {
                let value: Value = serde_json::from_str(line).expect("valid JSON");
                d.record(
                    &s,
                    &Record {
                        value: &value,
                        in_window: true,
                    },
                );
            }
        }
        d.end(&s);
        assert_eq!(d.totals, vec![0]);
        assert!(!d.pieces.contains_key("tool schemas (loaded)"));
    }
}
