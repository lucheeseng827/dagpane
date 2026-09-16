# dagpane-core — the reactive engine

**Pure: no I/O, no async runtime, no network, no clock, no `unsafe`.** One dependency
(`serde`, for the value model — nothing here opens a file or a socket). `cargo test -p
dagpane-core` is a complete test of the reactive semantics on a machine with no browser and
no network.

This is the only surface where the product's claim can be *wrong*. A missed invalidation
shows somebody a stale number and the page looks fine; a spurious one is the exact tax this
project exists to remove. Both are bugs in this crate and nowhere else.

## Architecture

```mermaid
flowchart TD
    builder["GraphBuilder<br/>declare cells and their inputs"]
    graph["Graph — immutable, shared<br/>topological order · per-cell height · reverse edges"]
    session["Session — one per viewer<br/>slots: value · digest · input_digests"]
    trace["Trace<br/>what the pass did"]

    builder -->|"build(): Kahn's algorithm,<br/>cycle rejected here"| graph
    graph -->|"Arc, shared by every session"| session
    session --> trace

    digest["digest — 128-bit FNV<br/>taken once, when a value is produced"]
    value["value — scalars, lists,<br/>a small columnar Table"]
    transform["transform — filter · select · sort<br/>limit · group_by"]

    session -.-> digest
    value -.-> digest
    transform --> value
```

## What each module decides

| module | what it decides |
|---|---|
| `graph` | what a cell is, what it reads, and — at build time — the topological order, the per-cell height, the reverse edges, and whether there is a cycle |
| `session` | the recompute pass: which cells are visited, which run, which serve a cached value |
| `digest` | whether two values are the same, in O(1), whatever their size |
| `value` | what can travel along an edge: scalars, lists, and a small columnar `Table` |
| `transform` | the built-in table operations, so an app can be written without a compiler |
| `trace` | what a pass did — the product claim as data rather than as a sentence |
| `error` | the three kinds of failure, kept apart because only one may reach a user mid-session |

## Event flow

One interaction is one pass. The diagram is the whole of `Session::commit`.

```mermaid
sequenceDiagram
    participant C as Caller
    participant S as Session
    participant G as Graph
    participant F as a cell's Compute

    C->>S: set("threshold", 1.0)
    Note over S: staged, nothing runs yet
    C->>S: commit()
    S->>S: digest the staged value
    alt equal to the value already held
        S-->>C: empty Trace — no cell visited
    else changed
        S->>G: dependents, transitively
        G-->>S: the dirty closure, sorted by (height, id)
        loop each cell, ascending height
            S->>S: compare inputs' digests to the ones recorded last run
            alt unchanged
                Note over S: Reused — the compute never runs
            else changed
                S->>F: eval(inputs)
                F-->>S: a value
                S->>S: digest it; changed = digest differs
            end
        end
        S-->>C: Trace — visited · evaluated · reused · changed
    end
```

Ascending height is what makes it glitch-free: every edge runs from a lower height to a
strictly higher one, so a cell's inputs are final before it runs.

## Quickstart

```rust
use dagpane_core::{Graph, Session, Value};

let mut b = Graph::builder();
b.source("threshold", Value::float(10.0));
b.cell("label", ["threshold"], |i| Ok(Value::text(format!("{}", i.float(0)?))));
let mut s = Session::new(b.build()?);

s.refresh();                                  // the first pass computes everything
s.set("threshold", Value::float(1.0))?;
let t = s.commit();                           // and this one computes what depends on it
assert_eq!(t.evaluated(), 1);

s.set("threshold", Value::float(1.0))?;       // the value it already holds
assert_eq!(s.commit().visited(), 0);          // so no cell is even looked at
```

The last two lines are the non-obvious part: a client re-sending its state on reconnect costs
nothing, because an input set to the value it already holds is not a change.

```sh
cargo test -p dagpane-core
```

## The four ideas, in the order they matter

1. **Edges are declared**, so the graph is checked once and shared immutably across every
   session, and a cycle is a build error rather than something a user runs into.
2. **Evaluation is in ascending height**, which is what makes it glitch-free.
3. **A value that did not change stops the pass**, by content digest rather than comparison.
4. **Every pass reports what it did**, so the claim is an assertion in a test rather than a
   sentence in a README.

Each has an ADR under `docs/adr/` saying what breaks if you change it.

## Errors are values

A cell whose compute fails holds its error; every cell below it holds one naming the cell
that *actually* failed. The pass finishes. One broken column takes out one number, not the
page — and not the control that will fix it.
