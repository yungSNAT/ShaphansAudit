//! Test runs whose exit status a pipe into `tail` or `head` hides (Claude Bash and
//! Codex commands).

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

use super::{
    codex_call, codex_call_line, codex_key, codex_output, codex_output_line, input_str, name_of,
    paired, text_of, tool_results, tool_uses,
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
    /// Unguarded runs by Claude `tool_use` id or by `codex_key`.
    pending: HashSet<String>,
    /// Codex calls whose output has not been read yet, with how many commands each ran.
    codex_calls: HashMap<String, usize>,
    n: Counts,
    by_agent: BTreeMap<Agent, Counts>,
}

impl Detector for Pipes {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude, Agent::Codex]
    }

    fn wants(&self, s: &Session, line: &str) -> bool {
        match s.agent {
            Agent::Claude => {
                line.contains("\"Bash\"")
                    || (!self.pending.is_empty() && line.contains("\"tool_result\""))
            }
            Agent::Codex => {
                codex_call_line(line)
                    || (codex_output_line(line)
                        && self.codex_calls.keys().any(|id| line.contains(id.as_str())))
            }
        }
    }

    fn begin(&mut self, _s: &Session) {
        self.pending.clear();
        self.codex_calls.clear();
    }

    fn record(&mut self, s: &Session, r: &Record<'_>) {
        match s.agent {
            Agent::Claude => {
                for b in tool_uses(r).filter(|b| name_of(b) == "Bash") {
                    if r.in_window {
                        let id = b.get("id").and_then(Value::as_str).unwrap_or("");
                        self.command(s.agent, input_str(b, "command").unwrap_or(""), id);
                    }
                }
                for b in tool_results(r) {
                    let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                    if self.pending.remove(id) {
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
            summary: fields.iter().map(|(k, v)| ((*k).to_owned(), json!(v))).collect(),
            tables: vec![table],
            notes: vec![
                "An unguarded pipe reports tail's or head's exit status, not the test runner's."
                    .to_owned(),
                "Fix: set -o pipefail before the pipe, or keep the runner's own exit status."
                    .to_owned(),
                "A Codex result counts only when its exec cell printed one exec_command result per command. Codex may run PowerShell, where pipefail does not apply; a failure shown whilst the exit code was 0 is hidden there too."
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

    /// Judges one command; an unguarded test run awaits its result under `id`.
    fn command(&mut self, agent: Agent, cmd: &str, id: &str) {
        let check = pipe_check(cmd);
        if !check.runner {
            return;
        }
        self.add(agent, |c| c.test_runs += 1);
        if !check.piped {
            return;
        }
        self.add(agent, |c| c.piped += 1);
        if check.remote {
            self.add(agent, |c| c.remote += 1);
        } else if check.guarded {
            self.add(agent, |c| c.guarded += 1);
        } else {
            self.add(agent, |c| c.unguarded += 1);
            if !id.is_empty() {
                self.pending.insert(id.to_owned());
            }
        }
    }

    /// Reads a Codex call's commands, or the results of its unguarded test runs.
    fn codex(&mut self, s: &Session, r: &Record<'_>) {
        if let Some(call) = codex_call(r) {
            let commands: Vec<&str> = call.commands().collect();
            if r.in_window && !call.call_id.is_empty() {
                for (index, cmd) in commands.iter().enumerate() {
                    self.command(s.agent, cmd, &codex_key(&call.call_id, index));
                }
                self.codex_calls
                    .insert(call.call_id.clone(), commands.len());
            }
        } else if let Some(out) = codex_output(r)
            && let Some(commands) = self.codex_calls.remove(&out.call_id)
        {
            let results = paired(&out, commands);
            for index in 0..commands {
                let key = codex_key(&out.call_id, index);
                if self.pending.remove(&key)
                    && let Some(res) = results.get(index)
                {
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
