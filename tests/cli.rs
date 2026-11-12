//! Runs the `shaphan` binary over the synthetic fixtures in `tests/fixtures`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{Value, json};

const CLAUDE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude");
const CODEX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/codex");
const SINCE: &str = "2026-11-01T00:00:00Z";
/// A folder of empty stub programs for the fixtures' commands, put first on `PATH`
/// so their labels do not depend on the machine.
fn stubs() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("stub-programs");
        std::fs::create_dir_all(&dir).expect("stub folder");
        for name in ["agent-heavy", "rtk"] {
            let file = dir.join(if cfg!(windows) {
                format!("{name}.exe")
            } else {
                name.to_owned()
            });
            std::fs::write(&file, "").expect("stub program");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))
                    .expect("execute bit");
            }
        }
        dir
    })
}

/// Runs `shaphan` from the crate folder, where fixture hook commands name scripts in
/// `tests/fixtures/hooks`, with the stub programs first on `PATH`.
fn run(args: &[&str]) -> (i32, String, String) {
    run_env(args, &[])
}

/// As `run`, with extra environment variables.
fn run_env(args: &[&str], env: &[(&str, &Path)]) -> (i32, String, String) {
    let system = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(stubs().to_path_buf()).chain(std::env::split_paths(&system)),
    )
    .expect("PATH");
    let out = Command::new(env!("CARGO_BIN_EXE_shaphan"))
        .args(args)
        .env("PATH", path)
        .envs(env.iter().copied())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("binary runs");
    let text = |b: Vec<u8>| String::from_utf8(b).expect("UTF-8 output");
    (
        out.status.code().unwrap_or(-1),
        text(out.stdout),
        text(out.stderr),
    )
}

fn fixture_args<'a>(cmd: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        cmd,
        "--claude-root",
        CLAUDE,
        "--codex-root",
        CODEX,
        "--since",
        SINCE,
    ];
    args.extend_from_slice(extra);
    args
}

fn report(cmd: &str, extra: &[&str]) -> Value {
    let mut args = fixture_args(cmd, extra);
    args.push("--json");
    let (code, stdout, stderr) = run(&args);
    assert_eq!(code, 0, "stderr: {stderr}");
    serde_json::from_str(&stdout).expect("valid JSON")
}

fn rows<'a>(doc: &'a Value, detector: &str, table: usize) -> &'a Vec<Value> {
    doc["detectors"][detector]["tables"][table]["rows"]
        .as_array()
        .expect("rows")
}

fn find<'a>(rows: &'a [Value], pairs: &[(&str, Value)]) -> &'a Value {
    rows.iter()
        .find(|r| pairs.iter().all(|(k, v)| &r[*k] == v))
        .unwrap_or_else(|| panic!("no row matching {pairs:?} in {rows:?}"))
}

#[test]
fn startup_measures_main_sessions_started_in_the_window() {
    let doc = report("startup", &[]);
    let s = &doc["detectors"]["startup"]["summary"];
    assert_eq!(s["sessions"], 1);
    assert_eq!(s["entry_points"], "claude-desktop 1");
    assert_eq!(s["median_total_chars"], 230);
    let r = rows(&doc, "startup", 0);
    for (piece, chars) in [
        ("tool schemas (loaded)", 121),
        ("skill listing", 30),
        ("file: C:/Users/tester/.claude/CLAUDE.md", 25),
        ("hook: hub-digest.js", 19),
        ("deferred tool names", 19),
        ("mcp instructions: firecrawl", 16),
    ] {
        assert_eq!(
            find(r, &[("piece", json!(piece))])["median_chars"],
            chars,
            "{piece}"
        );
    }
    assert_eq!(
        r.len(),
        6,
        "nothing after the first reply and nothing from the old session"
    );
}

#[test]
fn schemas_report_idle_groups_first_and_skip_deferred_tools() {
    let doc = report("schemas", &[]);
    let s = &doc["detectors"]["schemas"]["summary"];
    assert_eq!(
        (s["main_sessions"].clone(), s["subagent_sessions"].clone()),
        (json!(1), json!(1))
    );
    let r = rows(&doc, "schemas", 0);
    assert_eq!(r[0]["tool"], "mcp__unused__*");
    assert_eq!(
        (r[0]["idle_chars"].clone(), r[0]["sessions_called"].clone()),
        (json!(28), json!(0))
    );
    let browser = find(
        r,
        &[("scope", json!("main")), ("tool", json!("mcp__browser__*"))],
    );
    assert_eq!(browser["tools"], 2);
    assert_eq!(browser["calls"], 1);
    assert_eq!(browser["median_chars"], 63);
    assert_eq!(browser["idle_chars"], 0);
    assert!(
        r.iter().all(|row| row["tool"] != "WebFetch"),
        "deferred tools are not loaded"
    );
    assert_eq!(
        find(r, &[("scope", json!("subagent")), ("tool", json!("Bash"))])["calls"],
        2
    );
}

#[test]
fn pipes_count_unguarded_test_runs_and_masked_failures() {
    let doc = report("pipes", &[]);
    let s = &doc["detectors"]["pipes"]["summary"];
    let expected = json!({
        "test_runs": 5,
        "piped_into_tail_or_head": 5,
        "over_ssh_not_judged": 1,
        "guarded_with_pipefail_or_pipestatus": 1,
        "unguarded": 3,
        "unguarded_results_found": 2,
        "unguarded_reported_success": 2,
        "showed_a_failure_but_reported_success": 1,
        "showed_no_pass_or_fail_line": 0,
    });
    assert_eq!(s, &expected);
}

#[test]
fn polls_separate_repeats_from_reruns_after_an_edit() {
    let doc = report("polls", &[]);
    let totals = rows(&doc, "polls", 0);
    for (agent, scope, commands, polls, reruns) in [
        ("claude", "main", 14, 3, 1),
        ("claude", "subagent", 2, 1, 0),
        ("codex", "main", 5, 1, 1),
        ("codex", "subagent", 2, 1, 0),
    ] {
        let row = find(totals, &[("agent", json!(agent)), ("scope", json!(scope))]);
        assert_eq!(
            (
                row["commands"].clone(),
                row["polls"].clone(),
                row["reruns_after_edit"].clone()
            ),
            (json!(commands), json!(polls), json!(reruns)),
            "{agent} {scope}"
        );
    }
    let verbs = rows(&doc, "polls", 1);
    assert_eq!(verbs[0]["verb"], "agent-heavy");
    assert_eq!(verbs[0]["polls"], 2);
    assert_eq!(
        find(
            verbs,
            &[
                ("agent", json!("codex")),
                ("verb", json!("ssh agent-heavy"))
            ]
        )["polls"],
        1
    );
    assert_eq!(
        find(
            verbs,
            &[("agent", json!("claude")), ("verb", json!("cargo"))]
        )["reruns_after_edit"],
        1
    );
}

#[test]
fn polls_report_result_characters_and_repeats_of_a_failed_run() {
    let doc = report("polls", &[]);
    let totals = rows(&doc, "polls", 0);
    let cost = |row: &Value| {
        (
            row["polls_after_failure"].clone(),
            row["poll_chars"].clone(),
        )
    };
    // `agent-heavy status` fails, is polled (17 characters back) and polled again after
    // that success (15 and 5 characters in 2 text blocks). The old session's `git status`
    // failed before the window and is polled inside it, with no result in the transcript.
    let main = find(
        totals,
        &[("agent", json!("claude")), ("scope", json!("main"))],
    );
    assert_eq!(cost(main), (json!(2), json!(37)));
    let sub = find(
        totals,
        &[("agent", json!("claude")), ("scope", json!("subagent"))],
    );
    assert_eq!(cost(sub), (json!(0), json!(0)));
    let verbs = rows(&doc, "polls", 1);
    for (verb, expected) in [
        ("agent-heavy", (json!(1), json!(37))),
        ("git", (json!(1), json!(0))),
    ] {
        let row = find(verbs, &[("agent", json!("claude")), ("verb", json!(verb))]);
        assert_eq!(cost(row), expected, "{verb}");
    }
    let codex: Vec<&Value> = totals
        .iter()
        .chain(verbs)
        .filter(|row| row["agent"] == "codex")
        .collect();
    assert!(!codex.is_empty());
    for row in codex {
        for column in ["polls_after_failure", "poll_chars"] {
            assert_eq!(row.get(column), Some(&Value::Null), "{column} in {row}");
        }
    }
}

const RESULTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/results/claude");

/// The main-scope row of a report over the result-pairing fixture.
fn results_row(cmd: &str) -> Value {
    let (code, stdout, stderr) = run(&[
        cmd,
        "--claude-root",
        RESULTS,
        "--agent",
        "claude",
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    find(rows(&doc, cmd, 0), &[("scope", json!("main"))]).clone()
}

#[test]
fn polls_pair_results_by_id_within_one_session_and_the_window() {
    let row = results_row("polls");
    // After a failure: the first run in the window, whose last run failed before it, and
    // its repeat. Not the rerun after an edit, the poll after the fixed run, 2 polls
    // issued before either result, or the next session's poll.
    assert_eq!(row["polls_after_failure"], 2);
    // 22 + 14 + 12 + 10 + 11 in one session and 11 in the next. Not the 13 of a poll
    // before the window, a first run's or a rerun's result, or a result in the second
    // session that carries an id from the first.
    assert_eq!(row["poll_chars"], 80);
    assert_eq!(
        (
            row["commands"].clone(),
            row["polls"].clone(),
            row["reruns_after_edit"].clone()
        ),
        (json!(11), json!(7), json!(1))
    );
}

#[test]
fn rereads_count_a_call_once_and_only_in_the_window() {
    let row = results_row("rereads");
    // One `cat` re-reads 2 files and returns 18 characters. A re-read before the window
    // adds nothing, though its result is inside it, and nor does a later read.
    assert_eq!(
        (
            row["reads"].clone(),
            row["rereads"].clone(),
            row["reread_chars"].clone()
        ),
        (json!(3), json!(2), json!(18))
    );
}

#[test]
fn rereads_count_files_read_again_after_compaction() {
    let doc = report("rereads", &["--paths"]);
    let r = rows(&doc, "rereads", 0);
    let claude = find(r, &[("agent", json!("claude")), ("scope", json!("main"))]);
    assert_eq!(claude["compactions"], 1);
    assert_eq!(claude["reads"], 6);
    assert_eq!(claude["rereads"], 2);
    assert_eq!(claude["reread_share"], 0.33);
    // The `Read` re-read returns 17 and 3 characters of text around an image, and the
    // `tail` re-read 13. The first reads and a second read since the compaction add none.
    assert_eq!(claude["reread_chars"], 33);
    let codex = find(r, &[("agent", json!("codex")), ("scope", json!("main"))]);
    assert_eq!(
        (codex["reads"].clone(), codex["rereads"].clone()),
        (json!(3), json!(1))
    );
    assert_eq!(codex.get("reread_chars"), Some(&Value::Null));
    let paths = rows(&doc, "rereads", 1);
    let names: Vec<&str> = paths
        .iter()
        .map(|p| p["path"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(names, vec!["c:/work/a.md", "c:/work/b.md", "c:/work/x.md"]);
}

#[test]
fn rereads_hide_paths_without_the_flag() {
    let doc = report("rereads", &[]);
    assert_eq!(
        doc["detectors"]["rereads"]["tables"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
}

#[test]
fn hooks_rank_cancellations_first_and_skip_old_runs() {
    let doc = report("hooks", &[]);
    let r = rows(&doc, "hooks", 0);
    assert_eq!(r.len(), 3);
    assert_eq!(r[0]["hook"], "rtk");
    assert_eq!(
        (r[0]["cancelled"].clone(), r[0]["timed_out"].clone()),
        (json!(1), json!(1))
    );
    assert_eq!(find(r, &[("hook", json!("guard.exe"))])["median_ms"], 40);
    let digest = find(r, &[("hook", json!("hub-digest.js"))]);
    assert_eq!(
        (
            digest["max_ms"].clone(),
            digest["median_output_chars"].clone()
        ),
        (json!(120), json!(19))
    );
}

#[test]
fn all_reports_each_root_and_counts_the_partial_last_line_once() {
    let doc = report("all", &[]);
    let roots = doc["roots"].as_array().expect("roots");
    assert_eq!(roots[0]["agent"], "claude");
    assert_eq!(roots[0]["files"], 3);
    assert_eq!(roots[0]["lines_skipped"], 1);
    assert_eq!(roots[1]["files"], 2);
    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["detectors"].as_object().map(|o| o.len()), Some(6));
}

#[test]
fn codex_default_reads_archived_sessions_and_counts_compressed_files() {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("home-archived");
    let _ = std::fs::remove_dir_all(&home);
    let rollout = std::fs::read(format!("{CODEX}/2026/11/08/rollout-main.jsonl")).expect("fixture");
    for (dir, name) in [
        ("sessions/2026/11/08", "rollout-live.jsonl"),
        ("archived_sessions", "rollout-archived.jsonl"),
        ("archived_sessions", "rollout-packed.jsonl.zst"),
    ] {
        let dir = home.join(".codex").join(dir);
        std::fs::create_dir_all(&dir).expect("folder");
        std::fs::write(dir.join(name), &rollout).expect("transcript");
    }
    let (code, stdout, stderr) = run_env(
        &["polls", "--agent", "codex", "--since", SINCE, "--json"],
        &[("HOME", &home), ("USERPROFILE", &home)],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    let roots = doc["roots"].as_array().expect("roots");
    let counts: Vec<(&str, &Value, &Value)> = roots
        .iter()
        .map(|r| {
            let root = r["root"].as_str().expect("root");
            (
                root.rsplit('/').next().unwrap_or(root),
                &r["files"],
                &r["compressed_skipped"],
            )
        })
        .collect();
    assert_eq!(
        counts,
        [
            ("sessions", &json!(1), &json!(0)),
            ("archived_sessions", &json!(1), &json!(1)),
        ]
    );
}

/// `rollout-main.jsonl` compressed by the zstd 1.5.7 command line tool with `--check`.
const ZST: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/zst/rollout-main.jsonl.zst"
);

/// Runs `polls` for Codex over one folder holding the given files.
fn codex_polls(name: &str, files: &[(&str, &[u8])]) -> Value {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("folder");
    for (file, bytes) in files {
        std::fs::write(dir.join(file), bytes).expect("transcript");
    }
    let root = dir.to_str().expect("UTF-8 path");
    let (code, stdout, stderr) = run(&[
        "polls",
        "--agent",
        "codex",
        "--codex-root",
        root,
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    serde_json::from_str(&stdout).expect("valid JSON")
}

fn root_counts(doc: &Value) -> [&Value; 4] {
    let r = &doc["roots"][0];
    [
        &r["files"],
        &r["unreadable"],
        &r["lines_skipped"],
        &r["compressed_skipped"],
    ]
}

#[test]
fn codex_reads_compressed_rollouts_as_it_reads_plain_ones() {
    let plain = std::fs::read(format!("{CODEX}/2026/11/08/rollout-main.jsonl")).expect("fixture");
    let packed = std::fs::read(ZST).expect("fixture");
    let from_plain = codex_polls("zst-plain", &[("rollout.jsonl", &plain)]);
    let from_packed = codex_polls("zst-packed", &[("rollout.jsonl.zst", &packed)]);
    assert_eq!(
        root_counts(&from_packed),
        [&json!(1), &json!(0), &json!(0), &json!(0)]
    );
    assert_eq!(
        from_packed["detectors"]["polls"],
        from_plain["detectors"]["polls"]
    );
    assert!(
        rows(&from_packed, "polls", 0)
            .iter()
            .any(|r| r["polls"] != 0)
    );
}

#[test]
fn codex_counts_every_compressed_rollout_it_cannot_decode() {
    let packed = std::fs::read(ZST).expect("fixture");
    let mut bad_sum = packed.clone();
    *bad_sum.last_mut().expect("checksum") ^= 0xff;
    let two_frames = [packed.as_slice(), packed.as_slice()].concat();
    let doc = codex_polls(
        "zst-bad",
        &[
            ("cut.jsonl.zst", &packed[..packed.len() / 2]),
            ("sum.jsonl.zst", &bad_sum),
            ("two.jsonl.zst", &two_frames),
            ("text.jsonl.zst", b"{\"type\":\"session_meta\"}\n"),
        ],
    );
    assert_eq!(
        root_counts(&doc),
        [&json!(3), &json!(3), &json!(0), &json!(1)]
    );
}

/// Runs `all` over two roots with each output flag and checks that no sentinel
/// appears in any case: verb classes and paths are lower-cased.
fn assert_no_sentinel(claude: &str, codex: &str) {
    for extra in [&[][..], &["--json"], &["--paths"], &["--json", "--paths"]] {
        let mut args = vec![
            "all",
            "--claude-root",
            claude,
            "--codex-root",
            codex,
            "--since",
            SINCE,
        ];
        args.extend_from_slice(extra);
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 0, "{stderr}");
        assert!(
            !stdout.to_lowercase().contains("sentinel"),
            "content leaked with {extra:?}:\n{stdout}"
        );
        assert!(!stderr.to_lowercase().contains("sentinel"), "{stderr}");
    }
}

#[test]
fn output_never_contains_transcript_content() {
    assert_no_sentinel(CLAUDE, CODEX);
    assert_no_sentinel(RESULTS, CODEX);
}

#[test]
fn probe_inputs_never_reach_the_output() {
    let probe = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/probe");
    let (claude, codex) = (format!("{probe}/claude"), format!("{probe}/codex"));
    assert_no_sentinel(&claude, &codex);
    // The probe must reach the detectors, or the check above proves nothing.
    let (code, stdout, stderr) = run(&[
        "all",
        "--claude-root",
        &claude,
        "--codex-root",
        &codex,
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    let verbs = rows(&doc, "polls", 1);
    for (agent, verb) in [
        ("claude", "(assignment)"),
        ("claude", "ssh ls"),
        ("claude", "(other)"),
        ("codex", "(assignment)"),
        ("codex", "bash"),
    ] {
        find(verbs, &[("agent", json!(agent)), ("verb", json!(verb))]);
    }
    let hooks = rows(&doc, "hooks", 0);
    find(
        hooks,
        &[
            ("hook_name", json!("(unknown)")),
            ("hook", json!("(other)")),
        ],
    );
    find(
        hooks,
        &[("hook_name", json!("(unknown)")), ("hook", json!("node"))],
    );
}

#[test]
fn text_output_has_a_section_per_detector() {
    let (code, stdout, _) = run(&fixture_args("all", &["--limit", "1"]));
    assert_eq!(code, 0);
    for name in ["startup", "schemas", "pipes", "polls", "rereads", "hooks"] {
        assert!(stdout.contains(&format!("== {name}: ")), "{name}");
    }
    assert!(stdout.contains("more rows; raise --limit"));
}

#[test]
fn a_detector_without_its_agent_says_so() {
    let doc = report("pipes", &["--agent", "codex"]);
    let notes = doc["detectors"]["pipes"]["notes"]
        .as_array()
        .expect("notes");
    assert_eq!(
        notes[0],
        "Reads claude transcripts only, and none were scanned."
    );
}

#[test]
fn help_and_version_exit_zero() {
    let (code, stdout, _) = run(&["--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("Usage: shaphan"));
    let (code, stdout, _) = run(&["--version"]);
    assert_eq!(code, 0);
    assert_eq!(
        stdout.trim(),
        format!("shaphan {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn usage_errors_exit_two() {
    assert_eq!(run(&[]).0, 2);
    assert_eq!(run(&["polls", "--no-such-flag"]).0, 2);
    assert_eq!(run(&["polls", "--agent", "hermes"]).0, 2);
}

#[test]
fn domain_errors_exit_one_with_a_message() {
    let (code, _, stderr) = run(&["polls", "--claude-root", CLAUDE, "--since", "last week"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("--since"), "{stderr}");
    let (code, _, stderr) = run(&[
        "polls",
        "--claude-root",
        "no/such/folder",
        "--agent",
        "claude",
    ]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("claude transcript folder not found"),
        "{stderr}"
    );
}

#[test]
fn an_early_since_is_an_error_not_a_panic() {
    for since in ["1500-01-01", "200000d"] {
        let (code, _, stderr) = run(&["hooks", "--claude-root", CLAUDE, "--since", since]);
        assert_eq!(code, 1, "{since}: {stderr}");
        assert!(stderr.contains("--since"), "{stderr}");
    }
}

#[test]
fn a_far_future_since_finds_nothing() {
    let (code, stdout, stderr) = run(&[
        "polls",
        "--claude-root",
        CLAUDE,
        "--since",
        "9999-12-31",
        "--json",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(doc["roots"][0]["files"], 0);
}

#[test]
fn folders_for_agents_no_chosen_detector_reads_are_not_checked() {
    let (code, _, stderr) = run(&[
        "hooks",
        "--claude-root",
        CLAUDE,
        "--codex-root",
        "no/such/folder",
        "--since",
        SINCE,
    ]);
    assert_eq!(code, 0, "{stderr}");
}

#[test]
fn hooks_without_output_show_null() {
    let doc = report("hooks", &[]);
    let rtk = find(rows(&doc, "hooks", 0), &[("hook", json!("rtk"))]);
    assert_eq!(rtk["median_output_chars"], Value::Null);
}

#[test]
fn paths_under_home_start_with_a_tilde() {
    let home = concat!(env!("CARGO_MANIFEST_DIR"), "/tests");
    let out = Command::new(env!("CARGO_BIN_EXE_shaphan"))
        .args([
            "hooks",
            "--agent",
            "claude",
            "--claude-root",
            CLAUDE,
            "--since",
            SINCE,
            "--json",
        ])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .output()
        .expect("binary runs");
    assert!(out.status.success());
    let doc: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(doc["roots"][0]["root"], "~/fixtures/claude");
}

#[test]
fn files_modified_before_the_window_are_not_read() {
    let dir = std::env::temp_dir().join(format!("shaphan-mtime-{}", std::process::id()));
    let file = dir.join("proj").join("s.jsonl");
    std::fs::create_dir_all(file.parent().expect("parent")).expect("temp folder");
    std::fs::copy(format!("{CLAUDE}/proj-a/s-main.jsonl"), &file).expect("copy fixture");
    let october_2025 = UNIX_EPOCH + Duration::from_secs(1_760_000_000);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .and_then(|f| f.set_modified(october_2025))
        .expect("set the modification time");
    let root = dir.to_str().expect("UTF-8 path").to_owned();
    let (code, stdout, stderr) = run(&[
        "hooks",
        "--agent",
        "claude",
        "--claude-root",
        &root,
        "--since",
        SINCE,
        "--json",
    ]);
    std::fs::remove_dir_all(&dir).expect("remove the temporary folder");
    assert_eq!(code, 0, "{stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(doc["roots"][0]["files"], 0);
}

#[test]
fn roots_no_detector_reads_are_left_out_and_cut_lines_still_count() {
    let doc = report("hooks", &[]);
    let roots = doc["roots"].as_array().expect("roots");
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0]["agent"], "claude");
    assert_eq!(roots[0]["lines_skipped"], 1);
}
