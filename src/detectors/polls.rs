//! Commands repeated within a session, with or without a file edit in between.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use serde_json::{Value, json};

use super::{
    CodexCalls, EST_NOTE, chars, codex_call, codex_call_line, codex_output, est_tokens, input_str,
    name_of, result_chars, tool_results, tool_uses,
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
    fn cells(&self) -> [Value; 5] {
        [
            json!(self.polls),
            json!(self.polls_after_failure),
            json!(self.poll_chars),
            json!(est_tokens(self.poll_chars)),
            json!(self.reruns_after_edit),
        ]
    }
}

/// A run whose result has not been read yet.
struct Awaited {
    command: String,
    /// The verb class, when the run is a poll counted in the window.
    poll: Option<String>,
    /// The run's number in the scan.
    run: u64,
}

/// What is known of a command's latest run.
#[derive(PartialEq, Eq)]
enum Latest {
    /// Its result, by run number, is awaited.
    Awaited(u64),
    /// Its result was read and reported an error.
    Failed,
}

/// Exact repeats of a command, split by whether a file edit came between.
#[derive(Default)]
pub struct Polls {
    last_seen: HashMap<String, u64>,
    edits: u64,
    /// The runs awaited so far, which number each one.
    runs: u64,
    /// Claude runs by `tool_use` id.
    awaited: HashMap<String, Awaited>,
    /// The ids of the session's Claude `tool_use` blocks so far.
    claude_ids: HashSet<String>,
    /// Codex `exec` calls whose output has not been read yet, with each command's run.
    codex: CodexCalls<Option<Awaited>>,
    /// By command, its latest run's awaited result or failure. A command whose latest
    /// run succeeded, or whose result will never be read, has no entry.
    latest: HashMap<String, Latest>,
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
                    || (line.contains("\"tool_result\"")
                        && self.awaited.keys().any(|id| line.contains(id.as_str())))
            }
            Agent::Codex => codex_call_line(line) || self.codex.wants(line),
        }
    }

    fn begin(&mut self, _s: &Session) {
        self.last_seen.clear();
        self.edits = 0;
        self.awaited.clear();
        self.claude_ids.clear();
        self.latest.clear();
    }

    fn end(&mut self, _s: &Session) {
        self.codex.end();
    }

    fn record(&mut self, s: &Session, r: &Record<'_>) {
        match s.agent {
            Agent::Claude => {
                for b in tool_uses(r) {
                    let id = b.get("id").and_then(Value::as_str);
                    // An id carried by more than one block pairs with neither from
                    // then on, as their results cannot be told apart.
                    let fresh = id.is_some_and(|id| self.claude_ids.insert(id.to_owned()));
                    if !fresh && let Some(id) = id {
                        self.awaited.remove(id);
                    }
                    let name = name_of(b);
                    if name == "Bash" {
                        let cmd = input_str(b, "command").unwrap_or("");
                        if let Some(run) = self.observe(s, cmd, r.in_window, fresh)
                            && let Some(id) = id
                        {
                            self.awaited.insert(id.to_owned(), run);
                        }
                    } else if CLAUDE_EDIT_TOOLS.contains(&name) {
                        self.edits += 1;
                    }
                }
                for b in tool_results(r) {
                    let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                    if let Some(run) = self.awaited.remove(id) {
                        let failed = b.get("is_error").and_then(Value::as_bool) == Some(true);
                        self.result(s, run, failed, || {
                            result_chars(b.get("content").unwrap_or(&Value::Null))
                        });
                    }
                }
            }
            Agent::Codex => self.codex(s, r),
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
            row.extend(c.cells());
            totals.rows.push(row);
        }
        totals.truncate(opts.limit);
        let mut rows: Vec<(u64, Vec<Value>)> = self
            .verbs
            .iter()
            .filter(|(_, c)| c.polls + c.reruns_after_edit > 0)
            .map(|((agent, verb), c)| {
                let mut row = vec![json!(agent.name()), json!(verb)];
                row.extend(c.cells());
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
            summary: vec![
                (
                    "codex_outputs_not_paired".to_owned(),
                    json!(self.codex.unpaired),
                ),
                (
                    "codex_commands_not_paired".to_owned(),
                    json!(self.codex.unpaired_commands),
                ),
            ],
            tables: vec![totals, table],
            notes: vec![
                "A poll repeats a command with no file edit since its last run; a rerun follows an edit.".to_owned(),
                "Edits made through shell commands are not seen, so some polls may be reruns.".to_owned(),
                "Polls after failure repeat a command whose previous run's result was read before the repeat and marked as an error, which a hook's denial also is; poll chars are the characters of text in polls' results. A Codex result counts only when its exec cell printed one whole exec_command result per command, alone or inside a printed list or object; Codex marks a failure by a non-zero exit code. Codex outputs not paired counts the exec cells in the window whose results could not be paired: the cell had no call id, another exec cell in the session had its call id, its output never came or it did not print one result per command. Codex commands not paired counts their commands, whose results were not judged.".to_owned(),
                EST_NOTE.to_owned(),
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

    /// Counts one run of a command and gives the run whose result to await, if
    /// `awaits`. The run becomes the command's latest, so until its result is read
    /// the next run does not follow a failure; a run whose result is not awaited
    /// leaves that unknown. A blank command counts as a command, as
    /// `codex_commands_not_paired` counts it too, but is never a poll or rerun.
    fn observe(
        &mut self,
        s: &Session,
        cmd: &str,
        in_window: bool,
        awaits: bool,
    ) -> Option<Awaited> {
        let key = cmd.trim();
        if in_window {
            self.totals
                .entry((s.agent, s.scope()))
                .or_default()
                .commands += 1;
        }
        if key.is_empty() {
            return None;
        }
        let previous = self.last_seen.insert(key.to_owned(), self.edits);
        let mut poll = None;
        if in_window {
            let total = self.totals.entry((s.agent, s.scope())).or_default();
            if let Some(prev_edits) = previous {
                let class = verb_class(key, &mut |w| self.programs.has(w));
                let verb = self.verbs.entry((s.agent, class.clone())).or_default();
                if prev_edits == self.edits {
                    total.polls += 1;
                    verb.polls += 1;
                    if self.latest.get(key) == Some(&Latest::Failed) {
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
        if !awaits {
            self.latest.remove(key);
            return None;
        }
        self.runs += 1;
        self.latest
            .insert(key.to_owned(), Latest::Awaited(self.runs));
        Some(Awaited {
            command: key.to_owned(),
            poll,
            run: self.runs,
        })
    }

    /// Reads a Codex call's commands, or its output.
    fn codex(&mut self, s: &Session, r: &Record<'_>) {
        if let Some(call) = codex_call(r) {
            let awaits = call.exec && !call.call_id.is_empty();
            let mut runs = Vec::new();
            for step in call.steps {
                match step {
                    Step::Command(c) => runs.push(self.observe(s, &c, r.in_window, awaits)),
                    Step::Edit => self.edits += 1,
                }
            }
            if call.exec {
                let runs = (!runs.is_empty()).then_some(runs);
                self.codex.call(&call.call_id, runs, r.in_window);
            }
        } else if let Some(out) = codex_output(r) {
            for (run, res) in self.codex.pair(&out) {
                if let Some(run) = run {
                    self.result(s, run, res.exit_code != 0, || chars(&res.output));
                }
            }
        }
    }

    /// Reads the result of an awaited run: its size when the run was a counted poll,
    /// and, when it is still its command's latest run, whether it failed.
    fn result(&mut self, s: &Session, run: Awaited, failed: bool, size: impl FnOnce() -> u64) {
        if let Some(class) = run.poll {
            let n = size();
            self.totals
                .entry((s.agent, s.scope()))
                .or_default()
                .poll_chars += n;
            self.verbs.entry((s.agent, class)).or_default().poll_chars += n;
        }
        if self.latest.get(&run.command) != Some(&Latest::Awaited(run.run)) {
            return;
        }
        if failed {
            self.latest.insert(run.command, Latest::Failed);
        } else {
            self.latest.remove(&run.command);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_results_of_awaited_runs_are_parsed() {
        let s = Session {
            agent: Agent::Claude,
            subagent: false,
        };
        let mut d = Polls::default();
        let run = d.observe(&s, "git status", true, true).expect("a run");
        d.awaited.insert("toolu_a".to_owned(), run);
        let result = |id: &str| {
            format!(
                r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"{id}"}}]}}}}"#
            )
        };
        assert!(d.wants(&s, &result("toolu_a")));
        assert!(!d.wants(&s, &result("toolu_b")));
    }

    const CLAUDE: Session = Session {
        agent: Agent::Claude,
        subagent: false,
    };

    /// A Claude `tool_use` block running `git status` with id `id`.
    fn call(id: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"{id}","name":"Bash","input":{{"command":"git status"}}}}]}}}}"#
        )
    }

    /// The main-scope totals after one Claude session of `lines`, all in the window.
    fn claude_totals(lines: &[String]) -> Counts {
        let mut d = Polls::default();
        d.begin(&CLAUDE);
        for line in lines {
            if d.wants(&CLAUDE, line) {
                let value: Value = serde_json::from_str(line).expect("valid JSON");
                d.record(
                    &CLAUDE,
                    &Record {
                        value: &value,
                        in_window: true,
                    },
                );
            }
        }
        d.end(&CLAUDE);
        d.totals[&(Agent::Claude, "main")]
    }

    #[test]
    fn a_claude_run_whose_result_never_comes_hides_an_earlier_failure() {
        let failure = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_a","is_error":true}]}}"#;
        let c = claude_totals(&[
            call("toolu_a"),
            failure.to_owned(),
            call("toolu_b"),
            call("toolu_c"),
        ]);
        // b follows the failure; c follows b, whose result never came.
        assert_eq!((c.polls, c.polls_after_failure), (2, 1));
    }

    #[test]
    fn a_reused_claude_tool_use_id_pairs_with_neither_run() {
        let result = |is_error: bool| {
            format!(
                r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_a","is_error":{is_error},"content":"clean"}}]}}}}"#
            )
        };
        // The repeat is a poll, but the one result may be the first run's, so its
        // characters are not the poll's.
        let c = claude_totals(&[call("toolu_a"), call("toolu_a"), result(false)]);
        assert_eq!((c.polls, c.poll_chars), (1, 0));
        // Nor does a failure read for the reused id mark the next run.
        let c = claude_totals(&[
            call("toolu_a"),
            call("toolu_a"),
            result(true),
            call("toolu_b"),
        ]);
        assert_eq!((c.polls, c.polls_after_failure), (2, 0));
    }
}
