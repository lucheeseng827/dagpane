//! The differential test the seam exists to make possible: **the backend must not change
//! what the scheduler does.**
//!
//! `dagpane-core`'s own oracle proves incremental evaluation agrees with a full recompute.
//! It cannot prove anything about representations, because core knows one. This runs the
//! same shape of corpus twice over the same seeds — once with frame-valued cells returning
//! `Table`, once returning `ArrowFrame` — and asserts the two runs are indistinguishable.
//!
//! Two assertions, and the second is the interesting one.
//!
//!   * **Values.** Every cell holds the same value in both runs. `Value`'s equality walks
//!     frames cell by cell, so this compares content across representations rather than
//!     pointers.
//!   * **Traces.** Every pass evaluated the *same cells*, reused the same ones, and
//!     short-circuited the same number of times. This is what makes the seam real: the
//!     digest short-circuit is driven by content hashes, so if a backend's digest disagreed
//!     with `Table`'s by even one bit, cells would recompute in one run and be reused in the
//!     other and the traces would diverge. A digest bug cannot hide from this.
//!
//! It lives here rather than in core because it needs both backends, and core must not
//! depend on one of its own backends.

use std::sync::Arc;

use dagpane_core::frame::Frame;
use dagpane_core::value::{Column, Table};
use dagpane_core::{CellError, Graph, Outcome, Session, Value};
use dagpane_frame_arrow::ArrowFrame;

/// The same LCG core's oracle uses, for the same reason: a failing seed has to be
/// reproducible from the number in the assertion message, for the life of the project.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Which representation a frame-producing cell hands back.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Backend {
    Table,
    Arrow,
}

/// Build the same logical frame either way. Deliberately lossy in its row count, so
/// different inputs often produce an identical frame and the short-circuit has to fire on
/// a value it can only compare by walking.
fn make_frame(backend: Backend, args: &[i64]) -> Value {
    let n = args.iter().map(|a| a.rem_euclid(3)).sum::<i64>() as usize % 4 + 1;
    let columns = vec![
        Column::int("id", (0..n).map(|i| Some(i as i64)).collect()),
        Column::text(
            "label",
            (0..n)
                .map(|i| Some(["north", "south", "east"][i % 3].to_string()))
                .collect(),
        ),
        Column::bool(
            "flag",
            (0..n)
                .map(|i| if i % 3 == 0 { None } else { Some(i % 2 == 0) })
                .collect(),
        ),
    ];
    match backend {
        Backend::Table => Value::table(Table::new(columns).expect("one length")),
        Backend::Arrow => Value::frame(Arc::new(ArrowFrame::from_columns(&columns))),
    }
}

fn scalar_of(i: &dagpane_core::graph::Inputs<'_>, n: usize) -> Result<i64, CellError> {
    match i.get(n) {
        Value::Frame { v } => {
            let f = v.as_frame();
            let col = f.column_index("id").expect("generated frames carry `id`");
            let mut acc = f.rows() as i64;
            for row in 0..f.rows() {
                if let Some(x) = f.value_at(row, col).as_int() {
                    acc = acc.wrapping_add(x);
                }
            }
            Ok(acc)
        }
        _ => i.int(n),
    }
}

struct Generated {
    graph: Arc<Graph>,
    sources: Vec<String>,
    cells: Vec<String>,
}

/// One graph, built twice from the same seed with only the backend differing. Because the
/// generator is deterministic, the two graphs are structurally identical and any divergence
/// downstream is the representation and nothing else.
fn generate(seed: u64, backend: Backend) -> Generated {
    let mut rng = Lcg(seed.wrapping_mul(0x9E3779B97F4A7C15));
    let source_count = 2 + rng.below(4);
    let computed_count = 6 + rng.below(24);

    let mut b = Graph::builder();
    let mut sources = Vec::new();
    let mut names = Vec::new();
    for i in 0..source_count {
        let name = format!("in_{i}");
        b.source(&name, Value::int(rng.below(6) as i64));
        sources.push(name.clone());
        names.push(name);
    }
    for i in 0..computed_count {
        let name = format!("c_{i}");
        let arity = 1 + rng.below(3.min(names.len()));
        let inputs: Vec<String> = (0..arity)
            .map(|_| names[rng.below(names.len())].clone())
            .collect();
        let makes_frame = rng.below(3) == 0;
        b.cell(&name, inputs, move |i| {
            let mut args = Vec::with_capacity(i.len());
            for n in 0..i.len() {
                args.push(scalar_of(&i, n)?);
            }
            if makes_frame {
                Ok(make_frame(backend, &args))
            } else {
                Ok(Value::int(
                    args.iter()
                        .fold(0i64, |a, b| a.wrapping_add(*b))
                        .rem_euclid(5),
                ))
            }
        });
        names.push(name);
    }
    Generated {
        graph: b.build().expect("acyclic by construction"),
        sources,
        cells: names,
    }
}

/// A trace reduced to what a backend must not be able to change.
fn fingerprint(t: &dagpane_core::Trace) -> (usize, usize, usize, usize, Vec<String>) {
    (
        t.visited(),
        t.evaluated(),
        t.reused(),
        t.short_circuited(),
        t.steps.iter().map(|s| s.cell.clone()).collect(),
    )
}

#[test]
fn the_backend_cannot_change_what_the_scheduler_does() {
    let mut frame_cells = 0usize;
    let mut short_circuits = 0usize;

    for seed in 1..=120u64 {
        let a = generate(seed, Backend::Table);
        let b = generate(seed, Backend::Arrow);
        assert_eq!(
            a.cells, b.cells,
            "seed {seed}: the two graphs must be the same shape"
        );

        let mut live_a = Session::new(Arc::clone(&a.graph));
        let mut live_b = Session::new(Arc::clone(&b.graph));
        live_a.refresh();
        live_b.refresh();

        // The same interaction sequence, driven from one generator so both sessions see it.
        let mut rng = Lcg(seed ^ 0xDEAD_BEEF);
        for step in 0..10 {
            let which = rng.below(a.sources.len());
            let value = rng.below(6) as i64;
            let name = &a.sources[which];
            live_a.set(name, Value::int(value)).unwrap();
            live_b.set(name, Value::int(value)).unwrap();

            let trace_a = live_a.commit();
            let trace_b = live_b.commit();
            short_circuits += trace_a.short_circuited();

            // The load-bearing assertion. A digest that differed by one bit between the
            // backends would show up here as a cell evaluated in one run and reused in the
            // other, long before anyone noticed a wrong number on a page.
            assert_eq!(
                fingerprint(&trace_a),
                fingerprint(&trace_b),
                "seed {seed} step {step}: the two backends took different passes\n  \
                 table: {}\n  arrow: {}",
                trace_a.summary(),
                trace_b.summary()
            );

            for cell in &a.cells {
                let (va, vb) = (live_a.get(cell).unwrap(), live_b.get(cell).unwrap());
                if matches!(
                    &va,
                    Outcome::Value {
                        value: Value::Frame { .. }
                    }
                ) {
                    frame_cells += 1;
                }
                assert_eq!(
                    va, vb,
                    "seed {seed} step {step}: cell `{cell}` differs between backends"
                );
            }
        }
    }

    // Guards on the generator, not the engine: a corpus that stopped producing frames, or
    // stopped short-circuiting, would be green and prove nothing.
    assert!(
        frame_cells > 2000,
        "only {frame_cells} frame-valued cells compared"
    );
    assert!(
        short_circuits > 100,
        "the corpus short-circuited only {short_circuits} times"
    );
}

#[test]
fn a_frame_is_equal_across_backends_and_unequal_when_content_differs() {
    let columns = vec![
        Column::int("id", vec![Some(1), Some(2)]),
        Column::text("k", vec![Some("a".into()), Some("a".into())]),
    ];
    let table = Value::table(Table::new(columns.clone()).unwrap());
    let arrow = Value::frame(Arc::new(ArrowFrame::from_columns(&columns)));
    assert_eq!(table, arrow, "same content, different representation");

    let mut other = columns;
    other[0] = Column::int("id", vec![Some(1), Some(3)]);
    assert_ne!(
        table,
        Value::frame(Arc::new(ArrowFrame::from_columns(&other)))
    );
}

// ── the representation survives a whole pipeline ──────────────────────────────

/// Every verb must hand back a frame of the kind it was given.
///
/// This is what `Frame::backend` exists for. Comparing values cannot show it: two frames
/// with identical content are *equal by design*, so a verb that quietly dropped to a
/// `Table` halfway through a chain would pass every equality assertion in this file while
/// throwing away the representation the source was loaded in.
#[test]
fn no_verb_drops_the_backend_on_the_floor() {
    use dagpane_core::transform::{self, Agg, AggSpec, Comparison, Filter, GroupBy};

    let n = 400;
    let columns = vec![
        dagpane_core::value::Column::int("id", (0..n).map(|i| Some(i as i64)).collect()),
        dagpane_core::value::Column::text(
            "region",
            (0..n)
                .map(|i| Some(["north", "south", "east", "west"][i % 4].to_string()))
                .collect(),
        ),
        dagpane_core::value::Column::float("amount", (0..n).map(|i| Some(i as f64)).collect()),
    ];
    let arrow: Arc<dyn Frame> = Arc::new(ArrowFrame::from_columns(&columns));
    assert_eq!(arrow.backend(), "arrow");

    let filtered = transform::filter(
        &*arrow,
        &Filter {
            column: "amount".into(),
            op: Comparison::Ge,
            value: Value::float(50.0),
        },
    )
    .unwrap();
    assert_eq!(filtered.backend(), "arrow", "filter");

    let sorted = transform::sort(&*filtered, "amount", true).unwrap();
    assert_eq!(sorted.backend(), "arrow", "sort");

    let limited = transform::limit(&*sorted, 100);
    assert_eq!(limited.backend(), "arrow", "limit");

    let selected = transform::select(&*limited, &["region".into(), "amount".into()]).unwrap();
    assert_eq!(selected.backend(), "arrow", "select");

    // The one that used to break the chain: `group_by` builds rows rather than selecting
    // them, so it constructs its output — and before `same_kind` it constructed a `Table`.
    let grouped = transform::group_by(
        &*selected,
        &GroupBy {
            by: vec!["region".into()],
            aggs: vec![AggSpec {
                column: "amount".into(),
                agg: Agg::Sum,
                as_name: "total".into(),
            }],
        },
    )
    .unwrap();
    assert_eq!(
        grouped.backend(),
        "arrow",
        "group_by must not fall back to a table"
    );

    // And a chain that starts as a `Table` stays one — `same_kind` follows the input in
    // both directions, rather than preferring whichever backend happens to be linked.
    let table: Arc<dyn Frame> = Arc::new(Table::new(columns).unwrap());
    let grouped_table = transform::group_by(
        &*table,
        &GroupBy {
            by: vec!["region".into()],
            aggs: vec![AggSpec {
                column: "amount".into(),
                agg: Agg::Count,
                as_name: "n".into(),
            }],
        },
    )
    .unwrap();
    assert_eq!(grouped_table.backend(), "table");

    // Same answer either way, whatever holds it.
    let a = transform::group_by(
        &*arrow,
        &GroupBy {
            by: vec!["region".into()],
            aggs: vec![AggSpec {
                column: "amount".into(),
                agg: Agg::Count,
                as_name: "n".into(),
            }],
        },
    )
    .unwrap();
    assert_eq!(
        dagpane_core::frame::frame_digest(&*a),
        dagpane_core::frame::frame_digest(&*grouped_table),
        "the two backends must aggregate to the same thing"
    );
}

/// An `i64` above 2^53 must filter the same way whichever backend holds it.
///
/// This is the one the differential corpus above could never have caught: it generates small
/// integers, so every value it produces survives a round trip through `f64` unchanged. The
/// divergence lives in `Frame::compare_to_value`'s *default* body, which widened both sides to
/// `f64` before comparing — and `Table` overrides that method with an exact `i64` compare while
/// `ArrowFrame` inherits it. Two adjacent integers that differ by one, above the mantissa's
/// reach, therefore compared *equal* on one backend and *ordered* on the other, and a `filter`
/// kept a different set of rows depending on a representation choice the user never made.
///
/// 2^53 + 1 is the smallest integer `f64` cannot represent; it rounds to 2^53.
#[test]
fn a_big_integer_filters_the_same_on_both_backends() {
    use dagpane_core::transform::{self, Comparison, Filter};

    const TWO_53: i64 = 1 << 53;
    let rows = vec![
        Some(TWO_53),     // exactly representable
        Some(TWO_53 + 1), // rounds to TWO_53 as an f64 — the whole point
        Some(TWO_53 + 2),
        Some(i64::MAX),
        Some(i64::MAX - 1),
        Some(-TWO_53 - 1),
    ];
    let columns = vec![Column::int("n", rows.clone())];
    let table = Table::new(columns.clone()).unwrap();
    let arrow = ArrowFrame::from_columns(&columns);

    // Every row, as its own threshold, in both directions. If any comparison is done in f64 the
    // two backends part company somewhere in here.
    for &probe in rows.iter().flatten() {
        for op in [
            Comparison::Gt,
            Comparison::Ge,
            Comparison::Lt,
            Comparison::Le,
            Comparison::Eq,
            Comparison::Ne,
        ] {
            let spec = Filter {
                column: "n".into(),
                op,
                value: Value::int(probe),
            };
            let from_table = transform::filter(&table, &spec).unwrap();
            let from_arrow = transform::filter(&arrow, &spec).unwrap();
            assert_eq!(
                dagpane_core::frame::frame_digest(&*from_table),
                dagpane_core::frame::frame_digest(&*from_arrow),
                "filter n {op:?} {probe} kept different rows: \
                 table {} row(s), arrow {} row(s)",
                from_table.rows(),
                from_arrow.rows(),
            );
        }
    }
}
