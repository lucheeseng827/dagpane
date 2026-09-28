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

use dagpane_core::reads::{RowConstraint, RowRule};
use dagpane_core::transform::{Comparison, Filter};
use dagpane_core::value::{Column, Table};
use dagpane_core::{CellError, CellId, Cut, Graph, Outcome, Session, Split, Value};

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

// ── the column-level half ──────────────────────────────────────────────────────────────
//
// Everything above changes a whole source value and asserts against the *structural*
// closure. Sub-node invalidation makes a finer claim — a cell reading three columns of two
// hundred sleeps through a change to the other hundred and ninety-seven — and a finer claim
// needs a finer oracle. `ROADMAP.md` §3 is explicit that this comes before the scheme is
// trusted, and the reason is the direction of the failure: a column key that is too narrow
// keeps a cached value after something the cell genuinely read has moved, and the user sees
// a stale number on a page that looks like it is working. Nobody reports that.
//
// So the same two assertions, against a finer closure:
//
//   * **Correctness.** After rewriting one column of one source frame, every cell holds the
//     value a from-scratch session computes. This is the assertion that catches a key that
//     skipped a recompute it owed.
//   * **Economy.** A cell that does not transitively read the rewritten column did not
//     evaluate. This is the assertion that says the granularity is real and not decorative.

/// How wide the generated source frames are. Wide enough that a random subset is a small
/// fraction of the whole, which is the condition under which the claim means anything.
const WIDE: usize = 24;
/// Rows per generated source frame. Small: this test is about which cells ran, not about
/// how fast they ran.
const DEEP: usize = 6;
/// The floor a generated predicate filters on. Chosen so that a salt bump moves values across
/// it often but not always — a corpus where every edit moved every selection would exercise
/// the constraint machinery without ever letting a constraint hold.
const FLOOR: i64 = 3_000;

/// One cell of a generated source frame.
///
/// Two terms, and the second exists for the ordering constraints. `base` alone rises with the
/// row, so every column of every frame would rank the rows `0, 1, 2, …` whatever its salt, and
/// an ordering constraint would hold on every edit without ever being tested. `wobble` is a
/// salt-dependent jitter of at most ±10 against a row step of 7: big enough to swap
/// neighbouring rows on some salts and not on others, and two orders of magnitude too small to
/// move a value across [`FLOOR`] and disturb the predicate corpus.
fn cell_value(salt: i64, c: usize, r: usize) -> i64 {
    let base = salt
        .wrapping_mul(1000)
        .wrapping_add((c * 31 + r * 7) as i64);
    let wobble = salt.wrapping_mul(7).wrapping_add((c * 13 + r * 5) as i64) % 11 - 5;
    base.wrapping_add(wobble.wrapping_mul(2))
}

/// A source frame's contents, as one salt per column. Rewriting "a column" is bumping one
/// of these, which changes every value in that column and nothing anywhere else.
type Salts = Vec<i64>;

fn frame_of(salts: &Salts) -> Value {
    let columns = salts
        .iter()
        .enumerate()
        .map(|(c, salt)| {
            Column::int(
                format!("c{c}"),
                (0..DEEP).map(|r| Some(cell_value(*salt, c, r))).collect(),
            )
        })
        .collect();
    Value::table(Table::new(columns).expect("every column is built to one length"))
}

/// What a generated cell does with the columns it read.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ColOp {
    /// Emit a frame holding exactly the columns it read, in order. The projection case, and
    /// what makes a chain of narrowing cells possible: this cell's output column `j` is its
    /// input's read column `j`, so a consumer narrowing on *this* frame is still ultimately
    /// narrowing on a source column.
    Project,
    /// Emit the sum of every value in every column it read. A scalar consumer.
    Total,
    /// Filter the frame on one column and total *other* columns over the surviving rows,
    /// recording a **predicate constraint** rather than a dependency on the filtered column.
    ///
    /// This is the risky path: the cell is allowed to keep its cached value after the filtered
    /// column has demonstrably changed, on the strength of the selection being the same. The
    /// correctness assertion is what says the engine may.
    Constrained,
    /// Sort the frame on one column and total *other* columns over the **top half** of the
    /// resulting order, recording an **ordering constraint** rather than a dependency on the
    /// sorted column.
    ///
    /// The top half is what makes this a test rather than a formality. A sum over every row is
    /// the same sum in any order, so a cell that totalled the whole frame would agree with the
    /// oracle even when the engine had wrongly decided an ordering still held. Taking a prefix
    /// of the order makes the value depend on the order itself, which is exactly what the
    /// constraint claims to pin — and it is what a real top-N pane does.
    Ordered,
}

/// What a value looks like to a consumer: the source columns behind each of its output
/// columns, or `None` when it is a scalar and has none.
type Shape = Option<Vec<(usize, usize)>>;

/// One generated cell's declared reads: for each input, which of that input's columns.
type Reads = Vec<(usize, Vec<usize>)>;

struct ColGraph {
    graph: Arc<Graph>,
    sources: Vec<String>,
    cells: Vec<String>,
    /// Per cell, the source columns it transitively depends on, as `(source, column)`.
    depends: Vec<(String, HashSet<(usize, usize)>)>,
    /// Per cell, the source columns it may *legitimately* wake for without depending on
    /// their values: the ones it placed a row constraint on, and — transitively — the ones
    /// anything it reads placed a constraint on.
    ///
    /// A constrained cell is allowed to wake when its filtered column moves, because the
    /// selection may have moved with it; and if it does wake and its value changes, every
    /// cell below it wakes too. So the economy assertion exempts the whole cone rather than
    /// asserting either way. What none of them may do is keep a *wrong* value, and that is
    /// what the correctness assertion is for.
    constrained: Vec<(String, HashSet<(usize, usize)>)>,
}

/// A random graph over wide frame sources, where every cell narrows honestly.
///
/// "Honestly" is the whole point: each cell reads exactly the columns it declares, so the
/// correctness assertion tests the *engine's* comparison rather than the generator's
/// bookkeeping. A cell that read more than it declared would be a bug in this file and would
/// show up as a correctness failure, which is the right way round.
fn generate_columnar(rng: &mut Lcg) -> ColGraph {
    let source_count = 2 + rng.below(2);
    let computed_count = 6 + rng.below(12);

    let mut b = Graph::builder();
    let mut sources = Vec::new();
    // What each named value looks like to a consumer: the source columns behind each of its
    // output columns, or `None` for a scalar.
    let mut shape: Vec<(String, Shape)> = Vec::new();
    let mut depends: Vec<(String, HashSet<(usize, usize)>)> = Vec::new();
    let mut constrained: Vec<(String, HashSet<(usize, usize)>)> = Vec::new();

    for s in 0..source_count {
        let name = format!("src_{s}");
        b.source(&name, frame_of(&vec![0; WIDE]));
        sources.push(name.clone());
        shape.push((
            name.clone(),
            Some((0..WIDE).map(|c| (s, c)).collect::<Vec<_>>()),
        ));
        depends.push((name.clone(), HashSet::new()));
        constrained.push((name, HashSet::new()));
    }

    for i in 0..computed_count {
        let name = format!("k_{i}");
        let arity = 1 + rng.below(2.min(shape.len()));
        let mut input_names: Vec<String> = Vec::new();
        let mut reads: Reads = Vec::new();
        let mut mine: HashSet<(usize, usize)> = HashSet::new();
        // The source columns behind this cell's own output columns, in output order.
        let mut out: Vec<(usize, usize)> = Vec::new();

        for a in 0..arity {
            let pick = rng.below(shape.len());
            let (pick_name, pick_shape) = shape[pick].clone();
            input_names.push(pick_name.clone());
            match pick_shape {
                Some(cols) => {
                    // A random non-empty subset of that frame's columns.
                    let take = 1 + rng.below(3.min(cols.len()));
                    let mut chosen: Vec<usize> = Vec::new();
                    for _ in 0..take {
                        let c = rng.below(cols.len());
                        if !chosen.contains(&c) {
                            chosen.push(c);
                        }
                    }
                    chosen.sort_unstable();
                    for c in &chosen {
                        mine.insert(cols[*c]);
                        out.push(cols[*c]);
                    }
                    reads.push((a, chosen));
                }
                None => {
                    // A scalar input. Nothing to narrow, and it carries its own dependencies.
                    let (_, was) = depends
                        .iter()
                        .find(|(n, _)| *n == pick_name)
                        .expect("declared earlier");
                    mine.extend(was.iter().copied());
                }
            }
        }

        // Inherit the dependencies of every frame input as well.
        for (a, _) in &reads {
            let n = &input_names[*a];
            if let Some((_, was)) = depends.iter().find(|(d, _)| d == n) {
                mine.extend(was.iter().copied());
            }
        }

        // The column the rule reads, when this cell is going to be a constrained one: one of
        // the columns of its first frame input, chosen from the whole width rather than from
        // what it reads, so the rule's column is usually one whose values never reach the
        // output.
        let pivot: Option<(usize, usize, (usize, usize))> = reads.first().and_then(|(at, _)| {
            let (pick_name, pick_shape) = shape
                .iter()
                .find(|(n, _)| n == &input_names[*at])
                .cloned()?;
            let _ = pick_name;
            let cols = pick_shape?;
            let c = rng.below(cols.len());
            Some((*at, c, cols[c]))
        });

        let op = match rng.below(4) {
            0 if !reads.is_empty() => ColOp::Project,
            1 if pivot.is_some() => ColOp::Constrained,
            2 if pivot.is_some() => ColOp::Ordered,
            _ => ColOp::Total,
        };

        // A constrained cell depends on its pivot column's *answer* — which rows a predicate
        // kept, or what order a sort produced — and not on its values, so it is not added to
        // `mine`. It is recorded separately: the economy assertion has to allow such a cell to
        // wake when that column moves, because the answer may have moved with it.
        // This cell's own constraint, plus everything its inputs may wake for.
        let mut soft: HashSet<(usize, usize)> = HashSet::new();
        if let (ColOp::Constrained | ColOp::Ordered, Some((_, _, origin))) = (op, pivot) {
            soft.insert(origin);
        }
        for n in &input_names {
            if let Some((_, was)) = constrained.iter().find(|(d, _)| d == n) {
                soft.extend(was.iter().copied());
            }
        }

        let plan = reads.clone();
        let constrain = match op {
            ColOp::Constrained | ColOp::Ordered => pivot.map(|(at, c, _)| (at, c)),
            _ => None,
        };
        b.cell(&name, input_names.clone(), move |i| {
            let mut kept: Vec<Column> = Vec::new();
            let mut acc: i64 = 0;
            // When this cell is a constrained one, the rows it looks at are the ones its rule
            // chose — and the rule is recorded instead of the column it read.
            let mut rows_kept: Option<Vec<usize>> = None;
            if let Some((at, c)) = constrain {
                if let Value::Frame { v } = i.get(at) {
                    let f = v.as_frame();
                    let column = f.schema()[c].0.clone();
                    match op {
                        ColOp::Ordered => {
                            if let Ok(order) = dagpane_core::transform::ordering(f, &column, false)
                            {
                                i.reads_constraint(
                                    at,
                                    RowConstraint {
                                        column: c as u32,
                                        rule: RowRule::Sort {
                                            column,
                                            descending: false,
                                        },
                                        rows: dagpane_core::transform::ordering_digest(&order),
                                    },
                                );
                                // The top half of the order. A prefix, so the value moves when
                                // the ranking does.
                                rows_kept = Some(order[..order.len().div_ceil(2)].to_vec());
                            }
                        }
                        _ => {
                            let spec = Filter {
                                column,
                                op: Comparison::Ge,
                                value: Value::int(FLOOR),
                            };
                            if let Ok(keep) = dagpane_core::transform::selection(f, &spec) {
                                i.reads_constraint(
                                    at,
                                    RowConstraint {
                                        column: c as u32,
                                        rule: RowRule::Filter(spec),
                                        rows: dagpane_core::transform::selection_digest(&keep),
                                    },
                                );
                                rows_kept = Some(keep);
                            }
                        }
                    }
                }
            }
            for (at, cols) in &plan {
                let Value::Frame { v } = i.get(*at) else {
                    acc = acc.wrapping_add(i.get(*at).as_int().unwrap_or(0));
                    continue;
                };
                // The claim, made before the read and covering exactly it.
                i.reads_only_columns(*at, cols);
                let f = v.as_frame();
                let rows: Vec<usize> = match (&rows_kept, constrain) {
                    (Some(keep), Some((fat, _))) if fat == *at => keep.clone(),
                    _ => (0..f.rows()).collect(),
                };
                for c in cols {
                    let values: Vec<Option<i64>> =
                        rows.iter().map(|r| f.value_at(*r, *c).as_int()).collect();
                    for x in values.iter().flatten() {
                        acc = acc.wrapping_add(*x);
                    }
                    kept.push(Column::int(format!("p{}", kept.len()), values));
                }
            }
            // Every input that is not a frame still contributes, and is compared whole.
            for n in 0..i.len() {
                if !matches!(i.get(n), Value::Frame { .. }) {
                    acc = acc.wrapping_add(i.get(n).as_int().unwrap_or(0));
                }
            }
            match op {
                ColOp::Total | ColOp::Constrained | ColOp::Ordered => Ok(Value::int(acc)),
                ColOp::Project => Ok(Value::table(
                    Table::new(kept).expect("every projected column has the input's row count"),
                )),
            }
        });

        shape.push((
            name.clone(),
            match op {
                ColOp::Project => Some(out),
                ColOp::Total | ColOp::Constrained | ColOp::Ordered => None,
            },
        ));
        depends.push((name.clone(), mine));
        constrained.push((name.clone(), soft));
    }

    let cells = shape.iter().map(|(n, _)| n.clone()).collect();
    ColGraph {
        graph: b.build().expect("acyclic by construction"),
        sources,
        cells,
        depends,
        constrained,
    }
}

/// A from-scratch session over the same source contents.
fn columnar_oracle(graph: &Arc<Graph>, sources: &[String], salts: &[Salts]) -> Session {
    let mut s = Session::new(Arc::clone(graph));
    for (name, salt) in sources.iter().zip(salts) {
        s.set(name, frame_of(salt)).expect("a generated source");
    }
    s.refresh();
    s
}

#[test]
fn changing_one_column_agrees_with_a_full_recompute_and_wakes_only_its_readers() {
    let mut interactions = 0usize;
    let mut slept = 0usize;
    let mut woken = 0usize;

    for seed in 1..=120u64 {
        let mut rng = Lcg(seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(17));
        let g = generate_columnar(&mut rng);

        let mut salts: Vec<Salts> = g.sources.iter().map(|_| vec![0i64; WIDE]).collect();
        let mut live = Session::new(Arc::clone(&g.graph));
        for (name, salt) in g.sources.iter().zip(&salts) {
            live.set(name, frame_of(salt)).unwrap();
        }
        live.refresh();

        for _ in 0..8 {
            // One interaction: rewrite exactly one column of exactly one source frame.
            let which = rng.below(salts.len());
            let column = rng.below(WIDE);
            salts[which][column] = salts[which][column].wrapping_add(1 + rng.below(4) as i64);
            live.set(&g.sources[which], frame_of(&salts[which]))
                .unwrap();
            let trace = live.commit();
            interactions += 1;

            // ── correctness ───────────────────────────────────────────────────────────
            // The assertion that catches a column key too narrow to have noticed.
            let truth = columnar_oracle(&g.graph, &g.sources, &salts);
            for name in &g.cells {
                assert_eq!(
                    live.get(name).unwrap(),
                    truth.get(name).unwrap(),
                    "seed {seed}: cell `{name}` disagrees with a full recompute after \
                     rewriting column {column} of `{}`\n  trace: {}",
                    g.sources[which],
                    trace.summary()
                );
            }

            // ── economy ───────────────────────────────────────────────────────────────
            // A cell that does not read the rewritten column must not have run. This is the
            // whole claim of sub-node invalidation, stated as an assertion.
            let moved = (which, column);
            for step in &trace.steps {
                let reads_it = g
                    .depends
                    .iter()
                    .find(|(n, _)| *n == step.cell)
                    .map(|(_, d)| d.contains(&moved))
                    .unwrap_or(true)
                    // A cell holding a row constraint on this column may legitimately wake:
                    // its answer is allowed to have moved with the values. What it may *not*
                    // do is keep a wrong value, and that is the other assertion.
                    || g.constrained
                        .iter()
                        .find(|(n, _)| *n == step.cell)
                        .map(|(_, c)| c.contains(&moved))
                        .unwrap_or(false);
                if matches!(
                    step.outcome,
                    dagpane_core::trace::StepOutcome::Evaluated { .. }
                ) {
                    assert!(
                        reads_it,
                        "seed {seed}: `{}` evaluated after column {column} of `{}` changed, \
                         but it reads none of that column\n  trace: {}",
                        step.cell,
                        g.sources[which],
                        trace.summary()
                    );
                    woken += 1;
                }
            }
            // Cells the pass reached, that read nothing that moved, and that actually
            // reused. Counted off the trace rather than off the declared dependency sets:
            // the first version of this counter walked `g.depends`, which holds every source
            // with an empty set, so every source counted as having slept on every
            // interaction and the guard below cleared itself on those alone — while never
            // once looking at what the pass did.
            for step in &trace.steps {
                if g.sources.contains(&step.cell) {
                    continue;
                }
                let outside = g
                    .depends
                    .iter()
                    .find(|(n, _)| *n == step.cell)
                    .is_some_and(|(_, d)| !d.contains(&moved));
                if outside && matches!(step.outcome, dagpane_core::trace::StepOutcome::Reused) {
                    slept += 1;
                }
            }
        }
    }

    assert_eq!(interactions, 960);
    // Assertions about the *corpus*, not the engine: a run in which nothing ever woke, or in
    // which nothing ever slept, would be green and worthless.
    assert!(
        woken > 200,
        "only {woken} cells ever woke — the corpus is inert"
    );
    // 4,000 is set from the measurement, not guessed: this corpus produces about 5,700, and
    // narrowing the generated frames from 24 columns to 2 — the degenerate case where there
    // is nothing for granularity to do — drops it to about 1,750. The threshold sits between
    // the two so that a generator change which stopped exercising sleeping fails here.
    assert!(
        slept > 4000,
        "only {slept} cell-interactions reused while reading nothing that moved — the \
         generated frames are not wide enough for this test to be measuring anything"
    );
}

// ── the cut ────────────────────────────────────────────────────────────────────────────
//
// `crates/core/src/placement.rs` argues that a graph split across a wire stays glitch-free
// as long as the placement is monotone along every edge and each pass's boundary values are
// applied together. An argument in a module doc is a claim. This is the check.
//
// The shape is the same as the oracle above and for the same reason: a count assertion on a
// hand-drawn split tests what the author expected the split to do. What has to hold is much
// stronger — **a split session and an unsplit one are indistinguishable by their values** —
// and that is a property, so it gets random graphs, random cuts and random interactions.

/// A random ADMISSIBLE client set.
///
/// Monotone placement means the client set is closed under dependents: if a cell runs in the
/// page then everything reading it must too, or a value would have to come back across the
/// cut. `Graph::closure` over the reverse edges is exactly that closure, so seeding it with
/// any cells at all yields a cut `Cut::new` will accept. Generating admissible cuts directly
/// — rather than generating arbitrary ones and discarding the rejects — is what keeps this
/// test dense: every seed exercises a real split instead of mostly exercising the refusal.
fn random_cut(graph: &Arc<Graph>, rng: &mut Lcg) -> Cut {
    let seeds: Vec<CellId> = (0..graph.len())
        .map(|i| graph.order()[i])
        .filter(|_| rng.below(6) == 0)
        .collect();
    let client: Vec<String> = graph
        .closure(&seeds)
        .into_iter()
        .map(|id| graph.name(id).to_string())
        .collect();
    Cut::of_client(graph, &client).expect("an upward-closed set is monotone by construction")
}

/// Both halves of a split app, driven the way a transport would drive them.
struct Pair {
    split: Split,
    server: Session,
    client: Session,
}

impl Pair {
    /// Open both sides. The client's boundary sources start at `Null`, so the server's first
    /// frontier has to reach it *before* its first pass — which is why a real transport puts
    /// the frontier in the opening frame rather than sending it after.
    fn open(graph: &Arc<Graph>, cut: &Cut, inputs: &[(String, i64)]) -> Pair {
        let split = graph.split(cut);
        let mut server = Session::new(Arc::clone(&split.server));
        let mut client = Session::new(Arc::clone(&split.client));

        for (name, v) in inputs {
            let value = Value::int(*v);
            if server.graph().id(name).is_some() {
                server.set(name, value).expect("a generated source");
            } else {
                client.set(name, value).expect("a client-placed source");
            }
        }
        server.refresh();
        // The FULL frontier, not a delta. A source the caller set to the value it already
        // held did not "change", and a client seeded only with changes would read null from
        // it forever. See `Split::full_frontier`.
        let frontier = split.full_frontier(&server);
        split
            .deliver(&mut client, &frontier)
            .expect("both halves came from one graph");
        client.refresh();

        Pair {
            split,
            server,
            client,
        }
    }

    /// One interaction, routed to whichever side owns the input.
    ///
    /// Returns whether the wire was used, so the caller can assert the economy claim: an
    /// interaction whose whole closure is in the page sends nothing.
    fn set_and_commit(&mut self, name: &str, v: i64) -> bool {
        if self.server.graph().id(name).is_some() {
            let epoch = self.server.epoch() + 1;
            self.server.set(name, Value::int(v)).expect("a source");
            self.server.commit();
            let frontier = self.split.frontier(&self.server, epoch);
            let sent = !frontier.is_empty();
            self.split
                .deliver(&mut self.client, &frontier)
                .expect("one graph");
            // ONE commit, after the whole frontier is staged. Committing inside `deliver`'s
            // loop is the glitch this design exists to make unavailable, and the reason
            // `deliver` stages rather than applies.
            self.client.commit();
            sent
        } else {
            // A client-placed control. The server is not told and does not run.
            self.client.set(name, Value::int(v)).expect("a source");
            self.client.commit();
            false
        }
    }

    /// What this pair believes a cell holds, from whichever side owns it.
    fn get(&self, name: &str) -> &Outcome {
        match self.server.graph().id(name) {
            // A boundary cell lives on both sides; the server's is the computed one and the
            // client's is the copy, so reading the server here is reading the original. That
            // the two agree is asserted separately below.
            Some(_) => self.server.get(name).expect("a server cell"),
            None => self.client.get(name).expect("a client cell"),
        }
    }
}

#[test]
fn a_split_graph_agrees_cell_for_cell_with_an_unsplit_one() {
    let mut cuts_that_split = 0usize;
    let mut boundary_total = 0usize;
    let mut local_interactions = 0usize;
    let mut wired_interactions = 0usize;

    for seed in 1..=200u64 {
        let mut rng = Lcg(seed.wrapping_mul(0x9E3779B97F4A7C15) ^ 0x5851F42D4C957F2D);
        let g = generate(&mut rng);
        let cut = random_cut(&g.graph, &mut rng);
        if cut.is_split() {
            cuts_that_split += 1;
            boundary_total += cut.boundary().len();
        }

        let mut inputs: Vec<(String, i64)> = g
            .sources
            .iter()
            .map(|n| (n.clone(), rng.below(6) as i64))
            .collect();

        let mut pair = Pair::open(&g.graph, &cut, &inputs);
        // The unsplit session: the same graph, undivided, driven with the same interactions.
        let mut whole = Session::new(Arc::clone(&g.graph));
        for (name, v) in &inputs {
            whole.set(name, Value::int(*v)).expect("a source");
        }
        whole.refresh();

        for _ in 0..12 {
            let pick = rng.below(inputs.len());
            let value = rng.below(6) as i64;
            inputs[pick].1 = value;
            let name = inputs[pick].0.clone();

            let sent = pair.set_and_commit(&name, value);
            whole.set(&name, Value::int(value)).expect("a source");
            whole.commit();

            // THE PROPERTY. Every cell of the original graph holds, in the split pair,
            // exactly what it holds undivided — values and errors alike.
            for cell in &g.cells {
                let undivided = whole.get(cell).expect("a generated cell");
                let divided = pair.get(cell);
                assert_eq!(
                    undivided,
                    divided,
                    "seed {seed}: `{cell}` disagrees after setting `{name}` to {value}\n\
                     cut: {} of {} cells on the client, boundary {:?}",
                    cut.client_cells(),
                    g.graph.len(),
                    pair.split.boundary,
                );
            }

            // The copy of a boundary cell the client holds is the value the server computed.
            // Asserted separately from the loop above, which reads boundary cells from the
            // server side and so could not catch a frontier that never arrived.
            for name in &pair.split.boundary {
                assert_eq!(
                    pair.server.get(name).expect("a boundary cell"),
                    pair.client.get(name).expect("a boundary source"),
                    "seed {seed}: the frontier did not carry `{name}`"
                );
            }

            // THE ECONOMY CLAIM. An interaction whose whole closure is in the page uses no
            // wire. `is_local` is decided from structure alone, before anything runs, so it
            // has to agree with what actually happened.
            let root = g.graph.id(&name).expect("a source");
            if cut.is_local(&g.graph, root) {
                local_interactions += 1;
                assert!(
                    !sent,
                    "seed {seed}: `{name}` is local to the client and still used the wire"
                );
            }
            if sent {
                wired_interactions += 1;
            }
        }
    }

    // The test is worthless if the generator mostly produced undivided graphs, so say what
    // it actually covered rather than trusting that it did.
    assert!(
        cuts_that_split > 150,
        "only {cuts_that_split} of 200 seeds produced a real split"
    );
    assert!(
        boundary_total > 200,
        "only {boundary_total} boundary cells across all seeds — the cuts are not crossing \
         enough edges to be testing the frontier"
    );
    assert!(
        local_interactions > 50,
        "only {local_interactions} interactions were client-local, so the no-wire claim is \
         barely exercised"
    );
    assert!(
        wired_interactions > 500,
        "only {wired_interactions} interactions crossed the wire"
    );
}
