//! The binary, run as a user runs it.
//!
//! These assert on the exact text the commands print, because the README quotes that text
//! and a README whose transcripts drift is a README nobody trusts twice.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn example() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/sales.toml")
}

fn dagpane(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dagpane"))
        .args(args)
        .output()
        .expect("the binary was built by the test harness")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn check_accepts_the_bundled_example() {
    let out = dagpane(&["check", example().to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    assert!(text.contains("Sales explorer — 11 cells"), "{text}");
    assert!(text.ends_with("  ok\n"), "{text}");
}

#[test]
fn check_reports_a_broken_app_and_exits_non_zero() {
    // Target-scoped AND process-unique: `CARGO_TARGET_TMPDIR` alone is shared by every
    // process in a run, so two concurrent invocations would race on the same broken.toml.
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("broken-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("broken.toml");
    std::fs::write(
        &path,
        "[app]\ntitle = \"t\"\n[[cell]]\nname = \"a\"\nfrom = \"ghost\"\n",
    )
    .unwrap();

    let out = dagpane(&["check", path.to_str().unwrap()]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("`ghost`"), "{err}");
}

#[test]
fn explain_prints_the_interaction_and_names_what_was_never_looked_at() {
    let out = dagpane(&[
        "explain",
        example().to_str().unwrap(),
        "--set",
        "min_amount=400",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);

    assert!(text.contains("looked at 8 of 11 cells"), "{text}");
    assert!(text.contains("reused   channel_count"), "{text}");
    assert!(text.contains("same value — nothing below it ran"), "{text}");
    assert!(
        text.contains("3 cell(s) never looked at: sales, region, all_time_revenue"),
        "{text}"
    );
    assert!(
        text.contains("patch: 3 of 7 panes — revenue, order_count, region_totals"),
        "{text}"
    );
    assert!(
        !text.lines().any(|l| l.ends_with(' ')),
        "no line may end in whitespace; a README quoting this would carry it"
    );
}

#[test]
fn explain_with_nothing_set_says_so_rather_than_printing_an_empty_pass() {
    let out = dagpane(&["explain", example().to_str().unwrap()]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("nothing was set"), "{}", stdout(&out));
}

#[test]
fn explain_reports_a_no_op_interaction_as_a_no_op() {
    // The dropdown already reads "all". Setting it there again must not report a pass.
    let out = dagpane(&[
        "explain",
        example().to_str().unwrap(),
        "--set",
        "region=all",
    ]);
    let text = stdout(&out);
    assert!(text.contains("nothing changed"), "{text}");
}

#[test]
fn explain_sets_several_inputs_in_one_pass() {
    let out = dagpane(&[
        "explain",
        example().to_str().unwrap(),
        "--set",
        "min_amount=100",
        "--set",
        "region=north",
    ]);
    let text = stdout(&out);
    assert!(
        text.contains("set min_amount = 100, region = north"),
        "{text}"
    );
    assert!(text.contains("epoch 2"), "one pass, not two:\n{text}");
}

#[test]
fn explain_rejects_a_value_a_widget_could_not_produce() {
    let out = dagpane(&[
        "explain",
        example().to_str().unwrap(),
        "--set",
        "min_amount=99999",
    ]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("outside its range"), "{err}");
}

#[test]
fn explain_json_carries_the_same_numbers_as_the_text() {
    let out = dagpane(&[
        "explain",
        example().to_str().unwrap(),
        "--set",
        "min_amount=400",
        "--json",
    ]);
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    let trace = &json["interaction"]["trace"];
    assert_eq!(trace["total_cells"], 11);
    assert_eq!(trace["steps"].as_array().unwrap().len(), 8);
    assert_eq!(
        json["interaction"]["panes_sent"].as_array().unwrap().len(),
        3
    );
}

#[test]
fn graph_prints_every_cell_in_height_order() {
    let out = dagpane(&["graph", example().to_str().unwrap()]);
    let text = stdout(&out);
    let heights: Vec<u32> = text
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter_map(|h| h.parse().ok())
        .collect();
    assert_eq!(heights.len(), 11);
    assert!(
        heights.windows(2).all(|w| w[0] <= w[1]),
        "heights are not ascending: {heights:?}"
    );
}

#[test]
fn graph_mermaid_is_a_flowchart_with_one_edge_per_declared_input() {
    let out = dagpane(&["graph", example().to_str().unwrap(), "--format", "mermaid"]);
    let text = stdout(&out);
    assert!(text.starts_with("flowchart TD\n"), "{text}");
    // Node ids are synthetic and positional; the author's name only ever appears inside a
    // quoted label, so a cell called `a"]; b[[evil` cannot reshape the diagram.
    assert!(text.contains("c0[(\"sales\")]"), "{text}");
    assert!(text.contains("c1([\"min_amount\"])"), "{text}");
    assert!(text.contains("c3[\"filtered\"]"), "{text}");
    assert!(
        text.contains("c0 --> c3") && text.contains("c1 --> c3"),
        "{text}"
    );
    assert_eq!(
        text.matches(" --> ").count(),
        10,
        "one arrow per declared edge:\n{text}"
    );
}

#[test]
fn graph_json_describes_the_structure_a_tool_would_want() {
    let out = dagpane(&["graph", example().to_str().unwrap(), "--format", "json"]);
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    let cells = json["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 11);
    let filtered = cells.iter().find(|c| c["name"] == "filtered").unwrap();
    assert_eq!(filtered["kind"], "cell");
    assert_eq!(filtered["inputs"].as_array().unwrap().len(), 3);
}
