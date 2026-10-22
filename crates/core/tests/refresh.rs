//! A refresh of an app's data is a new graph, and a live session moves onto it.
//!
//! `Graph::with_sources` and `Session::rebase` are the whole of what a scheduled refresh asks
//! of the engine, and every promise they make is one a refresh that got it wrong would break
//! silently: a viewer's own filter reset to the default, a source hashed once per viewer, the
//! old table kept alive by every page that sat through a refresh, or a cell that should have
//! recomputed keeping its old answer. Each test below is one of those.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use dagpane_core::frame::Frame;
use dagpane_core::value::{Column, Table};
use dagpane_core::{CellError, Graph, Session, SessionError, Value};

fn table(values: &[i64]) -> Value {
    Value::table(
        Table::new(vec![Column::int(
            "amount",
            values.iter().copied().map(Some).collect(),
        )])
        .expect("one column"),
    )
}

/// The sum of every amount at or above the floor.
fn total_above(frame: &dyn Frame, floor: i64) -> i64 {
    (0..frame.rows())
        .filter_map(|r| frame.value_at(r, 0).as_int())
        .filter(|v| *v >= floor)
        .sum()
}

/// `sales` is data, `floor` is a control, `kept` reads both, `label` reads only the control,
/// and `rows` reads only the data. `runs` counts how often `label` actually computes.
fn app(runs: Arc<AtomicUsize>) -> Arc<Graph> {
    let mut b = Graph::builder();
    b.source("sales", table(&[10, 20, 30]));
    b.source("floor", Value::int(15));
    b.cell("kept", ["sales", "floor"], |i| {
        Ok(Value::int(total_above(i.frame(0)?, i.int(1)?)))
    });
    b.cell("rows", ["sales"], |i| {
        Ok(Value::int(i.frame(0)?.rows() as i64))
    });
    b.cell("label", ["floor"], move |i| {
        runs.fetch_add(1, Ordering::SeqCst);
        Ok(Value::int(i.int(0)? * 100))
    });
    b.build().expect("acyclic")
}

fn int(s: &Session, name: &str) -> i64 {
    s.get(name)
        .expect("a cell")
        .value()
        .and_then(Value::as_int)
        .unwrap_or_else(|| panic!("`{name}` holds no int"))
}

fn id(g: &Graph, name: &str) -> dagpane_core::CellId {
    g.id(name).expect("a cell")
}

// ── the new graph ──────────────────────────────────────────────────────────────────────

#[test]
fn a_refreshed_graph_shares_everything_it_did_not_replace() {
    let g = app(Arc::new(AtomicUsize::new(0)));
    let next = g
        .with_sources([("sales", table(&[10, 20, 30, 40]))])
        .unwrap()
        .expect("new data is a new graph");

    let (old, new) = (Session::new(Arc::clone(&g)), Session::new(Arc::new(next)));
    // The control was not named, so both graphs hold the one allocation for it — which is
    // what lets a session carry its own inputs from one to the other untouched.
    assert!(Arc::ptr_eq(
        &old.share(id(&g, "floor")),
        &new.share(id(&g, "floor"))
    ));
    assert!(!Arc::ptr_eq(
        &old.share(id(&g, "sales")),
        &new.share(id(&g, "sales"))
    ));
    // And the new value was weighed when the graph was built: a session over it starts with
    // the digest a session would have computed for itself.
    assert_eq!(
        new.digest_of(id(&g, "sales")),
        dagpane_core::Digestible::digest(new.get("sales").unwrap())
    );
}

#[test]
fn the_same_data_again_is_not_a_new_graph() {
    let g = app(Arc::new(AtomicUsize::new(0)));
    // A file rewritten with the bytes it had: read again, and nothing to build.
    assert!(g
        .with_sources([("sales", table(&[10, 20, 30]))])
        .unwrap()
        .is_none());
    assert!(g.with_sources(std::iter::empty()).unwrap().is_none());

    // One source moved and one did not: the one that did not keeps its allocation.
    let next = g
        .with_sources([
            ("sales", table(&[10, 20, 30, 40])),
            ("floor", Value::int(15)),
        ])
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(
        &Session::new(Arc::clone(&g)).share(id(&g, "floor")),
        &Session::new(Arc::new(next)).share(id(&g, "floor"))
    ));
}

#[test]
fn a_refresh_is_refused_for_whatever_a_session_would_refuse() {
    let g = app(Arc::new(AtomicUsize::new(0)));
    assert_eq!(
        g.with_sources([("nope", table(&[1]))]).unwrap_err(),
        SessionError::UnknownCell("nope".into())
    );
    assert!(matches!(
        g.with_sources([("kept", Value::int(1))]).unwrap_err(),
        SessionError::NotAnInput { .. }
    ));
    assert!(matches!(
        g.with_sources([("sales", Value::int(1))]).unwrap_err(),
        SessionError::TypeMismatch { .. }
    ));
}

// ── a session moving onto it ───────────────────────────────────────────────────────────

#[test]
fn a_moved_session_recomputes_only_below_the_data_and_keeps_its_own_inputs() {
    let runs = Arc::new(AtomicUsize::new(0));
    let g = app(Arc::clone(&runs));
    let mut s = Session::new(Arc::clone(&g));
    s.refresh();
    // The viewer moved the control: this is the state a refresh must not reset.
    s.set("floor", Value::int(25)).unwrap();
    s.commit();
    assert_eq!(int(&s, "kept"), 30);
    let label_runs = runs.load(Ordering::SeqCst);

    let next = Arc::new(
        g.with_sources([("sales", table(&[10, 20, 30, 40]))])
            .unwrap()
            .unwrap(),
    );
    s.rebase(Arc::clone(&next)).unwrap();
    let trace = s.commit();

    assert_eq!(trace.roots, ["sales"], "{}", trace.summary());
    let ran: Vec<&str> = trace.steps.iter().map(|s| s.cell.as_str()).collect();
    assert!(
        !ran.contains(&"label"),
        "a cell that reads only the control was visited: {}",
        trace.summary()
    );
    assert_eq!(runs.load(Ordering::SeqCst), label_runs);
    assert_eq!(int(&s, "floor"), 25, "the viewer's control survived");
    assert_eq!(int(&s, "kept"), 70, "30 + 40, at the viewer's floor");
    assert_eq!(int(&s, "rows"), 4);
}

#[test]
fn a_moved_session_agrees_with_one_opened_on_the_new_graph() {
    let g = app(Arc::new(AtomicUsize::new(0)));
    let mut moved = Session::new(Arc::clone(&g));
    moved.refresh();
    moved.set("floor", Value::int(12)).unwrap();
    moved.commit();

    let next = Arc::new(
        g.with_sources([("sales", table(&[5, 15, 25]))])
            .unwrap()
            .unwrap(),
    );
    moved.rebase(Arc::clone(&next)).unwrap();
    moved.commit();

    let mut fresh = Session::new(Arc::clone(&next));
    fresh.set("floor", Value::int(12)).unwrap();
    fresh.refresh();
    for cell in ["sales", "floor", "kept", "rows", "label"] {
        assert_eq!(
            moved.get(cell).unwrap(),
            fresh.get(cell).unwrap(),
            "`{cell}` differs between a viewer who sat through the refresh and one who \
             arrived after it"
        );
    }
}

#[test]
fn moving_onto_the_new_graph_lets_the_old_data_go() {
    let g = app(Arc::new(AtomicUsize::new(0)));
    let mut s = Session::new(Arc::clone(&g));
    s.refresh();
    let old = Arc::downgrade(&s.share(id(&g, "sales")));

    let next = Arc::new(
        g.with_sources([("sales", table(&[1, 2, 3, 4]))])
            .unwrap()
            .unwrap(),
    );
    s.rebase(Arc::clone(&next)).unwrap();
    s.commit();
    drop(g);

    // Nothing holds the old table any more: not the session's slot, and not the graph it was
    // opened on, which the session let go of. Staging the new table onto a session that kept
    // its old graph would pass every other test in this file and fail this one.
    assert!(
        old.upgrade().is_none(),
        "the pre-refresh table is still alive after every holder moved on"
    );
    assert!(Arc::ptr_eq(s.graph(), &next));
}

#[test]
fn two_sessions_moved_onto_one_graph_share_its_allocation() {
    let g = app(Arc::new(AtomicUsize::new(0)));
    let next = Arc::new(
        g.with_sources([("sales", table(&[7, 8, 9]))])
            .unwrap()
            .unwrap(),
    );
    let mut a = Session::new(Arc::clone(&g));
    let mut b = Session::new(Arc::clone(&g));
    for s in [&mut a, &mut b] {
        s.refresh();
        s.rebase(Arc::clone(&next)).unwrap();
        s.commit();
    }
    let fresh = Session::new(Arc::clone(&next));
    let sales = id(&g, "sales");
    assert!(Arc::ptr_eq(&a.share(sales), &b.share(sales)));
    assert!(
        Arc::ptr_eq(&a.share(sales), &fresh.share(sales)),
        "a moved session holds a copy rather than the graph's allocation"
    );
}

#[test]
fn a_source_the_session_set_itself_is_kept() {
    let g = app(Arc::new(AtomicUsize::new(0)));
    let mut s = Session::new(Arc::clone(&g));
    s.refresh();
    // What `explain --change-column` does: the session's own table, not the graph's.
    s.set("sales", table(&[100])).unwrap();
    s.commit();

    let next = Arc::new(
        g.with_sources([("sales", table(&[1, 2]))])
            .unwrap()
            .unwrap(),
    );
    s.rebase(next).unwrap();
    let trace = s.commit();
    assert!(trace.roots.is_empty(), "{}", trace.summary());
    assert_eq!(int(&s, "rows"), 1, "the session's own table was replaced");

    // And a value staged but not yet committed is the session's own as well.
    let g = app(Arc::new(AtomicUsize::new(0)));
    let mut s = Session::new(Arc::clone(&g));
    s.refresh();
    s.set("sales", table(&[100, 200, 300, 400, 500])).unwrap();
    let next = Arc::new(
        g.with_sources([("sales", table(&[1, 2]))])
            .unwrap()
            .unwrap(),
    );
    s.rebase(next).unwrap();
    s.commit();
    assert_eq!(int(&s, "rows"), 5);

    // A committed value equal to the one it replaced changed nothing, so it is not the
    // session's: the slot still holds the graph's rows, and the next refresh is followed.
    let g = app(Arc::new(AtomicUsize::new(0)));
    let mut s = Session::new(Arc::clone(&g));
    s.refresh();
    s.set("sales", table(&[10, 20, 30])).unwrap();
    assert!(s.commit().roots.is_empty());
    let next = Arc::new(
        g.with_sources([("sales", table(&[1, 2]))])
            .unwrap()
            .unwrap(),
    );
    s.rebase(next).unwrap();
    s.commit();
    assert_eq!(
        int(&s, "rows"),
        2,
        "a value equal to the default kept the old rows"
    );
}

#[test]
fn a_graph_of_another_shape_is_refused_and_nothing_changes() {
    let g = app(Arc::new(AtomicUsize::new(0)));
    let mut s = Session::new(Arc::clone(&g));
    s.refresh();

    // The same manifest compiled twice is two sets of closures, so even a graph built to look
    // identical is another graph: nothing can show that its computations are the same ones.
    let lookalike = app(Arc::new(AtomicUsize::new(0)));
    assert_eq!(s.rebase(lookalike).unwrap_err(), SessionError::OtherGraph);

    let mut b = Graph::builder();
    b.source("sales", table(&[1]));
    b.cell("rows", ["sales"], |i| -> Result<Value, CellError> {
        Ok(Value::int(i.frame(0)?.rows() as i64))
    });
    assert_eq!(
        s.rebase(b.build().unwrap()).unwrap_err(),
        SessionError::OtherGraph
    );

    assert!(Arc::ptr_eq(s.graph(), &g), "a refused move moved nothing");
    assert!(s.commit().roots.is_empty());
}

#[test]
fn moving_onto_the_graph_it_is_already_on_does_nothing() {
    let g = app(Arc::new(AtomicUsize::new(0)));
    let mut s = Session::new(Arc::clone(&g));
    s.refresh();
    s.rebase(Arc::clone(&g)).unwrap();
    assert!(s.commit().steps.is_empty());
}

#[test]
fn two_moves_before_one_commit_land_on_the_newest_graph() {
    // `rebase` stages and the commit is the caller's, so a caller may move twice before it
    // commits. The second move must replace what the first one staged: that value is the graph
    // in between's, not the session's own, and committing it would leave the session on the
    // newest graph holding the rows of the one it passed through.
    let g = app(Arc::new(AtomicUsize::new(0)));
    let mut s = Session::new(Arc::clone(&g));
    s.refresh();
    let middle = Arc::new(
        g.with_sources([("sales", table(&[1, 2]))])
            .unwrap()
            .unwrap(),
    );
    let newest = Arc::new(
        middle
            .with_sources([("sales", table(&[3, 4, 5]))])
            .unwrap()
            .unwrap(),
    );
    let passed_through = Arc::downgrade(&Session::new(Arc::clone(&middle)).share(id(&g, "sales")));

    s.rebase(Arc::clone(&middle)).unwrap();
    s.rebase(Arc::clone(&newest)).unwrap();
    s.commit();
    assert_eq!(
        int(&s, "rows"),
        3,
        "the session holds the rows it passed through"
    );

    let mut fresh = Session::new(Arc::clone(&newest));
    fresh.refresh();
    for cell in ["sales", "kept", "rows"] {
        assert_eq!(s.get(cell).unwrap(), fresh.get(cell).unwrap(), "`{cell}`");
    }
    drop(middle);
    assert!(
        passed_through.upgrade().is_none(),
        "the rows of the graph in between are still held"
    );
}

#[test]
fn rows_that_come_back_before_a_commit_are_the_new_graphs_and_follow_the_next_refresh() {
    // A, then B, then A again as a new copy — two moves before one commit. The last rows equal
    // the ones the session already holds, so nothing changes and nothing recomputes; but the
    // session must hold the new graph's copy, or it pins the old one and the next refresh
    // takes that for a value the session set itself and leaves it behind. Both ways a pass
    // can drain what was staged: a commit, and a full recompute.
    for full in [false, true] {
        let g = app(Arc::new(AtomicUsize::new(0)));
        let mut s = Session::new(Arc::clone(&g));
        s.refresh();
        let sales = id(&g, "sales");
        let away = Arc::new(
            g.with_sources([("sales", table(&[1, 2]))])
                .unwrap()
                .unwrap(),
        );
        let back = Arc::new(
            away.with_sources([("sales", table(&[10, 20, 30]))])
                .unwrap()
                .expect("the rows from before `away` are new rows to `away`"),
        );
        let original = Arc::downgrade(&s.share(sales));

        s.rebase(Arc::clone(&away)).unwrap();
        s.rebase(Arc::clone(&back)).unwrap();
        let trace = if full { s.refresh() } else { s.commit() };
        assert!(
            trace.roots.is_empty(),
            "full={full}: rows equal to the ones held were a change"
        );
        assert!(
            Arc::ptr_eq(
                &s.share(sales),
                &Session::new(Arc::clone(&back)).share(sales)
            ),
            "full={full}: the session kept its own copy instead of the graph's"
        );
        drop((g, away));
        assert!(
            original.upgrade().is_none(),
            "full={full}: the first copy of the rows is still pinned"
        );

        let next = Arc::new(
            back.with_sources([("sales", table(&[7, 8, 9, 10]))])
                .unwrap()
                .unwrap(),
        );
        s.rebase(next).unwrap();
        s.commit();
        assert_eq!(
            int(&s, "rows"),
            4,
            "full={full}: the refresh after the rows came back left the session behind"
        );
    }
}

#[test]
fn any_number_of_moves_before_a_commit_lands_where_a_later_viewer_would() {
    // The two tests above are the sequences review found by hand. This is the property behind
    // them, over sequences nobody picked: between commits a session moves onto one to four
    // graphs, each over rows drawn from a pool of three — so rows come back as often as not —
    // and sometimes the viewer moves their control too. After every commit the session must
    // agree with a viewer who opened on the newest graph at the same control, hold the
    // graph's own copy of the rows, and have let go of every graph it moved off.
    let pool: [&[i64]; 3] = [&[10, 20, 30], &[5, 40], &[1, 2, 3, 4]];
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = |n: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % n as u64) as usize
    };

    let mut graph = app(Arc::new(AtomicUsize::new(0)));
    let mut s = Session::new(Arc::clone(&graph));
    s.refresh();
    let mut floor = 15;
    let sales = id(&graph, "sales");
    let mut commits = 0;
    let mut returns = 0;
    for _ in 0..200 {
        let mut left = Vec::new();
        for _ in 0..1 + next(4) {
            let rows = pool[next(pool.len())];
            if let Some(moved) = graph.with_sources([("sales", table(rows))]).unwrap() {
                left.push(Arc::downgrade(&graph));
                graph = Arc::new(moved);
                s.rebase(Arc::clone(&graph)).unwrap();
            }
        }
        if next(3) == 0 {
            floor = [0, 15, 25][next(3)];
            s.set("floor", Value::int(floor)).unwrap();
        }
        let trace = s.commit();
        commits += 1;
        if trace.roots.is_empty() && !left.is_empty() {
            returns += 1;
        }

        let mut later = Session::new(Arc::clone(&graph));
        later.set("floor", Value::int(floor)).unwrap();
        later.refresh();
        for cell in ["sales", "floor", "kept", "rows", "label"] {
            assert_eq!(
                s.get(cell).unwrap(),
                later.get(cell).unwrap(),
                "commit {commits}: `{cell}` differs from a viewer who opened on the newest graph"
            );
        }
        assert!(
            Arc::ptr_eq(&s.share(sales), &later.share(sales)),
            "commit {commits}: the session holds its own copy of the rows, not the graph's"
        );
        assert!(
            left.iter().all(|g| g.upgrade().is_none()),
            "commit {commits}: a graph the session moved off is still held"
        );
    }
    // A corpus where rows never came back would not exercise what the tests above are about.
    assert!(
        returns > 10,
        "only {returns} of {commits} commits saw rows come back"
    );
}
