//! The semantics of a recompute pass, pinned.
//!
//! Every test here is a property somebody could plausibly break while making the engine
//! faster. The unit tests inside the crate check the parts; these check the promise.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use dagpane_core::{BuildError, CellError, Graph, Session, SessionError, StepOutcome, Value};

/// A counter a cell bumps every time its compute actually runs. The whole test suite is
/// really about this number.
fn counter() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

fn count(c: &Arc<AtomicUsize>) -> usize {
    c.load(Ordering::SeqCst)
}

// ── the first pass, and what an interaction costs after it ─────────────────────────────

#[test]
fn the_first_pass_computes_everything_and_says_so() {
    let mut b = Graph::builder();
    b.source("x", Value::int(1));
    b.cell("a", ["x"], |i| Ok(Value::int(i.int(0)? + 1)));
    b.cell("c", ["a"], |i| Ok(Value::int(i.int(0)? * 2)));
    let mut s = Session::new(b.build().unwrap());

    let t = s.refresh();
    assert_eq!(t.total_cells, 3);
    assert_eq!(t.evaluated(), 2, "both computed cells run once");
    assert_eq!(t.reused(), 0);
    assert_eq!(s.get("c").unwrap().value().unwrap().as_int(), Some(4));
}

#[test]
fn an_interaction_visits_only_the_dirty_closure() {
    // 200 cells; three of them depend on `pick`. This is the product claim as an assertion.
    let mut b = Graph::builder();
    b.source("pick", Value::int(0));
    b.source("other", Value::int(0));
    b.cell("near_1", ["pick"], |i| Ok(Value::int(i.int(0)? + 1)));
    b.cell("near_2", ["near_1"], |i| Ok(Value::int(i.int(0)? + 1)));
    b.cell("near_3", ["near_2"], |i| Ok(Value::int(i.int(0)? + 1)));
    for n in 0..195 {
        b.cell(format!("far_{n}"), ["other"], move |i| {
            Ok(Value::int(i.int(0)? + n as i64))
        });
    }
    let graph = b.build().unwrap();
    assert_eq!(graph.len(), 200);

    let mut s = Session::new(graph);
    let first = s.refresh();
    assert_eq!(first.evaluated(), 198, "the first render is a full pass");

    s.set("pick", Value::int(1)).unwrap();
    let t = s.commit();

    assert_eq!(
        t.visited(),
        4,
        "the source plus its three dependents: {}",
        t.summary()
    );
    assert_eq!(t.evaluated(), 3);
    assert_eq!(t.untouched(), 196);
    assert!(
        !t.steps.iter().any(|s| s.cell.starts_with("far_")),
        "no cell on the unrelated branch was even looked at"
    );
}

#[test]
fn an_unrelated_branch_is_never_visited() {
    let left = counter();
    let right = counter();
    let (l, r) = (Arc::clone(&left), Arc::clone(&right));

    let mut b = Graph::builder();
    b.source("l_in", Value::int(0));
    b.source("r_in", Value::int(0));
    b.cell("l_out", ["l_in"], move |i| {
        l.fetch_add(1, Ordering::SeqCst);
        Ok(Value::int(i.int(0)?))
    });
    b.cell("r_out", ["r_in"], move |i| {
        r.fetch_add(1, Ordering::SeqCst);
        Ok(Value::int(i.int(0)?))
    });
    let mut s = Session::new(b.build().unwrap());
    s.refresh();
    assert_eq!((count(&left), count(&right)), (1, 1));

    s.set("l_in", Value::int(9)).unwrap();
    s.commit();
    assert_eq!(
        (count(&left), count(&right)),
        (2, 1),
        "setting the left input must not run the right cell"
    );
}

// ── glitch freedom ─────────────────────────────────────────────────────────────────────

#[test]
fn a_diamond_join_runs_once_per_pass() {
    let joins = counter();
    let j = Arc::clone(&joins);

    let mut b = Graph::builder();
    b.source("a", Value::int(1));
    b.cell("b", ["a"], |i| Ok(Value::int(i.int(0)? + 1)));
    b.cell("c", ["a"], |i| Ok(Value::int(i.int(0)? * 2)));
    b.cell("d", ["b", "c"], move |i| {
        j.fetch_add(1, Ordering::SeqCst);
        Ok(Value::int(i.int(0)? + i.int(1)?))
    });
    let mut s = Session::new(b.build().unwrap());
    s.refresh();
    assert_eq!(count(&joins), 1);

    s.set("a", Value::int(5)).unwrap();
    let t = s.commit();
    assert_eq!(
        count(&joins),
        2,
        "the join must not run twice for one change"
    );
    assert_eq!(t.evaluated(), 3);
    assert_eq!(s.get("d").unwrap().value().unwrap().as_int(), Some(6 + 10));
}

#[test]
fn a_diamond_join_never_sees_a_mixed_state() {
    // `b` is a + 1 and `c` is a * 2, so any consistent observation satisfies
    // (b - 1) * 2 == c. A glitch — the join running once with the new `b` and the old `c` —
    // breaks that equation, and this test is the only thing that would notice.
    let seen: Arc<Mutex<Vec<(i64, i64)>>> = Arc::new(Mutex::new(Vec::new()));
    let s2 = Arc::clone(&seen);

    let mut b = Graph::builder();
    b.source("a", Value::int(1));
    b.cell("b", ["a"], |i| Ok(Value::int(i.int(0)? + 1)));
    b.cell("c", ["a"], |i| Ok(Value::int(i.int(0)? * 2)));
    b.cell("d", ["b", "c"], move |i| {
        let (b, c) = (i.int(0)?, i.int(1)?);
        s2.lock().unwrap().push((b, c));
        Ok(Value::int(b + c))
    });
    let mut s = Session::new(b.build().unwrap());
    s.refresh();
    for v in [2, 3, 4, 100, -7] {
        s.set("a", Value::int(v)).unwrap();
        s.commit();
    }

    let observations = seen.lock().unwrap().clone();
    assert_eq!(observations.len(), 6, "one observation per pass, no more");
    for (b, c) in observations {
        assert_eq!((b - 1) * 2, c, "the join saw b={b} against c={c}");
    }
}

#[test]
fn a_deep_chain_settles_in_one_pass() {
    let mut b = Graph::builder();
    b.source("x", Value::int(0));
    b.cell("s0", ["x"], |i| Ok(Value::int(i.int(0)? + 1)));
    for n in 1..50 {
        b.cell(format!("s{n}"), [format!("s{}", n - 1)], |i| {
            Ok(Value::int(i.int(0)? + 1))
        });
    }
    let mut s = Session::new(b.build().unwrap());
    s.refresh();
    s.set("x", Value::int(100)).unwrap();
    let t = s.commit();
    assert_eq!(t.evaluated(), 50, "each link runs exactly once");
    assert_eq!(s.get("s49").unwrap().value().unwrap().as_int(), Some(150));
}

// ── the value-equality short circuit ───────────────────────────────────────────────────

#[test]
fn a_recomputation_to_the_same_value_stops_the_pass() {
    let downstream = counter();
    let d = Arc::clone(&downstream);

    let mut b = Graph::builder();
    b.source("raw", Value::int(1));
    // Whatever `raw` is, this is always 0. The cell below it must therefore run exactly
    // once, ever — that is the difference between a reactive runtime and a rerun.
    b.cell("flattened", ["raw"], |_| Ok(Value::int(0)));
    b.cell("expensive", ["flattened"], move |i| {
        d.fetch_add(1, Ordering::SeqCst);
        Ok(Value::int(i.int(0)?))
    });
    let mut s = Session::new(b.build().unwrap());
    s.refresh();
    assert_eq!(count(&downstream), 1);

    for v in [2, 3, 4] {
        s.set("raw", Value::int(v)).unwrap();
        let t = s.commit();
        assert_eq!(t.evaluated(), 1, "only `flattened` runs");
        assert_eq!(t.short_circuited(), 1, "and it produced the same value");
        assert_eq!(t.reused(), 1, "so `expensive` served its cache");
    }
    assert_eq!(count(&downstream), 1, "the expensive cell never ran again");
}

#[test]
fn an_over_declared_edge_costs_one_recomputation_not_a_cascade() {
    // `report` declares `mode` but only reads it on a branch it never takes here. The
    // declared edge means it recomputes; the digest means nothing below it does.
    let below = counter();
    let bc = Arc::clone(&below);

    let mut b = Graph::builder();
    b.source("mode", Value::text("summary"));
    b.source("n", Value::int(3));
    b.cell("report", ["mode", "n"], |i| {
        if i.text(0)? == "detail" {
            Ok(Value::text("detailed"))
        } else {
            Ok(Value::int(i.int(1)?))
        }
    });
    b.cell("rendered", ["report"], move |i| {
        bc.fetch_add(1, Ordering::SeqCst);
        Ok(Value::text(format!("{:?}", i.get(0))))
    });
    let mut s = Session::new(b.build().unwrap());
    s.refresh();

    s.set("mode", Value::text("also-summary")).unwrap();
    let t = s.commit();
    assert_eq!(
        t.evaluated(),
        1,
        "`report` re-ran because the edge is declared"
    );
    assert_eq!(t.short_circuited(), 1);
    assert_eq!(count(&below), 1, "and `rendered` did not");
}

#[test]
fn setting_an_input_to_the_value_it_already_holds_does_nothing() {
    let runs = counter();
    let r = Arc::clone(&runs);

    let mut b = Graph::builder();
    b.source("x", Value::int(7));
    b.cell("y", ["x"], move |i| {
        r.fetch_add(1, Ordering::SeqCst);
        Ok(Value::int(i.int(0)?))
    });
    let mut s = Session::new(b.build().unwrap());
    s.refresh();
    assert_eq!(count(&runs), 1);

    s.set("x", Value::int(7)).unwrap();
    let t = s.commit();
    assert_eq!(t.visited(), 0, "an empty pass: {}", t.summary());
    assert!(t.roots.is_empty());
    assert_eq!(
        count(&runs),
        1,
        "a client re-sending its state costs nothing"
    );
}

#[test]
fn a_second_refresh_reuses_everything() {
    let mut b = Graph::builder();
    b.source("x", Value::int(1));
    b.cell("y", ["x"], |i| Ok(Value::int(i.int(0)?)));
    b.cell("z", ["y"], |i| Ok(Value::int(i.int(0)?)));
    let mut s = Session::new(b.build().unwrap());
    s.refresh();
    let again = s.refresh();
    assert_eq!(again.evaluated(), 0);
    assert_eq!(again.reused(), 2, "a reconnect costs nothing but the walk");
}

// ── one interaction is one pass ────────────────────────────────────────────────────────

#[test]
fn two_inputs_set_in_one_commit_produce_one_pass() {
    let joins = counter();
    let j = Arc::clone(&joins);

    let mut b = Graph::builder();
    b.source("lo", Value::int(0));
    b.source("hi", Value::int(10));
    b.cell("span", ["lo", "hi"], move |i| {
        j.fetch_add(1, Ordering::SeqCst);
        Ok(Value::int(i.int(1)? - i.int(0)?))
    });
    let mut s = Session::new(b.build().unwrap());
    s.refresh();

    s.set("lo", Value::int(2)).unwrap();
    s.set("hi", Value::int(20)).unwrap();
    let t = s.commit();
    assert_eq!(count(&joins), 2, "one pass, not two");
    assert_eq!(t.roots.len(), 2);
    assert_eq!(s.get("span").unwrap().value().unwrap().as_int(), Some(18));
}

#[test]
fn last_write_wins_within_one_pass() {
    let mut b = Graph::builder();
    b.source("x", Value::int(0));
    b.cell("y", ["x"], |i| Ok(Value::int(i.int(0)?)));
    let mut s = Session::new(b.build().unwrap());
    s.refresh();

    for v in 1..=20 {
        s.set("x", Value::int(v)).unwrap();
    }
    let t = s.commit();
    assert_eq!(t.evaluated(), 1, "a slider drag is one recomputation");
    assert_eq!(s.get("y").unwrap().value().unwrap().as_int(), Some(20));
}

#[test]
fn changed_since_reports_only_cells_whose_value_moved() {
    let mut b = Graph::builder();
    b.source("x", Value::int(1));
    b.cell("moves", ["x"], |i| Ok(Value::int(i.int(0)? * 2)));
    b.cell("constant", ["x"], |_| Ok(Value::text("fixed")));
    let graph = b.build().unwrap();
    let mut s = Session::new(Arc::clone(&graph));
    s.refresh();

    let before = s.epoch() + 1;
    s.set("x", Value::int(2)).unwrap();
    s.commit();

    let changed: Vec<&str> = s
        .changed_since(before)
        .into_iter()
        .map(|id| graph.name(id))
        .collect();
    assert_eq!(changed, vec!["x", "moves"], "`constant` is not on the wire");
}

// ── errors are values ──────────────────────────────────────────────────────────────────

#[test]
fn a_failing_cell_does_not_take_down_the_page() {
    let mut b = Graph::builder();
    b.source("d", Value::int(0));
    b.cell("ratio", ["d"], |i| {
        let d = i.int(0)?;
        if d == 0 {
            Err(CellError::failed("division by zero"))
        } else {
            Ok(Value::int(100 / d))
        }
    });
    b.cell("unrelated", ["d"], |i| Ok(Value::int(i.int(0)? + 1)));
    let mut s = Session::new(b.build().unwrap());
    let t = s.refresh();

    assert_eq!(t.failed(), 1);
    assert!(s.get("ratio").unwrap().is_err());
    assert_eq!(
        s.get("unrelated").unwrap().value().unwrap().as_int(),
        Some(1),
        "the rest of the app still rendered"
    );
}

#[test]
fn an_error_eight_cells_down_still_names_the_cell_that_failed() {
    let mut b = Graph::builder();
    b.source("x", Value::int(0));
    b.cell("broken", ["x"], |_| {
        Err(CellError::failed("the real problem"))
    });
    b.cell("s0", ["broken"], |i| Ok(Value::int(i.int(0)?)));
    for n in 1..8 {
        b.cell(format!("s{n}"), [format!("s{}", n - 1)], |i| {
            Ok(Value::int(i.int(0)?))
        });
    }
    let mut s = Session::new(b.build().unwrap());
    s.refresh();

    let err = s.get("s7").unwrap().error().unwrap().clone();
    assert_eq!(err.cause("s7"), "broken");
    assert_eq!(err.message(), "the real problem");
}

#[test]
fn a_recovered_cell_wakes_everything_below_it() {
    let mut b = Graph::builder();
    b.source("d", Value::int(0));
    b.cell("ratio", ["d"], |i| {
        let d = i.int(0)?;
        if d == 0 {
            Err(CellError::failed("division by zero"))
        } else {
            Ok(Value::int(100 / d))
        }
    });
    b.cell("shown", ["ratio"], |i| {
        Ok(Value::text(format!("{}", i.int(0)?)))
    });
    let mut s = Session::new(b.build().unwrap());
    s.refresh();
    assert!(s.get("shown").unwrap().is_err());

    s.set("d", Value::int(4)).unwrap();
    s.commit();
    assert_eq!(
        s.get("shown").unwrap().value().unwrap().as_text(),
        Some("25")
    );
}

#[test]
fn the_same_failure_twice_does_not_re_wake_the_page() {
    let below = counter();
    let bc = Arc::clone(&below);

    let mut b = Graph::builder();
    b.source("x", Value::int(1));
    b.cell("always_fails", ["x"], |_| Err(CellError::failed("nope")));
    b.cell("below", ["always_fails"], move |i| {
        bc.fetch_add(1, Ordering::SeqCst);
        Ok(Value::int(i.int(0)?))
    });
    let mut s = Session::new(b.build().unwrap());
    s.refresh();
    s.set("x", Value::int(2)).unwrap();
    let t = s.commit();

    assert_eq!(t.failed(), 1, "only the cell that fails re-runs");
    assert_eq!(
        t.reused(),
        1,
        "the cell below it held the same error, so it served its cache: {}",
        t.summary()
    );
    assert!(s.get("below").unwrap().is_err(), "and it is still in error");
    assert_eq!(
        count(&below),
        0,
        "the compute below a failing cell is never called"
    );
}

#[test]
fn a_failure_that_moves_to_a_new_origin_stops_naming_the_old_one() {
    // Found by the differential oracle, not by anyone's imagination: two upstream cells
    // failing with the same words used to digest alike, so the cell below them served a
    // cached error naming a cell that was no longer the problem.
    let mut b = Graph::builder();
    b.source("pick", Value::int(0));
    b.cell("left", ["pick"], |i| {
        if i.int(0)? == 0 {
            Err(CellError::failed("bad input"))
        } else {
            Ok(Value::int(1))
        }
    });
    b.cell("right", ["pick"], |i| {
        if i.int(0)? == 1 {
            Err(CellError::failed("bad input"))
        } else {
            Ok(Value::int(1))
        }
    });
    b.cell("shown", ["left", "right"], |i| {
        Ok(Value::int(i.int(0)? + i.int(1)?))
    });
    let mut s = Session::new(b.build().unwrap());
    s.refresh();
    assert_eq!(
        s.get("shown").unwrap().error().unwrap().cause("shown"),
        "left"
    );

    s.set("pick", Value::int(1)).unwrap();
    s.commit();
    assert_eq!(
        s.get("shown").unwrap().error().unwrap().cause("shown"),
        "right",
        "the same message from a different cell is a different value"
    );
}

#[test]
fn a_type_error_inside_a_cell_names_the_input() {
    let mut b = Graph::builder();
    b.source("word", Value::text("hello"));
    b.cell("doubled", ["word"], |i| Ok(Value::int(i.int(0)? * 2)));
    let mut s = Session::new(b.build().unwrap());
    s.refresh();
    let msg = s
        .get("doubled")
        .unwrap()
        .error()
        .unwrap()
        .message()
        .to_string();
    assert!(msg.contains("`word`"), "{msg}");
    assert!(msg.contains("an int"), "{msg}");
}

// ── the graph is checked once, at build time ───────────────────────────────────────────

#[test]
fn a_cycle_is_a_build_error_and_names_the_loop() {
    let mut b = Graph::builder();
    b.source("seed", Value::int(0));
    b.cell("a", ["seed", "c"], |i| Ok(Value::int(i.int(0)?)));
    b.cell("b", ["a"], |i| Ok(Value::int(i.int(0)?)));
    b.cell("c", ["b"], |i| Ok(Value::int(i.int(0)?)));

    let BuildError::Cycle { path } = b.build().unwrap_err() else {
        panic!("expected a cycle");
    };
    assert_eq!(path.first(), path.last(), "the reported path is closed");
    for name in ["a", "b", "c"] {
        assert!(path.contains(&name.to_string()), "{path:?}");
    }
}

#[test]
fn a_self_referencing_cell_is_a_cycle() {
    let mut b = Graph::builder();
    b.cell("loop", ["loop"], |i| Ok(Value::int(i.int(0)?)));
    assert!(matches!(b.build(), Err(BuildError::Cycle { .. })));
}

#[test]
fn an_unknown_input_is_a_build_error() {
    let mut b = Graph::builder();
    b.cell("a", ["ghost"], |_| Ok(Value::Null));
    assert_eq!(
        b.build().unwrap_err(),
        BuildError::UnknownInput {
            cell: "a".into(),
            input: "ghost".into()
        }
    );
}

#[test]
fn two_cells_with_one_name_is_a_build_error() {
    let mut b = Graph::builder();
    b.source("x", Value::int(1));
    b.source("x", Value::int(2));
    assert_eq!(
        b.build().unwrap_err(),
        BuildError::DuplicateName("x".into())
    );
}

#[test]
fn a_cell_may_be_declared_before_the_input_it_names() {
    let mut b = Graph::builder();
    b.cell("out", ["in"], |i| Ok(Value::int(i.int(0)? + 1)));
    b.source("in", Value::int(1));
    let mut s = Session::new(b.build().unwrap());
    s.refresh();
    assert_eq!(s.get("out").unwrap().value().unwrap().as_int(), Some(2));
}

#[test]
fn naming_the_same_input_twice_is_legal_and_runs_the_cell_once() {
    let runs = counter();
    let r = Arc::clone(&runs);
    let mut b = Graph::builder();
    b.source("x", Value::int(3));
    b.cell("twice", ["x", "x"], move |i| {
        r.fetch_add(1, Ordering::SeqCst);
        Ok(Value::int(i.int(0)? + i.int(1)?))
    });
    let mut s = Session::new(b.build().unwrap());
    s.refresh();
    s.set("x", Value::int(4)).unwrap();
    s.commit();
    assert_eq!(count(&runs), 2, "once per pass, not once per edge");
    assert_eq!(s.get("twice").unwrap().value().unwrap().as_int(), Some(8));
}

// ── the session API refuses to be used wrongly ─────────────────────────────────────────

#[test]
fn a_computed_cell_cannot_be_set_from_outside() {
    let mut b = Graph::builder();
    b.source("x", Value::int(1));
    b.cell("y", ["x"], |i| Ok(Value::int(i.int(0)?)));
    let mut s = Session::new(b.build().unwrap());
    assert_eq!(
        s.set("y", Value::int(9)),
        Err(SessionError::NotAnInput { name: "y".into() })
    );
}

#[test]
fn setting_an_unknown_cell_is_an_error() {
    let mut b = Graph::builder();
    b.source("x", Value::int(1));
    let mut s = Session::new(b.build().unwrap());
    assert_eq!(
        s.set("nope", Value::int(1)),
        Err(SessionError::UnknownCell("nope".into()))
    );
}

#[test]
fn the_wrong_type_is_refused_at_the_edge_not_inside_a_cell() {
    let mut b = Graph::builder();
    b.source("threshold", Value::float(1.0));
    let mut s = Session::new(b.build().unwrap());
    assert!(matches!(
        s.set("threshold", Value::text("banana")),
        Err(SessionError::TypeMismatch { .. })
    ));
    assert!(
        s.set("threshold", Value::int(3)).is_ok(),
        "a browser sends a whole number as an int; a float input must take it"
    );
}

#[test]
fn a_null_source_accepts_anything() {
    let mut b = Graph::builder();
    b.source("host_supplied", Value::Null);
    let mut s = Session::new(b.build().unwrap());
    assert!(s.set("host_supplied", Value::text("later")).is_ok());
    assert!(s.set("host_supplied", Value::int(1)).is_ok());
}

// ── the trace itself ───────────────────────────────────────────────────────────────────

#[test]
fn session_is_send_and_sync() {
    // Pins what the type actually is, because the module docs used to claim it was `!Sync`
    // and nothing checked. If a future field makes `Session` non-`Send` (an `Rc`, a raw
    // pointer) or deliberately non-`Sync` (a `Cell`), this stops compiling and whoever did it
    // has to update `session.rs`'s comment in the same change.
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Session>();
    assert_send_sync::<dagpane_core::Graph>();
}

#[test]
fn the_trace_counts_add_up() {
    let mut b = Graph::builder();
    b.source("x", Value::int(1));
    b.cell("moves", ["x"], |i| Ok(Value::int(i.int(0)?)));
    b.cell("constant", ["x"], |_| Ok(Value::int(0)));
    b.cell("below_constant", ["constant"], |i| {
        Ok(Value::int(i.int(0)?))
    });
    b.cell("fails", ["x"], |_| Err(CellError::failed("x")));
    let mut s = Session::new(b.build().unwrap());
    s.refresh();

    s.set("x", Value::int(2)).unwrap();
    let t = s.commit();
    assert_eq!(
        t.visited(),
        t.set() + t.evaluated() + t.reused() + t.failed()
    );
    assert_eq!(t.evaluated(), t.changed() + t.short_circuited());
    assert_eq!(t.visited() + t.untouched(), t.total_cells);
}

#[test]
fn a_trace_round_trips_through_json() {
    let mut b = Graph::builder();
    b.source("x", Value::int(1));
    b.cell("y", ["x"], |i| Ok(Value::int(i.int(0)?)));
    let mut s = Session::new(b.build().unwrap());
    let t = s.refresh();
    let json = serde_json::to_string(&t).unwrap();
    let back: dagpane_core::Trace = serde_json::from_str(&json).unwrap();
    assert_eq!(t, back);
    assert!(matches!(
        back.steps[0].outcome,
        StepOutcome::Evaluated { changed: true }
    ));
}

#[test]
fn sessions_over_one_graph_do_not_share_values() {
    let mut b = Graph::builder();
    b.source("x", Value::int(0));
    b.cell("y", ["x"], |i| Ok(Value::int(i.int(0)? * 10)));
    let graph = b.build().unwrap();

    let mut a = Session::new(Arc::clone(&graph));
    let mut c = Session::new(Arc::clone(&graph));
    a.refresh();
    c.refresh();

    a.set("x", Value::int(4)).unwrap();
    a.commit();
    assert_eq!(a.get("y").unwrap().value().unwrap().as_int(), Some(40));
    assert_eq!(
        c.get("y").unwrap().value().unwrap().as_int(),
        Some(0),
        "one user's slider is not another user's"
    );
}
