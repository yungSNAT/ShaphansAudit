# ShaphansAudit

`shaphan` reads the transcripts that Claude Code and Codex save on your machine and reports where tokens and checks are wasted: context loaded before the first reply, tool schemas loaded but never called, test output piped past its exit status, repeated polling, files re-read after a compaction and slow hooks.

It prints names, paths, counts, sizes and times, as listed under [Privacy](#privacy). It never prints message text, tool inputs or tool outputs, and it makes no network calls.

## Install

Build from source with Rust 1.99 or later:

```bash
git clone https://github.com/yungSNAT/ShaphansAudit
cd ShaphansAudit
cargo install --locked --path .
```

## Use

```bash
shaphan all
```

This reads the last 7 days of `~/.claude/projects` and `~/.codex/sessions` and prints one section per detector. Run one detector by name, for example `shaphan hooks --since 24h`.

| Command | Agents | Reports |
|---|---|---|
| `startup` | Claude | Characters loaded before the first reply of each main session, by source: instruction files, hook output, listings, tool schemas and MCP instructions |
| `schemas` | Claude | Per tool or MCP server: sessions that loaded its schema, sessions that called it, and the characters loaded in sessions that never called it |
| `pipes` | Claude | Test runs piped into `tail` or `head` without `pipefail` or `PIPESTATUS`, and how many printed a failure whilst the call reported success |
| `polls` | Claude, Codex | Commands repeated exactly within a session, split into polls (no file edit since the last run) and reruns after an edit |
| `rereads` | Claude, Codex | Files read again after a context compaction that had been read before it |
| `hooks` | Claude | Hook runs, cancellations, timeouts and durations, by event, matcher and hook |

| Option | Default | Meaning |
|---|---|---|
| `--since` | `7d` | Start of the window: `Nd`, `Nh`, a date (local midnight) or an RFC 3339 time |
| `--agent` | `all` | `claude`, `codex` or `all` |
| `--claude-root DIR` | `~/.claude/projects` | Claude Code transcript folder |
| `--codex-root DIR` | `~/.codex/sessions` | Codex transcript folder |
| `--json` | off | Print one JSON object with `schema_version` 1 |
| `--limit N` | 20 | Rows per table |
| `--paths` | off | List the most re-read file paths |

Exit codes: 0 success, 1 a runtime error such as a missing transcript folder or a bad `--since`, 2 a usage error.

## Privacy

Output holds tool and MCP server names, instruction file paths, hook names and labels, attachment types, entry points, command verb classes, the scanned folders, counts, sizes and times. Paths under your home folder start with `~`. Re-read file paths appear only with `--paths`. A verb class or hook label prints only the name of a program found on your machine (a common command, a common PowerShell cmdlet, a program on `PATH` or an existing script); anything else prints as `(other)`, so labels can differ between machines. A script written as a network share path, such as `\\host\share\run.ps1`, is never looked up on disk, because the lookup would contact that host; only its file name is checked. A path your system maps to a network location, such as a drive letter mapped to a share or an automounted folder, is looked up like a local path. Read the output before you share it.

The tests run on synthetic fixtures. They plant sentinel strings in message text, tool inputs, tool outputs and hook commands, and in a probe of each way found so far that input text could reach a verb class, hook label or path, then check that none reaches the output in any letter case.

## Limits

Both transcript formats are undocumented and change between releases. `shaphan` skips a line it cannot parse and reports how many it skipped. Version 0.1 reads Codex transcripts for `polls` and `rereads` only. [docs/DESIGN.md](docs/DESIGN.md) defines each measure.

For usage and cost totals, use [ccusage](https://github.com/ryoppippi/ccusage).

Named after Shaphan, the scribe who counted the temple silver and read the recovered scroll to the king (2 Kings 22, [NET](https://netbible.org)).

## Licence

Licensed under the Functional Source License, Version 1.1, ALv2 Future License (FSL-1.1-ALv2). You may use, modify and redistribute it for any purpose except a Competing Use as defined in [LICENSE](LICENSE), and each version becomes available under Apache-2.0 2 years after it is made available.

<sub>(John 6:12, [NET](https://netbible.org))</sub>
