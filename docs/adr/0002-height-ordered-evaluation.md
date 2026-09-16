# ADR-0002: A pass evaluates in ascending height, and height is computed once at build time

**Status:** accepted · **Date:** 2026-09

## Context

A **glitch** is a cell observing a mixture of old and new upstream values — a state that was
never true of the app. The smallest example is a diamond, and it is the one in the test
suite: `a` feeds `b` and `c`, both feed `d`, with `b = a + 1` and `c = a * 2`. Every
consistent observation satisfies `(b - 1) * 2 == c`. Set `a` and there is exactly one right
answer for `d`, but several ways to arrive at a wrong one.

**Alternative 1: push eagerly, in the order the walk reaches cells.** Mark `a` dirty, walk
the reverse edges, evaluate as you go. Depth-first from `a` reaches `b`, runs it, reaches
`d`, and runs `d` on the new `b` against the **old** `c`. Then it comes back for `c` and runs
`d` again. Two things are wrong and only one of them is the wasted work: `d`'s compute was
called on a state that never existed, and if `d` does anything beyond returning a value —
appends to a log, increments a counter, writes a patch a socket is about to flush — that
happened. Insertion order has the same defect for the same reason: the order a walk produces
is not a topological order of the graph.

**Alternative 2: re-run until stable.** Evaluate the dirty set in any order and repeat until
nothing changes. It converges on a DAG, so the *final* values are right, and that is exactly
what makes it seductive and dangerous — every count assertion about final state passes. It
is worse on three counts. The intermediate states are real, so `d` still runs at least twice
and its first run still saw the mixed state. The number of iterations is a function of the
graph's shape and of the order chosen, so the cost of an interaction is not bounded by the
size of its closure. And "stable" is detected by comparing values, which is the digest
comparison of ADR-0003 — so the hashing cost is paid once per iteration instead of once.

## Decision

`GraphBuilder::build` runs Kahn's algorithm once and, in the same loop, computes

```rust
height[dep.index()] = height[dep.index()].max(height[id.index()] + 1);
```

**Height is the longest path from any source.** It is final by the time Kahn pops a node,
because Kahn visits every predecessor of a node before the node, so the `max` was taken over
all of them. `build` then re-sorts `order` by `(height, id)`.

The property that follows is the whole argument: **every edge runs from a lower height to a
strictly higher one**, so ascending height is a topological order, and a cell's inputs have
all reached their final value for this pass before the cell is evaluated.

`Session::dirty_closure` walks the reverse edges from the roots that actually changed —
marking with the epoch rather than clearing marks, since epochs only increase and a stale
stamp can never read as fresh — and ends with one line:

```rust
out.sort_by_key(|id| (graph.height(*id), id.0));
```

`Session::evaluate` then walks that plan once, and its doc comment states the contract:
`plan` must already be in ascending height order. `Session::refresh` hands it
`graph.order()`, which is the same order over the whole graph, so the first render and every
later pass share one loop.

**Why height rather than Kahn's own index**, which is also a topological order and would also
be glitch-free: a Kahn index is an artefact of the queue's tie-breaking, so inserting one
cell renumbers the whole app, while height changes only for the cells downstream of the new
one. Height is a fact about a cell rather than a position in a list, which is why it can be
published — `Graph::height` is public, `dagpane graph --format text` prints it as the left
column with a line explaining what it means, and `--format json` carries it per cell. And
cells at equal height are provably independent, which is what a level-parallel evaluator
would need; nothing uses that today, and it is a reason the shape was kept, not a feature.

Ties are broken by ascending id everywhere — in the ready queue during the build and in the
plan sort — so a trace is deterministic and two runs of `dagpane explain --json` are
diffable.

## Consequences

**The guarantee is structural, not defensive.** There is no re-entrancy guard, no
"already evaluating" flag, no second pass, and no convergence loop anywhere in `session.rs`.
`evaluate` is one `for` loop and each planned cell is visited exactly once. One line in
`dirty_closure` carries all of it; delete the sort and the engine still produces correct
*final* values on most graphs, which is precisely why the test that guards it has to look at
something other than final values.

**`a_diamond_join_never_sees_a_mixed_state` is that test.** `d`'s compute pushes the pair it
was given into a `Mutex<Vec<(i64, i64)>>`, and the test asserts `(b - 1) * 2 == c` for every
observation and exactly six observations across six passes. Its sibling
`a_diamond_join_runs_once_per_pass` asserts the count, and the count alone would not catch a
scheduler that ran `d` once on a mixed state.

**The dirty closure is *structural*, and a pass therefore visits cells that turn out to
reuse.** Everything downstream of the changed input is planned, including cells whose inputs
did not move. Visiting one is a digest comparison per input and a push, not a compute — but
it is not zero, and it grows with the closure rather than with the work. `Trace::visited()`
reports it and `Trace::evaluated()` is a subset; the trace has no way to report the smaller
number as the headline. On the bundled 11-cell example, `--set min_amount=400` visits 8 —
one source set, six computes, and `channel_count`, which reused.

**Sorting is over the closure, not the graph.** A one-cell change in a thousand-cell app
sorts a handful of ids, so the height order costs O(k log k) in k touched cells and nothing
per pass in the untouched remainder.

**Height order is asserted end to end, not only in the engine.**
`graph_prints_every_cell_in_height_order` runs the CLI against the bundled example, reads
the height column off eleven lines, and asserts it never decreases — so a change that broke
the ordering would fail a test somebody runs even if they never opened `session.rs`.

**A debug build re-checks the arithmetic on every pass.** `debug_assert_trace` asserts that
every visited cell has exactly one outcome and that the pass visited no more cells than it
planned. It lives in `session.rs` rather than in the tests so that every test gets it: a
scheduler change that double-counts a cell or steps outside its own closure fails the next
test anybody runs, not the one somebody remembered to write.
