//! Files read again after a context compaction that were read before it.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

use super::{codex_call, codex_call_line, input_str, name_of, tool_uses};
use crate::report::{Opts, Section, Table, display_path, ratio};
use crate::scan::{Agent, Detector, Record, Session};
use crate::shell::{normalise_path, read_targets};

#[derive(Default, Clone, Copy)]
struct Counts {
    sessions: u64,
    compactions: u64,
    reads: u64,
    rereads: u64,
}

/// Re-reads per compaction, for both agents.
#[derive(Default)]
pub struct Rereads {
    before: HashSet<String>,
    current: HashSet<String>,
    compacted_in_window: bool,
    totals: BTreeMap<(Agent, &'static str), Counts>,
    paths: HashMap<String, u64>,
}

impl Detector for Rereads {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude, Agent::Codex]
    }

    fn wants(&self, s: &Session, line: &str) -> bool {
        match s.agent {
            Agent::Claude => line.contains("\"compact_boundary\"") || line.contains("\"tool_use\""),
            Agent::Codex => line.contains("\"type\":\"compacted\"") || codex_call_line(line),
        }
    }

    fn begin(&mut self, _s: &Session) {
        self.before.clear();
        self.current.clear();
        self.compacted_in_window = false;
    }

    fn record(&mut self, s: &Session, r: &Record<'_>) {
        let boundary = match s.agent {
            Agent::Claude => {
                r.kind() == "system"
                    && r.value.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
            }
            Agent::Codex => r.kind() == "compacted",
        };
        if boundary {
            self.boundary(s, r.in_window);
            return;
        }
        for path in read_paths(s.agent, r) {
            self.read(s, &path, r.in_window);
        }
    }

    fn end(&mut self, s: &Session) {
        if self.compacted_in_window {
            self.totals
                .entry((s.agent, s.scope()))
                .or_default()
                .sessions += 1;
        }
    }

    fn section(&self, opts: &Opts) -> Section {
        let mut table = Table::new(
            "By agent and scope",
            vec![
                "agent",
                "scope",
                "sessions_compacted",
                "compactions",
                "reads",
                "rereads",
                "rereads_per_compaction",
                "reread_share",
            ],
        );
        for ((agent, scope), c) in &self.totals {
            table.rows.push(vec![
                json!(agent.name()),
                json!(scope),
                json!(c.sessions),
                json!(c.compactions),
                json!(c.reads),
                json!(c.rereads),
                ratio(c.rereads, c.compactions),
                ratio(c.rereads, c.reads),
            ]);
        }
        table.truncate(opts.limit);
        let mut tables = vec![table];
        if opts.paths {
            tables.push(self.path_table(opts));
        }
        Section {
            name: "rereads",
            title: "files read again after a context compaction",
            summary: Vec::new(),
            tables,
            notes: vec![
                "A re-read is a file read after a compaction that was read before it and not yet since.".to_owned(),
                "Reread share divides re-reads by every read in the window, in compacted sessions or not.".to_owned(),
            ],
        }
    }
}

impl Rereads {
    fn boundary(&mut self, s: &Session, in_window: bool) {
        if in_window {
            self.compacted_in_window = true;
            self.totals
                .entry((s.agent, s.scope()))
                .or_default()
                .compactions += 1;
        }
        let current = std::mem::take(&mut self.current);
        self.before.extend(current);
    }

    fn read(&mut self, s: &Session, raw: &str, in_window: bool) {
        let path = normalise_path(raw);
        if path.is_empty() {
            return;
        }
        if in_window {
            let c = self.totals.entry((s.agent, s.scope())).or_default();
            c.reads += 1;
            if !self.current.contains(&path) && self.before.contains(&path) {
                c.rereads += 1;
                *self.paths.entry(path.clone()).or_default() += 1;
            }
        }
        self.current.insert(path);
    }

    fn path_table(&self, opts: &Opts) -> Table {
        let mut paths: Vec<(&String, &u64)> = self.paths.iter().collect();
        paths.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let mut table = Table::new("Most re-read paths", vec!["path", "rereads"]);
        table.rows = paths
            .into_iter()
            .map(|(p, n)| vec![json!(display_path(p, opts.home.as_deref())), json!(n)])
            .collect();
        table.truncate(opts.limit);
        table
    }
}

fn read_paths(agent: Agent, r: &Record<'_>) -> Vec<String> {
    let mut out = Vec::new();
    match agent {
        Agent::Claude => {
            for b in tool_uses(r) {
                match name_of(b) {
                    "Read" => out.extend(input_str(b, "file_path").map(str::to_owned)),
                    "Bash" => out.extend(read_targets(input_str(b, "command").unwrap_or(""))),
                    _ => {}
                }
            }
        }
        Agent::Codex => {
            if let Some(call) = codex_call(r) {
                out.extend(call.commands().flat_map(read_targets));
                out.extend(call.reads);
            }
        }
    }
    out
}
