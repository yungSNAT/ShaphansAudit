//! The detectors and the record helpers they share.

pub mod hooks;
pub mod pipes;
pub mod polls;
pub mod rereads;
pub mod schemas;
pub mod startup;

use std::rc::Rc;

use serde_json::Value;

use crate::cli::Command;
use crate::programs::Programs;
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

/// True for lines that may hold a Codex `exec` cell's output.
#[must_use]
pub fn codex_output_line(line: &str) -> bool {
    line.contains("\"custom_tool_call_output\"")
}

/// Reads a Codex `exec` cell's output: a list of text items, of which those that are
/// JSON objects with an `exit_code` and an `output` are `exec_command` results.
///
/// shortcut: results are found only where the cell prints a result object whole, as
/// `text(await tools.exec_command(...))` does; a cell that prints parts of one, or
/// none, gives no results. Reading the cell's code to follow each value would be the
/// upgrade.
#[must_use]
pub fn codex_output(r: &Record<'_>) -> Option<CodexOutput> {
    if r.kind() != "response_item" {
        return None;
    }
    let p = r.value.get("payload")?;
    if p.get("type")?.as_str()? != "custom_tool_call_output" {
        return None;
    }
    let texts: Vec<&str> = p
        .get("output")?
        .as_array()?
        .iter()
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect();
    Some(CodexOutput {
        call_id: p.get("call_id")?.as_str()?.to_owned(),
        chars: texts.iter().map(|t| chars(t)).sum(),
        results: texts.iter().filter_map(|t| command_result(t)).collect(),
    })
}

fn command_result(text: &str) -> Option<CommandResult> {
    let v: Value = serde_json::from_str(text).ok()?;
    Some(CommandResult {
        exit_code: v.get("exit_code")?.as_i64()?,
        output: v.get("output")?.as_str()?.to_owned(),
    })
}

/// The results of a call's commands, by position, when the cell printed one result
/// per command; otherwise none, as which result belongs to which command is unknown.
#[must_use]
pub fn paired(out: &CodexOutput, commands: usize) -> &[CommandResult] {
    if out.results.len() == commands {
        &out.results
    } else {
        &[]
    }
}

/// The key a Codex command's result is awaited under: its call and position.
#[must_use]
pub fn codex_key(call_id: &str, index: usize) -> String {
    format!("{call_id}#{index}")
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
