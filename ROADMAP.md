# dagpane — roadmap

v0.1.0 ships one thing: an engine that recomputes the cells an interaction reaches and
prints which ones. Everything below is something that engine does not do, ordered by what
forces the work rather than by effort or by appeal.

**No item here has a date, and that is the rule of this file.** A date on an unstarted item
is a guess wearing a plan's clothes. Every item instead carries **the trigger** — the
specific, checkable event after which the work is worth doing and before which it is
speculative. Several items say **not started**, in those words, because they are.

`POSITIONING.md` §3 grades this project against the four conditions the prior-art dossier
said had to hold for it to matter, and scores it zero of four fully met. This file is the
other half of that document: not what is wrong, but what would have to happen first.

---

## v0.1.0 — shipped

- [x] `dagpane-core`: declared edges, Kahn's algorithm for order and cycle detection,
      per-cell `height`, height-ordered evaluation, digest short-circuit, errors as values.
      One dependency (serde), `#![forbid(unsafe_code)]`, no I/O, no clock, no async runtime.
- [x] `dagpane-app`: panes, five widget kinds, seven pipeline verbs (`filter`, `select`,
      `sort`, `limit`, `group_by`, `scalar`, `count`), an in-tree RFC-4180-subset CSV reader
      with per-column type inference, a TOML manifest compiler, patch generation.
- [x] `dagpane-serve`: axum 0.8, one `AppSession` per WebSocket connection over one
      `Arc<App>`, and a single-file client with nothing fetched at run time.
- [x] `dagpane` CLI: `check`, `graph --format text|mermaid|json`, `explain --set NAME=VALUE`,
      `run --port --host`.
- [x] 122 tests. `crates/core/tests/oracle.rs` — 200 pseudo-random DAGs × 12 interactions =
      2,400 commits, each checked for **correctness** against a fresh full-recompute session
      and for **economy** against `graph.closure(roots)`. It found the error-digest bug that
      no hand-written test found.
- [x] Four ADRs, each stating what breaks if the decision is reversed.
- [x] CI: `cargo fmt`, `cargo clippy -D warnings`, MSRV checked with `cargo +1.85.0 check
      --workspace --locked` and `cargo +1.82.0 check -p dagpane-core --locked` — core
      promises an older toolchain than the workspace and both are enforced — and `cargo
      check --locked -p dagpane-core --target wasm32-unknown-unknown`.

---

## 1. Durable sessions and authentication — one item, not two

Today a session **is** a connection. `AppSession::open` builds a slot vector beside the
shared `Arc<App>`; closing the tab drops it. There is no session store, no eviction, no TTL
and no reconnection token, because a runtime that binds `127.0.0.1` has nobody to keep a
session for. A refresh of the page is a new session and a full first render.

These two ship together and the reason is not tidiness. **A session store without
authentication is a mechanism for reading somebody else's session.** The moment a session
outlives its socket it needs a name; the moment it has a name, something has to decide who
may say that name; and a resumption token that any client can guess is worse than no
resumption at all, because the first design at least fails visibly.

- [ ] A session identity and a store: `Session` is not `Serialize` today. `Value`,
      `Outcome` and `CellError` all are, so the slot vector is reachable — what is missing
      is a decision about what a stored session *is*, which is the manifest digest it was
      built against plus its input values, not its computed cells. Restoring computed cells
      is restoring a cache; restoring inputs and recomputing is restoring the app.
- [ ] Eviction and a TTL, both of which are memory policy and therefore need the number in
      item 7 before either can be set to anything but a guess.
- [ ] An authentication boundary in `serve`, ahead of the WebSocket upgrade, and a stated
      answer for what a session is scoped to — a user, a browser, or a link.
- [ ] A `--host` that binds a non-loopback address without printing a warning, which is the
      observable form of "this is now deployable".

**Trigger: the first deployment that is not on loopback.** Not a request for the feature, a
deployment — someone binding `0.0.0.0` and putting a page in front of people. Until then
`dagpane run` binds `127.0.0.1`, `--host` anything else prints a warning saying what that
means, and the honest position is that this runtime has not been designed for a hostile
network.

---

## 2. A real dataframe engine behind the value model

`Table` is `Vec<Option<T>>` per column, four column types, no chunking, no dictionary
encoding, no SIMD — sixteen bytes for an `i64`. `ARCHITECTURE.md` §6 names the seam
concretely: `Value::Table(Table)` becomes `Value::Frame(Arc<dyn Frame>)` with `rows()`,
`schema()`, `head(n)` and `digest()`; `transform.rs` becomes one implementation of a
`Transform` trait instead of the only one; the render path in `view.rs` already needs
nothing but `head`.

**The `Frame` half of that seam now exists** (`crates/core/src/frame.rs`). This file used to
say there was deliberately no such trait, because one with a single implementation is a
design fitted to that implementation. That objection was answered rather than dropped: the
trait shipped with **two** backends on its first commit — `Table`, the `Vec<Option<T>>`
representation that has always been here, and `dagpane-frame-arrow`, which is Arrow with
per-column dictionary encoding. A differential test holds them to each other cell for cell,
which is the only way to learn whether a seam is really a seam.

One rule carries the whole thing: **two frames with the same logical content must digest
identically, whatever they do with it physically.** The digest decides whether a cell's
output moved, so a backend hashing its own layout would invalidate the entire graph the day
anybody switched representation, while looking exactly like a correct pass. `digest_frame`
is therefore one canonical walk defined in terms of the trait's accessors, and every backend
delegates to it — dictionary-encoded and plain agree by construction, not by agreement.
(Writing it caught a real one: `bool` hashes as a single byte, not a `u64`. A canonical walk
that got that wrong would have changed every digest of every table containing a bool.)

**`Value` now holds a frame too.** `Value::Table { v: Table }` became
`Value::Frame { v: FrameRef }`, and the seven verbs in `transform.rs` take `&dyn Frame` and
return `Arc<dyn Frame>` — `filter`, `sort` and `limit` reduce to choosing row indices and
calling `take_rows`, `select` to a projection, and only `group_by` materialises, which it
was doing anyway to aggregate. The pipeline's old `Cow::Borrowed` fast path is now an `Arc`
bump that holds for *every* step rather than only the skipped ones.

**The wire did not move.** The variant is `Frame` and its serialised name is still `table`,
because the patch protocol is a published contract that the product site's replay and every
socket test read. `dagpane explain` on the bundled app still reports 8 of 11 cells and 3 of
7 panes — the same numbers the README prints and CI asserts.

Two decisions inside it are worth knowing about. Comparison lives on the trait
(`is_null`, `compare_in_column`, `compare_to_value`) rather than going through `value_at`,
because comparing two text cells through a `Value` allocates two `String`s and a sort does
that `n log n` times; `Table` overrides all three against its own storage, so the path
production takes today does exactly the work it did before. And there is deliberately **no
digest method on the trait** — see the module docs for why an overridable one is worse than
none.

**And sources are Arrow-backed**, behind the default-on `arrow-sources` feature. The CSV
reader's type inference is untouched — it now returns columns and the caller chooses the
representation, so no backend can move a published count by reading the file differently, and
the lean and Arrow binaries produce byte-identical output for `check`, `graph` and `explain`.
Measured end to end: **660 MB → 393 MB at a million rows, 41%**. Not the 4.1× the columns
alone give, because `group_by` still aggregates into a `Table` and several of the bundled
app's cells are group-bys — `BENCHMARKS.md` has both numbers and the reason they differ.

The feature costs 940 KB of binary (1.96 MB → 2.90 MB), which is why it is a feature: a
deployment that cares more about the artifact than about resident memory can have the old
one, and the cross-backend oracle is what makes that a size choice rather than a behaviour
one.

Every verb now keeps the representation it was handed, `group_by` included — it builds its
output through `Frame::same_kind` rather than naming a backend, which `transform.rs` cannot
do from a crate with one dependency. A chain stays Arrow end to end, and a chain that started
as a `Table` stays one.

That last change moved the resident figure by **nothing**, and the reason is worth writing
down: this app's group-bys collapse a million rows into four regions, and a four-row frame is
noise beside a million-row source. It matters for an app grouping on something
high-cardinality; it does not matter here.

**The reader is streamed.** It used to buffer every raw field into a `Vec<Vec<String>>`
before inferring any column's type — six million live strings for a 1M × 6 file. Inference
does need to see every value, but not to *keep* them: three booleans per column fold one row
at a time, so the first pass carries flags and discards fields and the second parses into the
type the first chose. **661 MB → 295 MB against the lean build, 55%**, and faster despite the
second scan. The old inference is kept as a `#[cfg(test)]` oracle and the new reader is
checked against it over a generated corpus, because inference decides what every published
count is a count of.

**What was left was the conversion, not the parse**, and the seam now has the builder that
closes it. `FrameBuilder` is somewhere to put a column of a given type — `begin_column`, a
`push_*` per row, `finish` — with text pushed **borrowed**, so a backend storing bytes
contiguously never allocates a `String` per cell. `csv.rs` fills it without naming a backend;
`TableBuilder` is the reference implementation and the Arrow one is checked against it.
A `ColumnHint` carries the row and distinct counts the inference pass already has, and is
advisory: a backend may ignore all of it and must produce identical content either way.

**Loading a million rows went 252 MB to 76 MB** — the text plus the columns plus slack, with
the intermediate gone, which is the floor the previous paragraph predicted.

**And the app's peak did not move: 294 MB before, 294 MB after.** `group_by` materialises its
*inputs*, so it rebuilds the same representation the loader stopped building, at about the
same cost, however cheaply the frame arrived. Remove the group-bys from the manifest and the
win is undiminished, 252 MB to 119 MB. Two copies of one mistake in two files: fixing the
loader's revealed the verb's rather than removing it, because a peak is a maximum and the
second-largest cost is invisible until the largest one goes. It is also why this change would
have scored zero on the headline figure alone — `BENCHMARKS.md` has the columns that make it
legible.

**And `group_by` now reads its inputs through the trait**, materialising nothing —
`transform.rs` only, no new trait method, exactly as the paragraph above predicted. The app
goes **294 MB to 119 MB at a million rows, and runs 13% faster**, which is the same surprise
the streamed reader produced and for the same reason: the old path made the identical
allocations and then kept them all alive.

The peak is now *equal* to the peak of the same app with every group-by deleted — 119 MB
against 119 MB. The verb costs nothing above the rest of the pipeline. Cumulatively that is
660 MB to 119 MB, **82%**, in five measured steps on the shipped binary.

The one to remember is the fourth: the builder moved the headline by a megabyte and looked
like a failure, and it is what made the fifth possible, because a peak is a maximum and the
load cost had to go before the verb's was visible at all.

What is still **not** done: `transform.rs` is one implementation of the verbs rather than one
of several behind a `Transform` trait. That is the rest of item 2, and it should wait for a
second verb implementation to justify it — the same rule that governed this trait.

- [ ] Measure first: the row count and column width at which the current `Table` stops
      being the right answer, on a real app, with a real source — not a synthetic sweep.
- [ ] Then pick the engine, and know what picking it costs.

**On the candidates — measured 2026-09-08, and the earlier note was wrong.**

This section used to say Polars does not build for `wasm32-unknown-unknown`, citing
`pola-rs/polars#16729` (2024-06-04, "the `parquet` feature pulls compression codecs that do
not cross-compile") and `pola-rs/polars#19211` (2024-10-12). It also said DataFusion's wasm
story had not been measured here. Both statements are now out of date, and the check that
replaced them is in `BENCHMARKS.md` with the harness that produced it.

**Every candidate builds for `wasm32-unknown-unknown`, including Polars with the `parquet`
feature.** The dates are the explanation: those issues are two years old and this was never
re-checked. What actually blocks a naive build is neither library — it is `getrandom` 0.3
refusing to pick an entropy backend for a target that has no OS to ask, which is one line of
`.cargo/config.toml` and applies to all of them equally. Note what that line implies: it
routes randomness through the host's `crypto.getRandomValues`, so the artifact then requires
a **JavaScript host**. A pure wasm runtime would need a different backend.

So the trade is not availability. It is **size**, and it spans a factor of twenty:

| candidate | raw | gzipped |
|---|---:|---:|
| `arrow-array` + `-schema` + `-select` + `-ord` | 1.6 MiB | **259 KiB** |
| the same, plus `parquet` | 1.7 MiB | 297 KiB |
| Polars (`lazy`, `parquet`) | 6.3 MiB | 1.4 MiB |
| DataFusion 54.1 | 27 MiB | **5.9 MiB** |

Read against §4: DataFusion at 27 MiB raw is in the same territory as DuckDB-Wasm's 34.25 MB,
which is the point §4 makes about shipping a query engine into a browser — it is a real cost,
and paying it is a decision rather than a detail. The Arrow row is the surprise. 259 KiB
gzipped is *smaller than several static pages in this repository already ship*, and it is
what the seam described above actually calls for: `Frame` needs `rows()`, `schema()`,
`head(n)` and `digest()`, and the seven verbs are hand-written kernels over that
representation. DataFusion is a **SQL engine**, and §6 puts SQL last and possibly never — so
on today's plan its 5.9 MiB buys nothing this runtime asks for.

**And Arrow alone does not deliver the memory win; dictionary encoding does.** One 1M-row
column of a four-value categorical — which is exactly what `region` and `channel` are:

| representation | resident |
|---|---:|
| `Vec<Option<String>>` (today) | 27.2 MB |
| `arrow::StringArray` | 11.8 MB |
| `arrow::DictionaryArray` | 3.8 MB |

Adopting Arrow and keeping every string as a `StringArray` recovers 2.3×; the 7.1× needs the
dictionary, which is a modelling decision on top of the dependency rather than a consequence
of it.

**Trigger: an app whose source does not fit the current `Table` — measured, not guessed.**
A row count and a memory figure from a real workload, not an argument from the shape of the
data structure.

---

## 3. Sub-node invalidation — column- and predicate-granular

**Not started.**

dagpane invalidates at whole-value granularity. A cell that reads three columns of a
200-column frame recomputes when any of the 200 changes. The mechanism that would fix this
is comemo's constrained memoization (comemo 0.5.1, 2026-01-29, MIT OR Apache-2.0, the
engine behind Typst): `#[track]` makes a type's *accesses* observable and `#[memoize]`
records which parts of an argument a computation touched, so a cached result survives a
change to the parts it did not read.

**Say the rest plainly, because it is the most important sentence in this file.** The
prior-art dossier this module was built from considers **this and only this** to be a
technical differentiator that survives contact with marimo. Node-level reactivity is
marimo's, shipped, free, and doing 2.42M PyPI downloads a month; Python cannot do the
sub-node version, and marimo's own docs say why — *"tracking mutations reliably is
impossible in Python"*. Streamlit cannot do it either: *"fragments can't detect a change in
input values."* Rust can, because access can be made observable. That is the whole argument
for this project having a technical claim at all, and **it is not built**.

- [ ] Column-granular access recording on `Table` reads, then on whatever `Frame` becomes.
- [ ] Predicate-range constraints — the harder half, and the one that makes a filter change
      that does not move the result set cost nothing.
- [ ] A per-session, budgeted, evictable cache. comemo's global cache with coarse eviction
      is correct for one compiler process and one document, and wrong for a server holding
      N sessions.
- [ ] Extend `crates/core/tests/oracle.rs` to random column-level edits before any of the
      above is trusted. The oracle already asserts economy against the structural closure;
      a sub-node scheme needs the same assertion against a finer closure, and getting that
      wrong shows a user a stale number on a page that looks correct.

**The claim does not get written down until it is measured.** The dossier's test is a
result of the form *"changed one column of a 200-column frame, recomputed 2 of 40 cells"*,
produced by `dagpane explain` on a real app. Until that line exists, sub-node invalidation
appears in this roadmap and nowhere else — not in the README, not in POSITIONING.md, not in
a commit message that implies it.

**Trigger: an app where the whole-value granularity is measurably the cost** — a wide frame
where `explain` shows cells recomputing that read none of the columns that moved. The
trigger is deliberately not "someone thinks this would be good", because everyone thinks
this would be good.

---

## 4. Client-side compute (WASM)

**What exists, exactly and completely:** `cargo check --locked -p dagpane-core --target
wasm32-unknown-unknown` exits 0, and CI runs it under the job named *"the engine still
builds for wasm32"*. That is a real constraint on the core — no fs, no clock, no threads —
and it is the entire claim. There is no `crates/wasm`, no wasm-bindgen, no client-side
compute, and **nothing runs in a browser.**

**Not started**, and the ceiling should be understood before it is:

* **"Rust/WASM means a smaller payload" is false** for anything that ships a SQL engine.
  DuckDB-Wasm `1.33.1-dev57.0` (2026-06-22), uncompressed, from jsDelivr: `duckdb-eh.wasm`
  **34.25 MB**, `duckdb-mvp.wasm` 39.41 MB, `duckdb-coi.wasm` 33.99 MB. shinylive's Pyodide
  base is **~13 MB** by Posit's own documentation. The Rust-side payload argument only wins
  if the client stays a thin evaluator and the query engine stays on the server — which is
  a smaller product than "the same engine, either side".
* **wasm32 caps at 4 GB.** DuckDB's docs, verbatim: *"WebAssembly limits the amount of
  available memory to 4 GB and browsers may impose even stricter limits."* marimo's WASM
  export documents a 2 GB limit for the same reason.
* **Multi-threading needs cross-origin isolation.** The threaded DuckDB bundle requires
  COOP/COEP headers, which breaks third-party embeds and several static hosts. A runtime
  that needs the deployer to control response headers has a deployment story, not a
  checkbox.
* **Somebody already shipped the architecture.** Perspective 5.3.1 (2026-09-04, Apache-2.0,
  OpenJS Foundation) is *"a symmetric client/server architecture — the same Client API
  connects to an engine in-process, in a Web Worker, or remotely over WebSocket"*, with a
  memory64 build that beats the 4 GB wall today. Evidence and shinylive have run
  browser-side compute in production for years. This is not a frontier; it is a lane with
  incumbents in it, and a roadmap that implies otherwise is lying to its own maintainers.

The defensible version of this item is not about capability. It is about **where the
boundary is drawn at deploy time** — the same graph, with the placement of each cell a
deployment decision rather than a rewrite — and even that is a smaller idea than it sounds,
because Perspective ships it.

**Trigger: an interaction whose latency is dominated by the round trip, measured on a real
app**, plus a decided answer to which of the seven verbs a browser-side evaluator would
run. Not "WASM would be interesting". If the measurement says the round trip is not the
cost, this item stays exactly where it is.

---

## 5. A Rust authoring surface worth shipping

`GraphBuilder::cell(name, inputs, closure)` is the whole authoring API for anything the
manifest cannot express. It works, it is tested, and it is verbose in the specific way a
closure with positional inputs is verbose: `i.float(0)?` and `i.table(1)?` index into a
list the caller wrote three lines earlier, and nothing checks that the order still matches
after an edit. There is no `#[cell]` proc macro.

The reason there isn't one is that **nobody has written a second Rust app.** A macro
designed against one call site encodes that call site's accidents. `venus` (0.1.2,
2026-07-15) infers dependencies from function parameter names, which is the obvious shape
and is worth crediting rather than rediscovering — but "obvious" is not "right for this
engine", and the difference only shows up in an app somebody actually maintained.

- [ ] `#[cell]` deriving the input list from parameter names, so the declaration and the
      use cannot drift apart.
- [ ] Typed accessors that fail at compile time rather than through `CellError`.
- [ ] Whatever the second app's author says hurt — collected before the design, not after.

**Trigger: somebody writes a second Rust app against `dagpane-core` and says what hurt.**
This is also the item most likely to be *deleted* rather than done: `POSITIONING.md` §3
records that the moment the README leads with a Rust closure as a cell body, the addressable
market is Rust programmers who want dashboards. A better Rust authoring surface makes the
wrong audience more comfortable.

---

## 6. More of the manifest — and SQL is last, for a stated reason

Seven verbs is not an authoring surface. Evidence proved SQL + Markdown is one; Mosaic
proved params and selections are one. Seven verbs over five widget kinds is neither, and
the eighth verb an author needs today is a Rust closure — which is item 5, which is the
item that makes the audience worse.

In rough order of how much each adds against how much it risks the edge invariant:

- [ ] **Derived columns** — a new column from existing ones. Lowest risk: the inputs are
      column names in the same cell's frame, so no new edge exists to get wrong.
- [ ] **Joins** — two cells in, one out. The edges are the two named cells, which the
      manifest already knows how to express. The work is in the transform, not the graph.
- [ ] **An expression sub-language** for filter predicates and derived columns. The edge
      question returns here: an expression that names an input is an edge, and the parser
      has to surface every name it saw, including the ones inside a branch that will not be
      taken. Over-declaring is the correct failure and the digest short-circuit contains it.
- [ ] **SQL cells** — hardest, last, and possibly never.

**Why SQL is last.** The manifest's job is to produce *edges*. Extracting the dependencies
of a SQL statement without a real SQL parser means matching table and column names out of
text with a regular expression, and a regular expression over SQL is wrong eventually —
against a CTE, a subquery alias, a quoted identifier, a comment containing a table name. A
missed edge is a cell that does not recompute when it should, which is a stale number on a
page that looks like it is working: the one failure mode this project exists to not have.
**A wrong edge is a wrong app.**

The honest path is therefore a real parser with resolved table and column scope, producing
a name set the compiler can turn into edges and refusing the statement when it cannot. That
is a dependency, a grammar decision, and a new class of build error — which is why it is a
phase and not a feature, and why it sits behind everything above it.

**Trigger: an app that cannot be written in the manifest and is not worth writing in Rust.**
One of those is a gap; a pattern of them is a specification. For SQL specifically the
trigger is stricter — a parser that can be shown to reject what it cannot resolve, rather
than guess.

---

## 7. apps-per-core, and the concurrency numbers that do not exist

**Not measured. There is no benchmark in this repository and no number in any document here
measured in seconds.** Every number this project prints is a count.

The dossier's argument is that the buyer is the platform team hosting hundreds of internal
apps and paying for a sticky-session-pinned, vertically-scaled container each —
`marimo-team/marimo#1831`, *"Support stateless for multi container scaling and
deployment"*, open since 2024-07-19 with no visible maintainer response, is the artifact of
that pain. The unit of value is then cost and blast radius per hosted app, in apps-per-core
and p99 under N concurrent viewers, **which is a number no incumbent publishes and this
project could win.**

The structure points the right way and that is all it does. The app, the graph and every
loaded source are one `Arc<App>` behind every connection; `Arc::ptr_eq` on an untouched
source across two sessions is asserted by a test rather than inferred from an RSS reading;
`Session` is `Send` and `Sync`, and every mutating entry point takes `&mut self`, so the
borrow checker is what stops a pass interleaving with another pass or with a read.
None of that is a measurement.

- [ ] Resident memory per additional session over one `Arc<App>`, measured, so the eviction
      policy in item 1 is set from a number.
- [ ] p99 interaction latency under N concurrent viewers of the bundled example.
- [ ] apps-per-core against a Streamlit or marimo baseline under identical load. This is
      the number, and it is the cheapest of the four dossier conditions to produce.
- [ ] A stated caveat that `dagpane run` serves one manifest on one port, so 400 apps is
      400 processes today. Each is a static binary with no interpreter and no package
      manager, which is cheaper than 400 containers — but "cheaper" is a word, and this
      item exists to replace it with a number.

**Trigger: before any performance claim appears in the README.** That is the whole rule.
The README currently says *"No performance claim"* and lists the counts as counts; the first
sentence that implies speed, density or cost is blocked on this section, not on review.

---

## What is deliberately not on this list

* **A notebook mode.** An editing surface is a different product with a different failure
  mode, and marimo already ships the good version of it.
* **A plugin system.** Four crates and one dependency direction that CI greps for is the
  asset here; an extension point is a second public API to keep stable before the first one
  has a second user.
* **A hosted cloud.** This runtime binds `127.0.0.1` and has no authentication; selling
  hosting for it would mean shipping item 1 as a product before it is shipped as a feature.
