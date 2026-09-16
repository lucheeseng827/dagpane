# ADR-0004: A failing cell holds an error the way it would hold a value

**Status:** accepted · **Date:** 2026-09

## Context

A data app is a place where one bad column should take out one number, not the page. The
number a user is looking at may be broken; the slider they would use to fix it must not be.

Both reflexes fail that. A panic ends the process and every session in it. A `?` out of the
pass ends the pass at the first failure, which in a runtime that re-renders top to bottom
means everything after the failing point is simply not drawn — including the control that
would change the input that caused it.

Making failure a caller's decision was the other option and it is worth saying why it was
rejected rather than only that it was. `Session::commit` would return
`Result<Trace, CellError>`, `evaluate` would stop at the first error, and the app layer
would decide what to render. Three things go wrong. The cells the pass had already evaluated
are now in an unknown state relative to the ones it had not, so the session no longer has a
coherent epoch. The trace describes a partial pass, so every number this project publishes
becomes conditional. And the only honest recovery is "recompute everything", which is the
rerun this engine exists to not be.

## Decision

`Outcome` is `Value { value }` or `Error { error }`, and a slot holds one of the two. An
error is a value: it is stored in a slot, digested, compared, and propagated by the same
machinery.

Four mechanics carry it:

**The compute is never called when an input is in error.** Before evaluating, `evaluate`
looks for an input already in error:

```rust
let upstream = held.iter().enumerate().find_map(|(i, o)| {
    o.error().map(|e| {
        let cause = e.cause(graph.name(node.inputs[i])).to_string();
        CellError::Upstream { message: e.message().to_string(), cause }
    })
});
```

If one is found the cell becomes `Upstream` without the compute running at all. That is why
`Inputs` carries `&Value` and not `Result` — errors never reach it, so a cell author writes
the happy path and nothing else.

**`CellError::Upstream { cause, message }` names the cell where the failure originated, not
the immediate parent.** `CellError::cause` returns the parent's own name only when the parent
is `Failed`; when the parent is itself `Upstream`, the existing `cause` passes through
unchanged. Attribution therefore survives an arbitrary chain, and
`an_error_eight_cells_down_still_names_the_cell_that_failed` builds eight cells below a
failure and asserts that `s7` still names `broken` and still carries the original message.

**The pass always finishes.** There is no early return in `evaluate`. A failure produces a
`StepOutcome::Failed { message }` step and the loop moves to the next id in the plan, so a
failing cell counts in the same arithmetic as every other visited cell — `debug_assert_trace`
asserts that visited equals set plus evaluated plus reused plus failed on every pass in a
debug build.

**A caller's mistake is a different type.** `SessionError` — `UnknownCell`, `NotAnInput`,
`TypeMismatch`, `ForeignCell` — is returned from `set` and `get`, not stored in a slot,
because none of those can be caused by data. A type mismatch is refused at the edge where it
arrives rather than surfacing inside somebody's compute three cells away from the socket
that caused it.

## Consequences

**A failing cell's siblings still evaluate.** In `a_failing_cell_does_not_take_down_the_page`,
`ratio` fails on a zero divisor while `unrelated`, which reads the same source, still holds
its value. The page renders with one broken number in it.

**An identical repeated failure does not re-wake the page.** Because an error is a value it
goes through the same digest comparison as one. In
`the_same_failure_twice_does_not_re_wake_the_page` the input moves, the failing cell re-runs
and fails with the same message, and the cell below it is **reused** — its compute is not
called, asserted by a counter that stays at zero.

**Recovery clears the whole poisoned subtree in one pass.** In
`a_recovered_cell_wakes_everything_below_it` the error's digest is replaced by a value's, so
every dependent's recorded input digests differ and the pass runs down the subtree once.
There is no separate error-clearing mode and no second pass.

**A failure that moves to a new origin is a different value.** This is the bug recorded in
ADR-0003 and the reason `CellError`'s `Digestible` impl hashes `cause` as well as `message`:
the message is what a user reads, the cause is what they act on, and both are the value.

**A cell author cannot see or handle an upstream error, and there is no escape hatch.** There
is no `Inputs::error(i)`, no `Result` in the compute's arguments, no fallback on an edge. A
cell that wants to render "no data" when its source fails cannot; it becomes `Upstream` like
everything else. The omission is deliberate — an escape hatch means calling a compute with an
error present, which makes every cell author responsible for propagation and stops the
guarantee being structural — but it is a real limitation with no design behind it today. The
place it would go is `Inputs`, which already holds the names and the values.

**When two inputs are in error at once, the cell names the first one declared.** `find_map`
returns on the first hit, so attribution is deterministic but arbitrary between two
simultaneous failures, and the message a viewer reads names one of the two. The information
is not lost — both failing cells appear as `Failed` steps in the trace, and `dagpane explain`
prints them — but the page shows one cause.

**An error is cached exactly like a value, so a compute that fails for a reason its inputs
cannot see stays failed until an input moves.** A cell whose inputs have not changed serves
its cached error without re-running; there is no retry. This is the same purity assumption
the whole engine rests on — `dagpane-core` has no clock and no I/O — but the consequence is
more surprising for a failure than for a value, so it is written down here.

**An error's text reaches the browser.** `CellError` derives `Serialize`/`Deserialize` under
`#[serde(tag = "kind")]` and `Display` renders `Upstream` as ``upstream cell `X` failed:
<message>``. The message is whatever the app author's compute put in `CellError::failed`, so
an app that formats a connection string into one puts it on a page. Nothing redacts it.
`dagpane run` binds 127.0.0.1 by default, which narrows who can read it and is not a
mitigation. The asymmetry is worth noticing: `Session`'s `Debug` impl deliberately withholds
the values it holds, and a `CellError` deliberately carries its message all the way to the
client.
