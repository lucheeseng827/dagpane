//! The exit criterion the roadmap sets for scheduled refresh, as a test:
//! **an unchanged source produces a pass with `visited == 0`.**
//!
//! And the three cases around it, because the interesting part is not the headline number —
//! it is that there are *two* filters, and each catches something the other cannot.

use std::collections::BTreeMap;

use dagpane_app::refresh::{refresh, RefreshOutcome, SourceRefresh};
use dagpane_app::{App, Version};
use dagpane_core::frame::{FrameBuilder, TableBuilder};
use dagpane_core::{Session, StepOutcome};

fn builder() -> Box<dyn FrameBuilder> {
    Box::new(TableBuilder::new())
}

const MANIFEST: &str = r#"
[app]
title = "Refresh"

[[source]]
name = "sales"
csv = "sales.csv"

[[input]]
name = "min_amount"
slider = { min = 0.0, max = 1000.0, step = 10.0, default = 0.0 }

[[cell]]
name = "filtered"
from = "sales"
[[cell.step]]
filter = { column = "amount", op = "ge", param = "min_amount" }

[[cell]]
name = "total"
from = "filtered"
[[cell.step]]
group_by = { agg = [{ column = "amount", agg = "sum", as = "t" }] }
[[cell.step]]
scalar = { column = "t" }

# Reads the INPUT and not the source. Never downstream of `sales`, so no refresh of `sales`
# may ever touch it — which is what makes `visited` a number about this app rather than
# about the whole graph.
[[cell]]
name = "floor_label"
from = "min_amount"

[[pane]]
cell = "total"
metric = { label = "Total" }
"#;

fn csv(rows: usize) -> String {
    let mut text = String::from("region,amount\n");
    for i in 0..rows {
        text.push_str(&format!("r{},{}.0\n", i % 4, (i % 29) * 10));
    }
    text
}

/// A compiled app over a CSV this test can rewrite.
fn fixture(rows: usize) -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sales.csv"), csv(rows)).unwrap();
    std::fs::write(dir.path().join("app.toml"), MANIFEST).unwrap();
    let app = dagpane_app::load(&dir.path().join("app.toml")).unwrap();
    (dir, app)
}

/// A session that has rendered once, plus the versions its app was compiled with.
fn opened(app: &App) -> (Session, BTreeMap<String, Version>) {
    let mut session = Session::new(app.graph.clone());
    session.refresh();
    let versions = app
        .sources
        .iter()
        .map(|b| (b.cell.clone(), b.loaded_version))
        .collect();
    (session, versions)
}

#[test]
fn an_unchanged_source_visits_nothing_and_is_not_even_read() {
    let (_dir, app) = fixture(200);
    let (mut session, versions) = opened(&app);

    let result = refresh(
        &app,
        &mut session,
        &versions,
        RefreshOutcome::IfChanged,
        builder,
    );

    assert_eq!(result.trace.visited(), 0, "the exit criterion");
    assert!(result.trace.roots.is_empty());
    assert!(result.complete());
    assert!(
        matches!(result.sources[0].1, SourceRefresh::Unchanged { .. }),
        "{:?}",
        result.sources
    );
    assert_eq!(
        result.trace.total_cells, 5,
        "a five-cell app visiting zero of them is the claim"
    );
}

#[test]
fn a_file_rewritten_with_identical_content_is_read_and_still_visits_nothing() {
    // The second filter, alone. The version moves — a rewrite touches the mtime — so the
    // source IS read; then the engine digests what came back, finds it equal, and declines
    // it. A runtime that re-rendered on every refresh would repaint the page here.
    let (dir, app) = fixture(200);
    let (mut session, versions) = opened(&app);

    // A rewrite with the same bytes. Sleeping first so the mtime is certainly different —
    // this test is about what happens when the version DOES move, so it must actually move.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(dir.path().join("sales.csv"), csv(200)).unwrap();

    let result = refresh(
        &app,
        &mut session,
        &versions,
        RefreshOutcome::IfChanged,
        builder,
    );

    assert_eq!(result.reloaded().collect::<Vec<_>>(), vec!["sales"]);
    assert_eq!(
        result.trace.visited(),
        0,
        "identical rows repainted something"
    );
}

#[test]
fn forcing_a_read_of_an_untouched_file_also_visits_nothing() {
    let (_dir, app) = fixture(200);
    let (mut session, versions) = opened(&app);

    let result = refresh(
        &app,
        &mut session,
        &versions,
        RefreshOutcome::Force,
        builder,
    );

    assert!(result.sources[0].1.reloaded(), "--force did not read");
    assert_eq!(result.trace.visited(), 0);
}

#[test]
fn a_changed_source_visits_its_closure_and_nothing_else() {
    let (dir, app) = fixture(200);
    let (mut session, versions) = opened(&app);

    std::fs::write(dir.path().join("sales.csv"), csv(150)).unwrap();
    let result = refresh(
        &app,
        &mut session,
        &versions,
        RefreshOutcome::IfChanged,
        builder,
    );

    assert_eq!(result.trace.roots, vec!["sales".to_string()]);
    // sales, filtered, total — and NOT `min_amount` or `floor_label`, which are not
    // downstream of the source that moved. That is the whole product, stated over data
    // changing rather than over a control moving.
    assert_eq!(result.trace.visited(), 3, "{:?}", result.trace.steps);
    let names: Vec<&str> = result.trace.steps.iter().map(|s| s.cell.as_str()).collect();
    assert_eq!(names, vec!["sales", "filtered", "total"]);
    assert!(
        result
            .trace
            .steps
            .iter()
            .filter(|s| matches!(s.outcome, StepOutcome::Evaluated { .. }))
            .count()
            >= 2
    );

    // The version was updated, so a second refresh over the same file is free again.
    let again = refresh(
        &app,
        &mut session,
        &result.versions,
        RefreshOutcome::IfChanged,
        builder,
    );
    assert_eq!(again.trace.visited(), 0, "the new version was not recorded");
}

#[test]
fn a_source_that_cannot_be_read_keeps_its_version_and_leaves_the_pane_alone() {
    let (dir, app) = fixture(200);
    let (mut session, versions) = opened(&app);
    let before = session.get("total").unwrap().value().cloned();

    std::fs::remove_file(dir.path().join("sales.csv")).unwrap();
    let result = refresh(
        &app,
        &mut session,
        &versions,
        RefreshOutcome::IfChanged,
        builder,
    );

    assert!(!result.complete());
    let (name, error) = result.failed().next().unwrap();
    assert_eq!(name, "sales");
    assert!(error.is_retryable(), "a missing file may come back");

    // The pane keeps the value it had. A dashboard that blanks because one source is
    // briefly unreadable is worse than one that shows an ageing number and says so.
    assert_eq!(session.get("total").unwrap().value().cloned(), before);
    assert_eq!(result.trace.visited(), 0);

    // The version was NOT advanced, so the next refresh tries again rather than deciding
    // the outage was a successful read of unchanged data.
    assert_eq!(
        result.versions.get("sales"),
        versions.get("sales"),
        "a failed read recorded a version"
    );
}

#[test]
fn one_pass_covers_every_source_that_moved() {
    // Two sources into one cell. If each were committed separately the shared cell would run
    // twice — once with a mix of old and new — which is the glitch the engine exists to not
    // have, and it is as true of a refresh as of a client moving two sliders at once.
    const TWO: &str = r#"
[app]
title = "Two"

[[source]]
name = "a"
csv = "a.csv"

[[source]]
name = "b"
csv = "b.csv"

[[cell]]
name = "from_a"
from = "a"
[[cell.step]]
count = true

[[cell]]
name = "from_b"
from = "b"
[[cell.step]]
count = true

[[pane]]
cell = "from_a"
metric = { label = "A" }
"#;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.csv"), csv(10)).unwrap();
    std::fs::write(dir.path().join("b.csv"), csv(10)).unwrap();
    std::fs::write(dir.path().join("app.toml"), TWO).unwrap();
    let app = dagpane_app::load(&dir.path().join("app.toml")).unwrap();
    let (mut session, versions) = opened(&app);

    std::fs::write(dir.path().join("a.csv"), csv(20)).unwrap();
    std::fs::write(dir.path().join("b.csv"), csv(30)).unwrap();
    let result = refresh(
        &app,
        &mut session,
        &versions,
        RefreshOutcome::IfChanged,
        builder,
    );

    assert_eq!(result.trace.roots.len(), 2, "two sources, one pass");
    assert_eq!(
        result.trace.epoch, 2,
        "the first render was epoch 1; a second commit would make this 3"
    );
    assert_eq!(
        session.get("from_a").unwrap().value().unwrap().as_int(),
        Some(20)
    );
    assert_eq!(
        session.get("from_b").unwrap().value().unwrap().as_int(),
        Some(30)
    );
}
