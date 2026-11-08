//! Test runs whose exit status a pipe into `tail` or `head` hides (Claude Bash).

use std::collections::HashSet;

use serde_json::{Value, json};

use super::{input_str, name_of, text_of, tool_results, tool_uses};
use crate::report::{Opts, Section};
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

/// Counts of test runs piped into `tail` or `head`.
#[derive(Default)]
pub struct Pipes {
    pending: HashSet<String>,
    n: Counts,
}

impl Detector for Pipes {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude]
    }

    fn wants(&self, _s: &Session, line: &str) -> bool {
        line.contains("\"Bash\"") || (!self.pending.is_empty() && line.contains("\"tool_result\""))
    }

    fn begin(&mut self, _s: &Session) {
        self.pending.clear();
    }

    fn record(&mut self, _s: &Session, r: &Record<'_>) {
        for b in tool_uses(r).filter(|b| name_of(b) == "Bash") {
            if r.in_window {
                self.command(b);
            }
        }
        for b in tool_results(r) {
            let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
            if self.pending.remove(id) {
                self.result(b);
            }
        }
    }

    fn section(&self, _opts: &Opts) -> Section {
        let n = self.n;
        Section {
            name: "pipes",
            title: "test runs piped into tail or head (Claude Bash)",
            summary: vec![
                ("test_runs".to_owned(), json!(n.test_runs)),
                ("piped_into_tail_or_head".to_owned(), json!(n.piped)),
                ("over_ssh_not_judged".to_owned(), json!(n.remote)),
                (
                    "guarded_with_pipefail_or_pipestatus".to_owned(),
                    json!(n.guarded),
                ),
                ("unguarded".to_owned(), json!(n.unguarded)),
                ("unguarded_results_found".to_owned(), json!(n.results)),
                (
                    "unguarded_reported_success".to_owned(),
                    json!(n.reported_success),
                ),
                (
                    "showed_a_failure_but_reported_success".to_owned(),
                    json!(n.failure_shown_success_reported),
                ),
                (
                    "showed_no_pass_or_fail_line".to_owned(),
                    json!(n.no_pass_or_fail_shown),
                ),
            ],
            tables: Vec::new(),
            notes: vec![
                "An unguarded pipe reports tail's or head's exit status, not the test runner's."
                    .to_owned(),
                "Fix: set -o pipefail before the pipe, or keep the runner's own exit status."
                    .to_owned(),
            ],
        }
    }
}

impl Pipes {
    fn command(&mut self, b: &Value) {
        let check = pipe_check(input_str(b, "command").unwrap_or(""));
        if !check.runner {
            return;
        }
        self.n.test_runs += 1;
        if !check.piped {
            return;
        }
        self.n.piped += 1;
        if check.remote {
            self.n.remote += 1;
        } else if check.guarded {
            self.n.guarded += 1;
        } else {
            self.n.unguarded += 1;
            if let Some(id) = b.get("id").and_then(Value::as_str) {
                self.pending.insert(id.to_owned());
            }
        }
    }

    fn result(&mut self, b: &Value) {
        let text = text_of(b.get("content").unwrap_or(&Value::Null));
        let success = b.get("is_error").and_then(Value::as_bool) != Some(true);
        self.n.results += 1;
        if success {
            self.n.reported_success += 1;
        }
        if shows_failure(&text) {
            if success {
                self.n.failure_shown_success_reported += 1;
            }
        } else if !shows_pass(&text) {
            self.n.no_pass_or_fail_shown += 1;
        }
    }
}
