# ADR-0008: placement is monotone, so the cut is a frontier

**Status:** accepted, and half-built on purpose — see *What is not built*.

## Context

ADR-0007 moved the whole graph into a browser and said what it was not:

> **Per-cell placement.** §4's defensible version was the placement of each *cell*. This
> moves the whole graph. The seam exists; splitting a graph across a wire with glitch
> freedom preserved across the cut is a separate piece of work with its own correctness
> argument.

This is that argument. The feature being asked for is narrow and worth stating exactly:
**where a cell runs should be a line in the manifest, not a different program.** Not a
second dialect for the client, not a "client mode" of the compiler, not a subset of the nine
verbs that happens to work in a page. One word per cell, and the same app.

The reason to want it is a number rather than a preference. `benches/roundtrip/` measured
that the wire is about half of every interaction on the friendliest network that exists, and
that wasm pays 2.2–2.8× on the pass for removing it. Moving the *whole* graph therefore
trades a real cost for a real saving, and which way the trade goes depends on the app. Moving
*part* of it is what makes the trade adjustable: put the cheap, interactive part in the page
and leave the expensive, shared part where the data is.

## Decision

### 1. One rule: no value comes back

A placement is admitted only if it is **monotone along every edge** — a cell on the client
may not be an input to a cell on the server. `dagpane check` refuses anything else, names the
edge, and names both ways to fix it:

```
dagpane: `region_totals` runs on the server and reads `filtered`, which runs on the
client: values cross the cut once and never come back. Move `region_totals` to the
client, or `filtered` to the server.
```

Everything below follows from that one rule, which is why it is a rule and not a warning.

### 2. Why the obvious design is wrong

Evaluation is in ascending height; that is what makes this engine glitch-free (ADR-0002). So
the obvious way to split a graph is to keep the height order and hop the wire whenever the
next cell is on the other side. It is correct, and it costs **a round trip per height
boundary the cut crosses**. An app whose placements alternate would pay six round trips for
one slider move — worse than the thing being replaced, and shipped as an improvement.

Monotonicity removes the alternation by construction. Values flow one way, so the cells whose
values must cross are a *set* and not a *sequence*: exactly the server cells with at least one
client dependent. Every one of them holds its final value for a pass by the time the server's
pass ends, because the server's pass is itself in ascending height. **One message per pass,
whatever the shape of the app.**

### 3. The client runs the ordinary engine

`Graph::split` returns two ordinary graphs. The client's is the client cells plus each
boundary cell **rewritten as a source**. It is not a special distributed evaluator; it is
`Session`, unmodified, over a graph whose sources happen to be filled by a message rather
than by a slider.

That is the part that makes the design cheap to believe rather than cheap to write. Ascending
height still holds on each side because each side is a graph that was built the usual way, and
there is no new code in the pass loop where a glitch could hide.

### 4. The whole obligation is atomicity

Glitch freedom across the cut needs exactly one thing beyond the above: the client applies a
pass's boundary values **all together or not at all**. Given that, a client cell reading two
boundary cells sees both from pass *N* or both from pass *N-1*, never one of each — the same
statement as the local one, with "the server's pass" where "a lower height" used to be.

So `Split::deliver` **stages** and never commits. The caller commits once, afterwards. A
`Frontier` is a type rather than a loop over pairs for the same reason: a transport cannot
deliver half of one by accident.

### 5. The first frontier is complete; every one after is a delta

A client's boundary sources start at `Null`. A client sent only what *changed* therefore
starts with holes — and not hypothetically. The split oracle caught it on seed 4, where a
source the caller re-set to the value it already held was legitimately "unchanged", so it was
absent from the delta and a client cell read a null it was never meant to see.

`full_frontier` pairs with the opening frame and `frontier(since)` with every pass after,
which is the same pairing `AppSession` already has between `full_views` and `patch`.

### 6. An error crosses as an error

`Session::set_outcome` exists so a boundary cell that failed arrives as a failure. A null
would be drawn as a legitimate empty answer — a broken pane that looks like an empty one.
Errors are values here (ADR-0004), so this widens the door by exactly as much as that
sentence already promised.

## How it is checked

Prose about glitch freedom is a claim. The checks are:

* **The split oracle** (`crates/core/tests/oracle.rs`). Two hundred generated graphs, each
  cut at a **random admissible place**, both halves driven through twelve interactions, and
  after every one the split pair must agree **cell for cell** with an undivided session over
  the same graph. Admissible cuts are generated directly — a client set is any set closed
  under dependents, which is what `Graph::closure` over the reverse edges returns — so every
  seed exercises a real split instead of mostly exercising the refusal. The test also asserts
  its own coverage: it fails if fewer than 150 seeds produced a real split, or if the
  client-local interactions it checked are too few to mean anything.
* **The same property on a real manifest** (`crates/app/tests/placement.rs`). The generated
  graphs are integer arithmetic; the values that cross a real cut are frames from `filter`,
  `group_by` and `sort` with column digests attached. This runs the bundled sales data
  through both halves and compares.
* **Four mutations, four caught.** Dropping a cell from the full frontier, dropping one from
  a delta, committing inside `deliver` instead of staging, and deep-copying a source in
  `clone_kind` each fail a different named test. The third is the glitch this ADR exists to
  prevent and the fourth would silently double an app's memory.

## Consequences

**The surface is one word.** `place = "client"` on a `[[cell]]` or an `[[input]]`.
`examples/apps/20-placed.toml` is `sales.toml`'s pipeline with four of them added and nothing
else changed. A manifest that names no placement compiles to `Cut::whole` and is the app this
project already had.

**`dagpane check` reports the two numbers a deployment turns on** — how wide the frontier is,
and how many controls would need no network. An app with a wide frontier and no local controls
has been cut the wrong way, and that shows up in a terminal rather than in a flame graph.

**A split costs memory on the server side too**, and this is the honest cost: the boundary
cell's value exists on both sides. Placing a filter in the page sends the rows to every
viewer, which is right for the tens of thousands of rows `BENCHMARKS.md` calls interactive and
wrong above that. The rule of thumb the check output is trying to support: **cut below the
data, not above it.**

**`[[source]]` takes no `place`.** Where a source's rows come from is the `Sources` seam from
ADR-0007, not this. A server-side source read by client cells becomes a boundary cell and
crosses, which is the behaviour anyone would want from writing `place` on it anyway.

### What is not built

**Nothing serves a split yet**, and the gap is now one layer thinner than it was. `AppSession`
runs either half, `init` and `patch` carry the frontier, and `dagpane-wasm` opens the page's
half and applies one — with
`the_two_halves_together_show_what_one_undivided_session_shows` requiring two sessions over the
real messages to produce the same panes as one undivided session.

What is missing is a page that can hold the other half, and the work above turned up two
questions that were not visible from here:

* **The page has to type-check its half without the data.** `compile_with` needs a schema for
  every `[[source]]` — a CSV's types are decided by reading it — and the point of cutting below
  the data is that the page does not get the data. The rows arrive as the frontier *after*
  compilation, so the opening frame has to carry the manifest and the boundary **schemas**, and
  `crates/connect` needs a source that offers a shape and no rows.
* **The stats bar needs a meaning under a split.** It is the product claim, live. With two
  halves each running a pass there are two sets of counts; showing one is a lie by omission and
  adding them is a different lie.

So `dagpane run` evaluates every cell on the server for a placed app and `dagpane check` says
so. Flipping it is one line in `crates/serve/src/lib.rs` and is deliberately not flipped: a
browser that cannot consume a frontier would render a placed app with its page-side panes
missing, which is worse than ignoring the cut.

Stating that plainly is the point of writing this down now. The surface is the part worth
reviewing first — if `place` is the wrong word, or monotonicity is the wrong rule, that is
much cheaper to discover before a protocol is written against it than after. The boundary
message and the split session are built; `ROADMAP.md` §4 tracks the one item left and the two
questions inside it.

**Per-cell placement across more than two sides** is not modelled. `Placement` has two
variants because there are two places to be. A third — an edge worker, a second server —
would generalise to a total order over sides with the same monotonicity rule, and nothing here
forecloses it, but writing it before anything needs it would be inventing requirements.
