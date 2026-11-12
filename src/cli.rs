//! Command-line interface.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// Find where your coding agent's tokens go, from its own transcripts.
///
/// Reads Claude Code and Codex transcripts on this machine and prints names,
/// counts and sizes. Never prints message text, tool inputs or tool outputs.
#[derive(Parser, Debug)]
#[command(name = "shaphan", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Count records at or after this time: 7d, 24h, YYYY-MM-DD (local midnight) or an RFC 3339 time
    #[arg(long, global = true, default_value = "7d", value_name = "WHEN")]
    pub since: String,

    /// Which agent's transcripts to read
    #[arg(long, global = true, value_enum, default_value_t = AgentChoice::All)]
    pub agent: AgentChoice,

    /// Claude Code transcript folder [default: ~/.claude/projects]
    #[arg(long, global = true, value_name = "DIR")]
    pub claude_root: Option<PathBuf>,

    /// Codex transcript folder [default: ~/.codex/sessions and ~/.codex/archived_sessions]
    #[arg(long, global = true, value_name = "DIR")]
    pub codex_root: Option<PathBuf>,

    /// Print one JSON object instead of tables
    #[arg(long, global = true)]
    pub json: bool,

    /// Most rows to print in each table
    #[arg(long, global = true, default_value_t = 20, value_name = "N")]
    pub limit: usize,

    /// Also list re-read file paths (rereads only)
    #[arg(long, global = true)]
    pub paths: bool,
}

/// What to report.
#[derive(Subcommand, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Every detector, in one pass
    All,
    /// What loads before the first reply of each main session (Claude)
    Startup,
    /// Tool schemas loaded against tools called (Claude)
    Schemas,
    /// Test runs whose exit status a pipe into tail or head hides (Claude)
    Pipes,
    /// Commands repeated with no edit in between (Claude, Codex)
    Polls,
    /// Files read again after a context compaction (Claude, Codex)
    Rereads,
    /// Hook durations, cancellations and timeouts (Claude)
    Hooks,
}

/// Agent selection for `--agent`.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentChoice {
    Claude,
    Codex,
    All,
}
