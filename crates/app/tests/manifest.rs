//! The declarative path, end to end: TOML in, a patch out.
//!
//! The assertion that matters is in `an_interaction_patches_the_panes_that_moved`. It is
//! the product claim expressed in the only two units a user can check — cells recomputed,
//! and panes sent.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use dagpane_app::{compile, load, manifest, App, AppSession, ManifestError, PaneUpdate, View};
use dagpane_core::Value;

fn example() -> Arc<App> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/sales.toml");
    Arc::new(load(&path).expect("the bundled example must compile"))
}

fn set(session: &mut AppSession, name: &str, value: Value) -> Vec<PaneUpdate> {
    let mut values = BTreeMap::new();
    values.insert(name.to_string(), value);
    session.set(&values).expect("a valid input");
    session.commit().1
}

#[test]
fn the_bundled_example_compiles_and_renders() {
    let app = example();
    assert_eq!(app.title, "Sales explorer");
    assert_eq!(app.graph.len(), 11, "1 data source + 2 controls + 8 cells");
    assert_eq!(app.widgets.len(), 2);
    assert_eq!(app.panes.len(), 7);

    let (mut s, trace) = AppSession::open(Arc::clone(&app));
    assert_eq!(trace.evaluated(), 8, "the first render computes every cell");
    let views = s.full_views();
    assert_eq!(views.len(), 7);

    let count = views.iter().find(|v| v.id == "order_count").unwrap();
    let View::Metric { value, .. } = &count.view else {
        panic!("`order_count` is a metric")
    };
    assert_eq!(value, "600", "no filter is applied yet");
}

#[test]
fn an_interaction_patches_the_panes_that_moved() {
    let app = example();
    let (mut s, _) = AppSession::open(Arc::clone(&app));
    s.full_views();

    let mut values = BTreeMap::new();
    values.insert("min_amount".to_string(), Value::float(400.0));
    s.set(&values).unwrap();
    let (trace, patch) = s.commit();

    // The whole claim, in one place. Eleven cells; the pass looked at eight, ran six, four
    // produced a new value, and three of seven panes went on the wire.
    assert_eq!(trace.total_cells, 11);
    assert_eq!(trace.visited(), 8, "{}", trace.summary());
    assert_eq!(trace.evaluated(), 6, "{}", trace.summary());
    assert_eq!(trace.reused(), 1, "{}", trace.summary());
    assert_eq!(trace.changed(), 4, "{}", trace.summary());
    assert_eq!(trace.untouched(), 3, "{}", trace.summary());

    let ids: Vec<&str> = patch.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, vec!["revenue", "order_count", "region_totals"]);
}

#[test]
fn a_branch_that_does_not_read_the_controls_is_never_visited() {
    let app = example();
    let (mut s, _) = AppSession::open(Arc::clone(&app));
    s.full_views();

    let mut values = BTreeMap::new();
    values.insert("min_amount".to_string(), Value::float(400.0));
    s.set(&values).unwrap();
    let (trace, patch) = s.commit();

    for name in ["sales", "all_time_revenue", "region"] {
        assert!(
            !trace.steps.iter().any(|st| st.cell == name),
            "`{name}` should not have been looked at: {}",
            trace.summary()
        );
    }
    assert!(
        !patch.iter().any(|p| p.id == "all_time_revenue"),
        "and its pane is not repainted"
    );
}

#[test]
fn a_cell_that_recomputes_to_the_same_value_stops_the_pass_and_the_patch() {
    // `channels` is the set of channels present. A $25 floor removes orders and does not
    // remove a channel, so `channels` re-runs, produces what it already had, and neither
    // `channel_count` below it nor either of their panes moves.
    let app = example();
    let (mut s, _) = AppSession::open(Arc::clone(&app));
    s.full_views();

    let mut values = BTreeMap::new();
    values.insert("min_amount".to_string(), Value::float(25.0));
    s.set(&values).unwrap();
    let (trace, patch) = s.commit();

    assert!(
        trace.short_circuited() >= 1,
        "at least one cell produced the value it already held: {}",
        trace.summary()
    );
    assert_eq!(
        trace.reused(),
        1,
        "`channel_count` served its cache: {}",
        trace.summary()
    );
    let ids: Vec<&str> = patch.iter().map(|p| p.id.as_str()).collect();
    assert!(!ids.contains(&"channels"), "{ids:?}");
    assert!(!ids.contains(&"channel_count"), "{ids:?}");
    assert!(
        !ids.contains(&"top_orders"),
        "a $25 floor keeps the top ten: {ids:?}"
    );
}

#[test]
fn a_skipped_filter_costs_nothing_and_a_narrowed_one_repaints() {
    // The dropdown starts on "all", whose filter `skip_when` turns off entirely, so
    // `filtered` is the source table with one filter applied rather than two.
    let app = example();
    let (mut s, _) = AppSession::open(Arc::clone(&app));
    s.full_views();

    let patch = set(&mut s, "region", Value::text("north"));
    let ids: Vec<&str> = patch.iter().map(|p| p.id.as_str()).collect();
    assert!(ids.contains(&"region_totals"), "{ids:?}");
    assert!(ids.contains(&"top_orders"), "{ids:?}");

    // Setting it to the region it already shows is not a change at all.
    let mut values = BTreeMap::new();
    values.insert("region".to_string(), Value::text("north"));
    s.set(&values).unwrap();
    let (trace, patch) = s.commit();
    assert_eq!(trace.visited(), 0, "{}", trace.summary());
    assert!(patch.is_empty());
}

#[test]
fn setting_an_input_to_its_current_value_sends_nothing() {
    let app = example();
    let (mut s, _) = AppSession::open(Arc::clone(&app));
    s.full_views();
    let patch = set(&mut s, "region", Value::text("all"));
    assert!(patch.is_empty());
}

#[test]
fn a_value_a_widget_would_not_produce_is_refused_before_anything_is_staged() {
    let app = example();
    let (mut s, _) = AppSession::open(Arc::clone(&app));

    let mut values = BTreeMap::new();
    values.insert("min_amount".to_string(), Value::float(1e9));
    values.insert("region".to_string(), Value::text("north"));
    let err = s.set(&values).unwrap_err();
    assert!(err.contains("outside its range"), "{err}");

    // And the good value in the same batch was not applied either.
    let (trace, patch) = s.commit();
    assert_eq!(trace.visited(), 0, "a rejected batch changes nothing");
    assert!(patch.is_empty());
}

#[test]
fn an_unknown_input_is_refused_by_name() {
    let app = example();
    let (mut s, _) = AppSession::open(app);
    let mut values = BTreeMap::new();
    values.insert("nope".to_string(), Value::int(1));
    assert!(s.set(&values).unwrap_err().contains("`nope`"));
}

#[test]
fn two_viewers_of_one_app_do_not_see_each_other() {
    let app = example();
    let (mut a, _) = AppSession::open(Arc::clone(&app));
    let (mut b, _) = AppSession::open(Arc::clone(&app));
    a.full_views();
    b.full_views();

    set(&mut a, "region", Value::text("north"));
    let b_views = b.full_views();
    let count = b_views.iter().find(|v| v.id == "order_count").unwrap();
    let View::Metric { value, .. } = &count.view else {
        panic!("`order_count` is a metric")
    };
    assert_eq!(value, "600", "the second viewer still sees every order");
}

// ── the eighth verb ────────────────────────────────────────────────────────────────────

// ── what the compiler refuses ──────────────────────────────────────────────────────────

fn compile_err(text: &str) -> ManifestError {
    compile_fixture(text).unwrap_err()
}

fn compile_fixture(text: &str) -> Result<App, ManifestError> {
    let m = manifest::parse(text).expect("this fixture parses");
    // Resolved against the examples directory so `HEAD` can name a real CSV. A fixture with
    // a real source is not a convenience: the compiler checks a step against the schema it
    // will be handed, and a manifest with nothing to load is a manifest it cannot check.
    compile(
        &m,
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples"),
    )
}

const HEAD: &str = r#"
[app]
title = "t"
[[source]]
name = "sales"
csv = "sales.csv"
[[input]]
name = "n"
slider = { min = 0.0, max = 1.0, default = 0.0 }
"#;

#[test]
fn an_input_with_no_control_is_rejected_and_lists_the_options() {
    let err = compile_err("[app]\ntitle=\"t\"\n[[input]]\nname=\"n\"\n");
    let msg = err.to_string();
    assert!(msg.contains("exactly one"), "{msg}");
    assert!(msg.contains("`slider`"), "{msg}");
}

#[test]
fn an_input_with_two_controls_names_both() {
    let err = compile_err(
        "[app]\ntitle=\"t\"\n[[input]]\nname=\"n\"\nslider={min=0.0,max=1.0,default=0.0}\ncheckbox={default=true}\n",
    );
    let msg = err.to_string();
    assert!(
        msg.contains("`slider`") && msg.contains("`checkbox`"),
        "{msg}"
    );
}

#[test]
fn a_filter_with_neither_value_nor_param_is_rejected() {
    let err = compile_err(&format!(
        "{HEAD}\n[[cell]]\nname=\"c\"\nfrom=\"sales\"\n[[cell.step]]\nfilter={{column=\"a\",op=\"eq\"}}\n"
    ));
    assert!(err.to_string().contains("exactly one of `value`"), "{err}");
}

#[test]
fn a_filter_reading_an_undeclared_input_is_rejected() {
    let err = compile_err(&format!(
        "{HEAD}\n[[cell]]\nname=\"c\"\nfrom=\"sales\"\n[[cell.step]]\nfilter={{column=\"a\",op=\"eq\",param=\"ghost\"}}\n"
    ));
    assert_eq!(
        err,
        ManifestError::UnknownParam {
            cell: "c".into(),
            param: "ghost".into()
        }
    );
}

#[test]
fn a_step_after_a_scalar_is_rejected() {
    let err = compile_err(&format!(
        "{HEAD}\n[[cell]]\nname=\"c\"\nfrom=\"sales\"\n[[cell.step]]\ncount=true\n[[cell.step]]\nlimit=1\n"
    ));
    assert_eq!(err, ManifestError::StepAfterScalar { cell: "c".into() });
}

#[test]
fn a_pane_showing_a_cell_that_does_not_exist_is_rejected() {
    let err = compile_err(&format!("{HEAD}\n[[pane]]\ncell=\"ghost\"\ntext=true\n"));
    assert_eq!(
        err,
        ManifestError::UnknownPaneCell {
            pane: "ghost".into(),
            cell: "ghost".into()
        }
    );
}

#[test]
fn two_panes_with_one_id_are_rejected() {
    let err = compile_err(&format!(
        "{HEAD}\n[[pane]]\ncell=\"n\"\ntext=true\n[[pane]]\ncell=\"n\"\ntext=true\n"
    ));
    assert_eq!(err, ManifestError::DuplicatePane("n".into()));
}

#[test]
fn a_cycle_in_a_manifest_is_a_compile_error() {
    let err = compile_err(&format!(
        "{HEAD}\n[[cell]]\nname=\"a\"\nfrom=\"b\"\n[[cell]]\nname=\"b\"\nfrom=\"a\"\n"
    ));
    assert!(err.to_string().contains("dependency cycle"), "{err}");
}

#[test]
fn an_unknown_field_is_rejected_rather_than_ignored() {
    // A silently-ignored key is how an app ends up not doing what its author wrote.
    let err = manifest::parse("[app]\ntitle=\"t\"\nsubtitel=\"typo\"\n").unwrap_err();
    assert!(err.to_string().contains("subtitel"), "{err}");
}

/// Not an assertion — a printer. `cargo test -p dagpane-app probe -- --nocapture --ignored`
/// prints the numbers the README quotes, so a change that moves them is visible rather than
/// argued about.
#[test]
#[ignore = "prints the README's numbers; run explicitly"]
fn probe() {
    let app = example();
    let (mut s, first) = AppSession::open(Arc::clone(&app));
    println!("cells={} first={}", app.graph.len(), first.summary());
    s.full_views();

    for (name, v) in [
        ("min_amount", Value::float(25.0)),
        ("min_amount", Value::float(400.0)),
        ("region", Value::text("north")),
        ("region", Value::text("all")),
    ] {
        let mut values = BTreeMap::new();
        values.insert(name.to_string(), v.clone());
        s.set(&values).unwrap();
        let (t, patch) = s.commit();
        let ids: Vec<&str> = patch.iter().map(|p| p.id.as_str()).collect();
        println!("set {name} -> {v:?}\n  {}\n  patch {ids:?}", t.summary());
    }
}

// ── derived columns ────────────────────────────────────────────────────────────────────

const DERIVED: &str = r#"
[app]
title = "Margins"
[[source]]
name = "sales"
csv = "sales.csv"
[[input]]
name = "discount"
slider = { min = 0.0, max = 0.5, step = 0.05, default = 0.0 }
[[cell]]
name = "net"
from = "sales"
[[cell.step]]
derive = { name = "net_amount", expr = "amount * (1 - $discount)" }
[[cell.step]]
derive = { name = "band", expr = "if(net_amount > 400.0, 'large', 'small')" }
[[cell]]
name = "by_band"
from = "net"
[[cell.step]]
group_by = { by = ["band"], agg = [{ column = "net_amount", agg = "sum", as = "revenue" }] }
[[cell.step]]
sort = { column = "band" }
[[cell]]
name = "untouched"
from = "sales"
[[cell.step]]
count = true
[[pane]]
cell = "by_band"
table = {}
[[pane]]
cell = "untouched"
metric = { label = "Orders" }
"#;

#[test]
fn a_dollar_in_an_expression_is_an_edge_and_a_bare_name_is_not() {
    let app = compile_fixture(DERIVED).expect("compiles");
    // `net` reads the table it came from and the control the expression named — and nothing
    // for `amount` or `net_amount`, which are columns. This is the whole argument for the
    // sigil, asserted rather than described.
    let net = app.graph.id("net").expect("declared");
    let inputs: Vec<&str> = app
        .graph
        .inputs_of(net)
        .iter()
        .map(|id| app.graph.name(*id))
        .collect();
    assert_eq!(inputs, vec!["sales", "discount"]);
}

#[test]
fn moving_a_control_recomputes_only_the_cells_the_expression_reaches() {
    let app = Arc::new(compile_fixture(DERIVED).expect("compiles"));
    let (mut s, first) = AppSession::open(Arc::clone(&app));
    assert_eq!(first.evaluated(), 3);
    s.full_views();

    let patch = set(&mut s, "discount", Value::float(0.25));
    // `untouched` reads the source, not the derived cell, so a discount cannot reach it.
    assert_eq!(patch.len(), 1, "only the band chart moved");
    assert_eq!(patch[0].id, "by_band");
}

#[test]
fn a_derived_column_is_computed_and_usable_downstream() {
    let app = Arc::new(compile_fixture(DERIVED).expect("compiles"));
    let (mut s, _) = AppSession::open(Arc::clone(&app));
    let views = s.full_views();
    let by_band = views.iter().find(|v| v.id == "by_band").unwrap();
    let View::Table { head, rows, .. } = &by_band.view else {
        panic!("`by_band` is a table")
    };
    // `band` came from an `if` over a column that came from an expression over the source.
    let names: Vec<&str> = head.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(names, vec!["band", "revenue"]);
    assert_eq!(rows.len(), 2, "large and small");
}

#[test]
fn check_names_a_bad_column_reference_before_the_app_runs() {
    // PLAN §12 PR 10's exit criterion, as an assertion.
    let err = compile_err(&DERIVED.replace("amount * (1 - $discount)", "amont * 2"));
    let message = err.to_string();
    assert!(message.contains("cell `net`, step `derive`"), "{message}");
    assert!(message.contains("no column `amont`"), "{message}");
    assert!(message.contains("did you mean `amount`?"), "{message}");
    assert!(message.contains("`region`"), "{message}");
}

#[test]
fn the_schema_a_step_is_checked_against_is_the_one_the_steps_before_it_produced() {
    // `band` exists only because the step above created it — a checker that looked at the
    // source's schema would reject this app, and one that looked at nothing would accept the
    // typo below it.
    assert!(compile_fixture(DERIVED).is_ok());

    let err = compile_err(&DERIVED.replace(
        r#"sort = { column = "band" }"#,
        r#"sort = { column = "bnad" }"#,
    ));
    let message = err.to_string();
    assert!(message.contains("step `sort`"), "{message}");
    assert!(message.contains("did you mean `band`?"), "{message}");
    // And the columns it lists are the group-by's output, not the source's.
    assert!(message.contains("`revenue`"), "{message}");
    assert!(!message.contains("`order_id`"), "{message}");
}

#[test]
fn a_column_a_select_dropped_is_gone_for_every_step_after_it() {
    let err = compile_err(&format!(
        "{HEAD}{}",
        r#"
[[cell]]
name = "c"
from = "sales"
[[cell.step]]
select = ["region"]
[[cell.step]]
derive = { name = "x", expr = "amount * 2" }
"#
    ));
    let message = err.to_string();
    assert!(message.contains("no column `amount`"), "{message}");
    assert!(message.contains("the table here has `region`"), "{message}");
}

#[test]
fn a_derived_column_may_not_shadow_one_the_table_already_has() {
    let err =
        compile_err(&DERIVED.replace(r#"name = "net_amount", expr"#, r#"name = "amount", expr"#));
    assert!(
        err.to_string().contains("already has a column `amount`"),
        "{err}"
    );
}

#[test]
fn an_expression_reading_an_undeclared_cell_is_the_same_error_a_filter_gives() {
    let err = compile_err(&DERIVED.replace("$discount", "$discont"));
    assert_eq!(
        err,
        ManifestError::UnknownParam {
            cell: "net".into(),
            param: "discont".into()
        }
    );
}

#[test]
fn an_expression_reading_a_table_is_refused_by_name() {
    let err = compile_err(&DERIVED.replace("$discount", "$sales"));
    let message = err.to_string();
    assert!(message.contains("`$sales` is a table"), "{message}");
}

#[test]
fn a_type_error_in_an_expression_is_found_before_the_app_runs() {
    for (expr, wanted) in [
        ("region * 2", "needs numbers"),
        ("concat(region, amount)", "needs text"),
        ("if(amount > 1.0, 1, 'no')", "share one type"),
        ("amount and true", "needs bools"),
        ("region > amount", "cannot compare"),
        ("null", "always null"),
    ] {
        let err = compile_err(&DERIVED.replace("amount * (1 - $discount)", expr));
        assert!(err.to_string().contains(wanted), "{expr}: {err}");
    }
}

#[test]
fn a_cell_with_a_step_may_not_read_a_control() {
    let err = compile_err(&format!(
        "{HEAD}{}",
        r#"
[[cell]]
name = "c"
from = "n"
[[cell.step]]
limit = 1
"#
    ));
    assert_eq!(
        err,
        ManifestError::NotATable {
            cell: "c".into(),
            from: "n".into(),
            found: "a float".into()
        }
    );
}

#[test]
fn a_cell_with_no_steps_is_an_alias_of_whatever_it_reads() {
    // Including a control. The documented "empty is legal and makes this cell an alias" was
    // true only for tables until the schema checker made the difference visible.
    let app = compile_fixture(&format!(
        "{HEAD}{}",
        r#"
[[cell]]
name = "c"
from = "n"
[[pane]]
cell = "c"
text = true
"#
    ))
    .expect("compiles");
    let (mut s, _) = AppSession::open(Arc::new(app));
    let views = s.full_views();
    let View::Text { text } = &views[0].view else {
        panic!("a text pane")
    };
    assert_eq!(text, "0");
}

// ── joins ──────────────────────────────────────────────────────────────────────────────

/// Group a table and join the result back onto its own rows — the commonest reason a
/// dashboard needs a join at all, and one that needs no second data file.
const JOINED: &str = r#"
[app]
title = "Share of region"
[[source]]
name = "sales"
csv = "sales.csv"
[[input]]
name = "floor"
label = "Floor"
slider = { min = 0.0, max = 800.0, step = 25.0, default = 0.0 }
[[cell]]
name = "scoped"
from = "sales"
[[cell.step]]
filter = { column = "amount", op = "ge", param = "floor" }
[[cell]]
name = "by_region"
from = "sales"
[[cell.step]]
group_by = { by = ["region"], agg = [
  { agg = "count", as = "orders_in_region" },
  { column = "amount", agg = "sum", as = "region_revenue" },
] }
[[cell]]
name = "widened"
from = "scoped"
[[cell.step]]
join = { with = "by_region", on = "region", how = "left" }
[[cell.step]]
derive = { name = "share_pct", expr = "amount / region_revenue * 100" }
[[cell.step]]
select = ["order_id", "region", "amount", "region_revenue", "share_pct"]
[[cell]]
name = "rows"
from = "widened"
[[cell.step]]
count = true
[[pane]]
cell = "widened"
table = {}
[[pane]]
cell = "rows"
metric = { label = "Rows" }
"#;

#[test]
fn a_joins_with_is_an_edge_and_the_graph_shows_both_sides() {
    let app = compile_fixture(JOINED).expect("compiles");
    let widened = app.graph.id("widened").expect("declared");
    let inputs: Vec<&str> = app
        .graph
        .inputs_of(widened)
        .iter()
        .map(|id| app.graph.name(*id))
        .collect();
    assert_eq!(inputs, vec!["scoped", "by_region"]);
}

#[test]
fn a_join_carries_the_right_hand_columns_onto_the_left_hand_rows() {
    let app = Arc::new(compile_fixture(JOINED).expect("compiles"));
    let (mut s, _) = AppSession::open(Arc::clone(&app));
    let views = s.full_views();
    let widened = views.iter().find(|v| v.id == "widened").unwrap();
    let View::Table { head, rows, .. } = &widened.view else {
        panic!("a table")
    };
    let names: Vec<&str> = head.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "order_id",
            "region",
            "amount",
            "region_revenue",
            "share_pct"
        ],
        "the right's key column is not carried across a second time"
    );
    // Every row got its own region's total, and the share is a fraction of it.
    let share = rows[0].last().unwrap().as_float().expect("a float");
    assert!(share > 0.0 && share < 100.0, "{share}");
}

#[test]
fn only_the_side_a_control_reaches_recomputes() {
    // `by_region` reads the source, `scoped` reads the slider. A join is two cells in, and
    // the closure of an interaction is still the closure of one of them.
    let app = Arc::new(compile_fixture(JOINED).expect("compiles"));
    let (mut s, _) = AppSession::open(Arc::clone(&app));
    s.full_views();
    let mut values = BTreeMap::new();
    values.insert("floor".to_string(), Value::float(400.0));
    s.set(&values).unwrap();
    let (trace, _) = s.commit();
    let visited: Vec<&str> = trace.steps.iter().map(|st| st.cell.as_str()).collect();
    assert!(!visited.contains(&"by_region"), "{visited:?}");
    assert!(visited.contains(&"widened"), "{visited:?}");
}

#[test]
fn check_names_a_bad_key_column_on_whichever_side_it_is_missing_from() {
    let err = compile_err(&JOINED.replace(
        r#"join = { with = "by_region", on = "region", how = "left" }"#,
        r#"join = { with = "by_region", left_on = "regoin", right_on = "region", how = "left" }"#,
    ));
    let message = err.to_string();
    assert!(message.contains("step `join`"), "{message}");
    assert!(message.contains("the left-hand table"), "{message}");
    assert!(message.contains("did you mean `region`?"), "{message}");

    let err = compile_err(&JOINED.replace(
        r#"join = { with = "by_region", on = "region", how = "left" }"#,
        r#"join = { with = "by_region", left_on = "region", right_on = "regoin", how = "left" }"#,
    ));
    let message = err.to_string();
    assert!(message.contains("the right-hand table"), "{message}");
    assert!(message.contains("`orders_in_region`"), "{message}");
}

#[test]
fn a_key_of_two_types_is_refused_before_the_app_runs() {
    // `region` is text and `orders_in_region` is an int, so this join would match nothing and
    // the page would be empty with no explanation. Named instead.
    let err = compile_err(&JOINED.replace(
        r#"join = { with = "by_region", on = "region", how = "left" }"#,
        r#"join = { with = "by_region", left_on = "region", right_on = "orders_in_region", how = "left" }"#,
    ));
    let message = err.to_string();
    assert!(message.contains("text on the left"), "{message}");
    assert!(message.contains("int on the right"), "{message}");
}

#[test]
fn a_column_on_both_sides_is_refused_and_says_how_to_fix_it() {
    // Joining `sales` to itself: every non-key column collides.
    let err = compile_err(&JOINED.replace(
        r#"join = { with = "by_region", on = "region", how = "left" }"#,
        r#"join = { with = "sales", on = "region", how = "left" }"#,
    ));
    let message = err.to_string();
    assert!(message.contains("both tables have a column"), "{message}");
    assert!(message.contains("suffix"), "{message}");
}

#[test]
fn a_suffix_resolves_a_collision_and_the_schema_follows_it() {
    let text = JOINED
        .replace(
            r#"join = { with = "by_region", on = "region", how = "left" }"#,
            r#"join = { with = "sales", on = "region", how = "left", suffix = "_all", multiple = true }"#,
        )
        .replace(
            r#"derive = { name = "share_pct", expr = "amount / region_revenue * 100" }"#,
            r#"derive = { name = "ratio", expr = "amount / amount_all" }"#,
        )
        .replace(
            r#"select = ["order_id", "region", "amount", "region_revenue", "share_pct"]"#,
            r#"select = ["order_id", "region", "amount", "amount_all", "ratio"]"#,
        );
    // The derive below the join resolves `amount_all`, which exists only because the suffix
    // put it there — so the schema really did follow the join.
    compile_fixture(&text).expect("compiles");
}

#[test]
fn semi_and_anti_refuse_the_two_fields_that_describe_carried_columns() {
    for field in [r#"multiple = true"#, r#"suffix = "_x""#] {
        let err = compile_err(&JOINED.replace(
            r#"join = { with = "by_region", on = "region", how = "left" }"#,
            &format!(r#"join = {{ with = "by_region", on = "region", how = "semi", {field} }}"#),
        ));
        assert!(
            err.to_string().contains("nothing to say about it"),
            "{field}: {err}"
        );
    }
}

#[test]
fn an_anti_join_is_a_filter_that_reads_another_table() {
    let text = JOINED
        .replace(
            r#"join = { with = "by_region", on = "region", how = "left" }"#,
            r#"join = { with = "by_region", on = "region", how = "anti" }"#,
        )
        .replace(
            "[[cell.step]]\nderive = { name = \"share_pct\", expr = \"amount / region_revenue * 100\" }\n",
            "",
        )
        .replace(
            r#"select = ["order_id", "region", "amount", "region_revenue", "share_pct"]"#,
            r#"select = ["order_id", "region", "amount"]"#,
        );
    // It compiles — which is the assertion: `region_revenue` is *not* in scope below an
    // `anti`, because an anti join carries nothing across, and the `select` above proves the
    // checker knows that.
    let app = Arc::new(compile_fixture(&text).expect("compiles"));
    let (mut s, _) = AppSession::open(Arc::clone(&app));
    let views = s.full_views();
    let rows = views.iter().find(|v| v.id == "rows").unwrap();
    let View::Metric { value, .. } = &rows.view else {
        panic!("a metric")
    };
    // Every region in `scoped` is in `by_region`, so nothing is unmatched.
    assert_eq!(value, "0");
}

#[test]
fn a_column_the_join_did_not_carry_is_not_in_scope_below_it() {
    let text = JOINED.replace(
        r#"join = { with = "by_region", on = "region", how = "left" }"#,
        r#"join = { with = "by_region", on = "region", how = "semi" }"#,
    );
    let message = compile_err(&text).to_string();
    assert!(message.contains("no column `region_revenue`"), "{message}");
}

#[test]
fn the_keys_have_to_be_named_one_way_or_the_other_and_not_both() {
    for keys in [
        r#""#,
        r#"on = "region", left_on = "region", right_on = "region","#,
        r#"left_on = "region","#,
    ] {
        let err = compile_err(&JOINED.replace(
            r#"join = { with = "by_region", on = "region", how = "left" }"#,
            &format!(r#"join = {{ with = "by_region", {keys} how = "left" }}"#),
        ));
        assert!(
            err.to_string().contains("`left_on` and `right_on`"),
            "{keys}: {err}"
        );
    }
}

#[test]
fn joining_with_a_control_is_refused_by_name() {
    let err = compile_err(&JOINED.replace(r#"with = "by_region""#, r#"with = "floor""#));
    let message = err.to_string();
    assert!(message.contains("`floor` is float"), "{message}");
}

#[test]
fn joining_with_an_undeclared_cell_is_the_same_error_a_filter_gives() {
    let err = compile_err(&JOINED.replace(r#"with = "by_region""#, r#"with = "by_regoin""#));
    assert_eq!(
        err,
        ManifestError::UnknownParam {
            cell: "widened".into(),
            param: "by_regoin".into()
        }
    );
}

#[test]
fn a_duplicated_key_is_a_cell_error_and_not_a_doubled_total() {
    // Row counts are data, so this one cannot be caught before the app runs — but it is
    // caught, and the pane says so instead of showing a number twice as big as it should be.
    let text = JOINED
        .replace(
            r#"join = { with = "by_region", on = "region", how = "left" }"#,
            r#"join = { with = "sales", on = "region", how = "left", suffix = "_b" }"#,
        )
        .replace(
            "[[cell.step]]\nderive = { name = \"share_pct\", expr = \"amount / region_revenue * 100\" }\n",
            "",
        )
        .replace(
            r#"select = ["order_id", "region", "amount", "region_revenue", "share_pct"]"#,
            r#"select = ["order_id", "region", "amount"]"#,
        );
    let app = Arc::new(compile_fixture(&text).expect("compiles: row counts are not a schema"));
    let (mut s, _) = AppSession::open(Arc::clone(&app));
    let views = s.full_views();
    let widened = views.iter().find(|v| v.id == "widened").unwrap();
    let View::Error { message, cause } = &widened.view else {
        panic!("the pane reports the failure: {:?}", widened.view)
    };
    assert!(message.contains("multiple = true"), "{message}");
    assert_eq!(cause, "widened");
}

#[test]
fn the_bundled_join_example_finds_the_service_nobody_deploys_to() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/apps/17-service-risk.toml");
    let app = Arc::new(load(&path).expect("the bundled example must compile"));
    let (mut s, _) = AppSession::open(Arc::clone(&app));
    let views = s.full_views();
    let unshipped = views.iter().find(|v| v.id == "unshipped").unwrap();
    let View::Table { rows, .. } = &unshipped.view else {
        panic!("a table")
    };
    let names: Vec<&str> = rows.iter().filter_map(|r| r[0].as_text()).collect();
    assert_eq!(
        names,
        vec!["reporting"],
        "it is in the SLO feed and never in the deploy log"
    );
}

// ── the extension point ────────────────────────────────────────────────────────────────
//
// A `custom` pane is what makes a new drawing cost one JavaScript function and one manifest
// stanza instead of five edits across two crates and a new server binary. These tests pin
// the two halves that decide whether that is actually true: the manifest reaches the view
// without anything in between reinterpreting it, and the parts that must NOT reach the wire
// on every patch do not.

const SALES_PANE: &str = r#"
[app]
title = "t"
renderers = ["renderers/heatmap.js"]
[[source]]
name = "sales"
csv = "sales.csv"
[[cell]]
name = "by_region"
from = "sales"
[[cell.step]]
group_by = { by = ["region"], agg = [{ column = "amount", agg = "sum", as = "revenue" }] }
[[pane]]
cell = "by_region"
custom = { renderer = "treemap", columns = ["region", "revenue"], max_rows = 20, options = { palette = "warm", floor = 10 } }
"#;

#[test]
fn a_custom_pane_reaches_the_view_with_its_options_intact() {
    let app = Arc::new(compile_fixture(SALES_PANE).expect("a custom pane compiles"));
    let dagpane_app::PaneKind::Custom {
        renderer,
        columns,
        max_rows,
        options,
    } = &app.panes[0].kind
    else {
        panic!("expected a custom pane, got {:?}", app.panes[0].kind)
    };
    assert_eq!(renderer, "treemap");
    assert_eq!(
        columns.as_deref(),
        Some(&["region".to_string(), "revenue".to_string()][..])
    );
    assert_eq!(*max_rows, 20);
    // Options are opaque to this crate on purpose: it must carry a renderer's vocabulary
    // without having one of its own, or every new drawing needs a Rust change after all.
    assert_eq!(options["palette"], serde_json::json!("warm"));
    assert_eq!(options["floor"], serde_json::json!(10));
}

#[test]
fn a_custom_pane_renders_the_projection_the_manifest_asked_for() {
    let app = Arc::new(compile_fixture(SALES_PANE).expect("compiles"));
    let (mut s, _) = AppSession::open(Arc::clone(&app));
    let views = s.full_views();
    let View::Custom {
        renderer,
        data: dagpane_app::CustomData::Table {
            head, total_rows, ..
        },
    } = &views[0].view
    else {
        panic!("expected a custom view, got {:?}", views[0].view)
    };
    assert_eq!(renderer, "treemap");
    assert_eq!(
        head.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
        vec!["region", "revenue"]
    );
    assert!(*total_rows > 0);
}

#[test]
fn a_custom_pane_is_still_absent_from_a_patch_when_its_view_did_not_move() {
    // The property a renderer must not be able to break. `custom` hands the *drawing* to a
    // client; it does not hand over the decision about what goes on the wire, which is the
    // whole product claim. So a pane whose rendered view is unchanged stays off the patch
    // exactly as a built-in one does.
    let text = format!(
        "{SALES_PANE}\n[[input]]\nname = \"floor\"\nslider = {{ min = 0.0, max = 1.0, default = 0.0 }}\n"
    );
    let app = Arc::new(compile_fixture(&text).expect("compiles"));
    let (mut s, _) = AppSession::open(Arc::clone(&app));
    s.full_views();
    // Nothing downstream of `floor` exists, so moving it cannot change the custom pane.
    let patch = set(&mut s, "floor", Value::float(0.5));
    assert!(
        patch.is_empty(),
        "a custom pane whose view did not move must not be sent: {patch:?}"
    );
}

#[test]
fn a_renderer_path_may_not_climb_out_of_the_app() {
    // The scripts an app declares are read at compile time and carried in the process, so
    // this list decides which bytes a served page will import. A `..` in it is a file read
    // wearing a manifest, and it is refused before anything is opened — note that
    // `/etc/passwd` and `../../etc/shadow` both exist on the machine running this test and
    // neither is read.
    for bad in ["../secrets.js", "a/../../secrets.js", "/etc/passwd"] {
        let text = format!("[app]\ntitle=\"t\"\nrenderers=[\"{bad}\"]\n");
        let err = compile_fixture(&text).expect_err("a traversal must be refused");
        let msg = err.to_string();
        assert!(msg.contains("renderer"), "{bad}: {msg}");
        assert!(
            msg.contains("relative") || msg.contains("climb"),
            "refused for the right reason, not for being missing: {bad}: {msg}"
        );
    }
    // A relative path that is there compiles.
    assert!(compile_fixture("[app]\ntitle=\"t\"\nrenderers=[\"renderers/heatmap.js\"]\n").is_ok());
}

#[test]
fn a_renderer_script_that_is_not_there_fails_the_app_rather_than_the_page() {
    // A `custom` pane whose script never loaded is a blank card in a page that otherwise
    // works, which is the failure mode this whole project is arranged not to have. Reading
    // the scripts when the app is compiled turns it into something `dagpane check` catches.
    let err = compile_fixture("[app]\ntitle=\"t\"\nrenderers=[\"nope.js\"]\n")
        .expect_err("a declared script that is missing is an error");
    let msg = err.to_string();
    assert!(msg.contains("nope.js"), "{msg}");
    assert!(msg.contains("could not be read"), "{msg}");
}

#[test]
fn a_renderers_source_travels_with_the_app_so_serving_one_reads_no_file() {
    let app = compile_fixture(SALES_PANE).expect("compiles");
    assert_eq!(app.renderers.len(), 1);
    assert_eq!(app.renderers[0].path, "renderers/heatmap.js");
    assert!(
        app.renderers[0]
            .source
            .contains("dagpane.renderer(\"heatmap\""),
        "the script itself is carried, not a path to be opened later"
    );
}

#[test]
fn a_pane_may_not_be_custom_and_something_else_at_once() {
    let text = format!(
        "{HEAD}\n[[pane]]\ncell = \"sales\"\ntext = true\ncustom = {{ renderer = \"x\" }}\n"
    );
    let err = compile_fixture(&text).expect_err("two presentations is not one");
    let msg = err.to_string();
    assert!(msg.contains("`custom`") && msg.contains("`text`"), "{msg}");
}

#[test]
fn a_renderer_may_not_be_a_url_however_it_is_spelled() {
    // The hole the path checks alone leave open. `https://evil.example/x.js` is not an
    // absolute PATH — `is_absolute()` is false for it and it has no `..` — but the client
    // resolves it with `new URL(path, document.baseURI)`, where it is absolutely a URL and
    // absolutely another origin. On a server the file read catches it; in a browser, whose
    // host supplies scripts by name, nothing would.
    for bad in [
        "https://evil.example/x.js",
        "//evil.example/x.js",
        "data:text/javascript,alert(1)",
    ] {
        let text = format!("[app]\ntitle=\"t\"\nrenderers=[\"{bad}\"]\n");
        let err = compile_fixture(&text).expect_err("a URL must be refused");
        let msg = err.to_string();
        // Refused for its SHAPE, before anything is opened — `//host/x.js` happens to trip
        // the absolute-path rule first and that is just as good. What must never happen is a
        // refusal that reads "could not be read", because that one goes away the moment a
        // host supplies the script by name, which is exactly what a browser does.
        assert!(
            msg.contains("not a URL") || msg.contains("must be relative"),
            "refused for being a URL rather than for being missing: {bad}: {msg}"
        );
        assert!(!msg.contains("could not be read"), "{bad}: {msg}");
    }
}
