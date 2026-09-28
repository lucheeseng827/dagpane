//! `place = "client"`, from a manifest through to two running halves.
//!
//! `crates/core` already proves the split is value-preserving over two hundred *generated*
//! graphs. Those graphs are made of arithmetic on integers and a table constructor, which is
//! the right shape for finding scheduler bugs and the wrong shape for finding this one: the
//! values that actually cross a real cut are **frames** produced by `filter`, `group_by` and
//! `sort`, loaded from a CSV, with column digests and row constraints attached.
//!
//! So this file re-runs the same property one layer up — a compiled manifest, the bundled
//! sales data, the real verbs — and asserts that where a cell runs changes nothing about
//! what it holds.

use std::path::Path;
use std::sync::Arc;

use std::collections::BTreeMap;

use dagpane_app::manifest::Sources;
use dagpane_app::{compile_with, load, manifest, ManifestError};
use dagpane_connect::file::FileFormat;
use dagpane_connect::{BytesSource, Source};
use dagpane_core::{Cut, Placement, Session, Value};

fn placed() -> dagpane_app::App {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/apps/20-placed.toml");
    load(&path).expect("the placed example must compile")
}

/// Compile a manifest against bytes rather than a directory, so a fixture needs no file.
fn compile_text(toml: &str) -> Result<dagpane_app::App, ManifestError> {
    let parsed = manifest::parse(toml).expect("valid TOML");
    let mut sources: BTreeMap<String, Arc<dyn Source>> = BTreeMap::new();
    // Keyed by the SOURCE NAME, which is what `Sources::Bound` looks up — not by the `csv`
    // path, which a host with no filesystem has nothing to do with.
    sources.insert(
        "rows".to_string(),
        Arc::new(BytesSource::new(
            "rows.csv",
            FileFormat::Csv,
            "region,amount\nnorth,10\nsouth,30\n",
        )),
    );
    let renderers = BTreeMap::new();
    compile_with(
        &parsed,
        Sources::Bound {
            sources: &sources,
            renderers: &renderers,
        },
    )
}

/// A control, a cell that reads it, and a cell that reads that. Enough to make the one edge
/// the rule is about, and small enough that the rule is the only thing under test.
const CHAIN: &str = r#"
[app]
title = "t"

[[source]]
name = "rows"
csv = "rows.csv"

[[input]]
name = "knob"
slider = { min = 0.0, max = 100.0, default = 0.0 }
PLACE_KNOB

[[cell]]
name = "filtered"
from = "rows"
PLACE_FILTERED
[[cell.step]]
filter = { column = "amount", op = "ge", param = "knob" }

[[cell]]
name = "counted"
from = "filtered"
PLACE_COUNTED
[[cell.step]]
count = true

# A pane on `filtered` matters: when only `counted` is placed, `filtered` is the BOUNDARY,
# present in both halves. Ownership has to pick one of them for this pane or the page gets
# two writers for one card.
[[pane]]
cell = "filtered"
table = { max_rows = 5 }

[[pane]]
cell = "counted"
metric = { label = "Rows" }
"#;

/// `CHAIN` with each placement either set or removed.
fn chain(knob: bool, filtered: bool, counted: bool) -> String {
    let word = |on: bool| if on { "place = \"client\"" } else { "" };
    CHAIN
        .replace("PLACE_KNOB", word(knob))
        .replace("PLACE_FILTERED", word(filtered))
        .replace("PLACE_COUNTED", word(counted))
}

#[test]
fn an_app_that_names_no_placement_is_the_app_it_always_was() {
    let app = placed();
    let plain = {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/sales.toml");
        load(&path).expect("the bundled example")
    };
    assert!(!plain.cut.is_split(), "sales.toml declares no placement");
    assert!(plain.cut.boundary().is_empty());
    // And the placed one does, so the test above is not passing by accident.
    assert!(app.cut.is_split());
}

#[test]
fn the_frontier_is_the_data_and_nothing_else() {
    let app = placed();
    let names: Vec<&str> = app
        .cut
        .boundary()
        .iter()
        .map(|&id| app.graph.name(id))
        .collect();
    // `sales` crosses because the page filters it. `all_time_revenue` is a server cell too,
    // but nothing in the page reads it, so it never goes on the wire — it reaches the viewer
    // as a rendered pane like any other server-side pane would.
    assert_eq!(names, vec!["sales"]);
    assert_eq!(app.cut.client_cells(), 4);
    assert_eq!(
        app.cut.placement(app.graph.id("sales").unwrap()),
        Placement::Server
    );
    assert_eq!(
        app.cut.placement(app.graph.id("filtered").unwrap()),
        Placement::Client
    );
}

#[test]
fn the_slider_is_answered_without_the_network() {
    // The claim the example's comment makes, checked. `min_amount` and its whole downstream
    // closure are in the page, so moving it cannot require a message.
    let app = placed();
    let knob = app.graph.id("min_amount").expect("a control");
    assert!(app.cut.is_local(&app.graph, knob));

    // And the negative: the data is not local, so a refresh of it does cross.
    let sales = app.graph.id("sales").expect("the source");
    assert!(!app.cut.is_local(&app.graph, sales));
}

#[test]
fn a_server_cell_reading_a_client_cell_is_refused_at_compile_time() {
    // The control is in the page and the cell reading it is not.
    let e = compile_text(&chain(true, false, false)).expect_err("backflow");
    let text = e.to_string();
    assert!(text.contains("`filtered`"), "{text}");
    assert!(text.contains("`knob`"), "{text}");
    assert!(text.contains("Move `filtered` to the client"), "{text}");
    assert!(text.contains("or `knob` to the server"), "{text}");

    // One further out is still backflow, one edge later: `counted` is on the server.
    let e = compile_text(&chain(true, true, false)).expect_err("backflow");
    assert!(e.to_string().contains("`counted`"), "{e}");

    // Placing the whole chain is fine. So is placing none of it, and so is placing only the
    // tail — a value may always flow outward, and `counted` alone in the page is a legal cut
    // whose frontier is `filtered`.
    assert!(compile_text(&chain(true, true, true)).is_ok());
    assert!(compile_text(&chain(false, false, false)).is_ok());
    let tail = compile_text(&chain(false, false, true)).expect("outward is fine");
    let frontier: Vec<&str> = tail
        .cut
        .boundary()
        .iter()
        .map(|&id| tail.graph.name(id))
        .collect();
    assert_eq!(frontier, vec!["filtered"]);
}

#[test]
fn a_placement_naming_a_cell_that_does_not_exist_is_a_compile_error() {
    // `place` can only appear on a cell or input that exists, so the unknown-name path is
    // unreachable from TOML — which is worth pinning, because the check lives in
    // `Cut::of_client` and a future surface (a CLI flag, a control plane) could reach it.
    let app = placed();
    assert!(Cut::of_client(&app.graph, &["not_a_cell"]).is_err());
}

/// The property: **where a cell runs does not change what it holds.**
///
/// Drives both halves the way a transport would — the server passes, the frontier crosses,
/// the client commits once — and compares every cell against an undivided session over the
/// same app after every interaction.
#[test]
fn a_split_app_computes_exactly_what_an_undivided_one_does() {
    let app = placed();
    let split = app.graph.split(&app.cut);

    let mut whole = Session::new(Arc::clone(&app.graph));
    let mut server = Session::new(Arc::clone(&split.server));
    let mut client = Session::new(Arc::clone(&split.client));

    whole.refresh();
    server.refresh();
    split
        .deliver(&mut client, &split.full_frontier(&server))
        .expect("both halves came from one graph");
    client.refresh();

    // A frame crossed, not an integer: this is the coverage the generated oracle does not
    // have. 600 rows of CSV, with the column digests the engine takes at production time.
    let crossed = client.get("sales").expect("the frontier source");
    assert_eq!(
        crossed.value().and_then(|v| v.as_frame()).map(|f| f.rows()),
        Some(600),
        "the rows did not survive the crossing"
    );

    for step in [0.0, 200.0, 400.0, 400.0, 25.0] {
        // Every control in this app is client-placed, so the server never runs and there is
        // nothing to deliver. Asserted rather than assumed: if a future edit moved a cell to
        // the server, this loop would keep passing while quietly using a wire, and the
        // `is_empty` below is what stops that.
        client
            .set("min_amount", Value::float(step))
            .expect("a control");
        client.commit();

        let epoch = server.epoch();
        let frontier = split.frontier(&server, epoch);
        assert!(
            frontier.is_empty(),
            "a client-only interaction produced a frontier: {frontier:?}"
        );

        whole
            .set("min_amount", Value::float(step))
            .expect("a control");
        whole.commit();

        for cell in [
            "filtered",
            "order_count",
            "region_totals",
            "all_time_revenue",
        ] {
            let undivided = whole.get(cell).expect("a cell of the app");
            let divided = match server.graph().id(cell) {
                Some(_) => server.get(cell).expect("a server cell"),
                None => client.get(cell).expect("a client cell"),
            };
            assert_eq!(
                undivided, divided,
                "`{cell}` disagrees with min_amount at {step}"
            );
        }
    }
}

#[test]
fn a_server_side_change_patches_the_server_side_pane() {
    // The half of the protocol the interaction test does not reach. Every control in the
    // placed example is page-side, so the server never runs a pass there and its `patch` is
    // never exercised — which is exactly where a cell id resolved against the wrong graph
    // hides. The server half numbers its cells 0..2; the whole app numbers the same names
    // 0..6, so id 1 is `all_time_revenue` on one and `min_amount` on the other.
    let (_, mut server, _) = halves();

    // The source moving is the one thing that changes a server cell here — no widget is
    // bound to it, which is why this goes through `set_source` and not `set`.
    let table = dagpane_core::Table::new(vec![
        dagpane_core::Column::text("region", vec![Some("north".into())]),
        dagpane_core::Column::float("amount", vec![Some(7.0)]),
    ])
    .expect("one row count");
    server
        .set_source("sales", Value::frame(Arc::new(table)))
        .expect("the server owns the data");
    let (trace, panes) = server.commit();

    assert!(trace.evaluated() > 0, "the server recomputed");
    let ids: Vec<&str> = panes.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["all_time_revenue"],
        "the server's own pane moved and must be on the wire"
    );
}

#[test]
fn the_server_half_never_recomputes_what_the_page_owns() {
    // The economy claim, as a count rather than as a sentence. The server's graph does not
    // contain the client's cells at all, so a `refresh` of it — the most expensive pass
    // there is — cannot touch them.
    let app = placed();
    let split = app.graph.split(&app.cut);
    let mut server = Session::new(Arc::clone(&split.server));
    let trace = server.refresh();

    assert_eq!(split.server.len(), 2, "sales and all_time_revenue");
    assert!(
        split.server.id("filtered").is_none(),
        "the server kept a cell the page owns"
    );
    assert!(
        trace.steps.iter().all(|s| s.cell != "region_totals"),
        "the server evaluated a client cell"
    );
}

// ── serving the split ──────────────────────────────────────────────────────────────────
//
// Everything above drives `Session` directly, which proves the engine's half. These drive
// `AppSession` through the **actual wire messages**, which is the half a browser sees: who
// owns which pane, what an `init` carries, and what a `patch` costs.

use dagpane_app::{AppSession, BoundaryValue, ServerMessage};

/// Both halves of the placed example, opened the way a server and a page open them.
fn halves() -> (Arc<dagpane_app::App>, AppSession, AppSession) {
    let app = Arc::new(placed());
    let (server, _) = AppSession::open_side(Arc::clone(&app), Placement::Server);
    let (client, _) = AppSession::open_side(Arc::clone(&app), Placement::Client);
    (app, server, client)
}

/// Pane ids each side claims, in display order.
fn owned(session: &AppSession, app: &dagpane_app::App) -> Vec<String> {
    app.panes
        .iter()
        .filter(|p| session.owns(p))
        .map(|p| p.id.clone())
        .collect()
}

#[test]
fn every_pane_belongs_to_exactly_one_side() {
    let (app, server, client) = halves();
    let s = owned(&server, &app);
    let c = owned(&client, &app);

    // The partition is the layout contract: a page draws `app.panes` in order and fills each
    // from whichever half sent it, so a pane owned by neither is a permanent blank and a pane
    // owned by both is two writers racing for one card.
    assert_eq!(s.len() + c.len(), app.panes.len(), "{s:?} + {c:?}");
    for pane in &app.panes {
        assert!(
            server.owns(pane) != client.owns(pane),
            "`{}` is owned by both halves or by neither",
            pane.id
        );
    }
    assert_eq!(s, vec!["all_time_revenue"]);
    assert_eq!(c, vec!["order_count", "region_totals"]);
}

#[test]
fn a_pane_on_the_boundary_belongs_to_the_server_alone() {
    // The case the app above does not have, and the reason ownership asks the CUT rather
    // than this half's graph. A boundary cell is in both graphs — the server computes it,
    // the page holds a copy fed by the frontier — so "is its cell in my graph" hands its
    // pane to both sides, and the page gets one card from two writers.
    let app = Arc::new(compile_text(&chain(false, false, true)).expect("outward is fine"));
    let frontier: Vec<&str> = app
        .cut
        .boundary()
        .iter()
        .map(|&id| app.graph.name(id))
        .collect();
    assert_eq!(frontier, vec!["filtered"], "the fixture's boundary");

    let (server, _) = AppSession::open_side(Arc::clone(&app), Placement::Server);
    let (client, _) = AppSession::open_side(Arc::clone(&app), Placement::Client);

    // Both halves HOLD `filtered`; exactly one may draw it.
    assert!(server.session().graph().id("filtered").is_some());
    assert!(client.session().graph().id("filtered").is_some());

    let boundary_pane = app
        .panes
        .iter()
        .find(|p| p.cell == "filtered")
        .expect("the fixture has one");
    assert!(server.owns(boundary_pane), "the server computes it");
    assert!(
        !client.owns(boundary_pane),
        "the page holds a copy and must not also draw it"
    );

    for pane in &app.panes {
        assert!(
            server.owns(pane) != client.owns(pane),
            "`{}` has two owners or none",
            pane.id
        );
    }
}

#[test]
fn the_opening_frame_carries_the_whole_frontier_and_only_the_servers_panes() {
    let (_, mut server, _) = halves();
    let trace = server.session().graph().len();
    assert_eq!(
        trace, 2,
        "the server half is `sales` and `all_time_revenue`"
    );

    let init = server.init_message(&dagpane_core::Trace::default(), None);
    let ServerMessage::Init {
        views, frontier, ..
    } = &init
    else {
        panic!("expected an init, got {init:?}");
    };

    assert_eq!(
        views.len(),
        1,
        "only the server's own pane is rendered here"
    );
    assert_eq!(views[0].id, "all_time_revenue");
    // The FULL frontier, because the page's boundary sources start at null. A delta here
    // would leave the page computing from a value it was never meant to see.
    assert_eq!(frontier.len(), 1);
    assert_eq!(frontier[0].cell, "sales");
    assert!(
        frontier[0]
            .outcome
            .value()
            .and_then(|v| v.as_frame())
            .is_some(),
        "the rows themselves cross, not a reference to them"
    );
}

#[test]
fn a_page_side_interaction_never_reaches_the_server() {
    let (app, server, mut client) = halves();

    // Seed the page the way a connection does.
    let full = BoundaryValue::of(&server.full_frontier());
    client
        .deliver(&BoundaryValue::into_frontier(&full, 1))
        .expect("one manifest, two halves");
    client.commit();

    let before = server.session().epoch();
    let mut values = BTreeMap::new();
    values.insert("min_amount".to_string(), Value::float(400.0));
    client.set(&values).expect("a page-side control");
    let (trace, panes) = client.commit();

    // The page repainted, and the server does not know it happened. That is the product of
    // the whole feature stated as two assertions.
    assert!(!panes.is_empty(), "the page's own panes moved");
    assert!(trace.evaluated() > 0);
    assert_eq!(server.session().epoch(), before, "the server ran a pass");
    assert!(
        server.frontier(before).is_empty(),
        "there was nothing to send"
    );

    // And the server still refuses to be told about a control it does not own — with a
    // message naming where it actually lives, not a bare `unknown cell`.
    let (_, mut server2, _) = halves();
    let e = server2
        .set(&values)
        .expect_err("the server does not own it");
    assert!(e.contains("client"), "{e}");
    assert!(e.contains("min_amount"), "{e}");
    let _ = app;
}

#[test]
fn the_two_halves_together_show_what_one_undivided_session_shows() {
    // The property one layer up from `a_split_app_computes_exactly_what_an_undivided_one_does`:
    // not "the cells agree" but "the PAGE agrees" — same panes, same rendered views, produced
    // by two sessions talking over the real messages instead of one session talking to itself.
    let app = Arc::new(placed());
    let (mut whole, _) = AppSession::open(Arc::clone(&app));
    let (mut server, _) = AppSession::open_side(Arc::clone(&app), Placement::Server);
    let (mut client, _) = AppSession::open_side(Arc::clone(&app), Placement::Client);

    let full = BoundaryValue::of(&server.full_frontier());
    client
        .deliver(&BoundaryValue::into_frontier(&full, 1))
        .expect("one manifest");
    client.commit();

    let mut expected: BTreeMap<String, dagpane_app::View> = whole
        .full_views()
        .into_iter()
        .map(|u| (u.id, u.view))
        .collect();
    let mut actual: BTreeMap<String, dagpane_app::View> = server
        .full_views()
        .into_iter()
        .chain(client.full_views())
        .map(|u| (u.id, u.view))
        .collect();
    assert_eq!(actual, expected, "the first paint differs");

    for step in [200.0, 400.0, 0.0] {
        let mut values = BTreeMap::new();
        values.insert("min_amount".to_string(), Value::float(step));

        // The page's side. Nothing crosses, so the server is not even asked.
        client.set(&values).expect("a page-side control");
        for u in client.commit().1 {
            actual.insert(u.id, u.view);
        }

        whole.set(&values).expect("a control");
        for u in whole.commit().1 {
            expected.insert(u.id, u.view);
        }

        assert_eq!(actual, expected, "the page differs at min_amount {step}");
    }
}

// ── compiling the page's half without the data ─────────────────────────────────────────
//
// `compile_with` loads every `[[source]]` because a CSV's column types are decided by
// reading it. That is a problem for exactly the cut worth making — filter in the page, rows
// on the server — since the page must type-check a pipeline over data it is not supposed to
// have. `SchemaSource` is the answer: the shape crosses, the rows do not, and the real values
// arrive afterwards as the frontier.
//
// The whole approach rests on one property, so it gets a test rather than a paragraph.

/// The placed example compiled against shapes rather than rows.
fn compiled_from_schemas_only(real: &dagpane_app::App) -> dagpane_app::App {
    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/apps/20-placed.toml"),
    )
    .expect("the placed example");
    let parsed = manifest::parse(&text).expect("valid TOML");

    let mut sources: BTreeMap<String, Arc<dyn Source>> = BTreeMap::new();
    for bound in &real.sources {
        let schema = bound.source.schema().expect("the server could read it");
        sources.insert(
            bound.cell.clone(),
            Arc::new(dagpane_app::SchemaSource::new(bound.cell.clone(), schema)),
        );
    }
    let renderers = BTreeMap::new();
    compile_with(
        &parsed,
        Sources::Bound {
            sources: &sources,
            renderers: &renderers,
        },
    )
    .expect("a shape is enough to compile against")
}

#[test]
fn a_half_compiled_from_shapes_is_the_same_half() {
    let real = placed();
    let from_shapes = compiled_from_schemas_only(&real);

    // The cut, the frontier and the client half must all come out identical: the page is
    // going to run this graph and the server is going to feed it, and the two only line up
    // if compiling against a shape decided everything compiling against rows decided.
    let names = |a: &dagpane_app::App| -> Vec<String> {
        let split = a.split.as_ref().expect("placed");
        split
            .client
            .order()
            .iter()
            .map(|&id| format!("{}={}", id.index(), split.client.name(id)))
            .collect()
    };
    assert_eq!(
        names(&real),
        names(&from_shapes),
        "the client halves differ"
    );
    assert_eq!(
        real.split.as_ref().unwrap().boundary,
        from_shapes.split.as_ref().unwrap().boundary,
        "the frontier differs"
    );
    assert_eq!(real.cut.client_cells(), from_shapes.cut.client_cells());

    // And it computes the same answers once the rows arrive. This is the sequence a served
    // split actually performs: the page compiles from shapes, the server sends the frontier,
    // the page commits once.
    let real = Arc::new(real);
    let from_shapes = Arc::new(from_shapes);
    let (server, _) = AppSession::open_side(Arc::clone(&real), Placement::Server);
    let (mut page, _) = AppSession::open_side(Arc::clone(&from_shapes), Placement::Client);
    let (mut reference, _) = AppSession::open_side(Arc::clone(&real), Placement::Client);

    let full = BoundaryValue::of(&server.full_frontier());
    let frontier = BoundaryValue::into_frontier(&full, 1);
    page.deliver(&frontier).expect("the shapes agreed");
    reference.deliver(&frontier).expect("one manifest");
    page.commit();
    reference.commit();

    let views = |s: &mut AppSession| -> BTreeMap<String, dagpane_app::View> {
        s.full_views().into_iter().map(|u| (u.id, u.view)).collect()
    };
    assert_eq!(
        views(&mut page),
        views(&mut reference),
        "a page compiled from shapes drew something different once the rows arrived"
    );
    // Not vacuous: the rows really did cross and really were used.
    assert_eq!(
        page.session()
            .get("filtered")
            .expect("a client cell")
            .value()
            .and_then(|v| v.as_frame())
            .map(|f| f.rows()),
        Some(600)
    );
}

#[test]
fn a_page_can_boot_from_the_opening_frame_alone() {
    // The whole point of `ClientHalf`, end to end and with nothing smuggled across. The page
    // gets ONE message — the server's `init` — and from it alone must compile its half, seed
    // it, and draw exactly what a page holding the real data would draw. No file is read on
    // the page's side and no CSV crosses: only a manifest, some column names and types, and
    // the frontier.
    let real = Arc::new(placed());
    let (mut server, _) = AppSession::open_side(Arc::clone(&real), Placement::Server);
    let init = server.init_message(&dagpane_core::Trace::default(), None);

    // Cross the wire for real, so nothing in this test can reach the server's memory.
    let json = serde_json::to_string(&init).expect("the init encodes");
    let arrived: ServerMessage = serde_json::from_str(&json).expect("and decodes");
    let ServerMessage::Init {
        client_half,
        frontier,
        ..
    } = arrived
    else {
        panic!("expected an init");
    };
    let boot = client_half.expect("a split app describes its other half");

    // Everything below uses ONLY `boot` and `frontier`.
    let parsed = manifest::parse(&boot.manifest).expect("the re-emitted manifest parses");
    let mut sources: BTreeMap<String, Arc<dyn Source>> = BTreeMap::new();
    for (name, columns) in &boot.sources {
        let shape = columns.iter().map(|c| (c.name.clone(), c.ty)).collect();
        sources.insert(
            name.clone(),
            Arc::new(dagpane_app::SchemaSource::new(name.clone(), shape)),
        );
    }
    let renderers = BTreeMap::new();
    let page_app = Arc::new(
        compile_with(
            &parsed,
            Sources::Bound {
                sources: &sources,
                renderers: &renderers,
            },
        )
        .expect("shapes are enough"),
    );

    let (mut page, _) = AppSession::open_side(Arc::clone(&page_app), Placement::Client);
    page.deliver(&BoundaryValue::into_frontier(&frontier, 1))
        .expect("the halves agree");
    page.commit();

    // The rows arrived, and they are the server's rows.
    assert_eq!(
        page.session()
            .get("filtered")
            .expect("a page cell")
            .value()
            .and_then(|v| v.as_frame())
            .map(|f| f.rows()),
        Some(600),
        "the page computed from the shape instead of the rows"
    );

    // And the two halves together are the undivided app, which is the property that has to
    // survive the whole round trip.
    let (mut whole, _) = AppSession::open(Arc::clone(&real));
    let expected: BTreeMap<String, dagpane_app::View> = whole
        .full_views()
        .into_iter()
        .map(|u| (u.id, u.view))
        .collect();
    let actual: BTreeMap<String, dagpane_app::View> = server
        .full_views()
        .into_iter()
        .chain(page.full_views())
        .map(|u| (u.id, u.view))
        .collect();
    assert_eq!(actual, expected);

    // What is cheap here and what is not, stated as numbers rather than hoped for.
    //
    // The BOOT BLOCK is a manifest and some column names: small, and constant in the size of
    // the data. The FRONTIER is the data — 600 rows of it — and the opening frame is
    // therefore as big as the app's rows, which is the cost of cutting below the data and the
    // same cost `dagpane export` pays. Asserting the whole message is small would be
    // asserting the feature does not work.
    let boot_bytes = serde_json::to_string(&boot).expect("encodes").len();
    assert!(
        boot_bytes < 4096,
        "the boot block should be a shape, not a table: {boot_bytes} bytes"
    );
    assert!(
        json.len() > boot_bytes * 4,
        "the frontier carries the rows, so the opening frame is dominated by them: \
         {} bytes total against {boot_bytes} of boot",
        json.len()
    );
}

#[test]
fn a_page_whose_half_declares_a_renderer_gets_the_renderer() {
    // A `custom` pane names a drawing that a `[app] renderers` script registers, and
    // `compile_with` under `Sources::Bound` demands those bytes by the path the manifest
    // declared — a declared script that is not supplied is `ManifestError::Renderer`, the
    // same error a missing file is, because a `custom` pane with no renderer is a blank card.
    //
    // So a `ClientHalf` carrying the manifest and the schemas and nothing else is enough only
    // for an app that declares no renderer. Add one and the page cannot compile its half at
    // all — not draw it wrong, not draw it late: fail at `compile_with`, holding a manifest
    // that names a script it was never given.
    //
    // Mutation check: dropping `renderers` from `ClientHalf::of` fails this at the `expect`
    // below, with "was not supplied to this host".
    const WITH_RENDERER: &str = r#"
[app]
title = "t"
renderers = ["draw.js"]

[[source]]
name = "rows"
csv = "rows.csv"

[[input]]
name = "cut_off"
slider = { min = 0.0, max = 100.0, step = 1.0, default = 0.0 }
place = "client"

[[cell]]
name = "kept"
from = "rows"
place = "client"
[[cell.step]]
filter = { column = "amount", op = "ge", param = "cut_off" }

[[pane]]
cell = "kept"
custom = { renderer = "sparkline" }
"#;

    let parsed = manifest::parse(WITH_RENDERER).expect("valid TOML");
    let mut sources: BTreeMap<String, Arc<dyn Source>> = BTreeMap::new();
    sources.insert(
        "rows".to_string(),
        Arc::new(BytesSource::new(
            "rows.csv",
            FileFormat::Csv,
            "region,amount\nnorth,10\nsouth,30\n",
        )),
    );
    let mut renderers = BTreeMap::new();
    renderers.insert(
        "draw.js".to_string(),
        "export const sparkline = () => {};".to_string(),
    );
    let real = Arc::new(
        compile_with(
            &parsed,
            Sources::Bound {
                sources: &sources,
                renderers: &renderers,
            },
        )
        .expect("the server compiles it, having the bytes on disk"),
    );

    // The opening frame, across a real encode/decode so nothing reaches the server's memory.
    let (mut server, _) = AppSession::open_side(Arc::clone(&real), Placement::Server);
    let init = server.init_message(&dagpane_core::Trace::default(), None);
    let json = serde_json::to_string(&init).expect("the init encodes");
    let arrived: ServerMessage = serde_json::from_str(&json).expect("and decodes");
    let ServerMessage::Init { client_half, .. } = arrived else {
        panic!("expected an init");
    };
    let boot = client_half.expect("a split app describes its other half");

    // From `boot` alone, exactly as a page would.
    let page_manifest = manifest::parse(&boot.manifest).expect("the re-emitted manifest parses");
    let mut page_sources: BTreeMap<String, Arc<dyn Source>> = BTreeMap::new();
    for (name, columns) in &boot.sources {
        let shape = columns.iter().map(|c| (c.name.clone(), c.ty)).collect();
        page_sources.insert(
            name.clone(),
            Arc::new(dagpane_app::SchemaSource::new(name.clone(), shape)),
        );
    }
    let page_app = compile_with(
        &page_manifest,
        Sources::Bound {
            sources: &page_sources,
            renderers: &boot.renderers,
        },
    )
    .expect("the opening frame carries everything the page's half needs to compile");

    // And the bytes are the server's bytes, not a path the page would have to go and fetch.
    assert_eq!(page_app.renderers.len(), 1);
    assert_eq!(page_app.renderers[0].path, "draw.js");
    assert_eq!(
        page_app.renderers[0].source,
        "export const sparkline = () => {};"
    );
}
