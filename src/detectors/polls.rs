//! Commands repeated within a session, with or without a file edit in between.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use serde_json::{Value, json};

use super::{codex_call, codex_call_line, input_str, name_of, tool_uses};
use crate::programs::Programs;
use crate::report::{Opts, Section, Table};
use crate::scan::{Agent, Detector, Record, Session};
use crate::shell::{Step, verb_class};

const CLAUDE_EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

#[derive(Default, Clone, Copy)]
struct Counts {
    commands: u64,
    polls: u64,
    reruns_after_edit: u64,
}

/// Exact repeats of a command, split by whether a file edit came between.
#[derive(Default)]
pub struct Polls {
    last_seen: HashMap<String, u64>,
    edits: u64,
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
            Agent::Claude => line.contains("\"tool_use\""),
            Agent::Codex => codex_call_line(line),
        }
    }

    fn begin(&mut self, _s: &Session) {
        self.last_seen.clear();
        self.edits = 0;
    }

    fn record(&mut self, s: &Session, r: &Record<'_>) {
        match s.agent {
            Agent::Claude => {
                for b in tool_uses(r) {
                    let name = name_of(b);
                    if name == "Bash" {
                        self.observe(s, input_str(b, "command").unwrap_or(""), r.in_window);
                    } else if CLAUDE_EDIT_TOOLS.contains(&name) {
                        self.edits += 1;
                    }
                }
            }
            Agent::Codex => {
                for step in codex_call(r).map(|c| c.steps).unwrap_or_default() {
                    match step {
                        Step::Command(c) => self.observe(s, &c, r.in_window),
                        Step::Edit => self.edits += 1,
                    }
                }
            }
        }
    }

    fn section(&self, opts: &Opts) -> Section {
        let mut totals = Table::new(
            "By agent and scope",
            vec!["agent", "scope", "commands", "polls", "reruns_after_edit"],
        );
        for ((agent, scope), c) in &self.totals {
            totals.rows.push(vec![
                json!(agent.name()),
                json!(scope),
                json!(c.commands),
                json!(c.polls),
                json!(c.reruns_after_edit),
            ]);
        }
        totals.truncate(opts.limit);
        let mut rows: Vec<(u64, Vec<Value>)> = self
            .verbs
            .iter()
            .filter(|(_, c)| c.polls + c.reruns_after_edit > 0)
            .map(|((agent, verb), c)| {
                (
                    c.polls,
                    vec![
                        json!(agent.name()),
                        json!(verb),
                        json!(c.polls),
                        json!(c.reruns_after_edit),
                    ],
                )
            })
            .collect();
        rows.sort_by_key(|(key, _)| std::cmp::Reverse(*key));
        let mut table = Table::new(
            "By verb class, most polls first",
            vec!["agent", "verb", "polls", "reruns_after_edit"],
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

    fn observe(&mut self, s: &Session, cmd: &str, in_window: bool) {
        let key = cmd.trim();
        if key.is_empty() {
            return;
        }
        let previous = self.last_seen.insert(key.to_owned(), self.edits);
        if !in_window {
            return;
        }
        let total = self.totals.entry((s.agent, s.scope())).or_default();
        total.commands += 1;
        let Some(prev_edits) = previous else { return };
        let class = verb_class(key, &mut |w| self.programs.has(w));
        let verb = self.verbs.entry((s.agent, class)).or_default();
        if prev_edits == self.edits {
            total.polls += 1;
            verb.polls += 1;
        } else {
            total.reruns_after_edit += 1;
            verb.reruns_after_edit += 1;
        }
    }
}
