//! The differential test: incremental evaluation against a full-recompute oracle.
//!
//! Every other test in this crate asserts a *count*, and a count assertion on a graph the
//! author drew is a test of the author's expectations. A scheduler that skips too much
//! passes all of them and is silently wrong — which is the failure this project must not
//! have, because a missed invalidation shows somebody a stale number on a page that looks
//! like it is working.
//!
//! So: two hundred pseudo-random graphs, a random sequence of interactions on each, and
//! after **every** commit two assertions.
//!
//!   * **Correctness.** Every cell's value equals the value a *fresh* session over the same
//!     graph computes from scratch with the same inputs. The oracle is the naive engine, the
//!     one nobody would ship and everybody trusts.
//!   * **Economy.** Every cell the pass evaluated is inside the structural closure of the
//!     inputs that changed — the engine may not do work it cannot justify.
//!
//! The generator is a 64-bit LCG written here rather than taken from `rand`, for the reason
//! this crate has one dependency: a failing seed has to be reproducible from the number
//! printed in the assertion message, on any machine, for the life of the project. A
//! generator whose sequence can change with a dependency bump cannot promise that.

use std::collections::HashSet;
use std::sync::Arc;

use dagpane_core::value::{Column, Table};
use dagpane_core::{CellError, Graph, Outcome, Session, Value};

/// Numerical Recipes' LCG constants. Any full-period generator would do; what matters is
/// that this one is written down.
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

/// The operations a generated cell can perform. Each is a total function of its inputs
/// except `Divide`, which is here precisely so the error path is exercised on random graphs
/// rather than only in the hand-written tests.
#[derive(Clone, Copy, Debug)]
enum Op {
    Sum,
    Max,
    /// Deliberately lossy: many different inputs map to the same output, so the
    /// value-equality short-circuit fires often and the oracle has to agree that it was
    /// right to.
    Parity,
    /// Ignores its inputs entirely. The strongest test of the short-circuit: everything
    /// below one of these must recompute exactly once, ever.
    Constant,
    /// Fails when any input is zero.
    Divide,
    /// Produces a **table** rather than a scalar.
    ///
    /// Everything else here yields an `int`, and an int's equality is a machine word. A
    /// table's is a walk over every cell, it is what the digest short-circuit actually has
    /// to decide on in a real app, and it is the one `Value` whose representation is
    /// pluggable. A scheduler that is correct on scalars and wrong on frames passes every
    /// other test in this file.
    MakeFrame,
}

impl Op {
    fn pick(rng: &mut Lcg) -> Op {
        match rng.below(12) {
            0..=3 => Op::Sum,
            4..=5 => Op::Max,
            6..=7 => Op::Parity,
            8 => Op::Constant,
            9 => Op::Divide,
            // Roughly one cell in six is frame-valued, so a generated graph reliably has
            // several and reliably has frames feeding frames.
            _ => Op::MakeFrame,
        }
    }

    fn apply(self, args: &[i64]) -> Result<Value, CellError> {
        match self {
            Op::Sum => Ok(Value::int(
                args.iter().fold(0i64, |a, b| a.wrapping_add(*b)),
            )),
            Op::Max => Ok(Value::int(args.iter().copied().max().unwrap_or(0))),
            Op::Parity => Ok(Value::int(
                args.iter()
                    .fold(0i64, |a, b| a.wrapping_add(*b))
                    .rem_euclid(3),
            )),
            Op::Constant => Ok(Value::int(7)),
            Op::Divide => {
                let mut acc = 1_000_000i64;
                for a in args {
                    if *a == 0 {
                        return Err(CellError::failed("division by zero"));
                    }
                    acc /= *a;
                }
                Ok(Value::int(acc))
            }
            // Deliberately lossy in the same way `Parity` is: `label` collapses to one of
            // three strings, so distinct inputs frequently produce an identical table and
            // the short-circuit has to fire on a value it can only compare by walking.
            Op::MakeFrame => {
                let n = args.iter().map(|a| a.rem_euclid(3)).sum::<i64>() as usize % 4 + 1;
                let ids: Vec<Option<i64>> = (0..n).map(|i| Some(i as i64)).collect();
                let labels: Vec<Option<String>> = (0..n)
                    .map(|i| Some(["north", "south", "east"][i % 3].to_string()))
                    .collect();
                let flags: Vec<Option<bool>> = (0..n)
                    .map(|i| if i % 3 == 0 { None } else { Some(i % 2 == 0) })
                    .collect();
                let t = Table::new(vec![
                    Column::int("id", ids),
                    Column::text("label", labels),
                    Column::bool("flag", flags),
                ])
                .expect("columns built to one length");
                Ok(Value::table(t))
            }
        }
    }
}

/// Read an input as a number, whatever kind of value it turned out to be.
///
/// Frame-valued cells feed scalar ones, so every consumer has to accept both. A table
/// reduces to a total over its `id` column plus its row count — deterministic, and
/// sensitive to any change in the frame, so a scheduler that hands a consumer a *stale*
/// table is caught by the consumer's own value rather than only by the direct comparison.
fn scalar(i: &dagpane_core::graph::Inputs<'_>, n: usize) -> Result<i64, CellError> {
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

/// A random acyclic graph. Inputs are chosen only from cells declared earlier, which is what
/// makes it acyclic by construction — the cycle path is tested elsewhere, on purpose.
fn generate(rng: &mut Lcg) -> Generated {
    let source_count = 2 + rng.below(4);
    let computed_count = 6 + rng.below(30);

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
        let mut inputs: Vec<String> = Vec::with_capacity(arity);
        for _ in 0..arity {
            let pick = names[rng.below(names.len())].clone();
            inputs.push(pick);
        }
        let op = Op::pick(rng);
        b.cell(&name, inputs, move |i| {
            let mut args = Vec::with_capacity(i.len());
            for n in 0..i.len() {
                args.push(scalar(&i, n)?);
            }
            op.apply(&args)
        });
        names.push(name);
    }

    Generated {
        graph: b
            .build()
            .expect("generated graphs are acyclic by construction"),
        sources,
        cells: names,
    }
}

/// The oracle: a brand-new session, every input set, everything computed from scratch.
fn oracle(graph: &Arc<Graph>, inputs: &[(String, i64)]) -> Session {
    let mut s = Session::new(Arc::clone(graph));
    for (name, v) in inputs {
        s.set(name, Value::int(*v)).expect("a generated source");
    }
    s.refresh();
    s
}

#[test]
fn incremental_evaluation_agrees_with_a_full_recompute() {
    let mut total_interactions = 0usize;
    let mut short_circuits = 0usize;
    let mut errors_seen = 0usize;
    let mut frame_cells = 0usize;

    for seed in 1..=200u64 {
        let mut rng = Lcg(seed.wrapping_mul(0x9E3779B97F4A7C15));
        let g = generate(&mut rng);

        // The session under test: created once, then only ever nudged.
        let mut live = Session::new(Arc::clone(&g.graph));
        let mut inputs: Vec<(String, i64)> = g
            .sources
            .iter()
            .map(|n| {
                let v = match live.get(n).unwrap() {
                    Outcome::Value { value } => value.as_int().unwrap(),
                    Outcome::Error { .. } => unreachable!("a source is never in error"),
                };
                (n.clone(), v)
            })
            .collect();
        live.refresh();

        for _ in 0..12 {
            // One interaction: one or two inputs, set together, committed once.
            let changes = 1 + rng.below(2);
            let mut roots: Vec<String> = Vec::new();
            for _ in 0..changes {
                let which = rng.below(inputs.len());
                let value = rng.below(6) as i64;
                inputs[which].1 = value;
                live.set(&inputs[which].0, Value::int(value)).unwrap();
                roots.push(inputs[which].0.clone());
            }
            let trace = live.commit();
            total_interactions += 1;
            short_circuits += trace.short_circuited();
            errors_seen += trace.failed();

            // ── correctness ───────────────────────────────────────────────────────────
            let truth = oracle(&g.graph, &inputs);
            for name in &g.cells {
                if matches!(
                    live.get(name).unwrap(),
                    Outcome::Value {
                        value: Value::Frame { .. }
                    }
                ) {
                    frame_cells += 1;
                }
                assert_eq!(
                    live.get(name).unwrap(),
                    truth.get(name).unwrap(),
                    "seed {seed}: cell `{name}` disagrees with a full recompute after \
                     setting {roots:?}\n  trace: {}",
                    trace.summary()
                );
            }

            // ── economy ───────────────────────────────────────────────────────────────
            let root_ids: Vec<_> = trace
                .roots
                .iter()
                .map(|n| g.graph.id(n).expect("a root is a real cell"))
                .collect();
            let allowed: HashSet<_> = g.graph.closure(&root_ids).into_iter().collect();
            for step in &trace.steps {
                assert!(
                    allowed.contains(&step.id),
                    "seed {seed}: the pass touched `{}`, which is not downstream of {:?}",
                    step.cell,
                    trace.roots
                );
            }
            assert!(
                trace.visited() <= allowed.len(),
                "seed {seed}: visited {} of a {}-cell closure",
                trace.visited(),
                allowed.len()
            );
        }
    }

    // Not assertions about the engine — assertions about the *generator*. A differential
    // test that never produced a short circuit or an error would be green and worthless,
    // and it would stay that way silently if the op weights were ever edited.
    assert_eq!(total_interactions, 2400);
    assert!(
        short_circuits > 200,
        "the corpus exercised the value-equality short circuit only {short_circuits} times"
    );
    assert!(
        errors_seen > 50,
        "the corpus exercised the error path only {errors_seen} times"
    );
    // Frames are the values whose equality is expensive and whose representation is
    // pluggable. If an op-weight edit ever stopped generating them, every backend claim
    // this corpus is supposed to support would quietly become vacuous.
    assert!(
        frame_cells > 1000,
        "the corpus compared only {frame_cells} frame-valued cells"
    );
}

#[test]
fn a_full_recompute_of_an_untouched_session_changes_nothing() {
    // The other half of the oracle's contract: if `refresh` on a settled session could
    // change a value, every comparison above would be meaningless.
    for seed in 1..=40u64 {
        let mut rng = Lcg(seed.wrapping_mul(0x2545F4914F6CDD1D));
        let g = generate(&mut rng);
        let mut s = Session::new(Arc::clone(&g.graph));
        s.refresh();
        let before: Vec<Outcome> = g.cells.iter().map(|n| s.get(n).unwrap().clone()).collect();

        let again = s.refresh();
        assert_eq!(
            again.evaluated(),
            0,
            "seed {seed}: a settled session re-ran work"
        );
        for (name, was) in g.cells.iter().zip(before) {
            assert_eq!(s.get(name).unwrap(), &was, "seed {seed}: `{name}` moved");
        }
    }
}

#[test]
fn two_sessions_share_one_allocation_per_untouched_source() {
    // The multi-tenancy claim, asserted deterministically. An RSS measurement would be a
    // flaky test on any CI runner; pointer identity is the actual property.
    let mut b = Graph::builder();
    b.source("data", Value::list((0..10_000).map(Value::int).collect()));
    b.source("threshold", Value::int(1));
    b.cell("n", ["data", "threshold"], |i| {
        Ok(Value::int(
            i.get(0).as_list().map(|l| l.len()).unwrap_or(0) as i64
        ))
    });
    let graph = b.build().unwrap();

    let a = Session::new(Arc::clone(&graph));
    let mut c = Session::new(Arc::clone(&graph));
    let data = graph.id("data").unwrap();
    assert!(
        Arc::ptr_eq(&a.share(data), &c.share(data)),
        "an untouched source is one allocation, however many viewers there are"
    );

    // And once a session sets one, only that session's slot moves.
    c.set("threshold", Value::int(9)).unwrap();
    c.commit();
    let threshold = graph.id("threshold").unwrap();
    assert!(!Arc::ptr_eq(&a.share(threshold), &c.share(threshold)));
    assert!(
        Arc::ptr_eq(&a.share(data), &c.share(data)),
        "and the big one is still shared"
    );
}
