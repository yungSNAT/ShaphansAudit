//! Shaphan's audit: where a coding agent's tokens and checks go, from its own transcripts.

pub mod cli;
pub mod detectors;
pub mod programs;
pub mod report;
pub mod scan;
pub mod shell;
pub mod time;

use std::path::PathBuf;

use cli::{AgentChoice, Cli};
use report::{Meta, Opts};
use scan::Agent;

/// Runs the chosen detectors and returns the rendered report.
///
/// # Errors
/// Returns a message when `--since` is invalid or no transcript folder can be read.
pub fn run(cli: &Cli) -> Result<String, String> {
    let since = time::parse_since(&cli.since, chrono::Utc::now())?;
    let home = std::env::home_dir().map(|h| report::display_path(&h.display().to_string(), None));
    let mut dets = detectors::build(
        cli.command,
        &std::rc::Rc::new(programs::Programs::from_env()),
    );
    let read: Vec<Agent> = dets.iter().flat_map(|d| d.agents()).copied().collect();
    let (roots, skipped) = resolve_roots(cli, &read, home.as_deref())?;
    let stats = scan::scan(&roots, since, &mut dets);
    let opts = Opts {
        limit: cli.limit,
        paths: cli.paths,
        home: home.clone(),
    };
    let scanned: Vec<Agent> = stats.iter().map(|s| s.agent).collect();
    let sections: Vec<_> = dets
        .iter()
        .map(|d| {
            let mut s = d.section(&opts);
            if !d.agents().iter().any(|a| scanned.contains(a)) {
                let names: Vec<&str> = d.agents().iter().map(|a| a.name()).collect();
                s.notes.insert(
                    0,
                    format!(
                        "Reads {} transcripts only, and none were scanned.",
                        names.join(" and ")
                    ),
                );
            }
            s
        })
        .collect();
    let meta = Meta {
        since,
        roots: &stats,
        skipped_roots: &skipped,
        home: home.as_deref(),
    };
    Ok(if cli.json {
        report::json(&meta, &sections)
    } else {
        report::text(&meta, &sections)
    })
}

type Roots = (Vec<(Agent, PathBuf)>, Vec<String>);

/// The transcript folders to scan for agents that `--agent` allows and the chosen
/// detectors read, and notes on default folders that are missing.
fn resolve_roots(cli: &Cli, read: &[Agent], home: Option<&str>) -> Result<Roots, String> {
    let wanted = |a: Agent| {
        read.contains(&a)
            && match cli.agent {
                AgentChoice::All => true,
                AgentChoice::Claude => a == Agent::Claude,
                AgentChoice::Codex => a == Agent::Codex,
            }
    };
    let candidates = [
        (Agent::Claude, cli.claude_root.clone(), ".claude/projects"),
        (Agent::Codex, cli.codex_root.clone(), ".codex/sessions"),
    ];
    let mut roots = Vec::new();
    let mut skipped = Vec::new();
    for (agent, explicit, default) in candidates {
        if !wanted(agent) {
            continue;
        }
        let given = explicit.is_some();
        let path = match explicit {
            Some(p) => p,
            None => PathBuf::from(home.ok_or("cannot find the home folder")?).join(default),
        };
        let problem = if !path.is_dir() {
            "not found"
        } else if std::fs::read_dir(&path).is_err() {
            "unreadable"
        } else {
            roots.push((agent, path));
            continue;
        };
        let shown = report::display_path(&path.display().to_string(), home);
        let message = format!("{} transcript folder {problem}: {shown}", agent.name());
        if given || cli.agent != AgentChoice::All {
            return Err(message);
        }
        skipped.push(message);
    }
    // With no folder tried, as for `hooks --agent codex`, each section notes that
    // none of its transcripts were scanned.
    if roots.is_empty() && !skipped.is_empty() {
        return Err(skipped.join("; "));
    }
    Ok((roots, skipped))
}
