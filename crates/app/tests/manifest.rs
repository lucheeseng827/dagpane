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

// ── what the compiler refuses ──────────────────────────────────────────────────────────

fn compile_err(text: &str) -> ManifestError {
    let m = manifest::parse(text).expect("this fixture parses");
    compile(&m, Path::new(".")).unwrap_err()
}

const HEAD: &str = r#"
[app]
title = "t"
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
        "{HEAD}\n[[cell]]\nname=\"c\"\nfrom=\"n\"\n[[cell.step]]\nfilter={{column=\"a\",op=\"eq\"}}\n"
    ));
    assert!(err.to_string().contains("exactly one of `value`"), "{err}");
}

#[test]
fn a_filter_reading_an_undeclared_input_is_rejected() {
    let err = compile_err(&format!(
        "{HEAD}\n[[cell]]\nname=\"c\"\nfrom=\"n\"\n[[cell.step]]\nfilter={{column=\"a\",op=\"eq\",param=\"ghost\"}}\n"
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
        "{HEAD}\n[[cell]]\nname=\"c\"\nfrom=\"n\"\n[[cell.step]]\ncount=true\n[[cell.step]]\nlimit=1\n"
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
