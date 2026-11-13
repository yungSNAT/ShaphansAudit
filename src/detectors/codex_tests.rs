//! Codex sessions replayed through the detectors that read `exec` cell outputs.

use serde_json::{Value, json};

use super::pipes::Pipes;
use super::polls::Polls;
use super::rereads::Rereads;
use crate::report::Opts;
use crate::scan::{Agent, Detector, Record, Session};

const MAIN: Session = Session {
    agent: Agent::Codex,
    subagent: false,
};

/// Feeds one Codex session to `d` as the scan does, parsing only the lines it
/// wants; each line comes with whether it is in the window.
fn replay(d: &mut dyn Detector, lines: &[(bool, String)]) {
    d.begin(&MAIN);
    for (in_window, line) in lines {
        if d.wants(&MAIN, line) {
            let value: Value = serde_json::from_str(line).expect("test record");
            d.record(
                &MAIN,
                &Record {
                    value: &value,
                    in_window: *in_window,
                },
            );
        }
    }
    d.end(&MAIN);
}

/// An `exec` call running `input`, with `id` as its call id unless `id` is "".
fn exec(id: &str, input: &str) -> String {
    let mut p = json!({"type": "custom_tool_call", "name": "exec", "input": input});
    if !id.is_empty() {
        p["call_id"] = json!(id);
    }
    json!({"type": "response_item", "payload": p}).to_string()
}

/// Cell code that prints the `exec_command` result of each command in turn.
fn commands(cmds: &[&str]) -> String {
    cmds.iter()
        .map(|c| format!("text(await tools.exec_command({{cmd: {}}}));", json!(c)))
        .collect()
}

/// An `exec` call's output: the status line, then one printed result per
/// `(exit_code, output)`, where the output is a string or null.
fn output(id: &str, results: &[(i64, Value)]) -> String {
    let texts: Vec<Value> = results
        .iter()
        .map(|(code, out)| result(*code, out))
        .collect();
    printed(id, &texts)
}

/// An `exec_command` result.
fn result(code: i64, out: &Value) -> Value {
    json!({"chunk_id": "k", "exit_code": code, "output": out})
}

/// An `exec` call's output: the status line, then each of `texts` printed as JSON.
fn printed(id: &str, texts: &[Value]) -> String {
    let head = json!({"type": "input_text", "text": "Script completed\nWall time: 0.5 seconds\nOutput:\n"});
    let items: Vec<Value> = std::iter::once(head)
        .chain(
            texts
                .iter()
                .map(|t| json!({"type": "input_text", "text": t.to_string()})),
        )
        .collect();
    json!({"type": "response_item", "payload": {"type": "custom_tool_call_output", "call_id": id, "output": items}})
        .to_string()
}

/// A printed `Promise.allSettled` entry holding `value`.
fn settled(value: Value) -> Value {
    json!({"status": "fulfilled", "value": value})
}

fn compacted() -> String {
    json!({"type": "compacted", "payload": {"message": "", "replacement_history": []}}).to_string()
}

fn in_window(lines: &[String]) -> Vec<(bool, String)> {
    lines.iter().map(|l| (true, l.clone())).collect()
}

/// Report options that keep every row.
const OPTS: Opts = Opts {
    limit: 100,
    paths: false,
    home: None,
};

/// A section's summary value under `key`, or null.
fn summary(d: &dyn Detector, key: &str) -> Value {
    d.section(&OPTS)
        .summary
        .into_iter()
        .find(|(k, _)| k == key)
        .map_or(Value::Null, |(_, v)| v)
}

/// The values under `columns` in the first row of a section's first table.
fn first_row(d: &dyn Detector, columns: &[&str]) -> Vec<Value> {
    let table = d.section(&OPTS).tables.swap_remove(0);
    let row = table.rows.first().expect("a row");
    columns
        .iter()
        .map(|c| {
            let i = table.columns.iter().position(|k| k == c).expect("column");
            row[i].clone()
        })
        .collect()
}

/// Polls' `(polls, polls_after_failure, poll_chars)` and outputs not paired.
fn polls(lines: &[(bool, String)]) -> (Vec<Value>, Value) {
    let mut d = Polls::default();
    replay(&mut d, lines);
    (
        first_row(&d, &["polls", "polls_after_failure", "poll_chars"]),
        summary(&d, "codex_outputs_not_paired"),
    )
}

fn counts(polls: u64, after_failure: u64, chars: u64) -> Vec<Value> {
    vec![json!(polls), json!(after_failure), json!(chars)]
}

/// Polls' outputs and commands not paired.
fn not_paired(lines: &[(bool, String)]) -> (Value, Value) {
    let mut d = Polls::default();
    replay(&mut d, lines);
    (
        summary(&d, "codex_outputs_not_paired"),
        summary(&d, "codex_commands_not_paired"),
    )
}

#[test]
fn results_printed_inside_lists_and_objects_pair_in_order() {
    let status_diff = commands(&["git status", "git diff"]);
    let lines = in_window(&[
        // `{i, status, value}` per command, as a cell numbering allSettled entries
        // prints them; the tool result between them is not a command's.
        exec("a", &status_diff),
        printed(
            "a",
            &[
                json!({"i": 0, "status": "fulfilled", "value": result(1, &json!("e"))}),
                json!({"i": 1, "status": "fulfilled", "value": {"content": [], "isError": false}}),
                json!({"i": 2, "status": "fulfilled", "value": result(0, &json!("d"))}),
            ],
        ),
        // `{i, result: {status, value}}`: git diff passed, so its poll does not
        // follow a failure.
        exec("b", &commands(&["git diff"])),
        printed(
            "b",
            &[json!({"i": 0, "result": settled(result(0, &json!("ab")))})],
        ),
        // A list of `{status, value}`: git status failed, so its poll does.
        exec("c", &status_diff),
        printed(
            "c",
            &[json!([
                settled(result(0, &json!("cde"))),
                settled(result(0, &json!("f")))
            ])],
        ),
    ]);
    assert_eq!(polls(&lines), (counts(3, 1, 6), json!(0)));
    assert_eq!(not_paired(&lines), (json!(0), json!(0)));
}

#[test]
fn results_under_2_keys_of_an_object_pair_with_neither_command() {
    // A parsed object does not keep its keys' order.
    let lines = in_window(&[
        exec("a", &commands(&["git status", "git diff"])),
        printed(
            "a",
            &[json!({"status": result(0, &json!("s")), "diff": result(1, &json!("d"))})],
        ),
    ]);
    assert_eq!(not_paired(&lines), (json!(1), json!(2)));
}

#[test]
fn a_result_still_running_or_printed_in_part_is_not_one() {
    let lines = in_window(&[
        exec("a", &commands(&["cargo build"])),
        printed(
            "a",
            &[json!({"chunk_id": "k", "session_id": 7, "output": "Compiling"})],
        ),
        exec("b", &commands(&["git status"])),
        printed("b", &[json!("clean")]),
    ]);
    assert_eq!(not_paired(&lines), (json!(2), json!(2)));
}

/// A cell running git status then git diff whose `texts` print git diff's failure
/// first, then a passing poll of git status: it must not follow a failure.
fn printed_out_of_order(texts: &[Value]) -> Vec<(bool, String)> {
    in_window(&[
        exec("a", &commands(&["git status", "git diff"])),
        printed("a", texts),
        exec("b", &commands(&["git status"])),
        output("b", &[(0, json!("ok"))]),
    ])
}

#[test]
fn entries_numbered_out_of_order_pair_with_neither_command() {
    let entry = |i: u64, code: i64| json!({"i": i, "status": "fulfilled", "value": result(code, &json!("x"))});
    for texts in [
        vec![json!([entry(1, 1), entry(0, 0)])],
        vec![
            entry(1, 1),
            json!({"i": 0, "result": settled(result(0, &json!("x")))}),
        ],
    ] {
        let lines = printed_out_of_order(&texts);
        assert_eq!(polls(&lines), (counts(1, 0, 2), json!(1)));
    }
    // Numbers that rise, with gaps for entries holding no result, keep their order.
    let lines = in_window(&[
        exec("a", &commands(&["git status", "git diff"])),
        printed(
            "a",
            &[
                entry(0, 1),
                json!({"i": 1, "status": "rejected"}),
                entry(2, 0),
            ],
        ),
        exec("b", &commands(&["git status"])),
        output("b", &[(0, json!("ok"))]),
    ]);
    assert_eq!(polls(&lines), (counts(1, 1, 2), json!(0)));
}

#[test]
fn a_cell_that_printed_a_result_still_running_pairs_with_neither_command() {
    // git status was still running when git diff failed; the last result, which
    // write_stdin printed when git status ended, is git status's.
    let running = json!({"chunk_id": "k", "session_id": 3, "output": "x"});
    let lines = printed_out_of_order(&[
        json!([running, result(1, &json!("x"))]),
        result(0, &json!("x")),
    ]);
    assert_eq!(polls(&lines), (counts(1, 0, 2), json!(1)));
}

#[test]
fn commands_not_paired_counts_each_command_of_each_cell_in_the_window() {
    let lines = [
        (true, exec("a", &commands(&["git status", "git diff"]))),
        (true, output("a", &[(0, json!("ok"))])),
        (true, exec("", &commands(&["git log", "git diff", "ls"]))),
        (true, exec("b", &commands(&["git status"]))),
        (true, output("b", &[(0, json!("ok"))])),
        (false, exec("c", &commands(&["git status"]))),
    ];
    assert_eq!(not_paired(&lines), (json!(2), json!(5)));
}

#[test]
fn a_null_output_is_an_empty_result_with_its_exit_code() {
    let git = commands(&["git status"]);
    let lines = in_window(&[
        exec("a", &git),
        output("a", &[(1, Value::Null)]),
        exec("b", &git),
        output("b", &[(0, json!("ok"))]),
    ]);
    assert_eq!(polls(&lines), (counts(1, 1, 2), json!(0)));

    let mut d = Pipes::default();
    let test = commands(&["cargo test 2>&1 | tail -1"]);
    replay(
        &mut d,
        &in_window(&[exec("c", &test), output("c", &[(0, Value::Null)])]),
    );
    assert_eq!(summary(&d, "unguarded_results_found"), 1);
    assert_eq!(summary(&d, "showed_no_pass_or_fail_line"), 1);
    assert_eq!(summary(&d, "codex_outputs_not_paired"), 0);
}

#[test]
fn a_null_result_beside_a_valid_one_leaves_one_command_unpaired() {
    let lines = in_window(&[
        exec("a", &commands(&["git status"])),
        output("a", &[(1, Value::Null), (0, json!("ok"))]),
    ]);
    let mut d = Polls::default();
    replay(&mut d, &lines);
    assert_eq!(summary(&d, "codex_outputs_not_paired"), 1);
}

#[test]
fn a_call_without_an_id_is_unpaired_and_its_result_unknown() {
    let git = commands(&["git status"]);
    let lines = in_window(&[
        exec("a", &git),
        output("a", &[(1, json!("e"))]),
        exec("", &git),
        exec("c", &git),
        output("c", &[(0, json!("ok"))]),
    ]);
    // Both repeats are polls. The first follows the failure; the second follows a
    // run whose result is unknown, so it does not.
    assert_eq!(polls(&lines), (counts(2, 1, 2), json!(1)));

    let mut d = Pipes::default();
    let test = commands(&["cargo test 2>&1 | tail -1"]);
    replay(&mut d, &in_window(&[exec("", &test)]));
    assert_eq!(summary(&d, "unguarded"), 1);
    assert_eq!(summary(&d, "codex_outputs_not_paired"), 1);

    let mut d = Rereads::default();
    let read = "const t = readFile('/work/y.md'); text(t);";
    replay(
        &mut d,
        &in_window(&[
            exec("a", read),
            output("a", &[]),
            compacted(),
            exec("", read),
        ]),
    );
    assert_eq!(first_row(&d, &["rereads"]), vec![json!(1)]);
    assert_eq!(summary(&d, "codex_outputs_not_paired"), 1);
}

#[test]
fn an_output_that_never_comes_is_unpaired() {
    let lines = in_window(&[exec("a", &commands(&["git status"]))]);
    assert_eq!(polls(&lines).1, 1);

    let mut d = Pipes::default();
    let test = commands(&["cargo test 2>&1 | tail -1"]);
    replay(&mut d, &in_window(&[exec("b", &test)]));
    assert_eq!(summary(&d, "codex_outputs_not_paired"), 1);
}

#[test]
fn a_call_id_carried_by_2_waiting_calls_pairs_with_neither() {
    // Both calls are issued before either output, so which output is whose is unknown.
    let git = commands(&["git status"]);
    let lines = in_window(&[
        exec("a", &git),
        exec("a", &git),
        output("a", &[(0, json!("ok"))]),
        output("a", &[(1, json!("failed"))]),
    ]);
    assert_eq!(polls(&lines), (counts(1, 0, 0), json!(2)));

    let mut d = Pipes::default();
    let test = commands(&["cargo test 2>&1 | tail -1"]);
    replay(
        &mut d,
        &in_window(&[
            exec("a", &test),
            exec("a", &test),
            output("a", &[(0, json!("x"))]),
            output("a", &[(0, json!("y"))]),
        ]),
    );
    assert_eq!(summary(&d, "unguarded"), 2);
    assert_eq!(summary(&d, "unguarded_results_found"), 0);
    assert_eq!(summary(&d, "codex_outputs_not_paired"), 2);

    let mut d = Rereads::default();
    let read = |path: &str| format!("const t = readFile('{path}'); text(t);");
    replay(
        &mut d,
        &in_window(&[
            exec("r", &read("/work/y.md")),
            exec("s", &read("/work/z.md")),
            compacted(),
            exec("a", &read("/work/y.md")),
            exec("a", &read("/work/z.md")),
            output("a", &[(0, json!("y"))]),
            output("a", &[(0, json!("z"))]),
        ]),
    );
    assert_eq!(
        first_row(&d, &["rereads", "reread_chars"]),
        vec![json!(2), json!(0)]
    );
    assert_eq!(summary(&d, "codex_outputs_not_paired"), 2);
}

#[test]
fn a_call_id_reused_after_its_output_is_not_paired_again() {
    let git = commands(&["git status"]);
    let lines = in_window(&[
        exec("a", &git),
        output("a", &[(1, json!("e"))]),
        exec("a", &git),
        output("a", &[(0, json!("ok"))]),
        exec("b", &git),
        output("b", &[(0, json!("ok"))]),
    ]);
    // The second run follows the failure, but its result is not read, so the third
    // follows a run whose result is unknown.
    assert_eq!(polls(&lines), (counts(2, 1, 2), json!(1)));
}

#[test]
fn a_run_whose_output_never_comes_hides_an_earlier_failure() {
    let git = commands(&["git status"]);
    // Before the window: a failure, then a repeat whose output never comes.
    let lines = [
        (false, exec("a", &git)),
        (false, output("a", &[(1, json!("e"))])),
        (false, exec("b", &git)),
        (true, exec("c", &git)),
        (true, output("c", &[(0, json!("ok"))])),
    ];
    assert_eq!(polls(&lines), (counts(1, 0, 2), json!(0)));

    // In the window: the repeat follows the failure, but the next poll does not.
    let lines = in_window(&[
        exec("a", &git),
        output("a", &[(1, json!("e"))]),
        exec("b", &git),
        exec("c", &git),
        output("c", &[(0, json!("ok"))]),
    ]);
    assert_eq!(polls(&lines), (counts(2, 1, 2), json!(1)));
}

#[test]
fn a_result_read_after_a_later_run_passed_sets_no_failure() {
    let git = commands(&["git status"]);
    let lines = in_window(&[
        exec("a", &git),
        exec("b", &git),
        output("b", &[(0, json!("ok"))]),
        output("a", &[(1, json!("e"))]),
        exec("c", &git),
        output("c", &[(0, json!("ok"))]),
    ]);
    // b, the latest run before c, passed; a's failure came later but is older.
    assert_eq!(polls(&lines), (counts(2, 0, 4), json!(0)));
}

#[test]
fn commands_of_one_cell_do_not_follow_each_others_results() {
    // The cell's code is written before any result, as parallel Claude calls are.
    let lines = in_window(&[
        exec("a", &commands(&["git status", "git status"])),
        output("a", &[(1, json!("e")), (0, json!("ok"))]),
    ]);
    assert_eq!(polls(&lines), (counts(1, 0, 2), json!(0)));
}

#[test]
fn an_unpaired_run_before_the_window_clears_the_failure() {
    let git = commands(&["git status"]);
    let lines = [
        (false, exec("a", &git)),
        (false, output("a", &[(1, json!("e"))])),
        (false, exec("b", &git)),
        (false, output("b", &[])),
        (true, exec("c", &git)),
        (true, output("c", &[(0, json!("ok"))])),
    ];
    // Only the outputs in the window count as not paired.
    assert_eq!(polls(&lines), (counts(1, 0, 2), json!(0)));
}

#[test]
fn a_blank_command_counts_as_a_command_and_as_not_paired() {
    let lines = in_window(&[
        exec("", &commands(&["", " "])),
        exec("", &commands(&["git status", "  "])),
    ]);
    let mut d = Polls::default();
    replay(&mut d, &lines);
    assert_eq!(
        first_row(&d, &["commands", "polls"]),
        vec![json!(4), json!(0)]
    );
    assert_eq!(summary(&d, "codex_commands_not_paired"), 4);
}
