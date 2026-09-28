# dagpane — the landscape, honestly

*Competitive facts verified 2026-09-08 against crates.io, npm, PyPI, pypistats, jsDelivr
and the projects' own documentation. Every number carries the date it was fetched. Facts
about dagpane itself were re-measured on this checkout the same day and the command that
produced each one is named.*

The brief this module was built from led with a sentence that is no longer true, and the
part that stopped being true is the part the whole pitch rested on. That correction is the
first section, because a README a reader can falsify in one search costs more than any
feature in this repository is worth.

---

## 1. The premise was stale in both of its clauses

### "Streamlit reruns your entire script on every interaction"

**Half-true since 2024-04-05. Not defensible as a headline since 2026-09-01.**

Streamlit's own fundamentals page still says it — *"Streamlit apps have a unique data flow:
any time something must be updated on the screen, Streamlit reruns your entire Python script
from top to bottom"* (`docs.streamlit.io/get-started/fundamentals/main-concepts`, fetched
2026-09-08) — and quoting that sentence without the ledger below is a quotation used to
mislead.

| date | version | what shipped |
|---|---|---|
| 2024-04-05 | 1.33 | `st.experimental_fragment` — a decorator that reruns one function independently of the page |
| 2024 | 1.37.0 | promoted to stable `st.fragment` |
| 2026-01-14 | 1.53.0 | session-scoped `st.cache_data` / `st.cache_resource` |
| 2026-04-29 | 1.57.0 | Starlette/Uvicorn replaces Tornado as the default server |
| 2026-05-28 | 1.58.0 | `@st.fragment(parallel=True)` — fragments run concurrently |
| 2026-07-06 | 1.59.0 | fragments can update any part of the app, including elements created before the fragment |
| 2026-08-04 | 1.61.0 | `refresh_mode="background"` (stale-while-revalidate) |
| 2026-08-19 | 1.62.0 | the deprecated `st.cache` removed |
| **2026-09-01** | **1.63.0** | **event-scoped fragment reruns** — `@st.fragment(key=…)` targeted by `st.rerun("filters")` or `st.rerun(["alpha","beta"])` from an `on_change` / `on_click` callback; `st.rerun()` and `st.switch_page()` now take effect inside widget callbacks; `on_change="ignore"` updates a widget in the browser without rerunning the app at all |

A Streamlit developer in September 2026 can mark N fragments, name them, fire exactly the
ones a widget should touch, run them in parallel, and suppress the round trip entirely for a
slider. That is a hand-written callback graph, which is to say Streamlit has converged on
Dash. Seven days ago.

**The sentence that survives** is narrower and is the one this project uses: *Streamlit's
partial reruns are boundaries you declare and target; dagpane's are derived from edges you
declare once.* What Streamlit still does not do is documented by Streamlit, verbatim, at
`docs.streamlit.io/develop/concepts/architecture/fragments`:

* *"Fragments can't detect a change in input values."*
* *"Using caching and fragments on the same function is unsupported."*
* *"Fragments can't render widgets in externally-created containers; widgets can only be in the main body of a fragment."*

And on `parallel=True`, from the `st.fragment` API page: *"some Streamlit commands are
restricted because they are not safe to call from concurrent threads"*, plus *"Avoid
unsynchronized mutations of shared mutable resources across fragments unless you coordinate
access explicitly."*

### "No per-user Python process"

**This attacks something neither incumbent does.**

Streamlit's per-session unit is a script-runner thread inside one shared server process, not
a process. marimo's is a sub-thread — its own deployment documentation: *"marimo is spinning
up a new computational kernel in a separate sub-thread (same process) for each new session /
app created."* And `marimo export html-wasm notebook.py -o out/ --mode run` produces a
read-only app that runs entirely in the browser on Pyodide with **no Python server at all**.
shinylive (Posit, Pyodide + webR) and `panel convert` ship the same capability.

"Deployable reactive data app with no per-user backend process" is not available as a claim.
It was taken, in production, by at least three projects, years ago.

### What is left after the correction

One sentence, and this document exists to keep the project inside it:

> **The edges are declared once; the set of cells that recompute is derived from them and
> from value equality, and the runtime will print that set before you deploy.**

Everything below tests that sentence against each named alternative.

---

## 2. The field

Version numbers, publication dates and download counts below are the dossier's, fetched
2026-09-08. Download figures are 30-day unless labelled otherwise.

| project | reach, dated | what it is | does it occupy dagpane's lane? |
|---|---|---|---|
| **Streamlit** | 1.63.0 (2026-09-01); **24,852,382 PyPI dl/30d** to 2026-09-06; Snowflake-owned | Script-rerun model with declared, named, event-scoped fragment boundaries since 1.63 | **Yes, and it is the incumbent by three orders of magnitude.** The honest gap is that a fragment boundary is drawn and fired by hand and *"can't detect a change in input values"*. Everything else in the old pitch against it is gone. |
| **marimo** | 0.24.0 (2026-08-17); **2,420,457 dl/30d**; 22.7k stars; Apache-2.0; CoreWeave acquisition announced 2025-10-30 | Reactive Python notebook + app mode. Statically analyses each cell for *references* and *definitions*; a single-definition-per-variable invariant makes the analysis sound; running a cell runs everything referencing what it defines | **Yes. It is the closest precedent and it is not a prototype.** dagpane has no derivation claim against marimo — marimo derives too, and derives *more* (it reads the source; dagpane makes you write the edge). The only honest line is marimo's own: it cannot see mutation or attribute assignment. See §3. |
| **Dash** | 4.3.0 (2026-06-18), Plotly | Explicit `@callback(Output, Input, State)` graph over React; background callbacks via Celery/Diskcache | **Yes, conceptually — and it is the honest version of what Streamlit 1.63 became.** The complaint against Dash is verbosity, not correctness, and "less verbose than Dash" is a UX argument a Rust runtime does not win. |
| **Shiny (R)** / **Shiny for Python** | py-shiny 1.6.0 (2026); 259,860 dl/30d | The original fine-grained reactive graph — `reactive.value`/`calc`/`effect`/`event`, `isolate`, `req`; dependencies captured at **read time**, not by AST; 1.6.0 added OpenTelemetry for inspecting the reactive graph in production | **It got here in 2012, and it is the prior art dagpane must credit — not marimo.** Not Rust and server-bound, which is the only gap. Its 1.6.0 telemetry is the nearest thing anyone ships to `dagpane explain`, and it inspects a running app rather than answering before deploy. |
| **shinylive** | Posit; Pyodide + webR | Compiles a Shiny app to run wholly in the browser, no server | **Yes for the no-server half, and it shipped first.** Posit's own limits: ~13 MB Pyodide base, +7.5 MB numpy, +13 MB pandas, +11.5 MB matplotlib; *"Code and data for the application must be sent to the browser, so it can't be kept secret"*; no raw sockets. |
| **Observable Framework** | 1.13.4, last npm publish **2026-03-02**; 44,847 dl/30d | Static site generator for data apps; data loaders precompute snapshots at build time; front end is Observable's reactive JS runtime — topologically sorted cell dataflow | **No — build-time snapshots, not runtime recompute.** Six months without a publish is a signal. The Observable *runtime* is nonetheless a canonical reactive-dataflow design and is cited as prior art in §5. |
| **Evidence.dev** | core-components 5.4.2, last publish **2026-02-06**; MIT; ~6.8k stars | BI-as-code: SQL + Markdown compiled to a static site; "Universal SQL" runs DuckDB-Wasm in the browser so filters re-query client-side | **Yes for client-side compute, in production, for years.** Its limit is that an interaction re-queries a pre-baked snapshot rather than recomputing arbitrary logic. Seven months without a core publish is worth checking before any head-to-head. |
| **Panel / HoloViz** | 1.9.4 (2026); 2,579,411 dl/30d | PyData-native app framework (Param + Bokeh); explicit `.param.watch` / `pn.bind`; `panel convert` produces Pyodide/PyScript WASM apps | **No — its reactivity is Param-declared, not derived.** But 2.58M downloads a month means it is a real incumbent for the same audience, and its declared-edge model is the closest in spirit to dagpane's. |
| **Perspective** | `@perspective-dev/viewer` 5.3.1 (**2026-09-04**); OpenJS Foundation; Apache-2.0; 11.2k stars; 29,502 npm dl/30d | *"A fast, memory-efficient streaming query engine written in C++ and compiled for WebAssembly (including a 64-bit memory64 build for in-browser datasets larger than 4GB), Python and Rust"*, with a **symmetric client/server architecture** — the same Client API against an engine in-process, in a Web Worker, or over WebSocket | **Yes, and it must be named before anyone finds it.** "Same engine, server or browser, decided at deploy time" was going to be dagpane's second headline; Perspective shipped it, in a foundation, with the 4 GB wall already beaten. dagpane has **not** built this. The only daylight is that Perspective has no reactive application graph and no authoring surface. |
| **Mosaic** | `@uwdata/mosaic-core` 0.31.0 (2026-08-25); BSD-3; TVCG 2024 paper; 149,816 dl/30d | A coordinator mediating client views through `Param`s and `Selection`s, with query caching, **query consolidation**, prefetching and automatic pre-aggregate materialization, over DuckDB (server, Wasm, or Jupyter) | **Yes for the coordination half, and it is peer-reviewed.** Mosaic gets most of the perceived "only recompute what changed" benefit at the *query* layer with no language-level reactive graph. Anyone evaluating dagpane will ask why a DAG is needed when query rewriting delivers the latency. §6 answers it; the answer is narrow. |
| **Rerun** | 11.4k stars; Apache-2.0 OR MIT; Rust | Column-chunk store for multimodal/temporal robotics data; SDKs in Python/Rust/C++; viewer runs native and in the browser via WASM | **No — different buyer, different data model, no reactive recompute product.** It is the best evidence that a Rust + WASM data viewer is buildable and fundable. Take its packaging discipline, not its lane. |
| **Rill** | 2.9k stars; Apache-2.0 | Local-first BI over DuckDB/ClickHouse; SQL models + YAML metrics → dashboards | **No — and correct the record: Rill's runtime is Go.** Do not cite it as Rust prior art. |
| **Reflex / Taipy / Solara / Preswald / Gradio** | Gradio 10,850,287 dl/30d; Taipy 19.3k stars | Pure-Python app frameworks; Reflex compiles Python to a React frontend with state deltas | **No individually. Decisive collectively.** Seven frameworks already exploit the "Streamlit is bad" opening and none has displaced it. That is the syllogism this project must not repeat. |

### The Rust attempts in this exact lane — all of them

| crate | latest publish | lifetime dl | 90-day dl | stars | what it is |
|---|---|---|---|---|---|
| `rustview` 0.1.8 | 2026-06-02 | 263 | 96 | 21 | *"A Streamlit/Gradio equivalent for pure Rust"* — Axum + per-session vDOM, **full function re-execution plus a JSON patch diff**. It is the strawman dagpane was built against, already built, in Rust. |
| `venus` / `venus-core` 0.1.2 | 2026-07-15 | 193 / 554 | 68 | 42 | *"Reactive notebook environment for Rust"*. Cells are `#[venus::cell]` functions in a `.rs` file; **dependencies inferred from function parameters** — a signature-level analogue of dagpane's declared inputs, arrived at independently and first. Cranelift backend, Monaco frontend, no sandbox. |
| `GORBIE` 0.18.1 | 2026-06-10 | 2,835 | 419 | 13 | *"minimalist notebook library for Rust"*, egui immediate-mode, redraws every frame, explicitly a library rather than a server, no WASM story in its README. |
| `streamlit` (crate) 0.1.2 | 2026-01-06 | 51 | 16 | — | A name claim on crates.io (`killf/streamlit-rust`). Dormant. |

**3,342 downloads lifetime, 599 across all four in 90 days.** The lane is thin. It is thin
because it has been tried four times in the last twelve months and the market answered, not
because nobody thought of it. That distinction is the argument in POSITIONING.md, and this
project does not get to write "no Rust framework does this."

---

## 3. What dagpane actually does that the named alternatives do not

Narrow, and each item is checkable by running a command in this repository.

**1. The invalidation set is derived from the declared edges, and the tool prints it.**

```
$ cargo run -p dagpane-cli -- explain examples/sales.toml --set min_amount=400
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

Run on this checkout, 2026-09-08. The app is 11 cells and 7 panes; one slider move visits 8
cells, recomputes 6, reuses 1, never looks at 3, and repaints 3 panes.

This is a claim against **Streamlit** (you draw the fragment and you name it in
`st.rerun("filters")` — Streamlit will not tell you which fragments a widget should touch,
and its docs say a fragment *"can't detect a change in input values"*) and against **Dash**
(the `Output`/`Input` list per callback is the invalidation set, written by hand). It is
**not** a claim against **marimo**, which derives its edges from the source text and would
produce a comparable answer without being asked. Shiny for Python 1.6.0 ships OpenTelemetry
that lets you inspect the reactive graph of a running app, which is closer than anything
else on the list; the difference is that `dagpane explain` answers before a server exists.

**2. Against marimo, the honest line is marimo's own, and it is the only one.**

From marimo's reactivity guide, verbatim:

* *"marimo does not track mutations to objects, e.g., mutations like `my_list.append(42)` or `my_object.value = 42` don't trigger reactive re-runs."* Rationale given by marimo: *"Tracking mutations reliably is impossible in Python."*
* *"marimo does not track mutations to variables, nor assignments to attributes. That means that if you assign an attribute like `foo.bar = 10`, other cells referencing `foo.bar` will not be run."*

dagpane has no such hole because it has no such analysis: a cell names its inputs and the
runtime reads their values. The cost is symmetric and this document states it in the same
breath — **a dagpane cell that reads an input only on one branch still declares it, so it
recomputes when that input moves.** marimo's failure mode is a stale number; dagpane's is
wasted work. The value-digest check in `crates/core/src/session.rs` is what keeps that cost
to one recomputation rather than a cascade: the cell runs, produces the value it already
had, and nothing below it runs. `examples/sales.toml`'s `channels` cell exists to make that
visible, and the `explain` output above shows it firing twice in one pass.

**What this document does not know:** whether marimo stops propagation when a rerun produces
an identical value. The sources gathered for the dossier do not say, and no one here has
measured it. Do not write that marimo lacks it.

**3. A cycle is a build error carrying the loop path, and a graph can be printed before it
runs.** Because edges are declared rather than traced, `GraphBuilder::build` runs Kahn's
algorithm and returns `BuildError` with the cycle; `dagpane graph --format mermaid` renders
the app without executing a cell. marimo's single-definition invariant gives it the same
static footing, so this is a claim against Streamlit and Dash only, where a callback loop is
found at run time.

**4. One `Arc<Graph>` and one `Arc<App>` behind every connection, asserted by pointer
identity rather than by an RSS measurement.** `crates/core/tests/oracle.rs` and the reactive
suite check `Arc::ptr_eq` on an untouched source across two sessions. A session is a vector
of value slots beside a shared immutable graph. Compare marimo, whose per-session unit is a
sub-thread with its own kernel heap, and whose deployment docs impose sticky sessions —
*"you will need to use sticky sessions in your load balancer to ensure that the same client
gets the same kernel each time"* — with `marimo-team/marimo#1831` ("Support stateless for
multi container scaling and deployment") open since **2024-07-19** with no maintainer
response visible.

**State the structure, not a benchmark.** dagpane has measured no apps-per-core number, no
p99 under concurrent viewers, and no memory figure. Until it has, this is a description of a
data structure, not a performance claim, and POSITIONING.md says so.

**5. The engine is checked against a full-recompute oracle on random graphs.**
`crates/core/tests/oracle.rs`: 200 pseudo-random DAGs from an in-tree 64-bit LCG (so a
failing seed is reproducible for the life of the project), 12 random interactions each =
2,400 interactions, and after every commit two assertions — every cell's value equals what a
fresh session computes from scratch, and every cell the pass evaluated lies inside
`graph.closure(roots)`. Correctness and economy, on graphs nobody drew.

That test found a real bug. An error's digest originally hashed only its message. Two
upstream cells failing with the same words digested alike, so a downstream cell whose
*cause* moved from one to the other served its cached error and went on naming a cell that
was no longer the problem. No hand-written test caught it; `CellError` now has its own
`Digestible` impl hashing cause and message, and
`a_failure_that_moves_to_a_new_origin_stops_naming_the_old_one` in
`crates/core/tests/reactive.rs` is the regression.

The dossier records no equivalent differential test for any named alternative. That is not
evidence none exists — nobody looked for one — and this document will not imply otherwise.

**6. 161 tests pass with no network, no browser and no async runtime below `serve`.**
`cargo test --workspace`, run on the released checkout 2026-09-17: 161 passed, 0 failed.
It was 122 when this was first written on 2026-09-08; the Arrow backend crate landed
after that, and the figure is re-measured rather than left to rot.
`dagpane-core` has exactly one dependency, serde, and `#![forbid(unsafe_code)]`.
`cargo check -p dagpane-core --target wasm32-unknown-unknown` exits 0 — also verified today,
and §4 says what that does and does not mean.

---

## 4. Sentences this project does not write

The dossier that preceded this module carried a ledger of claims a reader could falsify. It
is reproduced here as a standing rule, not as an appendix. A line added to this list is
cheaper than a line removed from a README after someone else removes it for you.

**About Streamlit**

* ❌ *"Streamlit reruns your entire script on every interaction."* Half-true since 1.33 (2024-04-05), misleading since 1.63 (2026-09-01). Write: *"Streamlit's partial reruns are boundaries you declare; dagpane's are derived from edges you declare once."*
* ❌ *"Streamlit has no partial rerun."* False. `st.fragment`; `parallel=True` (1.58); fragments writing to outer containers (1.59); event-scoped `st.rerun("filters")` from callbacks and `on_change="ignore"` (1.63).
* ❌ *"Streamlit spawns a process per user."* It is a thread inside one server process.
* ❌ *"Streamlit's caching is thread-unsafe."* `cache_data` pickles and returns a copy; `cache_resource` shares one instance and the docs require the value to be thread-safe; session-scoped caching shipped in 1.53.0 (2026-01-14). Describe the mechanism, not a defect.
* ❌ *"Streamlit is legacy / abandoned."* 1.63.0 on 2026-09-01; Starlette default since 1.57; 24.85M downloads a month.

**About marimo**

* ❌ *"marimo requires a Python process per user."* It is a sub-thread, per its own deployment docs — and `marimo export html-wasm --mode run` requires no server at all.
* ❌ *"dagpane is the first deployable reactive data app with no per-user backend process."* marimo, shinylive and Panel-in-Pyodide all ship that.
* ❌ *"marimo is a notebook, not an app framework."* `marimo run` is app mode and has been for years.
* ❌ *"marimo is a research project."* 22.7k stars, 2.42M downloads a month, acquired by CoreWeave (announced 2025-10-30), molab rebuilt on CoreWeave Cloud, public preview June 2026.
* ✅ *Permitted:* marimo cannot see mutations or attribute assignments — its own documentation says so — and its server path documents a sticky-session requirement and recommends vertical scaling first.

**About the reactive graph**

* ❌ *"The first true reactive dependency graph for data apps."* Shiny, 2012. Observable's runtime. marimo.
* ❌ *"Recomputes only the transitive closure of what changed"* offered as a differentiator. That sentence describes marimo exactly. The differentiator is what counts as *"what changed"* — and in v0.1.0 that answer is "a cell's value", which is smaller than a rerun but is not smaller than a cell.
* ❌ Listing *"salsa"* and *"the rust-analyzer red-green algorithm"* as two prior arts. They are one codebase, published by rust-analyzer's own maintainer. See §5.
* ❌ *"Built on Adapton."* Its last release was 2019-12-22 and it does 435 downloads a quarter. Cite the papers.

**About WASM and client-side compute**

* ❌ *"dagpane runs in the browser."* It does not. `cargo check -p dagpane-core --target wasm32-unknown-unknown` exits 0, CI runs it, and that is the entire claim. There is no `crates/wasm`, no wasm-bindgen, no client-side compute, and nothing has ever executed in a browser. The check is a real constraint on the core — no filesystem, no clock, no threads — and nothing more.
* ❌ *"Rust/WASM means a smaller download than Python/WASM."* False for anything shipping a SQL engine: `duckdb-eh.wasm` is 34.25 MB uncompressed (jsDelivr, 1.33.1-dev57.0, 2026-06-22) against shinylive's ~13 MB Pyodide base.
* ❌ *"Polars in the browser."* Polars does not build for `wasm32-unknown-unknown`; `pola-rs/polars#16729` (2024-06-04) and `#19211` (2024-10-12) are both open.
* ❌ *"First to run the same engine on server or client."* Perspective 5.3.1 (2026-09-04) ships exactly that, Apache-2.0, OpenJS Foundation, with a memory64 build past the 4 GB wall.
* ❌ Any gzip or brotli transfer figure not measured on the CDN that actually serves it.

**About the lane**

* ❌ *"No Rust framework does this."* `rustview`, `venus`, `GORBIE` and a `streamlit` crate all exist and all published in 2026. Write *"no Rust attempt has reached adoption"* and print 599 downloads across 90 days.
* ❌ *"Rill is a Rust BI tool."* Rill's runtime is Go.
* ❌ *"Nobody does interaction-to-minimal-query."* Mosaic (BSD-3, `mosaic-core` 0.31.0, 2026-08-25, TVCG 2024) does query consolidation, caching, prefetch and automatic pre-aggregation; Evidence ships DuckDB-Wasm re-query in production as "Universal SQL".
* ❌ Any argument resting on the GIL as a permanent condition. PEP 779 was accepted and free-threaded builds became officially supported in Python 3.14 (October 2025); single-thread overhead is reported in single digits, down from ~40% in 3.13.
* ❌ *"Deployable"* unqualified. Everything in §2 is deployable.
* ❌ Any apps-per-core, p99 or memory number. **None has been measured.** The only numbers this project may print today are cell counts, pane counts, test counts and the trace.

---

## 5. Rust incremental-computation prior art — what was taken and what was not

dagpane depends on none of these. `dagpane-core`'s dependency list is one line, serde. What
follows is an intellectual debt ledger, and it is a real one.

| prior art | state as of 2026-09-08 | taken | not taken |
|---|---|---|---|
| **salsa** — *and this is also "the rust-analyzer red-green algorithm"; they are the same codebase, published by rust-analyzer's maintainer Lukas Wirth, and presenting them as two entries is an error* | 0.28.2, published 2026-08-03; Apache-2.0 OR MIT; 7,957,243 lifetime / 2,254,440 downloads in 90 days; self-described *"(experimental)"* | **Backdating** — stop propagating when a recomputed value equals the previous one. dagpane's digest comparison is that idea at cell granularity, and it is the single mechanism that makes an over-declared edge cost one recomputation instead of a cascade. `crates/core/src/session.rs` is where it lives. | The **demand-driven pull** discipline and the global revision counter. dagpane pushes dirty marks over the structural closure and evaluates eagerly in ascending `height`; salsa validates lazily on demand. dagpane also did not take salsa's compile-time query set — this graph is built at run time from a manifest — nor **durability tiers**, which would be the right answer to "a 600-row CSV source should never be revalidated because a slider moved" and are not built. |
| **comemo** (Typst's engine) | 0.5.1, 2026-01-29; MIT OR Apache-2.0; 2,763,162 lifetime / 1,121,750 in 90 days | **Nothing, in v0.1.0.** Recorded here because it is the idea this project most owes and has not paid: `#[track]` makes a type's *accesses* observable and `#[memoize]` reuses a cached result when only untouched parts of an argument changed. Applied to a table, that is column-level provenance — a cell reading 3 of 200 columns not invalidating when column 47 moves. | All of it, for now. dagpane invalidates at whole-value granularity: change one cell of a `Table` and every cell reading that table recomputes. comemo's global cache with coarse eviction would also be wrong for a multi-session server even if the mechanism were adopted. This is the gap POSITIONING.md's condition 3 is about, and it is not closed. |
| **Shiny's reactive graph (R, 2012)** — and py-shiny 1.6.0 | py-shiny 1.6.0, 2026; 259,860 dl/30d | The **origin of the whole idea** in a data-app framework, and the credit belongs here rather than to marimo. Also the demonstration that a reactive graph is worth *inspecting*: Shiny 1.6.0's OpenTelemetry export is the ancestor of `dagpane explain`. | **Read-time dependency capture**, which is Shiny's actual mechanism and leptos's — the reader is recorded when a value is read, so the graph is exact and needs no analysis. The dossier this module was built from recommended it explicitly. dagpane went the other way, on purpose: read-time capture means the graph exists only after a run, so a cycle is a run-time surprise, the graph cannot be printed before it executes, and it cannot be built once and shared immutably across sessions. ADR-0001 records that decision and the cost it accepts — an over-declared edge — rather than pretending there is none. |
| **Observable's runtime** | Framework 1.13.4, last npm publish 2026-03-02 | The **topologically-sorted cell dataflow** shape. dagpane's ascending-`height` pass is the same family of answer to the same glitch problem: evaluate in an order where every input is final before its consumer runs. | Framework's build-time data-loader model, which is a different product — snapshots computed once at build, not recomputed per interaction. |
| **Adapton** | 0.3.31, last published 2019-12-22; 435 downloads in 90 days; MPL-2.0; dead | The **papers** — the Demanded Computation Graph, and **nominal matching**: naming allocations so a re-execution reuses the same node identity instead of allocating a fresh one. | The crate, which would be a finding in any review. And the mechanism, which v0.1.0 does not need: dagpane's graph is built once and never changes shape at run time, so node identity is trivially stable. The problem nominal Adapton names arrives the moment a manifest can add or remove cells while a session is live, and that is the day to read the papers properly. |
| **differential-dataflow / timely** | dd 0.25.1 (2026-07-15), timely 0.31.0 (2026-07-14); MIT; 60,589 / 70,720 downloads per 90 days | Nothing yet. The correct model for incrementality *inside* an operator — a changed row updating a join or aggregate in time proportional to the change — if dagpane ever grows a live table. | The core. DD requires the whole computation expressed in DD operators; a dagpane cell is an opaque `Fn(Inputs) -> Result<Value, CellError>` that DD cannot differentiate, so the cost of arrangements and traces would arrive with none of the benefit. Its per-dataflow memory footprint is also hostile to many concurrent sessions. |
| **leptos `reactive_graph`** | 0.3.0-beta2, 2026-07-18; MIT; 2,504,608 lifetime / 1,071,119 in 90 days | The **diamond discipline** — a join evaluated once, never on a mix of old and new — which dagpane gets structurally from height ordering rather than from batching. | Its granularity and its scheduler. `reactive_graph` is tuned for thousands of nanosecond-cheap nodes; a dagpane cell is a table scan. It is also pre-1.0, and a pre-release on the critical path of an engine whose whole product is a correctness claim is not a trade this project makes. See ADR-0002. |

---

## 6. What a skeptical reader will say, and what is true

**"Streamlit already does targeted partial reruns, as of a week ago."** Correct, and §1 says
so before anyone else can. The remaining difference is who computes the invalidation set. It
is a real difference and it is smaller than the brief implied.

**"marimo already derives the graph, and from source text, so it's strictly better
ergonomics."** Substantially correct, with one documented exception in each direction.
marimo cannot see mutation or attribute assignment and says so. dagpane cannot see a branch,
so it over-declares and pays for it in a recomputation the digest check then contains.
Neither of those is a knockout.

**"Mosaic gets the same perceived latency from query consolidation, without a DAG."** The
honest answer is that Mosaic's target is a dashboard of linked views over a SQL engine, and
its coordinator optimises *queries*. dagpane's cells are arbitrary Rust closures, and a
closure is not rewritable into a consolidated query. That means dagpane's approach applies
where Mosaic's does not — and equally that where the whole app *is* SQL over DuckDB, Mosaic
is the peer-reviewed answer and dagpane has nothing to add. Read the TVCG 2024 paper before
any head-to-head.

**"Perspective already ships symmetric client/server compute."** Yes — 5.3.1, 2026-09-04,
Apache-2.0, OpenJS Foundation, with a memory64 build past 4 GB. dagpane has not built engine
placement and must not imply it has. `cargo check --target wasm32-unknown-unknown` is a
constraint on the core, not a browser story.

**"Four Rust attempts already failed at this."** True: 599 downloads across 90 days between
`rustview`, `venus`, `GORBIE` and the `streamlit` crate. `rustview` is specifically the
full-re-execution design dagpane argues against, already implemented, in Rust, with 96
downloads in 90 days — which suggests the execution model was not what was holding it back.
This is the strongest argument against the module existing and it is answered, or not, in
POSITIONING.md rather than here.

**"You are asking a data scientist to write Rust."** Partly. The manifest path
(`examples/sales.toml`) means an app is TOML — sources, inputs, cells with a pipeline of
nine verbs (`filter`, `derive`, `join`, `select`, `sort`, `limit`, `group_by`, `scalar`,
`count`), and panes — with no Rust in it, **or as a `select` statement**, which is parsed and
lowered to those same nine verbs rather than run by a second engine (ADR-0005). So
`margin = revenue - cost`, "put the deploy count next to the error budget" and a
four-aggregate group-by are all things the manifest can now say. What it cannot say is an
aggregate inside an expression, a window function, a subquery, a `with` clause, a `having`, a
`full` outer join or a join on anything but equality — the dialect is closed on purpose, and an
author who knows SQL will hit its edges. Past them, the author is writing a
`GraphBuilder::cell` closure in Rust. Even inside SQL the edge rule does not bend: the tables
come from the resolved parse tree and never from scanning the text, and a control is written
`:name`. That is a defensible position and the ceiling above it is still real.

**"v0.1.0 has no authentication."** It did not; 0.1.1 has an optional one. `--auth-jwks`
verifies OIDC id tokens against a JWKS file, so the process still makes no outbound request,
and without it `dagpane run` binds 127.0.0.1 and `--host` anything else prints a warning. The
narrower claim is the one worth holding onto: access is per **app** and never per pane, an
accepted connection is not logged, and a token cannot be revoked before it expires.

Sessions are still per-connection: closing the tab discards one, and there is no store, no
eviction, no TTL and no server-side resumption token. The viewer's own control values ride in
their page's URL fragment instead, which is what makes a filtered dashboard a link without
making it a thing to name and guard. That removes the sticky-session problem by having no
state worth preserving across a reconnect — a smaller and more honest claim than making
session state serialisable, which this project still has not built. SECURITY.md is the place
for the rest.
