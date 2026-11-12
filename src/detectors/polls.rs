//! Commands repeated within a session, with or without a file edit in between.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use serde_json::{Value, json};

use super::{
    codex_call, codex_call_line, est_tokens, from_results, input_str, name_of, result_chars,
    tool_results, tool_uses,
};
use crate::programs::Programs;
use crate::report::{Opts, Section, Table};
use crate::scan::{Agent, Detector, Record, Session};
use crate::shell::{Step, verb_class};

const CLAUDE_EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

#[derive(Default, Clone, Copy)]
struct Counts {
    commands: u64,
    polls: u64,
    polls_after_failure: u64,
    poll_chars: u64,
    reruns_after_edit: u64,
}

impl Counts {
    /// The columns every row shares, after those that key it.
    const COLUMNS: [&'static str; 5] = [
        "polls",
        "polls_after_failure",
        "poll_chars",
        "poll_tokens_est",
        "reruns_after_edit",
    ];

    /// The cells under `COLUMNS`.
    fn cells(&self, agent: Agent) -> [Value; 5] {
        [
            json!(self.polls),
            from_results(agent, self.polls_after_failure),
            from_results(agent, self.poll_chars),
            from_results(agent, est_tokens(self.poll_chars)),
            json!(self.reruns_after_edit),
        ]
    }
}

/// A Claude Bash run whose result has not been read yet.
struct Awaited {
    command: String,
    /// The verb class, when the run is a poll counted in the window.
    poll: Option<String>,
}

/// Exact repeats of a command, split by whether a file edit came between.
#[derive(Default)]
pub struct Polls {
    last_seen: HashMap<String, u64>,
    edits: u64,
    /// Claude runs by `tool_use` id.
    awaited: HashMap<String, Awaited>,
    /// Commands whose last result read reported an error.
    failed: HashSet<String>,
    totals: BTreeMap<(Agent, &'static str), Counts>,
    verbs: BTreeMap<(Agent, String), Counts>,
    programs: Rc<Programs>,
}

impl Detector for Polls {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude, Agent::Codex]
    }

    fn wants(&self, s: &Session, line: &str) -> bool {
        match s.agent {
            Agent::Claude => {
                line.contains("\"tool_use\"")
                    || (!self.awaited.is_empty() && line.contains("\"tool_result\""))
            }
            Agent::Codex => codex_call_line(line),
        }
    }

    fn begin(&mut self, _s: &Session) {
        self.last_seen.clear();
        self.edits = 0;
        self.awaited.clear();
        self.failed.clear();
    }

    fn record(&mut self, s: &Session, r: &Record<'_>) {
        match s.agent {
            Agent::Claude => {
                for b in tool_uses(r) {
                    let name = name_of(b);
                    if name == "Bash" {
                        let id = b.get("id").and_then(Value::as_str);
                        self.observe(s, input_str(b, "command").unwrap_or(""), r.in_window, id);
                    } else if CLAUDE_EDIT_TOOLS.contains(&name) {
                        self.edits += 1;
                    }
                }
                for b in tool_results(r) {
                    self.result(s, b);
                }
            }
            Agent::Codex => {
                for step in codex_call(r).map(|c| c.steps).unwrap_or_default() {
                    match step {
                        Step::Command(c) => self.observe(s, &c, r.in_window, None),
                        Step::Edit => self.edits += 1,
                    }
                }
            }
        }
    }

    fn section(&self, opts: &Opts) -> Section {
        let columns = |keys: &[&'static str]| [keys, &Counts::COLUMNS[..]].concat();
        let mut totals = Table::new(
            "By agent and scope",
            columns(&["agent", "scope", "commands"]),
        );
        for ((agent, scope), c) in &self.totals {
            let mut row = vec![json!(agent.name()), json!(scope), json!(c.commands)];
            row.extend(c.cells(*agent));
            totals.rows.push(row);
        }
        totals.truncate(opts.limit);
        let mut rows: Vec<(u64, Vec<Value>)> = self
            .verbs
            .iter()
            .filter(|(_, c)| c.polls + c.reruns_after_edit > 0)
            .map(|((agent, verb), c)| {
                let mut row = vec![json!(agent.name()), json!(verb)];
                row.extend(c.cells(*agent));
                (c.polls, row)
            })
            .collect();
        rows.sort_by_key(|(key, _)| std::cmp::Reverse(*key));
        let mut table = Table::new(
            "By verb class, most polls first",
            columns(&["agent", "verb"]),
        );
        table.rows = rows.into_iter().map(|(_, r)| r).collect();
        table.truncate(opts.limit);
        Section {
            name: "polls",
            title: "commands repeated exactly within a session",
            summary: Vec::new(),
            tables: vec![totals, table],
            notes: vec![
                "A poll repeats a command with no file edit since its last run; a rerun follows an edit.".to_owned(),
                "Edits made through shell commands are not seen, so some polls may be reruns.".to_owned(),
                "Polls after failure repeat a command whose last result was marked as an error, which a hook's denial also is; poll chars are the characters of text in polls' results. Both are read for Claude only.".to_owned(),
                "Token estimates (tokens est) divide characters by 4; the model's own count differs.".to_owned(),
            ],
        }
    }
}

impl Polls {
    pub(super) fn new(programs: Rc<Programs>) -> Self {
        Self {
            programs,
            ..Self::default()
        }
    }

    /// Counts one run of a command. `id` is the Claude `tool_use` id that its result
    /// will carry.
    fn observe(&mut self, s: &Session, cmd: &str, in_window: bool, id: Option<&str>) {
        let key = cmd.trim();
        if key.is_empty() {
            return;
        }
        let previous = self.last_seen.insert(key.to_owned(), self.edits);
        let mut poll = None;
        if in_window {
            let total = self.totals.entry((s.agent, s.scope())).or_default();
            total.commands += 1;
            if let Some(prev_edits) = previous {
                let class = verb_class(key, &mut |w| self.programs.has(w));
                let verb = self.verbs.entry((s.agent, class.clone())).or_default();
                if prev_edits == self.edits {
                    total.polls += 1;
                    verb.polls += 1;
                    if self.failed.contains(key) {
                        total.polls_after_failure += 1;
                        verb.polls_after_failure += 1;
                    }
                    poll = Some(class);
                } else {
                    total.reruns_after_edit += 1;
                    verb.reruns_after_edit += 1;
                }
            }
        }
        if let Some(id) = id {
            let command = key.to_owned();
            self.awaited
                .insert(id.to_owned(), Awaited { command, poll });
        }
    }

    /// Reads the result of an awaited run: its size when the run was a counted poll,
    /// and whether it failed, for the next run of the same command.
    fn result(&mut self, s: &Session, b: &Value) {
        let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
        let Some(run) = self.awaited.remove(id) else {
            return;
        };
        if let Some(class) = run.poll {
            let n = result_chars(b.get("content").unwrap_or(&Value::Null));
            self.totals
                .entry((s.agent, s.scope()))
                .or_default()
                .poll_chars += n;
            self.verbs.entry((s.agent, class)).or_default().poll_chars += n;
        }
        if b.get("is_error").and_then(Value::as_bool) == Some(true) {
            self.failed.insert(run.command);
        } else {
            self.failed.remove(&run.command);
        }
    }
}
