<img src="docs/images/dagpane-logo.svg" alt="" width="72">

# dagpane

**An interaction recomputes the cells that depend on it, and nothing else — and the runtime
prints which ones, so you can check.**

A data app is a dependency graph whether or not its runtime treats it as one. dagpane
treats it as one: cells declare what they read, a change marks only what is downstream of
it, and a cell whose inputs still hold the same values serves the value it already
computed. What that saves is not an estimate — every pass returns a record of itself, and
`dagpane explain` prints it.

```console
$ dagpane explain examples/sales.toml --set min_amount=400
dagpane: Sales explorer — 11 cells, 7 panes
first render: 8 of 11 cells evaluated

set min_amount = 400
  epoch 2 — looked at 8 of 11 cells
    set      min_amount
    ran      filtered             changed
    ran      order_count          changed
    ran      region_totals        changed
    ran      channels             same value — nothing below it ran
    ran      top_orders           same value — nothing below it ran
    ran      revenue              changed
    reused   channel_count        its inputs had not moved
  3 cell(s) never looked at: sales, region, all_time_revenue
  patch: 3 of 7 panes — revenue, order_count, region_totals
```

Read the last four lines. `channels` — the set of channels present in the filtered data —
recomputed and produced the value it already had, so `channel_count` below it did not run at
all. Three cells were never even looked at, including the one holding 600 rows of source
data. And three of seven panes went on the wire; the other four are still correct, so they
were not sent.

One static binary. No Python, no Node, no package manager, no build step for the front end.

---

## Start here

```console
$ cargo install --path crates/cli          # or run it out of the workspace
$ dagpane check examples/sales.toml
dagpane: Sales explorer — 11 cells (2 inputs, 8 computed), 7 panes
  1 data source(s) loaded at start-up and shared by every session
  ok

$ dagpane graph examples/sales.toml
Sales explorer — 11 cells

 0  data   sales
 0  input  min_amount
 0  input  region
 1  cell   filtered           ← sales, min_amount, region
 1  cell   all_time_revenue   ← sales   [pane all_time_revenue]
 2  cell   order_count        ← filtered   [pane order_count]
 2  cell   region_totals      ← filtered   [pane region_totals]
 2  cell   channels           ← filtered   [pane channels]
 2  cell   top_orders         ← filtered   [pane top_orders]
 3  cell   revenue            ← region_totals   [pane revenue]
 3  cell   channel_count      ← channels   [pane channel_count]

the left column is height: a pass evaluates in ascending order, which is
what makes it impossible for a cell to see a mix of old and new inputs.

$ dagpane run examples/sales.toml
dagpane: Sales explorer — 11 cells, 7 panes
dagpane: http://127.0.0.1:8787
```

`check` and `graph` answer questions about an app that has not run — which is possible
precisely because the edges are written down rather than discovered while evaluating.
`explain` answers the question this project exists to answer, in a terminal. `run` is the
demo: the page shows the same counts live, above the panes, and repainted panes flash their
border so you can see which three of seven moved.

## In a container

```sh
docker pull mancube/dagpane:0.1.0

# run these from the directory holding your app manifest and its data
docker run --rm -v "$PWD:/app:ro" mancube/dagpane:0.1.0 check /app/your-app.toml
docker run --rm -p 8787:8787 -v "$PWD:/app:ro" \
  mancube/dagpane:0.1.0 run /app/your-app.toml --host 0.0.0.0
```

`scratch` plus one static musl binary: no shell, no package manager, no libc. The app is
mounted rather than baked in, because there is nothing in the image to bake it into. Note
the `--host 0.0.0.0` and read what it prints: **there is no authentication in this
version**, so put it behind something that authenticates before it is reachable by anyone
you would not show the data to. `SECURITY.md` covers this in full.

`linux/amd64` only. The build pins the `x86_64-unknown-linux-musl` target, and an `arm64`
manifest carrying an `x86_64` binary would advertise a container that cannot start.

`docs/DOCKERHUB.md` is the image overview, and the same text is the description on Docker
Hub. The image is built and pushed from this repository on the release tag, so it is made
of exactly the source published here.

## The one design decision everything follows from

**Invalidation is derived, not declared.**

An app author says what each cell *reads*. Nobody draws a boundary around "the part that
should re-run", because that boundary is a derived fact and deriving it is the runtime's
job. Streamlit's `@st.fragment` and Dash's callback lists are the other answer: the human
draws the boundary, and the boundary is wrong whenever the app changes and the annotation
does not.

That is a claim against annotation-based partial rerun, and it is deliberately **not** a
claim against every reactive system. marimo derives its graph too, from static analysis of
Python cell references, and has for years; Shiny shipped a real reactive graph in 2012.
`COMPETITORS.md` says exactly where dagpane is and is not different, including the two
places the brief this module was built from turned out to be out of date.

Three mechanisms fall out of that decision, and each has an ADR:

* **Edges are declared, so the graph is checked once and shared** ([ADR-0001](docs/adr/0001-declared-edges.md)).
  A cycle is a build error naming the loop, not something a user runs into. The graph is
  built at start-up and shared immutably — a hundred viewers are a hundred vectors of values
  over one app, and one allocation per source that nobody has touched.
* **Evaluation is in ascending height, which is what makes it glitch-free** ([ADR-0002](docs/adr/0002-height-ordered-evaluation.md)).
  Every edge runs from a lower height to a strictly higher one, so a cell's inputs are final
  before it runs. A diamond evaluates its join exactly once, and never with one new parent
  and one old one.
* **A value that did not change stops the pass** ([ADR-0003](docs/adr/0003-digest-equality.md)).
  Values are compared by a 128-bit content digest taken once when the value is produced, so
  the comparison is two `u64`s whether the value is a boolean or a table.

And errors are values ([ADR-0004](docs/adr/0004-errors-are-values.md)): a failing cell holds
its error, cells below it hold one naming the cell that actually failed, and the pass
finishes. One broken column takes out one number — not the page, and not the control that
will fix it.

## Writing an app

The manifest is the authoring surface. There is no compiler in the loop and no expression
language: a step reads a column and compares it against a literal or an input, and an input
it names is an edge.

```toml
[app]
title = "Sales explorer"

[[source]]
name = "sales"
csv = "sales.csv"

[[input]]
name = "min_amount"
label = "Minimum order value"
slider = { min = 0.0, max = 800.0, step = 25.0, default = 0.0 }

[[cell]]
name = "filtered"
from = "sales"
[[cell.step]]
filter = { column = "amount", op = "ge", param = "min_amount" }

[[cell]]
name = "region_totals"
from = "filtered"
[[cell.step]]
group_by = { by = ["region"], agg = [
  { agg = "count", as = "orders" },
  { column = "amount", agg = "sum", as = "revenue" },
] }

[[pane]]
cell = "region_totals"
title = "Revenue by region"
bar = { label_column = "region", value_column = "revenue" }
```

`param = "min_amount"` is the whole reactive wiring: it makes `filtered` depend on the
slider, and everything downstream of `filtered` follows. `examples/sales.toml` is the full
version of this app, and it is what every number on this page was measured on.

Steps are `filter`, `select`, `sort`, `limit`, `group_by`, `scalar` and `count`. That is the
entire vocabulary, and `dagpane check` will tell you when you have written something outside
it rather than ignoring it.

For anything the manifest cannot express, a cell is a Rust closure:

```rust
use dagpane_core::{CellError, Graph, Session, Value};

fn app(readings: Value) -> Result<Session, Box<dyn std::error::Error>> {
    let mut b = Graph::builder();
    b.source("threshold", Value::float(10.0));
    b.source("readings", readings);
    b.cell("above", ["threshold", "readings"], |i| {
        let threshold = i.float(0)?;
        let n = i
            .get(1)
            .as_list()
            .ok_or_else(|| CellError::failed("readings should be a list"))?
            .iter()
            .filter(|v| v.as_float().is_some_and(|x| x >= threshold))
            .count();
        Ok(Value::int(n as i64))
    });

    let mut session = Session::new(b.build()?);
    session.refresh();
    Ok(session)
}
```

`dagpane-core` is a library first: the engine has one dependency (serde), no I/O, no async
runtime and no `unsafe`, and it can be embedded without any of the rest of this.

## How it is checked

122 tests. The one that decides whether any of the above is true is
`crates/core/tests/oracle.rs`.

Every other test asserts a *count*, and a count asserted on a graph the author drew is a
test of the author's expectations: a scheduler that skips too much passes all of them and is
silently wrong, which is the one failure mode this project must not have — a stale number on
a page that looks like it is working. So the oracle generates **200 pseudo-random dependency
graphs**, runs 12 random interactions on each, and after every one of those 2,400 commits
asserts both halves:

* **correctness** — every cell's value is identical to what a brand-new session computes from
  scratch with the same inputs;
* **economy** — every cell the pass touched is inside the structural closure of what changed.

It has already earned its place. An error's digest originally covered its message and not
its attribution, so two upstream cells failing with the same words digested alike and a cell
below them kept naming the one that was no longer the problem. No hand-written test found
that; the oracle found it on a random graph, and `reactive.rs` now pins it.

## What this is not

Stated here rather than discovered later.

* **No authentication.** `dagpane run` binds `127.0.0.1`, and binding anything else prints a
  warning saying what it means. A session is a connection: closing the tab discards it.
  There is no session store, no eviction and no reconnection token, because there is nothing
  yet that needs one. `SECURITY.md` has the rest.
* **Nothing runs in a browser.** `cargo check -p dagpane-core --target wasm32-unknown-unknown`
  passes and CI runs it, which is a real constraint on the engine and the entire extent of
  the WASM story today. There is no client-side compute.
* **The table is small on purpose.** `Vec<Option<T>>` per column, no chunking, no dictionary
  encoding. It is not Arrow and does not pretend to be. `ARCHITECTURE.md` §6 names the seam a
  real engine plugs into and what has to change; there is no Polars and no DuckDB here, and
  the roadmap says what would have to be measured before there is.
* **The dirty closure is structural.** A pass *visits* every cell downstream of what
  changed, including the ones that turn out to reuse. Visiting is a digest comparison per
  input, so it is cheap — but it is not zero, and `Trace::visited` reports it rather than
  folding it into `evaluated`.
* **An over-declared edge costs a recomputation.** A cell that reads an input only on a
  branch it did not take still declares it and still re-runs. The digest short-circuit stops
  the damage at that cell's own boundary, so the cost is one recomputation and not a
  cascade — but it is a cost, and `explain` prints it as `same value`.
* **No performance claim.** There is no benchmark in this repository and no number in this
  file measured in seconds. The counts above are counts.

## Layout

```
crates/core    the engine. pure: no I/O, no async, no clock, no unsafe. one dependency.
crates/app     widgets, panes, the patch, the manifest compiler. no sockets.
crates/serve   axum, one session per connection, and the single-file client.
crates/cli     the `dagpane` binary.
examples/      the app every number here was measured on.
docs/adr/      the four decisions, and what breaks if you change them.
```

The dependency direction is `cli → serve → app → core`, and CI greps for it: `core` never
depends on an async runtime, `app` never depends on a server, and nothing in the workspace
depends on an HTTP client.

## Build and test

```sh
cargo build --release                 # the `dagpane` binary
cargo test --workspace                # everything, 161 tests
cargo test -p dagpane-core            # the reactive semantics, no browser needed
cargo test -p dagpane-serve --test socket   # the real wire, over a real socket
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
```

MSRV 1.85 for the workspace, and 1.82 for `dagpane-core` on its own — serde is the engine's
only direct runtime dependency, and nothing it pulls in needs a newer toolchain. CI checks
both rather than claiming them.

## Licence

Apache-2.0. See `LICENSE` and `NOTICE` — the latter names the prior art this engine owes,
which is most of it.
