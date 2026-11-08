//! Tool schemas loaded into Claude requests against the tools called.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::startup::loaded_tools;
use super::{name_of, tool_uses};
use crate::report::{Opts, Section, Table, median};
use crate::scan::{Agent, Detector, Record, Session};

#[derive(Default)]
struct Current {
    loaded: BTreeMap<String, u64>,
    called: BTreeMap<String, u64>,
    in_window: bool,
}

#[derive(Default)]
struct Group {
    tools: BTreeSet<String>,
    sessions_loaded: u64,
    sessions_called: u64,
    calls: u64,
    chars: Vec<u64>,
    idle_chars: u64,
}

/// Loaded-against-called counts, by tool or MCP server.
#[derive(Default)]
pub struct Schemas {
    cur: Current,
    groups: BTreeMap<(&'static str, String), Group>,
    sessions: BTreeMap<&'static str, u64>,
}

/// A built-in tool's own name, or `mcp__<server>__*` for an MCP tool.
#[must_use]
pub fn group_of(tool: &str) -> String {
    match tool
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split("__").next())
    {
        Some(server) => format!("mcp__{server}__*"),
        None => tool.to_owned(),
    }
}

impl Detector for Schemas {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude]
    }

    fn wants(&self, _s: &Session, line: &str) -> bool {
        line.contains("\"type\":\"api-request-shape\"") || line.contains("\"tool_use\"")
    }

    fn begin(&mut self, _s: &Session) {
        self.cur = Current::default();
    }

    fn record(&mut self, _s: &Session, r: &Record<'_>) {
        if !r.in_window {
            return;
        }
        if r.kind() == "api-request-shape" {
            self.cur.in_window = true;
            for (name, n) in loaded_tools(r.value) {
                let e = self.cur.loaded.entry(name.to_owned()).or_default();
                *e = (*e).max(n);
            }
        }
        for b in tool_uses(r) {
            *self.cur.called.entry(name_of(b).to_owned()).or_default() += 1;
        }
    }

    fn end(&mut self, s: &Session) {
        let cur = std::mem::take(&mut self.cur);
        if !cur.in_window {
            return;
        }
        *self.sessions.entry(s.scope()).or_default() += 1;
        let mut per_group: BTreeMap<String, (u64, u64, Vec<&String>)> = BTreeMap::new();
        for (tool, n) in &cur.loaded {
            let g = per_group.entry(group_of(tool)).or_default();
            g.0 += n;
            g.2.push(tool);
        }
        for (tool, calls) in &cur.called {
            if let Some(g) = per_group.get_mut(&group_of(tool)) {
                g.1 += calls;
            }
        }
        for (name, (chars, calls, tools)) in per_group {
            let g = self.groups.entry((s.scope(), name)).or_default();
            g.tools.extend(tools.into_iter().cloned());
            g.sessions_loaded += 1;
            g.calls += calls;
            g.chars.push(chars);
            if calls > 0 {
                g.sessions_called += 1;
            } else {
                g.idle_chars += chars;
            }
        }
    }

    fn section(&self, opts: &Opts) -> Section {
        let mut rows: Vec<(u64, Vec<Value>)> = self
            .groups
            .iter()
            .map(|((scope, name), g)| {
                (
                    g.idle_chars,
                    vec![
                        json!(scope),
                        json!(name),
                        json!(g.tools.len()),
                        json!(g.sessions_loaded),
                        json!(g.sessions_called),
                        json!(g.calls),
                        json!(median(&g.chars)),
                        json!(g.idle_chars),
                    ],
                )
            })
            .collect();
        rows.sort_by_key(|(key, _)| std::cmp::Reverse(*key));
        let mut table = Table::new(
            "By tool or MCP server, most idle characters first",
            vec![
                "scope",
                "tool",
                "tools",
                "sessions_loaded",
                "sessions_called",
                "calls",
                "median_chars",
                "idle_chars",
            ],
        );
        table.rows = rows.into_iter().map(|(_, r)| r).collect();
        table.truncate(opts.limit);
        Section {
            name: "schemas",
            title: "tool schemas loaded against tools called (Claude)",
            summary: vec![
                ("main_sessions".to_owned(), json!(self.sessions.get("main").copied().unwrap_or(0))),
                ("subagent_sessions".to_owned(), json!(self.sessions.get("subagent").copied().unwrap_or(0))),
            ],
            tables: vec![table],
            notes: vec![
                "Idle characters are schema characters loaded in sessions that never called the tool.".to_owned(),
                "Tools sent with defer_loading are not counted as loaded, and a tool only ever sent that way gets no row.".to_owned(),
            ],
        }
    }
}
