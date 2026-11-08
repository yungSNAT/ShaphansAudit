//! Reading commands without running them: words, verb classes, file reads,
//! test pipes and hook labels.

use std::borrow::Cow;
use std::iter::Peekable;
use std::str::Chars;
use std::sync::LazyLock;

use regex::Regex;

/// A test runner at the start of a command, optionally behind a package runner.
static RUNNER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:(?:npx|bunx|uv run|poetry run|python3? -m|py -m) )?(?:cargo (?:nextest run|test)|pytest|vitest|jest|node --test|npm (?:run )?test|go test|dotnet test)\b",
    )
    .expect("valid regex")
});
// shortcut: failure and pass markers cover cargo, pytest, TAP and node output; other
// runners' wording shows as "no pass or fail line". Extend the patterns per runner.
static FAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"test result: FAILED|\bFAILED\b|\b[1-9]\d* failed|# fail [1-9]|\bnot ok \d|panicked at|error: test failed|error: could not compile",
    )
    .expect("valid regex")
});
static PASS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)test result: ok|passed|# pass|\bok\b").expect("valid regex"));
// shortcut: Codex cells are read with regexes, not a JavaScript parser. Any `cmd:` key
// with a string literal counts as a command, a `cmd` built from variables is missed,
// and calls inside strings and comments count. Strings must count: Codex passes code
// that runs to REPL tools as a string. Patch bodies are skipped (`PATCH_BODY`).
static CMD_LITERAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)\bcmd["']?\s*:\s*("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`)"#)
        .expect("valid regex")
});
static READ_CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?s)\breadFile(?:Sync)?\(\s*("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`)"#,
    )
    .expect("valid regex")
});
/// The text an `apply_patch` call writes, from the string literal that opens it. A
/// `readFile(` line it adds is not a read.
static PATCH_BODY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)(["'`])\*\*\* Begin Patch.*?\*\*\* End Patch"#).expect("valid regex")
});

/// One simple command, and whether a `|` sends its output to the next one.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Segment {
    pub words: Vec<String>,
    pub piped: bool,
}

/// Splits a command at unquoted `|`, `|&`, `||`, `;`, `&`, `&&` and newlines, and
/// each part into words with quotes removed. `>&` and `<&` stay inside a word;
/// `#` comments, heredoc bodies and PowerShell here-string bodies are skipped.
///
/// shortcut: backslash escapes, `$(...)`, backticks and multi-line arrays are not
/// parsed, so a value can split into words or segments and land where a program
/// name is expected. Verb classes and hook labels print only names of programs
/// that exist (see `programs`), so such a value prints as `(other)`; counts can
/// still be off. A real shell parser would be the upgrade.
#[must_use]
pub fn split(cmd: &str) -> Vec<Segment> {
    let mut t = Splitter::default();
    let mut chars = cmd.chars().peekable();
    let mut quote: Option<char> = None;
    let mut prev = '\0';
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => t.word.push(c),
            (None, '\'' | '"') => quote = Some(c),
            (None, '#') if t.word.is_empty() => while chars.next_if(|c| *c != '\n').is_some() {},
            (None, '@') if t.word.is_empty() && opens_here_string(&chars) => {
                let quote = chars.next().unwrap_or('\'');
                skip_here_string(&mut chars, quote);
            }
            (None, '&') if prev == '>' || prev == '<' => t.word.push(c),
            (None, '|') => {
                let pipe = chars.next_if_eq(&'|').is_none();
                if pipe {
                    chars.next_if_eq(&'&');
                }
                t.end_segment(pipe);
            }
            (None, ';' | '&') => t.end_segment(false),
            (None, '\n') => {
                t.end_segment(false);
                for delimiter in std::mem::take(&mut t.heredocs) {
                    skip_heredoc(&mut chars, &delimiter);
                }
            }
            (None, c) if c.is_whitespace() => t.end_word(),
            (None, c) => t.word.push(c),
        }
        prev = c;
    }
    t.end_segment(false);
    t.segments
}

/// The words of each segment of a command.
#[must_use]
pub fn segments(cmd: &str) -> Vec<Vec<String>> {
    split(cmd).into_iter().map(|s| s.words).collect()
}

#[derive(Default)]
struct Splitter {
    segments: Vec<Segment>,
    words: Vec<String>,
    word: String,
    /// Delimiters of heredocs opened on the current line.
    heredocs: Vec<String>,
    delimiter_next: bool,
}

impl Splitter {
    fn end_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        let w = std::mem::take(&mut self.word);
        if std::mem::take(&mut self.delimiter_next) {
            self.heredocs.push(w.clone());
        } else if let Some(rest) = w
            .find("<<")
            .map(|i| &w[i + 2..])
            .filter(|r| !r.starts_with('<'))
        {
            let rest = rest.strip_prefix('-').unwrap_or(rest);
            if rest.is_empty() {
                self.delimiter_next = true;
            } else if rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c))
            {
                // Not `$((1<<2))`, whose "delimiter" would skip the rest of the command.
                self.heredocs.push(rest.to_owned());
            }
        }
        self.words.push(w);
    }

    fn end_segment(&mut self, piped: bool) {
        self.end_word();
        if !self.words.is_empty() {
            let words = std::mem::take(&mut self.words);
            self.segments.push(Segment { words, piped });
        }
    }
}

/// Skips a heredoc body up to and including its delimiter line.
fn skip_heredoc(chars: &mut Peekable<Chars<'_>>, delimiter: &str) {
    let mut line = String::new();
    for c in chars.by_ref() {
        if c != '\n' {
            line.push(c);
        } else if line.trim() == delimiter {
            return;
        } else {
            line.clear();
        }
    }
}

/// Whether `@` opens a PowerShell here-string: a quote, then the end of the line.
fn opens_here_string(chars: &Peekable<Chars<'_>>) -> bool {
    let mut ahead = chars.clone();
    matches!(ahead.next(), Some('\'' | '"')) && matches!(ahead.next(), Some('\n' | '\r'))
}

/// Skips a here-string body up to its closing quote and `@` at the start of a line.
fn skip_here_string(chars: &mut Peekable<Chars<'_>>, quote: char) {
    let mut line_start = false;
    while let Some(c) = chars.next() {
        if line_start && c == quote && chars.next_if_eq(&'@').is_some() {
            return;
        }
        line_start = c == '\n';
    }
}

/// A shell assignment such as `VAR=value`.
fn is_assignment(w: &str) -> bool {
    w.find('=').is_some_and(|i| {
        i > 0
            && w[..i]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// The last path component, keeping its extension.
fn file_name(w: &str) -> &str {
    w.rsplit(['/', '\\']).next().unwrap_or(w)
}

/// A program name: last path component, lower-cased, without `.exe`.
pub(crate) fn program(w: &str) -> String {
    let b = file_name(w).to_lowercase();
    b.strip_suffix(".exe").map_or(b.clone(), str::to_owned)
}

/// Whether a segment is a PowerShell assignment such as `$x = 1` or
/// `$env:KEY='v'`. Its value is never read as a command, so it cannot reach the
/// output.
fn is_ps_assignment(words: &[String]) -> bool {
    words.first().is_some_and(|w| w.starts_with('$'))
        && (words[0].contains('=')
            || words
                .get(1)
                .is_some_and(|n| n.starts_with('=') || (n.len() <= 3 && n.ends_with('='))))
}

/// The words after leading `VAR=value` assignments; none for a PowerShell
/// assignment.
fn after_assignments(words: &[String]) -> Vec<&str> {
    if is_ps_assignment(words) {
        return Vec::new();
    }
    words
        .iter()
        .map(String::as_str)
        .skip_while(|w| is_assignment(w))
        .collect()
}

/// A segment's words after `VAR=value` assignments and the prefixes `rtk`,
/// `rtk bypass`, `rtk proxy`, `rtk test`, `rtk err`, `sudo` and `time` with
/// their options.
fn command_words(words: &[String]) -> Vec<&str> {
    let words = after_assignments(words);
    let mut i = 0;
    while let Some(w) = words.get(i) {
        let options_with_value: &[&str] = match program(w).as_str() {
            "rtk" => {
                i += 1;
                if words
                    .get(i)
                    .is_some_and(|w| matches!(*w, "bypass" | "proxy" | "test" | "err"))
                {
                    i += 1;
                }
                continue;
            }
            "sudo" => &[
                "-u",
                "-g",
                "-C",
                "-h",
                "-p",
                "-U",
                "-r",
                "-t",
                "-D",
                "-T",
                "--user",
                "--group",
                "--close-from",
                "--host",
                "--prompt",
                "--other-user",
                "--role",
                "--type",
                "--chdir",
                "--command-timeout",
            ],
            "time" => &["-f", "-o", "--format", "--output"],
            _ => break,
        };
        i += 1;
        while let Some(o) = words.get(i).filter(|o| o.starts_with('-')) {
            i += if options_with_value.contains(o) { 2 } else { 1 };
        }
    }
    words[i.min(words.len())..].to_vec()
}

/// The first segment that is not a leading `cd` or only assignments.
fn main_segment(segs: &[Segment]) -> Option<&Segment> {
    let mut i = 0;
    while i + 1 < segs.len()
        && after_assignments(&segs[i].words)
            .first()
            .is_none_or(|w| program(w) == "cd")
    {
        i += 1;
    }
    segs.get(i)
}

/// Whether a word, as written, names a program on this machine.
pub type Known<'a> = dyn FnMut(&str) -> bool + 'a;

/// A command's verb class, such as `cargo`, `get-content` or `ssh agent-heavy`.
/// The program word must be one that `known` accepts, otherwise the class is
/// `(other)`: parsing is heuristic, and a value can land in a program's place.
#[must_use]
pub fn verb_class(cmd: &str, known: &mut Known<'_>) -> String {
    let segs = split(cmd);
    match main_segment(&segs) {
        None => "(empty)".to_owned(),
        Some(seg) if after_assignments(&seg.words).is_empty() => "(assignment)".to_owned(),
        Some(seg) => class_of(&command_words(&seg.words), known),
    }
}

fn class_of(words: &[&str], known: &mut Known<'_>) -> String {
    let Some(first) = words.first() else {
        return "(empty)".to_owned();
    };
    let v = program(first);
    if first.contains("://") || !is_program_name(&v) || !known(first) {
        return "(other)".to_owned();
    }
    if v != "ssh" {
        return v;
    }
    // The host is not printed: a misparse can put a value in its place.
    match ssh_remote(&words[1..]) {
        Some(remote) if !remote.is_empty() => format!("ssh {}", verb_class(&remote, known)),
        _ => "ssh".to_owned(),
    }
}

/// The remote command in `ssh` arguments, after the options and the host.
fn ssh_remote(args: &[&str]) -> Option<String> {
    const WITH_VALUE: &str = "BbcDEeFIiJLlmOoPpQRSWw";
    let mut i = 0;
    while let Some(a) = args.get(i).filter(|a| a.starts_with('-')) {
        i += if a.len() == 2 && WITH_VALUE.contains(&a[1..]) {
            2
        } else {
            1
        };
    }
    args.get(i)?;
    let remote: Vec<&str> = args
        .get(i + 1..)
        .unwrap_or_default()
        .iter()
        .copied()
        .skip_while(|w| w.starts_with('-'))
        .collect();
    Some(remote.join(" "))
}

const READERS: [&str; 9] = [
    "cat",
    "head",
    "tail",
    "sed",
    "less",
    "more",
    "bat",
    "get-content",
    "gc",
];
const PATH_FLAGS: [&str; 3] = ["-path", "-literalpath", "-lp"];

fn value_flags(verb: &str) -> &'static [&'static str] {
    match verb {
        "head" | "tail" => &[
            "-n",
            "-c",
            "-s",
            "--lines",
            "--bytes",
            "--sleep-interval",
            "--pid",
        ],
        "sed" => &["-e", "-f", "--expression", "--file"],
        "bat" => &[
            "-l",
            "--language",
            "-r",
            "--line-range",
            "--highlight-line",
            "--theme",
            "--style",
            "-m",
            "--map-syntax",
        ],
        "get-content" | "gc" => &[
            "-totalcount",
            "-head",
            "-tail",
            "-first",
            "-last",
            "-encoding",
            "-readcount",
            "-delimiter",
            "-stream",
            "-erroraction",
            "-ea",
            "-warningaction",
            "-wa",
        ],
        _ => &[],
    }
}

/// Files a command reads with a reader such as `cat`, `sed -n` or `Get-Content`.
#[must_use]
pub fn read_targets(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    for seg in segments(cmd) {
        // `$text = Get-Content x.md` reads x.md, though the segment is an assignment.
        let words: Vec<&str> = if is_ps_assignment(&seg) && seg.get(1).is_some_and(|w| w == "=") {
            seg[2..].iter().map(String::as_str).collect()
        } else {
            after_assignments(&seg)
        };
        let Some(first) = words.first() else { continue };
        let verb = program(first);
        if !READERS.contains(&verb.as_str()) {
            continue;
        }
        if verb == "sed"
            && words
                .iter()
                .any(|w| w.starts_with("-i") || w.starts_with("--in-place"))
        {
            continue;
        }
        out.extend(reader_args(&verb, &words[1..]));
    }
    out
}

fn reader_args(verb: &str, args: &[&str]) -> Vec<String> {
    let flags = value_flags(verb);
    let script_given = args.iter().any(|w| {
        flags
            .iter()
            .any(|f| w == f || w.starts_with(&format!("{f}=")))
    });
    let mut script_pending = verb == "sed" && !script_given;
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(w) = args.get(i) {
        let lw = w.to_lowercase();
        i += 1;
        if PATH_FLAGS.contains(&lw.as_str()) {
            out.extend(
                args.get(i)
                    .filter(|p| is_literal_path(p))
                    .map(|p| (*p).to_owned()),
            );
            i += 1;
        } else if flags.contains(&lw.as_str()) || w.ends_with('>') || matches!(*w, "<<" | "<<-") {
            i += 1;
        } else if w.starts_with(['-', '+'])
            || w.contains(['>', '<'])
            || w.chars().all(|c| c.is_ascii_digit())
        {
        } else if script_pending {
            script_pending = false;
        } else if is_literal_path(w) {
            out.push((*w).to_owned());
        }
    }
    out
}

/// A program name such as `git`, `get-content` or `python3.12`, as opposed to a
/// variable, expression or quoted text in the first word.
fn is_program_name(v: &str) -> bool {
    v.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c))
}

/// A path written out, not a variable or expression whose value is unknown.
fn is_literal_path(w: &str) -> bool {
    !w.contains('$') && !w.starts_with(['(', '@', '{'])
}

/// A path as compared for re-reads: forward slashes, lower case.
#[must_use]
pub fn normalise_path(p: &str) -> String {
    p.trim_matches(['"', '\'', '`'])
        .replace('\\', "/")
        .to_lowercase()
}

/// How a command runs tests, if it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PipeCheck {
    /// Runs a known test runner.
    pub runner: bool,
    /// Pipes the runner's output straight into `tail` or `head`.
    pub piped: bool,
    /// The piped runner runs over `ssh`, where the remote shell's settings are unknown.
    pub remote: bool,
    /// Sets `pipefail` or reads `PIPESTATUS`.
    pub guarded: bool,
}

/// Judges each segment of a command: a runner whose segment pipes into `tail` or
/// `head`, or an `ssh` segment whose remote command does.
#[must_use]
pub fn pipe_check(cmd: &str) -> PipeCheck {
    let segs = split(cmd);
    let mut check = PipeCheck {
        guarded: segs
            .iter()
            .flat_map(|s| &s.words)
            .any(|w| w.contains("pipefail") || w.contains("PIPESTATUS")),
        ..PipeCheck::default()
    };
    for (i, seg) in segs.iter().enumerate() {
        let words = command_words(&seg.words);
        let into_tail = seg.piped
            && segs.get(i + 1).is_some_and(|next| {
                command_words(&next.words)
                    .first()
                    .is_some_and(|w| matches!(program(w).as_str(), "tail" | "head"))
            });
        let remote = match words.first() {
            Some(w) if program(w) == "ssh" => ssh_remote(&words[1..]).map(|r| pipe_check(&r)),
            _ => None,
        };
        match remote {
            Some(inner) if inner.runner => {
                check.runner = true;
                if into_tail {
                    check.piped = true;
                } else if inner.piped {
                    check.piped = true;
                    check.remote = true;
                }
            }
            Some(_) => {}
            None if runs_tests(&words) => {
                check.runner = true;
                check.piped |= into_tail;
            }
            None => {}
        }
    }
    check
}

/// Programs that run the command after them, as in `agent-heavy -- cargo test`.
const WRAPPERS: [&str; 9] = [
    "agent-heavy",
    "nice",
    "timeout",
    "env",
    "nohup",
    "time",
    "sudo",
    "do",
    "then",
];

/// Whether a simple command runs a test runner, directly or behind wrappers with
/// their options and numeric arguments, such as `timeout 600` or `rtk test`.
fn runs_tests(words: &[&str]) -> bool {
    let mut j = 0;
    while j < words.len() {
        if RUNNER.is_match(&command_line(&words[j..])) {
            return true;
        }
        let w = words[j];
        let p = program(w);
        if p == "rtk"
            && words
                .get(j + 1)
                .is_some_and(|n| matches!(*n, "test" | "err" | "bypass" | "proxy"))
        {
            j += 2;
        } else if WRAPPERS.contains(&p.as_str())
            || w.starts_with('-')
            || is_assignment(w)
            || is_duration(w)
        {
            j += 1;
        } else {
            return false;
        }
    }
    false
}

/// A number with an optional `s`, `m`, `h` or `d` suffix, such as `600` or `10m`.
fn is_duration(w: &str) -> bool {
    let digits = w.trim_end_matches(['s', 'm', 'h', 'd']);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Words joined by spaces, with the first as a bare program name.
fn command_line(words: &[&str]) -> String {
    let mut line = words.first().map(|w| program(w)).unwrap_or_default();
    for w in words.iter().skip(1) {
        line.push(' ');
        line.push_str(w);
    }
    line
}

/// Whether test output shows a failure.
#[must_use]
pub fn shows_failure(text: &str) -> bool {
    FAIL.is_match(text)
}

/// Whether test output shows a pass.
#[must_use]
pub fn shows_pass(text: &str) -> bool {
    PASS.is_match(text)
}

const SCRIPT_EXTENSIONS: [&str; 9] = [
    ".js", ".mjs", ".cjs", ".py", ".exe", ".ps1", ".cmd", ".sh", ".bat",
];

/// Programs whose first script argument is the script they run.
const INTERPRETERS: [&str; 13] = [
    "bash",
    "bun",
    "cmd",
    "deno",
    "node",
    "powershell",
    "pwsh",
    "py",
    "python",
    "python3",
    "sh",
    "uv",
    "zsh",
];

/// Whether `arg` is an option with which `interpreter` runs code given on the
/// command line, so no later argument is the script being run. Short options may be
/// grouped, as in `bash -lc`; only PowerShell's options ignore case.
fn runs_inline_code(interpreter: &str, arg: &str) -> bool {
    let flag = arg.split('=').next().unwrap_or_default();
    let short_group_ending = |letters: &str| {
        flag.len() > 1
            && flag.starts_with('-')
            && !flag.starts_with("--")
            && flag.ends_with(|c| letters.contains(c))
    };
    match interpreter {
        "node" | "bun" | "deno" => ["-e", "-p", "--eval", "--print"].contains(&flag),
        "python" | "python3" | "py" => short_group_ending("cm"),
        "bash" | "sh" | "zsh" => short_group_ending("c"),
        "powershell" | "pwsh" => {
            ["-c", "-command", "-ec", "-encodedcommand"].contains(&flag.to_lowercase().as_str())
        }
        _ => false,
    }
}

/// A hook's label: the file name of its script, or its program when there is no
/// script. The script is the program itself or an interpreter's first script
/// argument before any option that runs inline code (`runs_inline_code`). Either
/// must be a word that `known` accepts, or the label is `(other)`.
/// Assignments and a leading `cd` are skipped, and arguments never print.
///
/// Recent Claude Code versions record `command hook N` (the hook's position under
/// its event and matcher) or its status message instead of the command; the first
/// is kept whole.
#[must_use]
pub fn hook_label(command: &str, known: &mut Known<'_>) -> String {
    if command
        .strip_prefix("command hook ")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    {
        return command.to_owned();
    }
    let segs = split(command);
    let words = main_segment(&segs)
        .map(|s| after_assignments(&s.words))
        .unwrap_or_default();
    // Only plain names reach the output; anything else could be part of a value.
    let plain = |w: &str| {
        !w.is_empty()
            && w.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c))
    };
    let is_script = |w: &str| {
        let n = file_name(w).to_lowercase();
        plain(file_name(w)) && SCRIPT_EXTENSIONS.iter().any(|e| n.ends_with(e))
    };
    let script = match words.first() {
        Some(w) if is_script(w) => Some(w),
        Some(w) if INTERPRETERS.contains(&program(w).as_str()) => {
            let interpreter = program(w);
            words[1..]
                .iter()
                .take_while(|a| !runs_inline_code(&interpreter, a))
                .find(|a| is_script(a))
        }
        _ => None,
    };
    if let Some(script) = script.filter(|s| known(s)) {
        return file_name(script).to_owned();
    }
    match words.first() {
        None => "(empty)".to_owned(),
        Some(w) if plain(file_name(w)) && known(w) => file_name(w).to_owned(),
        Some(_) => "(other)".to_owned(),
    }
}

/// One step of a Codex tool call, in source order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Command(String),
    Edit,
}

const EDIT_CALLS: [&str; 3] = ["apply_patch(", "writeFile(", "writeFileSync("];

/// `input` with each patch body after its opening quote blanked to spaces of the
/// same length, so offsets still give source order. A quote after `//` on its line
/// is in a comment and opens no body.
///
/// shortcut: a `//` straight after `:` is taken for a URL, and `/* */` comments are
/// not recognised; a JavaScript lexer would settle both.
fn without_patch_bodies(input: &str) -> Cow<'_, str> {
    PATCH_BODY.replace_all(input, |c: &regex::Captures<'_>| {
        let whole = c.get(0).expect("the whole match");
        let line_start = input[..whole.start()].rfind('\n').map_or(0, |i| i + 1);
        let line = &input[line_start..whole.start()];
        if line
            .match_indices("//")
            .any(|(i, _)| !line[..i].ends_with(':'))
        {
            whole.as_str().to_owned()
        } else {
            format!("{}{}", &c[1], " ".repeat(whole.len() - c[1].len()))
        }
    })
}

/// The shell commands (each `cmd` string literal, unescaped) and file edits of a
/// Codex `exec` cell, in source order.
#[must_use]
pub fn codex_steps(input: &str) -> Vec<Step> {
    let input = without_patch_bodies(input);
    let input = input.as_ref();
    let mut steps: Vec<(usize, Step)> = CMD_LITERAL
        .captures_iter(input)
        .filter_map(|c| c.get(1))
        .map(|m| (m.start(), Step::Command(js_unescape(m.as_str()))))
        .collect();
    for call in EDIT_CALLS {
        steps.extend(input.match_indices(call).map(|(i, _)| (i, Step::Edit)));
    }
    steps.sort_by_key(|(i, _)| *i);
    steps.into_iter().map(|(_, s)| s).collect()
}

/// Paths a Codex `exec` cell reads with `readFile` or `readFileSync`.
#[must_use]
pub fn codex_read_calls(input: &str) -> Vec<String> {
    READ_CALL
        .captures_iter(&without_patch_bodies(input))
        .map(|c| js_unescape(&c[1]))
        .filter(|p| is_literal_path(p))
        .collect()
}

/// The value of a JavaScript string literal, quotes included in `lit`.
#[must_use]
pub fn js_unescape(lit: &str) -> String {
    let inner = lit.get(1..lit.len().saturating_sub(1)).unwrap_or_default();
    let mut out = String::with_capacity(inner.len());
    let mut it = inner.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('x') => out.extend(hex_char(&mut it, 2)),
            Some('u') if it.peek() == Some(&'{') => {
                it.next();
                let hex: String = it.by_ref().take_while(|c| *c != '}').collect();
                out.extend(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32));
            }
            Some('u') => out.extend(hex_char(&mut it, 4)),
            Some('\n') => {}
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn hex_char(it: &mut impl Iterator<Item = char>, n: usize) -> Option<char> {
    let hex: String = it.take(n).collect();
    u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parsing alone: every word counts as a known program.
    fn vc(cmd: &str) -> String {
        verb_class(cmd, &mut |_| true)
    }

    fn hl(cmd: &str) -> String {
        hook_label(cmd, &mut |_| true)
    }

    #[test]
    fn segments_split_on_operators_but_not_in_quotes_or_redirects() {
        let s = segments("cd /x && cargo test 2>&1 | tail -5; echo 'a | b'");
        assert_eq!(
            s,
            vec![
                vec!["cd", "/x"],
                vec!["cargo", "test", "2>&1"],
                vec!["tail", "-5"],
                vec!["echo", "a | b"]
            ]
        );
    }

    #[test]
    fn split_marks_only_single_pipes() {
        let piped: Vec<bool> = split("a | b || c |& d && e")
            .iter()
            .map(|s| s.piped)
            .collect();
        assert_eq!(piped, vec![true, false, true, false, false]);
    }

    #[test]
    fn heredoc_bodies_are_not_commands() {
        let cmd = "git commit -F - <<'EOF'\nMore notes here\ncargo test | tail\nEOF\ncat a.md";
        assert_eq!(
            segments(cmd),
            vec![
                vec!["git", "commit", "-F", "-", "<<EOF"],
                vec!["cat", "a.md"]
            ]
        );
        assert_eq!(read_targets(cmd), vec!["a.md"]);
        assert!(!pipe_check(cmd).runner);
        assert_eq!(
            segments("cat << -X\nbody\n-X\nls"),
            vec![vec!["cat", "<<", "-X"], vec!["ls"]]
        );
    }

    #[test]
    fn verb_classes() {
        assert_eq!(
            vc("cd /c/x && RUST_LOG=1 rtk bypass cargo test --locked"),
            "cargo"
        );
        assert_eq!(
            vc("ssh -o BatchMode=yes l@ll 'agent-heavy status'"),
            "ssh agent-heavy"
        );
        assert_eq!(vc("ssh ll"), "ssh");
        assert_eq!(vc(r#""C:\Program Files\Git\bin\git.exe" status"#), "git");
        assert_eq!(vc("Get-Content -Raw x.md"), "get-content");
        assert_eq!(vc("sudo -u root agent-heavy status"), "agent-heavy");
        assert_eq!(vc("sudo --user root -- agent-heavy"), "agent-heavy");
        assert_eq!(vc("time --output t.txt cargo build"), "cargo");
        assert_eq!(vc("time -p cargo build"), "cargo");
        assert_eq!(vc("OUT=/tmp/zz; sleep 5"), "sleep");
        assert_eq!(vc("$x=1; & git status"), "git");
        assert_eq!(vc("$files.Count"), "(other)");
        assert_eq!(vc("rtk test cargo test"), "cargo");
        assert_eq!(vc(""), "(empty)");
    }

    #[test]
    fn assignment_values_never_become_classes() {
        for cmd in [
            "$x = 'sentinel'",
            "$stamp = Get-Date",
            "$env:NAME=sentinel",
            "$n += 1",
            "$i = 0",
            "X=1",
        ] {
            assert_eq!(vc(cmd), "(assignment)", "{cmd}");
        }
        assert_eq!(vc("$env:K = 'v'; cargo test"), "cargo");
    }

    #[test]
    fn odd_first_words_and_hosts_are_other() {
        assert_eq!(vc("https://example.test/sentinel"), "(other)");
        assert_eq!(vc("ssh 'me@sentinel host' ls"), "ssh ls");
        assert_eq!(vc("ssh $HOST ls"), "ssh ls");
        assert_eq!(vc("ssh ll -- cargo test"), "ssh cargo");
        assert_eq!(vc(r#"RUST_LOG="a b" cargo test"#), "cargo");
    }

    #[test]
    fn comments_and_here_strings_are_skipped() {
        assert_eq!(
            segments("cargo test # sentinel note\nls"),
            vec![vec!["cargo", "test"], vec!["ls"]]
        );
        for q in ['\'', '"'] {
            let cmd = format!("$body = @{q}\nsentinel | text\n{q}@\ngit status");
            assert_eq!(
                segments(&cmd),
                vec![vec!["$body", "="], vec!["git", "status"]],
                "{cmd}"
            );
        }
        assert_eq!(
            segments("cat<<EOF\nsentinel\nEOF\nls"),
            vec![vec!["cat<<EOF"], vec!["ls"]]
        );
        assert_eq!(
            segments("echo $((1<<2))\nls"),
            vec![vec!["echo", "$((1<<2))"], vec!["ls"]]
        );
    }

    #[test]
    fn read_targets_cover_common_readers() {
        assert_eq!(
            read_targets("cat a.md b.md | head -20"),
            vec!["a.md", "b.md"]
        );
        assert_eq!(
            read_targets("sed -n '1,40p' src/lib.rs"),
            vec!["src/lib.rs"]
        );
        assert_eq!(
            read_targets("head -n 30 notes.txt && tail -5 log.txt"),
            vec!["notes.txt", "log.txt"]
        );
        assert_eq!(
            read_targets(r"Get-Content -Raw -LiteralPath 'C:\hub\x.md' -TotalCount 50"),
            vec![r"C:\hub\x.md"]
        );
        assert_eq!(read_targets("gc -Encoding utf8 y.md"), vec!["y.md"]);
        assert_eq!(
            read_targets("Get-Content z.md -ErrorAction SilentlyContinue"),
            vec!["z.md"]
        );
    }

    #[test]
    fn read_targets_skip_writes_and_searches() {
        assert!(read_targets("sed -i 's/a/b/' x.rs").is_empty());
        assert!(read_targets("cat <<'EOF' > out.txt").is_empty());
        assert!(read_targets("cat > out.txt").is_empty());
        assert!(read_targets("rg -n foo src").is_empty());
        assert!(read_targets("type cargo").is_empty());
        assert!(read_targets("cat $f ${dir}/x.md").is_empty());
        assert!(read_targets("Get-Content -LiteralPath $p").is_empty());
        assert!(read_targets("cat << EOF\nbody\nEOF").is_empty());
        assert!(read_targets("cat << -X\nbody\n-X").is_empty());
        assert!(read_targets("sed --in-place=.bak 's/a/b/' x.rs").is_empty());
    }

    #[test]
    fn read_targets_skip_option_values() {
        assert_eq!(read_targets("bat --theme zqx a.md"), vec!["a.md"]);
        assert_eq!(read_targets("less +F b.md"), vec!["b.md"]);
        assert_eq!(read_targets("tail -s 5 c.md"), vec!["c.md"]);
        assert!(read_targets("cat <<'END-X'\ncat body.md\nEND-X").is_empty());
        assert_eq!(read_targets("sed --expression=1p a.md"), vec!["a.md"]);
        assert_eq!(read_targets("bat -l rust x.rs"), vec!["x.rs"]);
        assert_eq!(read_targets("$text = Get-Content x.md"), vec!["x.md"]);
    }

    #[test]
    fn pipe_checks() {
        let c = pipe_check("cargo test 2>&1 | tail -5");
        assert_eq!(
            c,
            PipeCheck {
                runner: true,
                piped: true,
                remote: false,
                guarded: false
            }
        );
        assert!(pipe_check("set -o pipefail; pytest -q | tail -3").guarded);
        assert!(pipe_check("cd x && npx vitest run | head -20").piped);
        assert!(!pipe_check("git log | head").runner);
        assert!(!pipe_check("cargo test --locked").piped);
        assert!(!pipe_check("cargo test > out.txt; tail out.txt").piped);
        assert!(!pipe_check("cargo test || tail log").piped);
        for wrapped in [
            "agent-heavy --wait 900 -- cargo test --locked 2>&1 | tail -3",
            "nice -n 19 pytest -q | tail",
            "timeout -k 5 10m cargo nextest run | head",
            "env RUST_LOG=1 rtk test cargo test | tail -5",
            "rtk test cargo test | tail",
            "for c in a b; do cargo test -p $c | tail -2; done",
        ] {
            assert!(pipe_check(wrapped).piped, "{wrapped}");
        }
        assert!(!pipe_check("echo cargo test | tail").runner);
        assert!(!pipe_check("agent-heavy status | tail").runner);
    }

    #[test]
    fn pipe_checks_ignore_quoted_text() {
        assert!(!pipe_check(r#"git commit -m "fix cargo test | tail output""#).runner);
        let local = pipe_check("cargo test -p ssh-config 2>&1 | tail -3");
        assert!(local.piped && !local.remote);
        assert!(!pipe_check("cat ~/.ssh/config | head").runner);
    }

    #[test]
    fn pipe_checks_over_ssh() {
        let remote = pipe_check("ssh ll 'cargo test | tail'");
        assert!(remote.runner && remote.piped && remote.remote);
        let local = pipe_check("ssh ll 'cargo test' | tail -3");
        assert!(local.runner && local.piped && !local.remote);
        let unpiped = pipe_check("ssh -p 22 ll cargo test");
        assert!(unpiped.runner && !unpiped.piped);
    }

    #[test]
    fn failure_and_pass_markers() {
        assert!(shows_failure("test result: FAILED. 1 passed; 2 failed"));
        assert!(shows_failure("thread 'x' panicked at src/lib.rs:3"));
        assert!(!shows_failure("0 failed"));
        assert!(!shows_failure("test result: ok. 4 passed; 0 failed"));
        assert!(shows_pass("test result: ok. 4 passed"));
    }

    #[test]
    fn hook_labels() {
        assert_eq!(
            hl(r#""C:\Users\x\.claude\hooks\guard.exe" --ccd-pr"#),
            "guard.exe"
        );
        assert_eq!(
            hl("node C:/hub/.scripts/hub-digest.js --codex"),
            "hub-digest.js"
        );
        assert_eq!(hl("rtk hook claude"), "rtk");
        assert_eq!(hl("node x.js --config=C:/secret/dir"), "x.js");
        assert_eq!(hl(""), "(empty)");
    }

    #[test]
    fn hook_labels_hide_assignments_and_arguments() {
        assert_eq!(hl("API_KEY=sk-SECRET rtk hook claude"), "rtk");
        assert_eq!(hl("cd /x && node h.js"), "h.js");
        assert_eq!(hl("curl https://example.test/SECRET"), "curl");
        assert_eq!(hl("TOKEN=abc"), "(empty)");
        assert_eq!(hl("command hook 12"), "command hook 12");
        assert_eq!(hl("Loading puritanical mode..."), "Loading");
        assert_eq!(hl("echo /abs/private.js"), "echo");
        assert_eq!(hl("cat notes.py"), "cat");
        assert_eq!(hl("node a.js b.py"), "a.js");
        assert_eq!(hl("node --eval 'console.log(1)' private.py"), "node");
        assert_eq!(hl("python -m pytest t.py"), "python");
        assert_eq!(hl("pwsh -NoProfile -Command x.ps1"), "pwsh");
        assert_eq!(hl("node -r dotenv/config x.js"), "x.js");
        assert_eq!(hl("node -e 'x' a.js"), "node");
        assert_eq!(hl("bash -e hooks/guard.sh"), "guard.sh");
        assert_eq!(hl("bash -lc 'x' run.sh"), "bash");
        assert_eq!(hl("python -E guard.py"), "guard.py");
        assert_eq!(hl("pwsh -File x.ps1"), "x.ps1");
    }

    #[test]
    fn hook_labels_keep_only_plain_names() {
        assert_eq!(hl(r#"bash -c "TOKEN=sentinel node x.js""#), "bash");
        assert_eq!(hl("$env:K='sentinel'; node h.js"), "h.js");
        assert_eq!(hl(r#""echo sentinel""#), "(other)");
        assert_eq!(hl("node 'a sentinel/x.js'"), "x.js");
        assert_eq!(hl("node x.js --a$b"), "x.js");
    }

    #[test]
    fn codex_cells() {
        let input = r#"const r = await tools.exec_command({cmd: "Get-Content -Raw 'C:\\hub\\a.md'"});
const s = readFile('C:\\hub\\b.md'); await tools.apply_patch(p);"#;
        assert_eq!(
            codex_steps(input),
            vec![
                Step::Command(r"Get-Content -Raw 'C:\hub\a.md'".to_owned()),
                Step::Edit
            ]
        );
        assert_eq!(codex_read_calls(input), vec![r"C:\hub\b.md"]);
        assert!(codex_read_calls("readFile(`${dir}/x.md`)").is_empty());
        assert_eq!(
            codex_steps(r#"tools.exec_command({"cmd": `git status`})"#),
            vec![Step::Command("git status".to_owned())]
        );
    }

    #[test]
    fn codex_steps_keep_source_order() {
        let input = r#"await tools.apply_patch(p); await tools.exec_command({cmd: "git status"}); writeFile('x', y);"#;
        assert_eq!(
            codex_steps(input),
            vec![
                Step::Edit,
                Step::Command("git status".to_owned()),
                Step::Edit
            ]
        );
    }

    #[test]
    fn patch_bodies_hold_no_calls() {
        let input = "await tools.apply_patch(`*** Begin Patch\n*** Add File: a.js\n\
            +const c = readFileSync('cfg.json'); writeFile('b', c);\n\
            +await tools.exec_command({cmd: 'git status'});\n*** End Patch`);\n\
            await tools.exec_command({cmd: 'cargo test'}); readFile('real.md');";
        assert_eq!(
            codex_steps(input),
            vec![Step::Edit, Step::Command("cargo test".to_owned())]
        );
        assert_eq!(codex_read_calls(input), vec!["real.md".to_owned()]);
        // Markers outside a string open no patch body.
        let commented =
            "// *** Begin Patch\nawait tools.exec_command({cmd: 'git status'});\n// *** End Patch";
        assert_eq!(
            codex_steps(commented),
            vec![Step::Command("git status".to_owned())]
        );
        let quoted = "// \"*** Begin Patch\nawait tools.exec_command({cmd: 'git status'});\n// *** End Patch";
        assert_eq!(
            codex_steps(quoted),
            vec![Step::Command("git status".to_owned())]
        );
        let after_url = "await fetch('https://x.test'); await tools.apply_patch(`*** Begin Patch\n\
            +readFileSync('cfg.json');\n*** End Patch`);";
        assert_eq!(codex_read_calls(after_url), Vec::<String>::new());
    }

    #[test]
    fn js_unescape_handles_escapes() {
        assert_eq!(
            js_unescape(r#""a\"b\\c\n\u0041\u{42}\x43""#),
            "a\"b\\c\nABC"
        );
        assert_eq!(js_unescape("''"), "");
    }

    #[test]
    fn labels_name_only_known_programs() {
        let mut known =
            |w: &str| ["node", "git", "gh", "ssh", "whoami"].contains(&program(w).as_str());
        for cmd in [
            "ARR=(\n  sentinel\n)\nprintf x",
            "$params = @{\n  sentinel = 'v'\n}\nInvoke-RestMethod @params",
            r"'C:\clients\sentinel.pdf' | Remove-Item",
            "\"owner/sentinel\" | Set-Clipboard",
            r#"TOKEN="ab\" ; sentinel rest" cmd"#,
            "'sentinel' | gh auth login --with-token",
        ] {
            assert_eq!(verb_class(cmd, &mut known), "(other)", "{cmd}");
        }
        assert_eq!(
            verb_class("sudo --user sentinel whoami", &mut known),
            "whoami"
        );
        assert_eq!(
            verb_class("ssh ll 'ARR=(\n sentinel\n)'", &mut known),
            "ssh (other)"
        );
        assert_eq!(verb_class("git status", &mut known), "git");
        assert_eq!(hook_label("node 'C:/a b/sentinel.js'", &mut known), "node");
        assert_eq!(
            hook_label("'sentinel' | gh auth login --with-token", &mut known),
            "(other)"
        );
        assert_eq!(
            hook_label("curl -s https://x.test/a/sentinel.sh", &mut known),
            "(other)"
        );
        assert_eq!(
            hook_label("Loading sentinel mode...", &mut known),
            "(other)"
        );
    }

    #[test]
    fn normalise_path_folds_case_and_slashes() {
        assert_eq!(normalise_path(r"'C:\Hub\A.md'"), "c:/hub/a.md");
    }
}
