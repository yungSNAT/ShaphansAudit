//! Test runs whose exit status a pipe into `tail` or `head` hides (Claude Bash and
//! Codex commands).

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{
    ClaudeCalls, CodexCalls, codex_call, codex_call_line, codex_output, input_str, name_of,
    text_of, tool_results, tool_uses,
};
use crate::report::{Opts, Section, Table};
use crate::scan::{Agent, Detector, Record, Session};
use crate::shell::{pipe_check, shows_failure, shows_pass};

#[derive(Default, Debug, Clone, Copy)]
struct Counts {
    test_runs: u64,
    piped: u64,
    remote: u64,
    guarded: u64,
    unguarded: u64,
    results: u64,
    reported_success: u64,
    failure_shown_success_reported: u64,
    no_pass_or_fail_shown: u64,
}

impl Counts {
    fn fields(&self) -> [(&'static str, u64); 9] {
        [
            ("test_runs", self.test_runs),
            ("piped_into_tail_or_head", self.piped),
            ("over_ssh_not_judged", self.remote),
            ("guarded_with_pipefail_or_pipestatus", self.guarded),
            ("unguarded", self.unguarded),
            ("unguarded_results_found", self.results),
            ("unguarded_reported_success", self.reported_success),
            (
                "showed_a_failure_but_reported_success",
                self.failure_shown_success_reported,
            ),
            ("showed_no_pass_or_fail_line", self.no_pass_or_fail_shown),
        ]
    }
}

/// Counts of test runs piped into `tail` or `head`.
#[derive(Default)]
pub struct Pipes {
    /// Unguarded Claude runs whose result has not been read yet, by `tool_use` id.
    claude: ClaudeCalls<()>,
    /// Codex `exec` calls with an unguarded test run whose output has not been read,
    /// with whether each command is one.
    codex: CodexCalls<bool>,
    n: Counts,
    by_agent: BTreeMap<Agent, Counts>,
}

impl Detector for Pipes {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude, Agent::Codex]
    }

    fn wants(&self, s: &Session, line: &str) -> bool {
        match s.agent {
            Agent::Claude => line.contains("\"tool_use\"") || self.claude.wants(line),
            Agent::Codex => codex_call_line(line) || self.codex.wants(line),
        }
    }

    fn begin(&mut self, _s: &Session) {
        self.claude.clear();
    }

    fn end(&mut self, _s: &Session) {
        self.codex.end();
    }

    fn record(&mut self, s: &Session, r: &Record<'_>) {
        match s.agent {
            Agent::Claude => {
                for b in tool_uses(r) {
                    let id = b.get("id").and_then(Value::as_str);
                    let fresh = self.claude.note(id);
                    if name_of(b) == "Bash"
                        && r.in_window
                        && self.command(s.agent, input_str(b, "command").unwrap_or(""))
                        && fresh
                        && let Some(id) = id
                    {
                        self.claude.wait(id, ());
                    }
                }
                for b in tool_results(r) {
                    let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                    if self.claude.take(id).is_some() {
                        let text = text_of(b.get("content").unwrap_or(&Value::Null));
                        let success = b.get("is_error").and_then(Value::as_bool) != Some(true);
                        self.result(s.agent, &text, success);
                    }
                }
            }
            Agent::Codex => self.codex(s, r),
        }
    }

    fn section(&self, opts: &Opts) -> Section {
        let fields = self.n.fields();
        let mut table = Table::new(
            "By agent",
            std::iter::once("agent")
                .chain(fields.iter().map(|(k, _)| *k))
                .collect(),
        );
        for (agent, c) in &self.by_agent {
            let mut row = vec![json!(agent.name())];
            row.extend(c.fields().iter().map(|(_, v)| json!(v)));
            table.rows.push(row);
        }
        table.truncate(opts.limit);
        Section {
            name: "pipes",
            title: "test runs piped into tail or head (Claude Bash, Codex commands)",
            summary: fields
                .iter()
                .map(|(k, v)| ((*k).to_owned(), json!(v)))
                .chain(std::iter::once((
                    "codex_outputs_not_paired".to_owned(),
                    json!(self.codex.unpaired),
                )))
                .collect(),
            tables: vec![table],
            notes: vec![
                "An unguarded pipe reports tail's or head's exit status, not the test runner's."
                    .to_owned(),
                "Fix: set -o pipefail before the pipe, or keep the runner's own exit status."
                    .to_owned(),
                "A Codex result counts only when its exec cell printed one whole exec_command result per command, alone or inside a printed list or object; Codex outputs not paired counts the calls with an unguarded test run whose results could not be paired: a function_call, whose output is not read, or an exec cell that had no call id, whose call id another exec cell in the session had, whose output never came or that did not print one result per command. Codex may run PowerShell, where pipefail does not apply; a failure shown whilst the exit code was 0 is hidden there too."
                    .to_owned(),
            ],
        }
    }
}

impl Pipes {
    /// Adds to the totals and to `agent`'s row.
    fn add(&mut self, agent: Agent, f: impl Fn(&mut Counts)) {
        f(&mut self.n);
        f(self.by_agent.entry(agent).or_default());
    }

    /// Judges one command and says whether it is an unguarded test run.
    fn command(&mut self, agent: Agent, cmd: &str) -> bool {
        let check = pipe_check(cmd);
        if !check.runner {
            return false;
        }
        self.add(agent, |c| c.test_runs += 1);
        if !check.piped {
            return false;
        }
        self.add(agent, |c| c.piped += 1);
        if check.remote {
            self.add(agent, |c| c.remote += 1);
        } else if check.guarded {
            self.add(agent, |c| c.guarded += 1);
        } else {
            self.add(agent, |c| c.unguarded += 1);
            return true;
        }
        false
    }

    /// Reads a Codex call's commands, or the results of its unguarded test runs.
    fn codex(&mut self, s: &Session, r: &Record<'_>) {
        if let Some(call) = codex_call(r) {
            let unguarded: Vec<bool> = if r.in_window {
                call.commands().map(|c| self.command(s.agent, c)).collect()
            } else {
                Vec::new()
            };
            let runs = unguarded.contains(&true).then_some(unguarded);
            if call.exec {
                self.codex.call(&call.call_id, runs, r.in_window);
            } else if let Some(runs) = runs {
                self.codex.not_paired(runs.len(), r.in_window);
            }
        } else if let Some(out) = codex_output(r) {
            for (unguarded, res) in self.codex.pair(&out) {
                if unguarded {
                    self.result(s.agent, &res.output, res.exit_code == 0);
                }
            }
        }
    }

    fn result(&mut self, agent: Agent, text: &str, success: bool) {
        let failure = shows_failure(text);
        let pass = shows_pass(text);
        self.add(agent, |c| {
            c.results += 1;
            if success {
                c.reported_success += 1;
            }
            if failure {
                if success {
                    c.failure_shown_success_reported += 1;
                }
            } else if !pass {
                c.no_pass_or_fail_shown += 1;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::detectors::{claude_tool_result, claude_tool_use, replay};

    #[test]
    fn a_reused_claude_tool_use_id_pairs_with_neither_run() {
        let bash = |cmd: &str| json!({"command": cmd});
        let failed = "test result: FAILED. 0 passed; 1 failed";
        let lines: Vec<(bool, String)> = [
            // The one result may be the ls block's, so the test run's is unknown.
            claude_tool_use("x", "Bash", &bash("cargo test 2>&1 | tail -1")),
            claude_tool_use("x", "Bash", &bash("ls")),
            claude_tool_result("x", failed),
            // A test run under an id that a Read block carried earlier.
            claude_tool_use("y", "Read", &json!({"file_path": "/work/a.md"})),
            claude_tool_use("y", "Bash", &bash("cargo test 2>&1 | tail -1")),
            claude_tool_result("y", failed),
        ]
        .into_iter()
        .map(|l| (true, l))
        .collect();
        let s = Session {
            agent: Agent::Claude,
            subagent: false,
        };
        let mut d = Pipes::default();
        replay(&mut d, &s, &lines);
        assert_eq!((d.n.unguarded, d.n.results), (2, 0));
    }
}
