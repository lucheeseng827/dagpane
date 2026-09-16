# Changelog

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning: [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Every count in this file was produced by `dagpane explain` on the bundled example —
`examples/sales.toml`, 600 rows of synthetic sales, 11 cells, 7 panes — and **a count only
means something against a named app.** "8 of 11 cells" is a fact about that manifest and
about the interaction named beside it; the same slider move over a differently shaped graph
gives a different number, and a number quoted without its app is not a number. Where a
figure appears below, the command that produced it appears with it.

There are no timings here and there is no benchmark in this repository. `dagpane-core` is
clockless on purpose — an engine that needs a clock to be tested behaves differently under
a test harness — so wall-clock microseconds are measured by `crates/serve` around a pass
and reported per interaction, never aggregated into a claim.

## [Unreleased]

## [0.1.0] — 2026-09-17

First public release, and the first publication of this source anywhere. The mirror
`lucheeseng827/dagpane` was created on 2026-09-10 and has carried nothing since but the
README it was initialised with: the sync publishes on a `dagpane-oss-v*` tag and no such
tag had been pushed, so nothing was ever mirrored. The code below is not new work — it
has been building and passing its gates in the private tree — but this is the first time
anyone outside it can read, clone or depend on it.

### Added

- **`dagpane` — a reactive runtime for data apps.** An interaction recomputes the cells
  that depend on it, and nothing else. `crates/core` is the engine: cells, declared edges,
  a build-time topological order, dirty propagation over the structural closure, and a
  trace saying which cells ran. Pure — no I/O, no async, no network, no clock, no `unsafe`,
  one dependency (serde).

- **Declared edges, and the three things that follow from them.** A cell names its inputs
  rather than having them traced at run time, so a cycle is a *build* error carrying the
  loop path, the graph is built once and shared immutably (`Arc<Graph>`) across every
  session, and `dagpane graph` can print the app before it runs. The cost is stated rather
  than hidden: a cell that reads an input only on a branch it did not take still declares
  it, and still re-runs when that input moves. See `docs/adr/0001-declared-edges.md`.

- **Height-ordered evaluation, which is what makes the pass glitch-free.** Every cell
  carries the longest path from any source, computed when the graph is built, and a pass
  evaluates in ascending height. Every edge runs from a lower height to a strictly higher
  one, so a cell's inputs are final before it runs: a diamond evaluates its join exactly
  once and never on a mixture of old and new values. No re-entrancy, no second pass, no
  recompute-until-stable loop. `docs/adr/0002-height-ordered-evaluation.md`.

- **A 128-bit content digest, taken once when a value is produced.** A cell reuses when its
  inputs' digests match the ones it recorded the last time it ran, which makes the decision
  two `u128` comparisons for a 600-row table instead of a walk over it. This is also what
  contains an over-declared edge: the cell recomputes, produces the value it already held,
  and nothing below it runs. Written in-tree, FNV-1a, seventy-nine lines with the comments
  stripped. `docs/adr/0003-digest-equality.md`.

- **Errors are values.** A failing cell holds a `CellError`; every cell below it holds
  `Upstream { cause, message }` naming the cell that actually failed rather than the
  immediate input. The pass finishes and the rest of the page renders, so one bad column
  takes out one number and leaves the slider that will fix it working.
  `docs/adr/0004-errors-are-values.md`.

- **A TOML manifest that compiles to a graph.** Sources (CSV), inputs with their widgets,
  cells as a pipeline of seven verbs — `filter`, `select`, `sort`, `limit`, `group_by`,
  `scalar`, `count` — and panes. No SQL and no expression language: the manifest's job is to
  produce *edges*, and a dependency inferred wrongly from SQL text by a regular expression
  is a wrong app. Unknown keys are refused rather than ignored.

- **A patch protocol, not a page.** `ServerMessage::Patch` carries only the panes whose
  *rendered view* differs from the one the viewer already has — a second check beyond the
  engine's, because a table pane sends its first 50 rows and a change in row nine thousand
  changes the cell and not the view. Every patch carries a `PassStats` block saying how many
  cells the pass looked at, ran, changed and never touched, so the claim is checkable in a
  browser's network tab.

- **The CLI: `check`, `graph`, `explain`, `run`.** `graph` prints text, Mermaid or JSON
  without executing a cell. `explain --set NAME=VALUE` runs one interaction and prints
  exactly what it cost, with `--json` for a CI gate that asserts on the numbers.

- **The server, and a client that is one file.** axum 0.8 and tokio in `crates/serve` only;
  one session per WebSocket connection; the whole front end compiled into the binary as a
  single HTML file with inline CSS and JS — no npm, no bundler, and nothing fetched at run
  time. A unit test enumerates every `http`-prefixed string in it and asserts there are
  exactly two, so a third one fails the build instead of quietly making an air-gapped
  deployment render blank.

- **The differential oracle**, `crates/core/tests/oracle.rs`. 200 pseudo-random DAGs of 8
  to 40 cells from a 64-bit LCG written in-tree (so a failing seed reproduces anywhere,
  forever), 12 random interactions each — 2,400 commits — and after every one of them two
  assertions: **correctness**, every cell's value is identical to what a fresh session
  computes from scratch with the same inputs; and **economy**, every cell the pass touched
  is inside `graph.closure(roots)`. A count assertion on a hand-drawn graph is a test of the
  author's expectations, and a scheduler that skips too much passes all of those.

- **Also asserted, because each is a property somebody could break while optimising:** two
  sessions over one `Arc<Graph>` share one allocation per untouched source (`Arc::ptr_eq`,
  not an RSS measurement, which is flaky on every runner); setting an input to the value it
  already holds produces an empty pass; a repeated identical failure does not re-wake the
  page; recovery clears a whole poisoned subtree in one pass.

- **A static musl binary and a `scratch` image.** No shell, no libc, no package manager, no
  Python and no Node — the front end is inside the executable. CI builds it, checks it is
  actually static, and runs `check` against the bundled app with it.

### Fixed

- **An error's digest covered its message and not its attribution, so a cell could go on
  naming a failure that had moved.** Two upstream cells failing with the same words digested
  alike; a downstream cell whose *cause* changed from one to the other compared equal,
  served its cached error, and kept naming the cell that was no longer the problem. The page
  showed a plausible message pointing at the wrong cell.

  Found by `tests/oracle.rs`, not by review and not by any hand-written test — every one of
  those used a single failing cell, and a single failing cell cannot exhibit it. The oracle
  found it because its `Divide` op fails on any zero input and 200 random graphs eventually
  put two of those under one join.

  Fixed by giving `CellError` its own `Digestible` impl, which tags the two variants apart
  and hashes `cause` before `message`. The regression test is
  `a_failure_that_moves_to_a_new_origin_stops_naming_the_old_one` in
  `crates/core/tests/reactive.rs`. The lesson recorded in ADR-0003 is not "hash more
  fields" — it is that anything the engine treats as a value must digest everything a
  reader will act on, and attribution is something a reader acts on.

  Found and fixed inside this development cycle, so no released version carries it.

### Measured

On this checkout, 2026-09-08, with the commands named:

```
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

The app is 11 cells — one CSV source, two controls, eight computed — and 7 panes. One
slider move visits 8 of them, runs 6, reuses 1, never looks at 3, and repaints 3 panes.
Two of the six that ran produced the value they already held, which is why nothing below
them ran. `.github/workflows/ci.yml`'s `claim` job reads those eight numbers out of `explain --json` and
fails the build when they move, because a document a reader can falsify in one command
costs more than the change that moved it.

`cargo test --workspace --locked`: **122 passed, 0 failed**, same checkout and date.
`cargo check -p dagpane-core --target wasm32-unknown-unknown` exits 0, and CI runs it.

### Known limits

Stated here as well as in the README, because a changelog is where people look for what
changed and not for what was never there.

- **No authentication.** `dagpane run` binds `127.0.0.1`; `--host` anything else prints a
  warning naming the consequence. A session is a connection — closing the tab discards it —
  and there is no session store, no eviction, no TTL and no reconnection token. `SECURITY.md`
  has the rest, including the fact that loopback is not a boundary against a browser: the
  WebSocket upgrade does not check `Origin`.

- **Nothing runs in a browser.** `cargo check -p dagpane-core --target
  wasm32-unknown-unknown` passes and CI runs it. That is the entire WASM claim. There is no
  `crates/wasm`, no wasm-bindgen and no client-side compute; the check is a real constraint
  on the core — no filesystem, no clock, no threads — and nothing more.

- **The table is small on purpose.** `Vec<Option<T>>` per column, four column types, no
  chunking, no dictionary encoding. It is not Arrow and does not pretend to be. There is no
  polars, no duckdb and no arrow; `ARCHITECTURE.md` §6 names the seam a real engine plugs
  into and exactly which types change when one does, and there is deliberately no trait in
  the tree with one implementation and no second caller.

- **Invalidation is at whole-value granularity.** Change one cell of a `Table` and
  everything reading that table recomputes. Column-level provenance — the comemo idea named
  in `NOTICE` — is the gap this version has not closed.

- **The dirty closure is structural.** A pass *visits* every cell downstream of what
  changed, including the ones that turn out to reuse. Visiting is a digest comparison per
  input, so it is cheap, but it is not zero and `Trace::visited` reports it rather than
  folding it into `evaluated`.

- **No performance claim of any kind.** No apps-per-core figure, no p99 under concurrent
  viewers, no memory measurement, and no comparison in seconds against any other runtime.
  None has been measured. The only numbers this project prints are cell counts, pane
  counts, test counts and the trace.
