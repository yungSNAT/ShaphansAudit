//! The detectors and the record helpers they share.

pub mod hooks;
pub mod pipes;
pub mod polls;
pub mod rereads;
pub mod schemas;
pub mod startup;

#[cfg(test)]
mod codex_tests;

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use serde_json::Value;

use crate::cli::Command;
use crate::programs::Programs;
#[cfg(test)]
use crate::scan::Session;
use crate::scan::{Detector, Record};
use crate::shell::{self, Step};

/// The detectors a subcommand runs, in report order.
#[must_use]
pub fn build(cmd: Command, programs: &Rc<Programs>) -> Vec<Box<dyn Detector>> {
    let all = cmd == Command::All;
    let mut v: Vec<Box<dyn Detector>> = Vec::new();
    if all || cmd == Command::Startup {
        v.push(Box::new(startup::Startup::new(Rc::clone(programs))));
    }
    if all || cmd == Command::Schemas {
        v.push(Box::<schemas::Schemas>::default());
    }
    if all || cmd == Command::Pipes {
        v.push(Box::<pipes::Pipes>::default());
    }
    if all || cmd == Command::Polls {
        v.push(Box::new(polls::Polls::new(Rc::clone(programs))));
    }
    if all || cmd == Command::Rereads {
        v.push(Box::<rereads::Rereads>::default());
    }
    if all || cmd == Command::Hooks {
        v.push(Box::new(hooks::Hooks::new(Rc::clone(programs))));
    }
    v
}

fn blocks<'a>(
    r: &'a Record<'_>,
    kind: &str,
    block: &'static str,
) -> impl Iterator<Item = &'a Value> {
    let content = if r.kind() == kind {
        r.value
            .pointer("/message/content")
            .and_then(Value::as_array)
    } else {
        None
    };
    content
        .into_iter()
        .flatten()
        .filter(move |b| b.get("type").and_then(Value::as_str) == Some(block))
}

/// The `tool_use` blocks of a Claude `assistant` record.
pub fn tool_uses<'a>(r: &'a Record<'_>) -> impl Iterator<Item = &'a Value> {
    blocks(r, "assistant", "tool_use")
}

/// The `tool_result` blocks of a Claude `user` record.
pub fn tool_results<'a>(r: &'a Record<'_>) -> impl Iterator<Item = &'a Value> {
    blocks(r, "user", "tool_result")
}

/// A block's `name`, or "".
#[must_use]
pub fn name_of(b: &Value) -> &str {
    b.get("name").and_then(Value::as_str).unwrap_or("")
}

/// A string field of a `tool_use` block's input.
#[must_use]
pub fn input_str<'a>(b: &'a Value, key: &str) -> Option<&'a str> {
    b.get("input")
        .and_then(|i| i.get(key))
        .and_then(Value::as_str)
}

/// The text of a message or tool result: a string, or the `text` of its blocks.
#[must_use]
pub fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Characters in a string.
#[must_use]
pub fn chars(s: &str) -> u64 {
    s.chars().count() as u64
}

/// About how many tokens `chars` characters make, for the `*_tokens_est` columns.
///
/// shortcut: 4 characters a token, a rough average for English text and code; the
/// model's tokenizer would give the true count, but it is not published.
#[must_use]
pub fn est_tokens(chars: u64) -> u64 {
    chars.div_ceil(4)
}

/// The note of each section with `*_tokens_est` columns.
pub const EST_NOTE: &str = "Token estimates (tokens est) divide each row's characters by 4, rounded up, so rows need not add up to a total; the model's own count differs.";

/// The size of a tool result: the characters of text in its content, which is a
/// string or a list of blocks. A block without text, such as an image, adds nothing.
#[must_use]
pub fn result_chars(content: &Value) -> u64 {
    match content {
        Value::String(s) => chars(s),
        Value::Array(items) => items
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .map(chars)
            .sum(),
        _ => 0,
    }
}

/// Characters in a value's compact JSON, or in the string itself.
#[must_use]
pub fn json_chars(v: &Value) -> u64 {
    match v {
        Value::String(s) => chars(s),
        other => chars(&other.to_string()),
    }
}

/// What one Codex tool call runs and edits, in order, and what it reads.
#[derive(Debug, Default)]
pub struct CodexCall {
    /// The id its output will carry, or "" when it has none.
    pub call_id: String,
    /// True for an `exec` cell, whose output `codex_output` reads.
    pub exec: bool,
    pub steps: Vec<Step>,
    pub reads: Vec<String>,
}

impl CodexCall {
    /// The shell commands among the steps.
    pub fn commands(&self) -> impl Iterator<Item = &str> {
        self.steps.iter().filter_map(|s| match s {
            Step::Command(c) => Some(c.as_str()),
            Step::Edit => None,
        })
    }
}

/// True for lines that may hold a Codex tool call.
#[must_use]
pub fn codex_call_line(line: &str) -> bool {
    line.contains("\"custom_tool_call\"") || line.contains("\"function_call\"")
}

/// Reads a Codex `exec` cell or a `function_call` with a `cmd` argument.
#[must_use]
pub fn codex_call(r: &Record<'_>) -> Option<CodexCall> {
    if r.kind() != "response_item" {
        return None;
    }
    let p = r.value.get("payload")?;
    let call_id = p
        .get("call_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    match p.get("type")?.as_str()? {
        "custom_tool_call" if name_of(p) == "exec" => {
            let input = p.get("input")?.as_str()?;
            Some(CodexCall {
                call_id,
                exec: true,
                steps: shell::codex_steps(input),
                reads: shell::codex_read_calls(input),
            })
        }
        "function_call" if name_of(p) == "apply_patch" => Some(CodexCall {
            call_id,
            steps: vec![Step::Edit],
            ..CodexCall::default()
        }),
        "function_call" => {
            let args: Value = serde_json::from_str(p.get("arguments")?.as_str()?).ok()?;
            let cmd = function_command(args.get("cmd").or_else(|| args.get("command"))?)?;
            Some(CodexCall {
                call_id,
                steps: vec![Step::Command(cmd)],
                ..CodexCall::default()
            })
        }
        _ => None,
    }
}

/// What a Codex `exec` cell printed.
#[derive(Debug, Default)]
pub struct CodexOutput {
    pub call_id: String,
    /// Characters of text in the output.
    pub chars: u64,
    /// The `exec_command` results it printed, in order.
    pub results: Vec<CommandResult>,
}

/// One `exec_command` result as an `exec` cell prints it.
#[derive(Debug)]
pub struct CommandResult {
    pub exit_code: i64,
    pub output: String,
}

/// Reads a Codex `exec` cell's output: a list of text items or one string. In the
/// texts that are JSON, the objects with an `exit_code` and an `output` are
/// `exec_command` results, whether a text is one, as `text(await
/// tools.exec_command(...))` prints it, or holds them in lists and objects, as a
/// printed `Promise.allSettled` does; a null `output` is empty. Results keep the
/// order printed. An object holding results under more than one key gives the
/// output no results, as the parsed object does not keep its keys' order, and so do
/// entries numbered by an `i` that does not rise in the order printed and a result
/// still running, after which a result may be another session's. Any other `output`
/// holds no text.
///
/// shortcut: results are found only where the cell prints a result object whole; a
/// cell that prints parts of one, such as its `output` alone, or none, gives no
/// results, and one that prints each result as it finishes can print them out of
/// order. Pairing each command with the `CommandExecution` item Codex records as it
/// ends would be the upgrade (DESIGN.md, Codex results).
#[must_use]
pub fn codex_output(r: &Record<'_>) -> Option<CodexOutput> {
    if r.kind() != "response_item" {
        return None;
    }
    let p = r.value.get("payload")?;
    if p.get("type")?.as_str()? != "custom_tool_call_output" {
        return None;
    }
    let texts: Vec<&str> = match p.get("output") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    };
    let mut results = Vec::new();
    let mut last_i = None;
    let ordered = texts.iter().all(|t| {
        serde_json::from_str::<Value>(t)
            .map_or(true, |v| collect_results(&v, &mut results, &mut last_i))
    });
    if !ordered {
        results.clear();
    }
    Some(CodexOutput {
        call_id: p.get("call_id")?.as_str()?.to_owned(),
        chars: texts.iter().map(|t| chars(t)).sum(),
        results,
    })
}

/// Adds the results in `v` to `out` in order, and gives false when their order is
/// unknown: an object holds results under more than one key, an entry numbered `i`
/// holds results but is not numbered above the one before it (`last_i`), or a result
/// is still running, after which a result may be another session's.
fn collect_results(v: &Value, out: &mut Vec<CommandResult>, last_i: &mut Option<u64>) -> bool {
    let before = out.len();
    let ordered = if let Some(r) = command_result(v) {
        out.push(r);
        true
    } else if v.get("session_id").is_some() && v.get("output").is_some() {
        false
    } else {
        match v {
            Value::Array(items) => items.iter().all(|x| collect_results(x, out, last_i)),
            Value::Object(map) => {
                let mut keys_with_results = 0;
                map.values().all(|x| {
                    let len = out.len();
                    let ordered = collect_results(x, out, last_i);
                    keys_with_results += usize::from(out.len() > len);
                    ordered
                }) && keys_with_results <= 1
            }
            _ => true,
        }
    };
    match v.get("i").and_then(Value::as_u64) {
        Some(i) if ordered && out.len() > before => {
            let follows = last_i.is_none_or(|last| i > last);
            *last_i = Some(i);
            follows
        }
        _ => ordered,
    }
}

fn command_result(v: &Value) -> Option<CommandResult> {
    let output = match v.get("output")? {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        _ => return None,
    };
    Some(CommandResult {
        exit_code: v.get("exit_code")?.as_i64()?,
        output,
    })
}

/// Codex `exec` calls whose output a detector awaits, each with what the detector
/// keeps for each of its commands, and how many in the window, and how many of their
/// commands, could not be paired with their results: calls without an id, calls
/// whose id another `exec` call in the session also carries, calls whose output
/// never came, and outputs that did not print one result per command.
pub struct CodexCalls<T> {
    /// The ids of the session's `exec` calls so far.
    seen: HashSet<String>,
    /// By call id: what the detector keeps for each command, and whether the call was
    /// in the window.
    waiting: HashMap<String, (Vec<T>, bool)>,
    pub unpaired: u64,
    /// The commands of the `unpaired` calls.
    pub unpaired_commands: u64,
}

impl<T> Default for CodexCalls<T> {
    fn default() -> Self {
        Self {
            seen: HashSet::new(),
            waiting: HashMap::new(),
            unpaired: 0,
            unpaired_commands: 0,
        }
    }
}

impl<T> CodexCalls<T> {
    /// Notes an `exec` call, which every detector reading outputs must do for every
    /// call so that a reused id is seen. `runs` holds what the detector keeps for
    /// each command, by position, or is `None` when it awaits none of the output. An
    /// id carried by more than one call pairs with neither from then on, as their
    /// outputs cannot be told apart.
    pub fn call(&mut self, call_id: &str, runs: Option<Vec<T>>, in_window: bool) {
        let fresh = !call_id.is_empty() && self.seen.insert(call_id.to_owned());
        if !fresh && let Some((earlier, w)) = self.waiting.remove(call_id) {
            self.not_paired(earlier.len(), w);
        }
        match runs {
            Some(runs) if fresh => {
                self.waiting.insert(call_id.to_owned(), (runs, in_window));
            }
            Some(runs) => self.not_paired(runs.len(), in_window),
            None => {}
        }
    }

    /// Counts a call of `commands` commands that will not be paired, if in the window,
    /// such as a `function_call`, whose output is not read.
    pub fn not_paired(&mut self, commands: usize, in_window: bool) {
        if in_window {
            self.unpaired += 1;
            self.unpaired_commands += commands as u64;
        }
    }

    /// True for lines that may hold an awaited call's output.
    #[must_use]
    pub fn wants(&self, line: &str) -> bool {
        line.contains("\"custom_tool_call_output\"")
            && self.waiting.keys().any(|id| line.contains(id.as_str()))
    }

    /// Whether the output's call was awaited; it is awaited no longer.
    pub fn take(&mut self, out: &CodexOutput) -> bool {
        self.waiting.remove(&out.call_id).is_some()
    }

    /// For an awaited call's output: what the detector kept for each command, with
    /// that command's result, when the cell printed one result per command. Otherwise
    /// none, as which result belongs to which command is unknown.
    pub fn pair<'o>(&mut self, out: &'o CodexOutput) -> Vec<(T, &'o CommandResult)> {
        let Some((runs, in_window)) = self.waiting.remove(&out.call_id) else {
            return Vec::new();
        };
        if runs.len() != out.results.len() {
            self.not_paired(runs.len(), in_window);
            return Vec::new();
        }
        runs.into_iter().zip(&out.results).collect()
    }

    /// Ends a session: the calls still awaiting output never get it.
    pub fn end(&mut self) {
        for (_, (runs, in_window)) in std::mem::take(&mut self.waiting) {
            self.not_paired(runs.len(), in_window);
        }
        self.seen.clear();
    }
}

/// Claude `tool_use` blocks whose result a detector awaits, by id, and the ids of the
/// session's blocks so far.
pub struct ClaudeCalls<T> {
    seen: HashSet<String>,
    awaited: HashMap<String, T>,
}

impl<T> Default for ClaudeCalls<T> {
    fn default() -> Self {
        Self {
            seen: HashSet::new(),
            awaited: HashMap::new(),
        }
    }
}

impl<T> ClaudeCalls<T> {
    /// Notes a `tool_use` block, which a detector reading results must do for every
    /// block so that a reused id is seen, and says whether its result can be
    /// awaited: it has a non-empty id that no earlier block in the session carried. An id
    /// carried by more than one block pairs with neither from then on, as their
    /// results cannot be told apart.
    pub fn note(&mut self, id: Option<&str>) -> bool {
        let id = id.filter(|id| !id.is_empty());
        let fresh = id.is_some_and(|id| self.seen.insert(id.to_owned()));
        if !fresh && let Some(id) = id {
            self.awaited.remove(id);
        }
        fresh
    }

    /// Awaits the result of a block that `note` said can be awaited.
    pub fn wait(&mut self, id: &str, run: T) {
        self.awaited.insert(id.to_owned(), run);
    }

    /// True for lines that may hold an awaited block's result.
    #[must_use]
    pub fn wants(&self, line: &str) -> bool {
        line.contains("\"tool_result\"") && self.awaited.keys().any(|id| line.contains(id.as_str()))
    }

    /// What the detector kept for the block whose result has id `id`, if awaited; it
    /// is awaited no longer.
    pub fn take(&mut self, id: &str) -> Option<T> {
        self.awaited.remove(id)
    }

    /// Starts a session.
    pub fn clear(&mut self) {
        self.seen.clear();
        self.awaited.clear();
    }
}

/// Feeds one session to `d` as the scan does, parsing only the lines it wants; each
/// line comes with whether it is in the window.
#[cfg(test)]
fn replay(d: &mut dyn Detector, s: &Session, lines: &[(bool, String)]) {
    d.begin(s);
    for (in_window, line) in lines {
        if d.wants(s, line) {
            let value: Value = serde_json::from_str(line).expect("test record");
            d.record(
                s,
                &Record {
                    value: &value,
                    in_window: *in_window,
                },
            );
        }
    }
    d.end(s);
}

/// A Claude `tool_use` block of tool `name` with `input`, under id `id`.
#[cfg(test)]
fn claude_tool_use(id: &str, name: &str, input: &Value) -> String {
    serde_json::json!({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": id, "name": name, "input": input}
    ]}})
    .to_string()
}

/// A Claude `tool_result` block for id `id` holding `text`.
#[cfg(test)]
fn claude_tool_result(id: &str, text: &str) -> String {
    serde_json::json!({"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": id, "content": text}
    ]}})
    .to_string()
}

/// A `cmd` argument as one command line; `[shell, flag, script]` gives the script.
fn function_command(cmd: &Value) -> Option<String> {
    match cmd {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => {
            let parts: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
            let shells = ["bash", "sh", "zsh", "pwsh", "powershell", "cmd"];
            // `bash -lc <script>`: the script is the element after the flag.
            let script = match parts.as_slice() {
                [first, rest @ ..] if shells.contains(&shell::program(first).as_str()) => rest
                    .iter()
                    .position(|w| {
                        matches!(w.to_lowercase().as_str(), "-c" | "-lc" | "-command" | "/c")
                    })
                    .and_then(|i| rest.get(i + 1)),
                _ => None,
            };
            Some(script.map_or_else(|| parts.join(" "), |s| (*s).to_owned()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_empty_or_missing_claude_id_is_never_awaited() {
        let mut calls = ClaudeCalls::<()>::default();
        assert!(!calls.note(None));
        assert!(!calls.note(Some("")));
        assert!(calls.note(Some("toolu_a")));
        assert!(!calls.note(Some("toolu_a")));
    }

    #[test]
    fn function_command_takes_the_script_after_the_shell_flag() {
        let cmd = |v: Value| function_command(&v);
        assert_eq!(
            cmd(json!(["bash", "-lc", "cargo test"])).as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            cmd(json!([
                r"C:\x\pwsh.exe",
                "-NoProfile",
                "-Command",
                "git status"
            ]))
            .as_deref(),
            Some("git status")
        );
        assert_eq!(
            cmd(json!(["bash", "run.sh", "arg"])).as_deref(),
            Some("bash run.sh arg")
        );
        assert_eq!(cmd(json!(["git", "status"])).as_deref(), Some("git status"));
    }
}
