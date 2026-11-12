//! Files read again after a context compaction that were read before it.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

use super::{
    codex_call, codex_call_line, est_tokens, from_results, input_str, name_of, result_chars,
    tool_results, tool_uses,
};
use crate::report::{Opts, Section, Table, display_path, ratio};
use crate::scan::{Agent, Detector, Record, Session};
use crate::shell::{normalise_path, read_targets};

#[derive(Default, Clone, Copy)]
struct Counts {
    sessions: u64,
    compactions: u64,
    reads: u64,
    rereads: u64,
    reread_chars: u64,
}

/// Re-reads per compaction, for both agents.
#[derive(Default)]
pub struct Rereads {
    before: HashSet<String>,
    current: HashSet<String>,
    compacted_in_window: bool,
    /// Claude `tool_use` ids of calls that re-read and whose result has not been read yet.
    awaited: HashSet<String>,
    totals: BTreeMap<(Agent, &'static str), Counts>,
    paths: HashMap<String, u64>,
}

impl Detector for Rereads {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude, Agent::Codex]
    }

    fn wants(&self, s: &Session, line: &str) -> bool {
        match s.agent {
            Agent::Claude => {
                line.contains("\"compact_boundary\"")
                    || line.contains("\"tool_use\"")
                    || (!self.awaited.is_empty() && line.contains("\"tool_result\""))
            }
            Agent::Codex => line.contains("\"type\":\"compacted\"") || codex_call_line(line),
        }
    }

    fn begin(&mut self, _s: &Session) {
        self.before.clear();
        self.current.clear();
        self.compacted_in_window = false;
        self.awaited.clear();
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
        match s.agent {
            Agent::Claude => {
                for b in tool_uses(r) {
                    let mut reread = false;
                    for path in claude_read_paths(b) {
                        reread |= self.read(s, &path, r.in_window);
                    }
                    if reread && let Some(id) = b.get("id").and_then(Value::as_str) {
                        self.awaited.insert(id.to_owned());
                    }
                }
                for b in tool_results(r) {
                    let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                    if self.awaited.remove(id) {
                        self.totals
                            .entry((s.agent, s.scope()))
                            .or_default()
                            .reread_chars += result_chars(b.get("content").unwrap_or(&Value::Null));
                    }
                }
            }
            Agent::Codex => {
                if let Some(call) = codex_call(r) {
                    let from_commands = call.commands().flat_map(read_targets);
                    for path in from_commands.chain(call.reads.iter().cloned()) {
                        self.read(s, &path, r.in_window);
                    }
                }
            }
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
                "reread_chars",
                "reread_tokens_est",
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
                from_results(*agent, c.reread_chars),
                from_results(*agent, est_tokens(c.reread_chars)),
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
                "Reread chars are the characters of text in the results of calls that re-read, each call counted once; a result holds what the tool returned, which can be more or less than the file. They are read for Claude only.".to_owned(),
                "Token estimates (tokens est) divide characters by 4; the model's own count differs.".to_owned(),
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

    /// Notes one read and says whether it is a re-read counted in the window.
    fn read(&mut self, s: &Session, raw: &str, in_window: bool) -> bool {
        let path = normalise_path(raw);
        if path.is_empty() {
            return false;
        }
        let mut reread = false;
        if in_window {
            let c = self.totals.entry((s.agent, s.scope())).or_default();
            c.reads += 1;
            if !self.current.contains(&path) && self.before.contains(&path) {
                c.rereads += 1;
                *self.paths.entry(path.clone()).or_default() += 1;
                reread = true;
            }
        }
        self.current.insert(path);
        reread
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

/// The paths one Claude `tool_use` block reads.
fn claude_read_paths(b: &Value) -> Vec<String> {
    match name_of(b) {
        "Read" => input_str(b, "file_path")
            .map(str::to_owned)
            .into_iter()
            .collect(),
        "Bash" => read_targets(input_str(b, "command").unwrap_or("")),
        _ => Vec::new(),
    }
}
