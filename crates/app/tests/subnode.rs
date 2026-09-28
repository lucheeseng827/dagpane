//! Sub-node invalidation, end to end: one column of a wide frame moves, and only the cells
//! that read that column wake up.
//!
//! `crates/core/tests/oracle.rs` proves the *engine* honours a column key. This file proves
//! the other half — that the column key a compiled pipeline reports is the truth about what
//! it read. The two failures are different and only one of them is loud: an engine that
//! compares the wrong thing shows up everywhere, and an analysis that claims too little
//! shows up as one stale pane on one app, months later.
//!
//! So the load-bearing test here is `the_analysis_is_never_narrower_than_the_truth`, which
//! does not assert a count at all. It changes one column, then checks every cell against a
//! session that recomputed from scratch. A provenance rule that forgot an input fails it.

use std::sync::Arc;

use dagpane_app::{compile, manifest, App};
use dagpane_core::frame::Frame;
use dagpane_core::trace::Trace;
use dagpane_core::value::{Column, Table};
use dagpane_core::{Session, Value};

/// Columns in the generated source. The number in the claim.
const WIDTH: usize = 200;
const ROWS: usize = 40;
/// Metric cells, each reading one column. Paired with one derived cell each, so the app has
/// forty computed cells over a two-hundred-column frame.
const METRICS: usize = 20;
/// Which source column metric `i` reads. Spread out so most columns are read by nobody,
/// which is the situation a wide frame actually puts an app in.
fn column_of(metric: usize) -> usize {
    metric * 10
}

fn wide_csv() -> String {
    let mut out = String::new();
    let header: Vec<String> = (0..WIDTH).map(|c| format!("c{c:03}")).collect();
    out.push_str(&header.join(","));
    out.push('\n');
    for r in 0..ROWS {
        let row: Vec<String> = (0..WIDTH)
            .map(|c| ((c * 7 + r * 13) % 97).to_string())
            .collect();
        out.push_str(&row.join(","));
        out.push('\n');
    }
    out
}

/// Forty cells over one wide source: twenty metrics that each total a single column, and
/// twenty that scale that column by its own metric.
fn wide_manifest() -> String {
    let mut m = String::from(
        "[app]\ntitle = \"Wide\"\n\n[[source]]\nname = \"wide\"\ncsv = \"wide.csv\"\n",
    );
    for i in 0..METRICS {
        let c = format!("c{:03}", column_of(i));
        m.push_str(&format!(
            "\n[[cell]]\nname = \"m_{i}\"\nfrom = \"wide\"\n\
             [[cell.step]]\ngroup_by = {{ agg = [{{ column = \"{c}\", agg = \"sum\", as = \"t\" }}] }}\n\
             [[cell.step]]\nscalar = {{ column = \"t\" }}\n"
        ));
        m.push_str(&format!(
            "\n[[cell]]\nname = \"d_{i}\"\nfrom = \"wide\"\n\
             [[cell.step]]\nderive = {{ name = \"scaled\", expr = \"{c} * $m_{i}\" }}\n\
             [[cell.step]]\ngroup_by = {{ agg = [{{ column = \"scaled\", agg = \"sum\", as = \"t\" }}] }}\n\
             [[cell.step]]\nscalar = {{ column = \"t\" }}\n"
        ));
    }
    m
}

fn wide_app() -> (tempfile::TempDir, Arc<App>) {
    let dir = tempfile::tempdir().expect("a temp dir");
    std::fs::write(dir.path().join("wide.csv"), wide_csv()).expect("write the csv");
    let parsed = manifest::parse(&wide_manifest()).expect("the generated manifest parses");
    let app = compile(&parsed, dir.path()).expect("the generated manifest compiles");
    (dir, Arc::new(app))
}

/// The frame a source holds, with one column's values moved and every other byte identical.
fn with_column_bumped(frame: &dyn Frame, col: usize) -> Value {
    with_columns_bumped(frame, &[col])
}

/// The same, for a set of columns.
fn with_columns_bumped(frame: &dyn Frame, cols: &[usize]) -> Value {
    let schema = frame.schema();
    let columns: Vec<Column> = schema
        .iter()
        .enumerate()
        .map(|(c, (name, _))| {
            let values: Vec<Option<i64>> = (0..frame.rows())
                .map(|r| {
                    frame
                        .value_at(r, c)
                        .as_int()
                        .map(|v| if cols.contains(&c) { v + 1 } else { v })
                })
                .collect();
            Column::int(name.clone(), values)
        })
        .collect();
    Value::table(Table::new(columns).expect("one row count throughout"))
}

/// The frame with one row of one column nudged by one, and every other byte identical.
///
/// A single-cell edit: one metric updated, which is what a refreshed extract mostly is. Used
/// to measure how often a ranking survives one, which is the question an ordering constraint
/// lives or dies by.
fn with_cell_bumped(frame: &dyn Frame, col: usize, row: usize) -> Value {
    let schema = frame.schema();
    let columns: Vec<Column> = schema
        .iter()
        .enumerate()
        .map(|(c, (name, _))| {
            let values: Vec<Option<i64>> = (0..frame.rows())
                .map(|r| {
                    frame
                        .value_at(r, c)
                        .as_int()
                        .map(|v| if c == col && r == row { v + 1 } else { v })
                })
                .collect();
            Column::int(name.clone(), values)
        })
        .collect();
    Value::table(Table::new(columns).expect("one row count throughout"))
}

/// The frame with one column's values all replaced by `to` — used to move a predicate's
/// *selection*, which nudging by one deliberately does not.
fn with_column_set(frame: &dyn Frame, col: usize, to: i64) -> Value {
    let schema = frame.schema();
    let columns: Vec<Column> = schema
        .iter()
        .enumerate()
        .map(|(c, (name, _))| {
            let values: Vec<Option<i64>> = (0..frame.rows())
                .map(|r| {
                    if c == col {
                        Some(to)
                    } else {
                        frame.value_at(r, c).as_int()
                    }
                })
                .collect();
            Column::int(name.clone(), values)
        })
        .collect();
    Value::table(Table::new(columns).expect("one row count throughout"))
}

/// A settled session over the app, and the frame its source holds.
fn settled(app: &Arc<App>) -> (Session, Value) {
    let mut s = Session::new(Arc::clone(&app.graph));
    s.refresh();
    let loaded = s
        .get("wide")
        .expect("the source cell")
        .value()
        .expect("a source is never in error")
        .clone();
    (s, loaded)
}

fn bump(session: &mut Session, loaded: &Value, col: usize) -> Trace {
    let frame = loaded.as_frame().expect("the source holds a frame");
    session
        .set("wide", with_column_bumped(frame, col))
        .expect("a frame into a frame source");
    session.commit()
}

#[test]
fn a_change_to_one_column_of_a_wide_frame_recomputes_only_its_readers() {
    // The shape of the sentence `ROADMAP.md` §3 gates the whole claim on.
    let (_dir, app) = wide_app();
    let (mut live, loaded) = settled(&app);

    assert_eq!(
        app.graph.len(),
        1 + METRICS * 2,
        "one source and forty cells"
    );

    let trace = bump(&mut live, &loaded, column_of(0));
    let ran = trace.names_evaluated();

    assert_eq!(
        ran,
        vec!["m_0", "d_0"],
        "changing column {} of a {WIDTH}-column frame should reach exactly the two cells \
         that read it, not {ran:?}",
        column_of(0)
    );
    assert_eq!(
        trace.evaluated(),
        2,
        "changed one column of a {WIDTH}-column frame; recomputed {} of {} cells",
        trace.evaluated(),
        METRICS * 2
    );
}

#[test]
fn a_column_nobody_reads_recomputes_nothing_at_all() {
    // The stronger half, and the one whole-value granularity can never do: the pass reaches
    // every cell structurally — they all declare an edge to `wide` — and every one of them
    // reuses, because none of their column keys moved.
    let (_dir, app) = wide_app();
    let (mut live, loaded) = settled(&app);

    // Column 3 sits between two that are read and is read by nobody.
    let trace = bump(&mut live, &loaded, 3);

    assert_eq!(
        trace.evaluated(),
        0,
        "a column no cell reads woke {:?}",
        trace.names_evaluated()
    );
    assert_eq!(
        trace.reused(),
        METRICS * 2,
        "every cell should have been visited and reused"
    );
}

#[test]
fn the_analysis_is_never_narrower_than_the_truth() {
    // The assertion that catches a wrong provenance rule. No counts: every cell is compared
    // against a session that computed it from scratch, for every column in the frame.
    let (_dir, app) = wide_app();
    let (mut live, loaded) = settled(&app);
    let names: Vec<String> = (0..METRICS)
        .flat_map(|i| [format!("m_{i}"), format!("d_{i}")])
        .collect();

    // Bumps accumulate, so each step moves exactly one column relative to the step before
    // it while every frame is still built from the one the source actually loaded.
    let mut moved: Vec<usize> = Vec::new();
    for col in 0..WIDTH {
        moved.push(col);
        let next = with_columns_bumped(loaded.as_frame().expect("a frame"), &moved);
        live.set("wide", next.clone()).expect("a frame");
        let trace = live.commit();

        let mut truth = Session::new(Arc::clone(&app.graph));
        truth.set("wide", next).expect("a frame");
        truth.refresh();

        for name in &names {
            assert_eq!(
                live.get(name).unwrap(),
                truth.get(name).unwrap(),
                "after moving column {col}, `{name}` disagrees with a full recompute\n  \
                 trace: {}",
                trace.summary()
            );
        }
    }
}

#[test]
fn count_depends_on_the_rows_and_on_no_column_of_data() {
    // `count` is the verb with the most to gain: it reads no value at all, so over a wide
    // frame it should sleep through every column that does not decide which rows exist.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("wide.csv"), wide_csv()).unwrap();
    let src = "[app]\ntitle = \"C\"\n\n[[source]]\nname = \"wide\"\ncsv = \"wide.csv\"\n\n\
               [[cell]]\nname = \"n\"\nfrom = \"wide\"\n[[cell.step]]\ncount = true\n\n\
               [[cell]]\nname = \"big\"\nfrom = \"wide\"\n\
               [[cell.step]]\nfilter = { column = \"c000\", op = \"ge\", value = 50 }\n\
               [[cell.step]]\ncount = true\n";
    let app = Arc::new(compile(&manifest::parse(src).unwrap(), dir.path()).unwrap());

    // A column neither cell mentions: both counts sleep.
    let (mut live, loaded) = settled(&app);
    let trace = bump(&mut live, &loaded, 7);
    assert_eq!(
        trace.evaluated(),
        0,
        "a count woke for a column it does not read: {:?}",
        trace.names_evaluated()
    );

    // The filter's own column, nudged by one. No value in `c000` sits on the boundary, so
    // the *same rows* still clear the floor — and `big` counts rows, not values. This is the
    // predicate constraint earning its keep: the column it filters on genuinely moved and the
    // cell still cannot have a different answer.
    let (mut live, loaded) = settled(&app);
    let before = live.get("big").unwrap().clone();
    let trace = bump(&mut live, &loaded, 0);
    assert_eq!(
        trace.evaluated(),
        0,
        "the filter column moved but its selection did not, so nothing should have run: {:?}",
        trace.names_evaluated()
    );
    // And the value it kept is the value a from-scratch run gets, which is the half that
    // matters: a constraint that skips a recompute it owed is the failure this scheme has to
    // be arranged not to have.
    let mut truth = Session::new(Arc::clone(&app.graph));
    truth
        .set("wide", with_column_bumped(loaded.as_frame().unwrap(), 0))
        .unwrap();
    truth.refresh();
    assert_eq!(live.get("big").unwrap(), truth.get("big").unwrap());
    assert_eq!(live.get("big").unwrap(), &before);

    // Now move the same column so the selection *does* change — every value below the floor.
    // The constraint must not hold, and `big` must run.
    let (mut live, loaded) = settled(&app);
    live.set("wide", with_column_set(loaded.as_frame().unwrap(), 0, 1))
        .unwrap();
    let trace = live.commit();
    assert_eq!(
        trace.names_evaluated(),
        vec!["big"],
        "emptying the filter column must wake the cell that filters on it"
    );
    assert_eq!(live.get("big").unwrap().value().unwrap().as_int(), Some(0));
}

#[test]
fn a_filters_column_is_a_constraint_when_its_values_do_not_reach_the_output() {
    // The rule this replaced was "a filter's column is read even though it changes no value",
    // and it was right until predicate constraints existed. What a cell downstream of a
    // filter depends on is *which rows survived*, and only that — so a change to the filter
    // column that picks the same rows cannot move the answer.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("wide.csv"), wide_csv()).unwrap();
    let src = "[app]\ntitle = \"F\"\n\n[[source]]\nname = \"wide\"\ncsv = \"wide.csv\"\n\n\
               [[cell]]\nname = \"picked\"\nfrom = \"wide\"\n\
               [[cell.step]]\nfilter = { column = \"c010\", op = \"ge\", value = 50 }\n\
               [[cell.step]]\nselect = [\"c020\"]\n\
               [[cell.step]]\ngroup_by = { agg = [{ column = \"c020\", agg = \"sum\", as = \"t\" }] }\n\
               [[cell.step]]\nscalar = { column = \"t\" }\n";
    let app = Arc::new(compile(&manifest::parse(src).unwrap(), dir.path()).unwrap());

    // The displayed column: a full dependency, as it always was.
    let (mut live, loaded) = settled(&app);
    assert_eq!(
        bump(&mut live, &loaded, 20).names_evaluated(),
        vec!["picked"]
    );

    // A column that is neither.
    let (mut live, loaded) = settled(&app);
    assert!(bump(&mut live, &loaded, 30).names_evaluated().is_empty());

    // The filter's column, moved without moving its selection: now a constraint, so nothing
    // runs — and the value kept is still the right one.
    let (mut live, loaded) = settled(&app);
    let trace = bump(&mut live, &loaded, 10);
    assert!(
        trace.names_evaluated().is_empty(),
        "the selection did not move, so `picked` cannot have: {:?}",
        trace.names_evaluated()
    );
    let mut truth = Session::new(Arc::clone(&app.graph));
    truth
        .set("wide", with_column_bumped(loaded.as_frame().unwrap(), 10))
        .unwrap();
    truth.refresh();
    assert_eq!(live.get("picked").unwrap(), truth.get("picked").unwrap());

    // The filter's column, moved so the selection empties. The cell must run.
    let (mut live, loaded) = settled(&app);
    live.set("wide", with_column_set(loaded.as_frame().unwrap(), 10, 0))
        .unwrap();
    let trace = live.commit();
    assert_eq!(trace.names_evaluated(), vec!["picked"]);
}

#[test]
fn a_filter_whose_column_is_shown_keeps_its_full_dependency() {
    // The constraint is only sound when the filter column's *values* never reach the output.
    // Here they do — the cell totals the very column it filters on — so nudging it has to
    // wake the cell even though the same rows survive.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("wide.csv"), wide_csv()).unwrap();
    let src = "[app]\ntitle = \"S\"\n\n[[source]]\nname = \"wide\"\ncsv = \"wide.csv\"\n\n\
               [[cell]]\nname = \"total\"\nfrom = \"wide\"\n\
               [[cell.step]]\nfilter = { column = \"c000\", op = \"ge\", value = 50 }\n\
               [[cell.step]]\ngroup_by = { agg = [{ column = \"c000\", agg = \"sum\", as = \"t\" }] }\n\
               [[cell.step]]\nscalar = { column = \"t\" }\n";
    let app = Arc::new(compile(&manifest::parse(src).unwrap(), dir.path()).unwrap());
    let (mut live, loaded) = settled(&app);
    let trace = bump(&mut live, &loaded, 0);
    assert_eq!(
        trace.names_evaluated(),
        vec!["total"],
        "the filter column is also the summed column, so its values are a real dependency"
    );

    let mut truth = Session::new(Arc::clone(&app.graph));
    truth
        .set("wide", with_column_bumped(loaded.as_frame().unwrap(), 0))
        .unwrap();
    truth.refresh();
    assert_eq!(live.get("total").unwrap(), truth.get("total").unwrap());
}

#[test]
fn a_sorts_column_is_a_constraint_when_its_values_do_not_reach_the_output() {
    // The same bargain a filter strikes, for the other verb that decides rows without
    // contributing a value. A top-five pane depends on *what order the rows ended up in* and
    // on the column it totals — not on the values that produced the order. Move every value
    // in the ranking column by the same amount and the ranking is identical, so the pane is
    // identical, so it must not run.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("wide.csv"), wide_csv()).unwrap();
    let src = "[app]\ntitle = \"O\"\n\n[[source]]\nname = \"wide\"\ncsv = \"wide.csv\"\n\n\
               [[cell]]\nname = \"top\"\nfrom = \"wide\"\n\
               [[cell.step]]\nsort = { column = \"c010\", descending = true }\n\
               [[cell.step]]\nlimit = 5\n\
               [[cell.step]]\ngroup_by = { agg = [{ column = \"c020\", agg = \"sum\", as = \"t\" }] }\n\
               [[cell.step]]\nscalar = { column = \"t\" }\n";
    let app = Arc::new(compile(&manifest::parse(src).unwrap(), dir.path()).unwrap());

    // The totalled column: a full dependency, as it always was.
    let (mut live, loaded) = settled(&app);
    assert_eq!(bump(&mut live, &loaded, 20).names_evaluated(), vec!["top"]);

    // A column that is neither.
    let (mut live, loaded) = settled(&app);
    assert!(bump(&mut live, &loaded, 30).names_evaluated().is_empty());

    // The ranking column, every value of it moved by one: the ranking cannot have changed,
    // so nothing runs — and the value kept is still the right one.
    let (mut live, loaded) = settled(&app);
    let trace = bump(&mut live, &loaded, 10);
    assert!(
        trace.names_evaluated().is_empty(),
        "the ordering did not move, so `top` cannot have: {:?}",
        trace.names_evaluated()
    );
    let mut truth = Session::new(Arc::clone(&app.graph));
    truth
        .set("wide", with_column_bumped(loaded.as_frame().unwrap(), 10))
        .unwrap();
    truth.refresh();
    assert_eq!(live.get("top").unwrap(), truth.get("top").unwrap());

    // The ranking column flattened to one value, which ranks the rows by row number instead.
    // The order moved, so the cell must run.
    let (mut live, loaded) = settled(&app);
    live.set("wide", with_column_set(loaded.as_frame().unwrap(), 10, 0))
        .unwrap();
    let trace = live.commit();
    assert_eq!(trace.names_evaluated(), vec!["top"]);
}

#[test]
fn a_sort_whose_column_is_shown_keeps_its_full_dependency() {
    // The constraint is only sound when the sorted column's *values* never reach the output.
    // Here they do — the cell totals the very column it ranks by — so nudging it has to wake
    // the cell even though the ranking is untouched.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("wide.csv"), wide_csv()).unwrap();
    let src = "[app]\ntitle = \"P\"\n\n[[source]]\nname = \"wide\"\ncsv = \"wide.csv\"\n\n\
               [[cell]]\nname = \"top\"\nfrom = \"wide\"\n\
               [[cell.step]]\nsort = { column = \"c000\", descending = true }\n\
               [[cell.step]]\nlimit = 5\n\
               [[cell.step]]\ngroup_by = { agg = [{ column = \"c000\", agg = \"sum\", as = \"t\" }] }\n\
               [[cell.step]]\nscalar = { column = \"t\" }\n";
    let app = Arc::new(compile(&manifest::parse(src).unwrap(), dir.path()).unwrap());
    let (mut live, loaded) = settled(&app);
    let trace = bump(&mut live, &loaded, 0);
    assert_eq!(
        trace.names_evaluated(),
        vec!["top"],
        "the ranking column is also the totalled column, so its values are a real dependency"
    );

    let mut truth = Session::new(Arc::clone(&app.graph));
    truth
        .set("wide", with_column_bumped(loaded.as_frame().unwrap(), 0))
        .unwrap();
    truth.refresh();
    assert_eq!(live.get("top").unwrap(), truth.get("top").unwrap());
}

#[test]
fn a_top_n_pane_mostly_sleeps_through_a_single_edit_to_its_ranking_column() {
    // How much the ordering constraint is worth, measured rather than asserted.
    //
    // The reason this was worth building is that a ranking is far more stable than the values
    // under it. Any order-preserving change leaves it exactly intact — a uniform shift, a
    // rescale, a re-baseline, a unit conversion — and a single value moving only disturbs it
    // if that value crosses a neighbour. Before this existed, every one of those recomputed
    // the pane.
    //
    // So: nudge one row of the ranking column at a time, all forty of them, and count. Each
    // edit is checked against a full recompute, so a constraint that held when it should not
    // have fails here rather than being counted as a win.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("wide.csv"), wide_csv()).unwrap();
    let src = "[app]\ntitle = \"N\"\n\n[[source]]\nname = \"wide\"\ncsv = \"wide.csv\"\n\n\
               [[cell]]\nname = \"top\"\nfrom = \"wide\"\n\
               [[cell.step]]\nsort = { column = \"c010\", descending = true }\n\
               [[cell.step]]\nlimit = 5\n\
               [[cell.step]]\ngroup_by = { agg = [{ column = \"c020\", agg = \"sum\", as = \"t\" }] }\n\
               [[cell.step]]\nscalar = { column = \"t\" }\n";
    let app = Arc::new(compile(&manifest::parse(src).unwrap(), dir.path()).unwrap());

    let mut slept = 0usize;
    for row in 0..ROWS {
        let (mut live, loaded) = settled(&app);
        let edit = with_cell_bumped(loaded.as_frame().unwrap(), 10, row);
        live.set("wide", edit.clone()).unwrap();
        let trace = live.commit();
        if trace.evaluated() == 0 {
            slept += 1;
        }

        let mut truth = Session::new(Arc::clone(&app.graph));
        truth.set("wide", edit).unwrap();
        truth.refresh();
        assert_eq!(
            live.get("top").unwrap(),
            truth.get("top").unwrap(),
            "row {row}: the pane disagrees with a full recompute\n  trace: {}",
            trace.summary()
        );
    }

    // 16 of 40 on this frame — a column of forty values spread over ninety-seven, where a
    // nudge of one crosses a neighbour rather often. The *order-preserving* edits score 40 of
    // 40, which is what `a_sorts_column_is_a_constraint_when_its_values_do_not_reach_the_output`
    // pins, and whole-value granularity scores 0 on both by construction.
    //
    // The bound is deliberately loose on both sides: what it is for is that the number is
    // neither 0 (the constraint never pays) nor 40 (the corpus cannot move a ranking at all
    // and the measurement is vacuous).
    assert!(
        (10..ROWS).contains(&slept),
        "a top-five pane slept through {slept} of {ROWS} single-row edits to its ranking \
         column; outside 10..{ROWS} this test is no longer measuring what it says"
    );
}

#[test]
fn a_cell_with_no_steps_is_still_compared_whole() {
    // An alias declares nothing about columns, so it keeps the whole-value comparison. That
    // is the conservative default doing its job, and it is worth pinning: if silence ever
    // started meaning "reads nothing", every alias in every app would go stale.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("wide.csv"), wide_csv()).unwrap();
    let src = "[app]\ntitle = \"A\"\n\n[[source]]\nname = \"wide\"\ncsv = \"wide.csv\"\n\n\
               [[cell]]\nname = \"same\"\nfrom = \"wide\"\n";
    let app = Arc::new(compile(&manifest::parse(src).unwrap(), dir.path()).unwrap());
    let (mut live, loaded) = settled(&app);

    let trace = bump(&mut live, &loaded, 42);
    assert_eq!(
        trace.names_evaluated(),
        vec!["same"],
        "an alias must wake for any column, because it hands on the whole frame"
    );
}

// ── row constraints, differentially ────────────────────────────────────────────────────
//
// A row constraint is the riskiest thing in this runtime: it lets a cell keep a cached value
// *after a column it read has demonstrably changed*, on the strength of an argument about
// which rows that column selects or what order it puts them in. If the argument is wrong
// anywhere — a filter whose column is also displayed, a second filter whose row indices mean
// something else, a `sort` that reordered underneath — the result is a stale number on a page
// that looks correct.
//
// So this does not assert a single count. It builds an app whose cells filter, rank and
// aggregate in every combination that matters, then walks every column of the frame through
// four kinds of edit and, after each one, compares every cell against a session that computed
// it from scratch. 200 columns × 4 edits × 11 cells is 8,800 comparisons per run.

/// Deterministic pseudo-randomness, written here for the same reason `core`'s oracle writes
/// its own: a failing seed has to be reproducible from the number in the message.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 11
    }
}

/// An app whose cells cover every shape the constraint argument has to survive.
fn constrained_app(dir: &std::path::Path) -> Arc<App> {
    std::fs::write(dir.join("wide.csv"), wide_csv()).unwrap();
    let src = r#"
[app]
title = "Filters"

[[source]]
name = "wide"
csv = "wide.csv"

# The case the constraint is for: filter on one column, total another.
[[cell]]
name = "constrained"
from = "wide"
[[cell.step]]
filter = { column = "c000", op = "ge", value = 50 }
[[cell.step]]
group_by = { agg = [{ column = "c001", agg = "sum", as = "t" }] }
[[cell.step]]
scalar = { column = "t" }

# The case it must NOT apply to: the filtered column is the totalled one.
[[cell]]
name = "same_column"
from = "wide"
[[cell.step]]
filter = { column = "c002", op = "ge", value = 50 }
[[cell.step]]
group_by = { agg = [{ column = "c002", agg = "sum", as = "t" }] }
[[cell.step]]
scalar = { column = "t" }

# Two filters. Only the first runs on the input's own rows, so only the first can be a
# constraint; the second has to fall back to a full dependency.
[[cell]]
name = "two_filters"
from = "wide"
[[cell.step]]
filter = { column = "c003", op = "ge", value = 40 }
[[cell.step]]
filter = { column = "c004", op = "le", value = 60 }
[[cell.step]]
count = true

# A sort between the filter and the end: the row order moved, so indices mean something else.
[[cell]]
name = "sorted_after"
from = "wide"
[[cell.step]]
filter = { column = "c005", op = "ge", value = 30 }
[[cell.step]]
sort = { column = "c006" }
[[cell.step]]
limit = 5
[[cell.step]]
group_by = { agg = [{ column = "c007", agg = "sum", as = "t" }] }
[[cell.step]]
scalar = { column = "t" }

# A filter on a *derived* column — not a passthrough, so never a constraint.
[[cell]]
name = "derived_filter"
from = "wide"
[[cell.step]]
derive = { name = "scaled", expr = "c008 * 2" }
[[cell.step]]
filter = { column = "scaled", op = "ge", value = 100 }
[[cell.step]]
count = true

# A filter whose column is dropped by a select, then a group over what is left.
[[cell]]
name = "projected_away"
from = "wide"
[[cell.step]]
filter = { column = "c009", op = "ge", value = 50 }
[[cell.step]]
select = ["c010", "c011"]
[[cell.step]]
group_by = { by = ["c010"], agg = [{ column = "c011", agg = "mean", as = "m" }] }

# A derive *before* the filter: the rows are still the input's, so the filter on a
# passthrough column is still constrainable even though a column was appended.
[[cell]]
name = "derive_then_filter"
from = "wide"
[[cell.step]]
derive = { name = "extra", expr = "c012 + 1" }
[[cell.step]]
filter = { column = "c013", op = "ge", value = 50 }
[[cell.step]]
group_by = { agg = [{ column = "extra", agg = "sum", as = "t" }] }
[[cell.step]]
scalar = { column = "t" }

# The whole filtered frame, every column of it.
[[cell]]
name = "passthrough"
from = "wide"
[[cell.step]]
filter = { column = "c014", op = "ge", value = 50 }
[[cell.step]]
count = true

# The case the ordering constraint is for: rank by one column, total another over the top of
# the order. The ranking column's values never reach the output.
[[cell]]
name = "top_n"
from = "wide"
[[cell.step]]
sort = { column = "c015", descending = true }
[[cell.step]]
limit = 5
[[cell.step]]
group_by = { agg = [{ column = "c016", agg = "sum", as = "t" }] }
[[cell.step]]
scalar = { column = "t" }

# The case it must NOT apply to: the ranked column is the totalled one.
[[cell]]
name = "ranked_shown"
from = "wide"
[[cell.step]]
sort = { column = "c017" }
[[cell.step]]
limit = 5
[[cell.step]]
group_by = { agg = [{ column = "c017", agg = "sum", as = "t" }] }
[[cell.step]]
scalar = { column = "t" }

# A filter and then a sort. Only the first runs on the input's own rows, so only the first can
# be a constraint; the sort has to fall back to a full dependency on what it ranked by.
[[cell]]
name = "filter_then_sort"
from = "wide"
[[cell.step]]
filter = { column = "c018", op = "ge", value = 50 }
[[cell.step]]
sort = { column = "c019" }
[[cell.step]]
limit = 3
[[cell.step]]
group_by = { agg = [{ column = "c021", agg = "sum", as = "t" }] }
[[cell.step]]
scalar = { column = "t" }
"#;
    Arc::new(compile(&manifest::parse(src).unwrap(), dir).unwrap())
}

#[test]
fn row_constraints_agree_with_a_full_recompute_under_every_edit() {
    let dir = tempfile::tempdir().unwrap();
    let app = constrained_app(dir.path());
    let names = [
        "constrained",
        "same_column",
        "two_filters",
        "sorted_after",
        "derived_filter",
        "projected_away",
        "derive_then_filter",
        "passthrough",
        "top_n",
        "ranked_shown",
        "filter_then_sort",
    ];

    let (mut live, loaded) = settled(&app);
    let base = loaded.as_frame().expect("a frame");
    let mut rng = Lcg(0x5EED);
    let mut woke = 0usize;
    let mut slept = 0usize;

    for col in 0..WIDTH {
        // Four edits per column, chosen to sit on both sides of every threshold the app uses:
        // a nudge that usually moves no selection, a value under every floor, a value over
        // every ceiling, and a random one that lands wherever it lands.
        let edits: [Value; 4] = [
            with_column_bumped(base, col),
            with_column_set(base, col, 0),
            with_column_set(base, col, 1_000),
            with_column_set(base, col, (rng.next() % 101) as i64),
        ];
        for (n, edit) in edits.into_iter().enumerate() {
            live.set("wide", edit.clone()).expect("a frame");
            let trace = live.commit();
            if trace.evaluated() == 0 {
                slept += 1;
            } else {
                woke += 1;
            }

            let mut truth = Session::new(Arc::clone(&app.graph));
            truth.set("wide", edit).expect("a frame");
            truth.refresh();

            for name in names {
                assert_eq!(
                    live.get(name).unwrap(),
                    truth.get(name).unwrap(),
                    "column {col}, edit {n}: `{name}` disagrees with a full recompute\n  \
                     trace: {}",
                    trace.summary()
                );
            }
        }
    }

    // Assertions about the corpus rather than the engine: a run in which nothing ever slept
    // would be green and would be testing nothing this change added.
    assert!(
        slept > 100,
        "only {slept} of {} edits let every cell sleep — the constraints are not firing",
        WIDTH * 4
    );
    assert!(
        woke > 20,
        "only {woke} edits woke anything — the app is inert"
    );
}

// ── the two conditions a constraint rests on, pinned ───────────────────────────────────
//
// The differential above passes with either condition removed, because removing them mostly
// causes *over*-invalidation on that corpus — safe, wasteful, invisible. They are still real
// safety properties, and these two tests are what say so, because nothing else does.

#[test]
fn only_the_first_filter_of_a_chain_can_be_a_constraint() {
    // A constraint is validated by re-running the predicate against the **input** frame, so
    // the selection it recorded has to be a selection over the input's rows. The first filter
    // in a chain runs on exactly those rows. The second runs on what the first left, and its
    // row indices mean something else entirely.
    //
    // Left unchecked this is genuine staleness, not merely waste: the second filter's indices
    // into the first's output can coincide with a selection over the whole input, and then a
    // change the cell needed to see looks like no change at all.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("wide.csv"), wide_csv()).unwrap();
    let src = "[app]\ntitle = \"T\"\n\n[[source]]\nname = \"wide\"\ncsv = \"wide.csv\"\n\n\
               [[cell]]\nname = \"two\"\nfrom = \"wide\"\n\
               [[cell.step]]\nfilter = { column = \"c000\", op = \"ge\", value = 50 }\n\
               [[cell.step]]\nfilter = { column = \"c001\", op = \"le\", value = 200 }\n\
               [[cell.step]]\ncount = true\n";
    let app = Arc::new(compile(&manifest::parse(src).unwrap(), dir.path()).unwrap());

    // The FIRST filter's column, nudged without moving its selection: a constraint, so the
    // cell sleeps.
    let (mut live, loaded) = settled(&app);
    assert!(
        bump(&mut live, &loaded, 0).names_evaluated().is_empty(),
        "the first filter of a chain should still be a constraint"
    );

    // The SECOND filter's column, nudged the same way. Both thresholds are chosen so that a
    // nudge of one moves nothing across them — no value in `c000` is 49, and every value in
    // `c001` clears 200 either way — so a constraint on *either* column would let the cell
    // sleep. It does not sleep, because a filter after another is not constrainable and its
    // column stays a full dependency. That is the cost of the restriction, and it is right.
    let (mut live, loaded) = settled(&app);
    assert_eq!(
        bump(&mut live, &loaded, 1).names_evaluated(),
        vec!["two"],
        "a filter after another cannot be a constraint, so its column is a full dependency"
    );
}

#[test]
fn a_sort_after_a_filter_can_never_be_a_constraint() {
    // The same rule as `only_the_first_filter_of_a_chain_can_be_a_constraint`, for the other
    // verb that can carry one: an ordering constraint is validated by re-running the sort
    // against the **input** frame, so the permutation it recorded has to be a permutation of
    // the input's rows. A sort that runs after a filter permutes what the filter left.
    //
    // This is a **cost** test, and it is labelled one because the mutation testing said so.
    // Removing the `pristine` condition from `sorted_by` does not make this fail, and cannot:
    // an ordering covers every row of the frame it ran on, so a permutation of the filtered
    // frame and a permutation of the input differ in *length*, and `ordering_digest` is
    // length-prefixed. The filtered case can only ever break the constraint, never wrongly
    // hold it. `docs/adr/0006` records the search for a case where the rule is load-bearing
    // for `sort`, and that none was found — the rule is kept because it is the same rule a
    // filter obeys and needs no second argument, not because a counterexample is known.
    //
    // What this test does pin is the price: `c001` is nudged by one, which cannot move any
    // ranking, and the cell wakes anyway. A `sort` at the top of a pipeline sleeps there —
    // `a_sorts_column_is_a_constraint_when_its_values_do_not_reach_the_output` is that half.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("wide.csv"), wide_csv()).unwrap();
    let src = "[app]\ntitle = \"FS\"\n\n[[source]]\nname = \"wide\"\ncsv = \"wide.csv\"\n\n\
               [[cell]]\nname = \"two\"\nfrom = \"wide\"\n\
               [[cell.step]]\nfilter = { column = \"c000\", op = \"ge\", value = 50 }\n\
               [[cell.step]]\nsort = { column = \"c001\" }\n\
               [[cell.step]]\nlimit = 5\n\
               [[cell.step]]\ngroup_by = { agg = [{ column = \"c002\", agg = \"sum\", as = \"t\" }] }\n\
               [[cell.step]]\nscalar = { column = \"t\" }\n";
    let app = Arc::new(compile(&manifest::parse(src).unwrap(), dir.path()).unwrap());

    // The FILTER's column, nudged without moving its selection: still a constraint, because
    // the filter is the first row-changing step. The cell sleeps.
    let (mut live, loaded) = settled(&app);
    assert!(
        bump(&mut live, &loaded, 0).names_evaluated().is_empty(),
        "the first row-changing step of a chain should still be a constraint"
    );

    // The SORT's column, nudged the same way. Not the first, so not a constraint.
    let (mut live, loaded) = settled(&app);
    assert_eq!(
        bump(&mut live, &loaded, 1).names_evaluated(),
        vec!["two"],
        "a sort after a filter cannot be a constraint, so its column is a full dependency"
    );

    // And the value is right, which is the assertion that would catch it if this ever did
    // start sleeping for the wrong reason.
    let mut truth = Session::new(Arc::clone(&app.graph));
    truth
        .set("wide", with_column_bumped(loaded.as_frame().unwrap(), 1))
        .unwrap();
    truth.refresh();
    assert_eq!(live.get("two").unwrap(), truth.get("two").unwrap());
}

#[test]
fn a_filter_on_a_derived_column_is_never_a_constraint() {
    // The other condition. A constraint re-runs its predicate against the input frame, and a
    // derived column does not exist there — so there is nothing to re-run it against, and the
    // columns the expression read are ordinary dependencies.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("wide.csv"), wide_csv()).unwrap();
    let src = "[app]\ntitle = \"D\"\n\n[[source]]\nname = \"wide\"\ncsv = \"wide.csv\"\n\n\
               [[cell]]\nname = \"derived\"\nfrom = \"wide\"\n\
               [[cell.step]]\nderive = { name = \"scaled\", expr = \"c000 * 2\" }\n\
               [[cell.step]]\nfilter = { column = \"scaled\", op = \"ge\", value = 100 }\n\
               [[cell.step]]\ncount = true\n";
    let app = Arc::new(compile(&manifest::parse(src).unwrap(), dir.path()).unwrap());

    let (mut live, loaded) = settled(&app);
    assert_eq!(
        bump(&mut live, &loaded, 0).names_evaluated(),
        vec!["derived"],
        "a filter on a derived column must keep a full dependency on what the expression read"
    );

    // And a column the expression does not read still costs nothing.
    let (mut live, loaded) = settled(&app);
    assert!(bump(&mut live, &loaded, 50).names_evaluated().is_empty());
}

#[test]
fn a_second_filters_indices_can_coincide_with_a_whole_frame_selection() {
    // The constructed case that makes the "first filter only" rule a correctness rule and not
    // a tidiness one. It is worth spelling out, because the differential above does not find
    // it and neither does any realistic app — which is exactly why it needs pinning.
    //
    // `keep` selects rows {2,3,4}. The second filter picks, *within that*, positions {0,1} —
    // which are input rows 2 and 3. So the recorded selection is the list `[0, 1]`.
    //
    // Now move `pick` so that input rows 0 and 1 pass and rows 2,3,4 do not. Re-running the
    // second predicate against the **input** frame yields `[0, 1]` — the very same list. A
    // constraint would compare those two digests, find them equal, and let the cell sleep.
    // The true answer went from 2 rows to 0.
    //
    // Nothing about this is exotic: it only needs the second filter's indices to be read in
    // the wrong frame of reference. The rule that a filter after another is never a constraint
    // is what makes it unreachable.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("t.csv"),
        "keep,pick\n0,99\n0,99\n10,1\n10,1\n10,99\n0,99\n",
    )
    .unwrap();
    let src = "[app]\ntitle = \"X\"\n\n[[source]]\nname = \"t\"\ncsv = \"t.csv\"\n\n\
               [[cell]]\nname = \"n\"\nfrom = \"t\"\n\
               [[cell.step]]\nfilter = { column = \"keep\", op = \"ge\", value = 5 }\n\
               [[cell.step]]\nfilter = { column = \"pick\", op = \"le\", value = 50 }\n\
               [[cell.step]]\ncount = true\n";
    let app = Arc::new(compile(&manifest::parse(src).unwrap(), dir.path()).unwrap());

    let mut live = Session::new(Arc::clone(&app.graph));
    live.refresh();
    assert_eq!(
        live.get("n").unwrap().value().unwrap().as_int(),
        Some(2),
        "rows 2 and 3 clear both filters"
    );

    // `pick` moves so that rows 0 and 1 carry the passing values instead.
    let moved = Value::table(
        Table::new(vec![
            Column::int(
                "keep",
                vec![Some(0), Some(0), Some(10), Some(10), Some(10), Some(0)],
            ),
            Column::int(
                "pick",
                vec![Some(1), Some(1), Some(99), Some(99), Some(99), Some(99)],
            ),
        ])
        .unwrap(),
    );
    live.set("t", moved).unwrap();
    live.commit();

    assert_eq!(
        live.get("n").unwrap().value().unwrap().as_int(),
        Some(0),
        "no row clears both filters now — a cell that slept here would be showing a stale 2"
    );
}
