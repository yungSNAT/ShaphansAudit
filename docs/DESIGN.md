# Design

`shaphan` reads the transcripts that Claude Code and Codex write on your machine and reports where tokens and checks are wasted. It prints names, paths, counts, sizes and times, as listed under Privacy. It never prints message text, tool inputs or tool outputs, and it makes no network calls.

Leave usage and cost totals to [ccusage](https://github.com/ryoppippi/ccusage); `shaphan` looks for waste that totals cannot show.

## Detectors

| Subcommand | Agents | Reports |
|---|---|---|
| `startup` | Claude | Characters loaded before the first reply of each main session, by source: instruction files, hook output, skill and agent listings, tool schemas, MCP instructions and other attachments |
| `schemas` | Claude | Per tool, or per MCP server: sessions whose requests loaded its schema, sessions that called it, and the characters loaded in sessions that never called it. A tool only ever sent with `defer_loading` gets no row: its schema enters context through ToolSearch results, which v0.1 does not measure |
| `pipes` | Claude | Test runs piped straight into `tail` or `head` without `pipefail` or `PIPESTATUS`, and how many of those printed a failure whilst the call reported success |
| `polls` | Claude, Codex | Commands repeated exactly within a session, split into repeats with no file edit since the previous run (polls) and reruns after an edit |
| `rereads` | Claude, Codex | Files read again after a context compaction that had been read before it |
| `hooks` | Claude | Hook runs, cancellations, timeouts and durations, by event, matcher and hook |

`shaphan all` runs every detector in one pass over the files.

Coverage limits in v0.1: `startup`, `schemas`, `pipes` and `hooks` read Claude transcripts only. `pipes` judges local Bash commands only; commands sent over `ssh` are counted but not judged, because the remote shell's settings are unknown. Test runners inside shell functions or `bash -c` scripts are not recognised. Codex runs PowerShell, where a pipe does not hide a native command's exit status in the same way.

## Inputs

- Claude Code: `~/.claude/projects/**/*.jsonl`. A file under a `subagents` folder is a subagent session. Records used: `assistant` (`tool_use` blocks), `user` (`tool_result` blocks), `attachment` (instruction files, hooks, listings, deltas), `system` with subtype `compact_boundary`, and `api-request-shape` (the tool definitions sent with a request; a tool with `defer_loading: true` is not loaded into context).
- Codex: `~/.codex/sessions/**/*.jsonl`. The first record, `session_meta`, marks a subagent session by its `parent_thread_id`. Records used: `custom_tool_call` named `exec`, whose input is code calling `tools.exec_command({cmd: ...})`, `readFile(...)` or `readFileSync(...)`, and the edits `apply_patch(...)`, `writeFile(...)` or `writeFileSync(...)`, taken in source order, skipping the text of patch bodies (from a string that opens with `*** Begin Patch` to `*** End Patch`, unless the quote follows `//` on its line other than in `://`), which is written, not run; `function_call` named `apply_patch` (an edit) or with a `cmd` or `command` argument; and `compacted`.

Both formats are undocumented and change between releases. `shaphan` skips a line that is cut short (does not end in `}`) or that a detector needs but cannot parse, and reports how many it skipped; it counts unreadable files and folders and never fails a run because of one. A file is read only if it was modified at or after `--since`; within it, a record counts only if its own timestamp is at or after `--since`. A record without a timestamp takes the last one seen in its file. Only the roots of agents that a chosen detector reads are scanned and listed. Session state, such as which files were read before a compaction, is built from the whole file, so a re-read early in the window is still recognised.

## Measures

- Sizes are characters (Unicode scalar values) of the text as stored, or of compact JSON for structured values.
- Commands are split into simple commands at unquoted `|`, `|&`, `||`, `;`, `&`, `&&` and newlines. `#` comments, heredoc bodies (also when written `cat<<EOF`) and PowerShell here-string bodies (from `@'` or `@"` to `'@` or `"@`) are skipped. Backslash escapes, `$(...)` and backticks are not parsed. A Codex `function_call` whose argument list starts with a shell is judged by the element after `-c`, `-lc`, `-Command` or `/c`; any other list is joined with spaces.
- A command's verb class is its first word, lower-cased and without a path or `.exe`, after skipping `VAR=value` assignments, leading segments that are only assignments or `cd <dir>`, and the prefixes `rtk`, `rtk bypass`, `rtk proxy`, `rtk test`, `rtk err`, `sudo` and `time` with their options. A command that is only assignments, including a PowerShell assignment such as `$x = 1` or `$env:KEY='v'`, gives `(assignment)`: an assigned value is never read as a command. For `ssh`, the class is `ssh <remote verb>`; the host is not printed, since a misparse can put a value in its place. A first word that is not a program name (ASCII letters, digits, `.`, `_`, `+` and `-`), such as a variable or a URL, gives `(other)`. The word must also name a program on this machine: a shell builtin, alias or common command from a fixed list, a common PowerShell cmdlet from a fixed list, a program on `PATH` (only absolute folders are searched; on Windows a file with no extension or one in `PATHEXT`, elsewhere a file with an execute bit), or, for a word with a folder in it, an existing file with a program or script extension. Otherwise the class is `(other)`. Parsing is heuristic, and this check keeps a value that lands in a program's place, such as an item of a multi-line array or a quoted literal piped to a command, out of the output.
- A test run is a simple command that starts with a test runner (`cargo test`, `cargo nextest run`, `pytest`, `vitest`, `jest`, `node --test`, `npm test`, `npm run test`, `go test` or `dotnet test`, optionally behind `npx`, `bunx`, `uv run`, `poetry run`, `python -m`, `python3 -m` or `py -m`), possibly after the wrappers `agent-heavy`, `nice`, `timeout`, `env`, `nohup`, `time`, `sudo`, `rtk test`, `rtk err`, `do` or `then` with their options and numeric arguments. A runner named in other text, such as `echo cargo test`, is not a test run. It is piped when a single `|` sends it straight into `tail` or `head`. An `ssh` command whose remote command is a piped test run counts as over `ssh`; one piped locally into `tail` or `head` is judged as local.
- A read is `Read`, or a `cat`, `head`, `tail`, `sed` (not `sed -i`), `less`, `more`, `bat`, `Get-Content` or `gc` command, or a `readFile` call in Codex code, found by pattern, so a call inside a string that never runs also counts; `$text = Get-Content x.md` reads `x.md`. Option values, such as `bat -l rust` or a heredoc delimiter, are not paths. Arguments holding `$` or starting with `(`, `@` or `{` are variables or expressions, not paths, and are skipped. Paths are compared lower-cased with forward slashes.
- A hook's label is the file name of its script or, when there is no script, its program; arguments never print. The script is the program itself or the first script argument of an interpreter (`node`, `bun`, `deno`, `python`, `python3`, `py`, `uv`, `bash`, `sh`, `zsh`, `pwsh`, `powershell` or `cmd`) before an option with which that interpreter runs inline code: `-e`, `-p`, `--eval` or `--print` for `node`, `bun` and `deno`; `-c` or `-m` for Python and `-c` for `bash`, `sh` and `zsh`, also at the end of a group such as `-lc`; and `-c`, `-Command`, `-ec` or `-EncodedCommand` in any case for PowerShell. It must be an existing file with a script or program extension, and the program must pass the verb-class check above; otherwise the label is `(other)`. The name must be plain (ASCII letters, digits, `.`, `_`, `+` and `-`). `VAR=value` and PowerShell assignments and a leading `cd` are skipped first. Recent Claude Code versions record `command hook N` (the hook's position under its event and matcher) or the hook's status message in place of the command; `command hook N` is kept whole, and a status message whose first word is not a program gives `(other)`. Hook rows are keyed by the transcript's `hookName`, which joins the event and matcher, such as `PreToolUse:Bash`, or by `hookEvent` when that is missing; a hook with neither is `(unknown)`. A fractional `durationMs` is rounded.
- A re-read is a read, after a compaction, of a file read before it and not yet read since. Reread share divides re-reads by every read in the window for that agent and scope, in compacted sessions or not.

## Privacy

Output holds tool names, MCP server names, instruction file paths, hook names (event and matcher), hook labels, attachment types, entry points, verb classes, the scanned folders, counts, sizes and times. Paths under the home folder print from `~`; other paths print in full. Re-read file paths print only with `--paths`.

A verb class or hook label names only a program found on the machine that runs `shaphan`, so a value from a command prints only if it is also such a name, and the same transcripts can give different labels on another machine.

Fixtures in this repository are synthetic. Tests check, ignoring case, that no sentinel string reaches the output: those in the main fixtures' message text, tool inputs, tool outputs and hook commands, and those in a probe fixture with one on each path found so far by which input text could reach a verb class, hook label or path (PowerShell assignments, hashtables and here-strings, multi-line arrays, comments, heredocs, escaped quotes, `ssh` hosts, URLs, quoted first words, `sudo` and `time` options, reader option values, hook commands, Codex argument lists, `readFile` calls and patch bodies). The probe test also checks that its records reach the detectors. The tests create stub programs and put them first on `PATH`, and their hook commands name stub scripts in `tests/fixtures/hooks`, so their labels do not depend on the machine.

## Interface

```text
shaphan <all|startup|schemas|pipes|polls|rereads|hooks> [--since 7d|24h|YYYY-MM-DD|RFC 3339]
        [--agent claude|codex|all] [--claude-root DIR] [--codex-root DIR] [--json] [--limit N] [--paths]
```

`--since` defaults to `7d`; a bare date means local midnight. `--limit` (default 20) caps each table's rows. `--json` prints one object with `schema_version` 1. Exit codes: 0 success, 1 a runtime error such as a missing transcript folder or a bad `--since`, 2 a usage error.

## Not in v0.1

Large tool results, cache rewrites after idle gaps, colour codes in tool results, Codex startup and tool schemas, Hermes `state.db` and RTK `history.db`.
