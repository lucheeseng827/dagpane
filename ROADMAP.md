# dagpane — roadmap

v0.1.0 ships one thing: an engine that recomputes the cells an interaction reaches and
prints which ones. Everything below is something that engine does not do, ordered by what
forces the work rather than by effort or by appeal.

**No item here has a date, and that is the rule of this file.** A date on an unstarted item
is a guess wearing a plan's clothes. Every item instead carries **the trigger** — the
specific, checkable event after which the work is worth doing and before which it is
speculative. Several items say **not started**, in those words, because they are.

`POSITIONING.md` §3 grades this project against the four conditions the prior-art dossier
said had to hold for it to matter, and scores it **two of four fully met, two partly**. This
file is the other half of that document: not what is wrong, but what would have to happen
first. *(This paragraph said "zero of four" for a round after §3 changed condition 1 to met —
the score was written down in four places and one of them was updated. See §7's note on the
same habit.)*

---

## v0.1.0 — shipped

- [x] `dagpane-core`: declared edges, Kahn's algorithm for order and cycle detection,
      per-cell `height`, height-ordered evaluation, digest short-circuit, errors as values.
      One dependency (serde), `#![forbid(unsafe_code)]`, no I/O, no clock, no async runtime.
- [x] `dagpane-app`: panes, five widget kinds, nine pipeline verbs (`filter`, `derive`,
      `join`, `select`, `sort`, `limit`, `group_by`, `scalar`, `count`), an in-tree
      RFC-4180-subset CSV reader with per-column type inference, a TOML manifest compiler,
      patch generation.
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

**The authentication half shipped. The session store deliberately did not, and this item's
own argument is the reason.**

A session still **is** a connection. `AppSession::open` builds a slot vector beside the shared
`Arc<App>`; closing the tab drops it. What changed is who holds the inputs: the client keeps
its control values in the page's URL **fragment**, which a browser never puts on the network,
and replays them when it connects. A reload comes back to the same filtered view, and a
filtered dashboard is a link somebody can be sent — with no session store in this process to
run, replicate, evict or leak.

That is this item's ordering taken to its conclusion rather than abandoned. **A session store
without authentication is a mechanism for reading somebody else's session:** the moment a
session outlives its socket it needs a name, the moment it has a name something has to decide
who may say that name, and a resumption token any client can guess is worse than no resumption
at all, because the first design at least fails visibly. Putting the state in the viewer's own
URL sidesteps the naming problem instead of solving it, which is the cheaper answer for as
long as nothing needs durability the viewer cannot supply.

- [x] **An authentication boundary in `serve`, ahead of the WebSocket upgrade.** `crates/auth`
      verifies an OIDC id token against a JWKS the operator supplies as a **file**, so this
      process still makes no outbound request: nothing to fail at start-up, nothing to
      redirect, and an air-gapped install works. `alg: none` and every `HS*` are unreachable
      rather than merely refused — the allowlist is an enum with no variant for them. What a
      session is scoped to is answered as **per app**, by a claim the operator names or an
      explicit "any app"; `SECURITY.md` states what that leaves out.
- [x] **A `--host` that binds a non-loopback address without printing a warning**, which this
      item called the observable form of "this is now deployable". With `--auth-jwks` there is
      no warning. Without it there still is, and it names what anyone who can reach the
      address can read.
- [ ] **A server-side session identity and store.** Still not built, and the fragment answers
      the case that motivated it. `Session` is not `Serialize`; `Value`, `Outcome` and
      `CellError` all are, so the slot vector remains reachable if it is ever wanted. What
      would actually need one: a session that must survive the viewer losing the URL, or be
      readable by something other than the browser holding it.
- [x] **Eviction and a TTL.** `crates/host` had the app-level half — a byte budget and an
      `idle_after` — and **nothing in the shipped binary called `Host::sweep_idle`**, so
      `dagpane host --idle-minutes` configured a sweep that never ran and an app left only on a
      redeploy or under budget pressure. `dagpane_serve::sweep_idle` is the caller, running at
      half the idle window and printing every eviction; `--idle-minutes 0` keeps the old
      behaviour. What remains open is the *sizing*: item 7's memory number still does not
      exist, so `--idle-minutes 60` is a default rather than a derived one.

**The trigger fired**, and the front door is what it produced. The honest position is narrower
now rather than gone: a token gets a viewer through the door, and everything behind it is per
app rather than per pane, unaudited, and revocable only by expiry. `SECURITY.md` lists those
three by name — read it before the first deployment rather than after the first incident.

---

## 2. A real dataframe engine behind the value model

`Table` is `Vec<Option<T>>` per column, four column types, no chunking, no dictionary
encoding, no SIMD — sixteen bytes for an `i64`. `ARCHITECTURE.md` §6 named the seam
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
`Value::Frame { v: FrameRef }`, and the verbs in `transform.rs` take `&dyn Frame` and
return `Arc<dyn Frame>` — `filter`, `sort` and `limit` reduce to choosing row indices and
calling `take_rows`, `select` to a projection, and only `group_by` materialises, which it
was doing anyway to aggregate. (`derive`, added later, materialises one column and shares
the rest through a `with_column` adapter, so it costs a column and never a frame.) The pipeline's old `Cow::Borrowed` fast path is now an `Arc`
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

- [x] Measure first: the row count and column width at which the current `Table` stops
      being the right answer, on a real app, with a real source — not a synthetic sweep.
      `BENCHMARKS.md` is that measurement on the bundled app: the interactive ceiling is
      around 100 000 rows, and memory amplifies about seventeen times over the CSV on disk.
- [x] Then pick the engine, and know what picking it costs. Arrow — three sub-crates and not
      the umbrella, at a measured 259 KiB gzipped — and the cost is written down rather than
      implied: the win is dictionary encoding rather than Arrow, so the decision is per
      column, and a whole session sees 41% where one representation shows 4.1×.

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
`head(n)` and `digest()`, and the verbs are hand-written kernels over that
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

## 3. Sub-node invalidation — column- and row-granular

**All three halves built and measured.**

dagpane used to invalidate at whole-value granularity: a cell that read three columns of a
200-column frame recomputed when any of the 200 changed. It now compares the columns a cell
actually read.

**Say the rest plainly, because it used to be the most important sentence in this file.**
The prior-art dossier this module was built from considers **this and only this** to be a
technical differentiator that survives contact with marimo. Node-level reactivity is
marimo's, shipped, free, and doing 2.42M PyPI downloads a month; Python cannot do the
sub-node version, and marimo's own docs say why — *"tracking mutations reliably is
impossible in Python"*. Streamlit cannot do it either: *"fragments can't detect a change in
input values."* Rust can, because access can be made observable. That paragraph used to end
"and **it is not built**". It is built.

- [x] Column-granular access recording. Not on `Table` reads in the end, and the difference
      is worth stating: recording per `value_at` would put an interior-mutable set in the
      hot loop and charge a set insert per *cell* of the table. What is recorded instead is
      per **step** — `core::reads::ReadLog` is where a compute declares what its output
      depended on, and `app::reads` works that out by tracking, through the nine verbs,
      where each column of the frame in hand came from. Once per step, never once per row.
- [x] Predicate-range constraints — the harder half, and the one that makes a filter change
      that does not move the result set cost nothing. What a cell downstream of a `filter`
      depends on is *which rows survived*, so what gets recorded is the predicate and the
      selection it produced, not the column. A later pass compares the column's digest first
      — an unmoved column cannot have moved its selection, so the common case costs nothing —
      and only re-runs the predicate when the column genuinely changed: one pass over one
      column, against a cell that would otherwise touch every column of every surviving row.

      **Two conditions, and both are correctness rather than tidiness.** A constraint is
      re-run against the *input* frame, so it has to mean the same thing there: the filtered
      column must be that input's own column, untouched, and the rows it ran over must be
      that input's rows. So a filter on a derived column is never a constraint, and neither
      is a filter after another — the second one's row indices are positions in the first
      one's output, and they can coincide with a selection over the whole frame while meaning
      something else entirely. `a_second_filters_indices_can_coincide_with_a_whole_frame_selection`
      in `crates/app/tests/subnode.rs` is that coincidence, constructed, and it is stale by
      two rows without the rule.
- [x] Ordering constraints on `sort` — the same bargain about *order* rather than about
      membership. A `sort` decides what order the rows are in and contributes no value; a
      top-N pane below it depends on the permutation and on the columns it totals, not on the
      column it ranked by. So what gets recorded is the ordering, under the same two
      conditions and validated the same way: the column's digest first, and the sort re-run
      over one column only when it genuinely moved.

      This file previously said sort was not worth doing, on the grounds that "the
      permutation of a column whose values moved is rarely the same permutation". That was an
      assertion nobody had measured, and it is wrong in the direction that matters. Any
      order-preserving change leaves the permutation *exactly* intact — a uniform shift, a
      rescale, a re-baseline, a unit conversion — and a single value moving disturbs it only
      if it crosses a neighbour. Measured:
      `a_top_n_pane_mostly_sleeps_through_a_single_edit_to_its_ranking_column` puts a top-five
      pane through forty single-row edits to its ranking column and it sleeps through 16 of
      them; the order-preserving edits score 40 of 40. Whole-value granularity scores 0 on
      both.

      **What it does not buy, stated because it is the honest half.** Nineteen of the twenty
      bundled apps sort *after* a `group_by`, which fails both conditions at once — the sorted
      column is derived, and the rows are no longer the input's. Every leaderboard in the
      corpus also displays the column it ranks by. Turning the feature off changed not one
      count in `examples/tools/verify.py` until `19-wide-telemetry` gained a cell written to
      the shape that qualifies. The shape it pays for is real — *rank the raw rows by one
      metric, report a different one over the top of them* — and it is narrower than the
      filter case.
- [x] A per-session, budgeted, evictable cache — **by construction, and it needs no work**.
      This bullet was written against comemo's design, where memoized results live in one
      process-global cache with coarse eviction; that is right for a compiler holding one
      document and wrong for a server holding N sessions. dagpane never adopted it. A
      `Session` has always been a vector of slots beside a shared `Arc<Graph>`, so the cache
      *is* per-session and is freed with the session. The budget and the eviction are
      `crates/host`'s, and they evict whole sessions and apps, which is the unit an operator
      can reason about.
- [x] Extend `crates/core/tests/oracle.rs` to random column-level edits before any of the
      above is trusted. 120 random graphs over wide frames, one column rewritten per
      interaction, asserting both halves — every cell equals a from-scratch recompute, and no
      cell that reads none of the moved column ran. Four mutations were run against it and
      each is caught. `crates/app/tests/subnode.rs` is the other half, holding the *analysis*
      to the same standard.

**What it costs, which this file should not round off.** Every frame value now carries one
digest per column — 16 bytes times the width, per frame-valued cell, per session. On the
122-column example that is about 2 KB per frame cell. A source's is taken once when the
graph is built and shared by every session, exactly as its whole-value digest always was; a
computed cell's is per session because the value is. The hashing itself is not new work: the
frame digest is now composed from the column digests rather than taken in one stream, so a
value that needs both still pays for one walk over its data.

**The claim, measured.** `examples/apps/19-wide-telemetry.toml` is a node exporter's scrape
flattened one column per metric — 122 columns, of which the page reads five.

```
$ dagpane explain examples/apps/19-wide-telemetry.toml --change-column metrics.disk_sdb_write_ops
  changed 1 column of a 122-column frame; recomputed 0 of 11 cells
  patch: 0 of 9 panes — nothing to send
```

Every cell was still *visited* — they all declare an edge to `metrics` and the engine cannot
know better until it looks — and not one of them ran. Whole-value invalidation cannot express
that: to it, the table changed.

The predicate half shows on the same app and the same column. `--set` settles the slider in
its own pass; the column change is what is measured.

| `busy_cpu` | rows crossing the floor | recomputed | panes sent |
|---|---|---|---|
| 40 | none | **0 of 11** | 0 of 9 |
| 45 | none | **0 of 11** | 0 of 9 |
| 35 | three | 4 of 11 | 3 of 9 |

Three cells filter on `cpu0_user` and every value in it moved. At a floor of 40, 129 of 204
rows clear it before and after — the *same* 129 — so none of the three can have a different
answer, and none runs. At 35, three rows cross, and they do. What the engine compares is not
the column but the question asked of it.

The ordering half shows on the same line. An eleventh cell, `busiest_ten_memory`, ranks the
scrape by `cpu0_user` and averages memory over the top ten. It is reused at 35 — the row that
crossed the floor did not cross its neighbour in the ranking — while the three filtering cells
run. One column, one edit, one pass: the filters' answer moved and the sort's did not. Drop
the ordering constraint and that same edit recomputes 5 of 11 instead of 4.

---

## 4. Client-side compute (WASM)

**Built and measured — and the measurement is not the one this section expected.**

`crates/wasm` is the engine in a browser: the same manifest, the same nine verbs, the same
`AppSession` and the same patch protocol, with a function call where the socket was. The
client did not change — it gained a `transport`, and a WebSocket and a wasm call are two of
them. `dagpane export` writes an app as a directory of static files with the data in the page.

This section used to open with four warnings. Three of them survive and one is now a number.

* **"Rust/WASM means a smaller payload" is false for anything that ships a SQL engine** —
  still true, and now the argument *for* the thin evaluator rather than a caution against the
  whole idea. DuckDB-Wasm is 34.25 MB and DataFusion 27 MiB; `dagpane-wasm` is **1.10 MiB raw,
  318 KiB gzipped**, with the reactive engine, the nine verbs, the expression language, the
  manifest compiler and the CSV reader in it. `BENCHMARKS.md` has the table and the method.
  The claim was only ever available to a client that stays a thin evaluator, and this is one.
* **wasm32 caps at 4 GB**, and a browser may cap lower. Unchanged, and it is the reason
  `dagpane export` inlines its rows rather than pretending it can stream: an exported app is
  as big as its data, the command prints the total, and `BENCHMARKS.md`'s row sweep says where
  that stops being reasonable. Tens of thousands of rows, not millions.
* **Multi-threading needs cross-origin isolation.** Sidestepped rather than solved: this
  module imports **nothing** and asks the host for nothing — no threads, no `getrandom`
  backend, no COOP/COEP — because `dagpane-core` was already clockless and I/O-free for
  testability. A single-threaded pass is what it has, and the measurement below is of that.
* **Somebody already shipped the architecture.** Still true and still worth saying.
  Perspective's symmetric client/server API is the same shape. What is here is not a frontier;
  it is a lane with incumbents, and the reason to be in it is that dagpane's evaluator is two
  hundred kilobytes rather than thirty megabytes.

**The trigger, and what it actually said.** This item would not open on "WASM would be
interesting": it demanded *an interaction whose latency is dominated by the round trip,
measured on a real app*. `benches/roundtrip/` is that measurement — three apps, 300
interactions each, over loopback, comparing a real WebSocket round trip against the same pass
run in wasm in the same process.

| app | round trip | the wire | wasm pass | wasm vs native | break-even |
|---|---:|---:|---:|---:|---:|
| `sales.toml` | 0.153 ms | 0.101 ms (66%) | 0.130 ms | 2.5× | **0.08 ms** |
| `15-unit-economics` | 0.165 ms | 0.080 ms (48%) | 0.235 ms | 2.8× | **0.15 ms** |
| `19-wide-telemetry` | 0.751 ms | 0.347 ms (46%) | 0.892 ms | 2.2× | **0.49 ms** |

The wire is about half of every interaction on the friendliest wire that exists, so the
trigger is met. **And wasm is 2.2–2.8× slower at the identical pass**, so on loopback the
browser is *slower* on two of the three apps. The figure that transfers off that machine is
the break-even, and it is a threshold on the **wire** rather than on the round trip: a browser
wins when `wasm pass < wire + server pass`, so it needs `wire > wasm pass − server pass`. That
is 0.08–0.49 ms of wire overhead, independent of the network; on the round trip the same
threshold is `wasm pass` itself, 0.13–0.89 ms.

Independent of the network, not of the app: a break-even is `wasm pass − server pass`, so it
is a property of the pipeline and a heavier one has a higher threshold. Those figures are the
three apps measured, and `benches/roundtrip/` is how to get the number for a fourth. Typical
round trips — ~0.5 ms in a datacentre, ~5 ms in a city, 30–100 ms across a continent — clear
them, but that is context for the scale rather than a floor this measurement establishes.

So: client-side compute buys you the wire and charges about 2.5× on the pass to do it. That is
a good trade for a dashboard on a real network and a bad one for an app whose pass is already
most of a frame budget. The honest sentence is not "it is faster" — it is **"it removes the
network, at a price that is written down"**.

**What is still not built**, and this section should not blur it:

* **Per-cell placement — the mechanism is built and nothing serves it yet.** §4's defensible
  version was always "the placement of each *cell* is a deployment decision rather than a
  rewrite", and the correctness argument the line above used to defer is now written down and
  checked: **ADR-0008**, `crates/core/src/placement.rs`, and a split oracle that cuts two
  hundred generated graphs at random admissible places and requires the two halves to agree
  cell for cell with an undivided session.

  The surface is one word. `place = "client"` on a `[[cell]]` or an `[[input]]`;
  `examples/apps/20-placed.toml` is `sales.toml`'s pipeline with four of them added and
  nothing else changed. Placement must be **monotone** — a server cell may not read a client
  cell — which is what makes the cut a frontier crossed by one message per pass rather than a
  sieve costing a round trip per height boundary. `dagpane check` refuses a bad cut by naming
  the edge and both ways out, and reports the two numbers a deployment turns on:

  ```
  $ dagpane check examples/apps/20-placed.toml
    placement: 4 cell(s) in the page, 2 on the server
    frontier: 1 cell(s) cross the wire — sales
    1 of 1 control(s) would be answered without the network — min_amount
    note: the cut is checked and reported, not yet served — `dagpane run` and
          `dagpane export` evaluate every cell on this side. ROADMAP.md §4.
  ```

  That note is the honest state of it, and the conditional mood in the line above it is doing
  the same work. The transport is built; what remains is a page to put on the other end of it:

  - [x] **A boundary message.** `ServerMessage::Init` carries the full frontier and
        `ServerMessage::Patch` the delta, pairing exactly as `full_views` and `patch` already
        do. `BoundaryValue` is the wire shape — an `Outcome`, so a failure upstream of the cut
        arrives as a failure rather than as a null the page would draw as an empty answer.
        Both fields are `skip_serializing_if = "Vec::is_empty"`, so an app that named no
        placement sends the bytes it sent before.
  - [x] **`AppSession` split the way `Session` now is.** `open_side` runs either half over its
        own graph, with its own rendered-view cache — a page-side pane repainted in the browser
        is not recorded on the server, which would otherwise make the stats block lie. **A pane
        belongs to whichever side holds its cell**, so no `Pane` field says which side it is
        on and no second answer can disagree with the cut. `dagpane-wasm` opens the client half
        on `side: "client"` and `dp_deliver` applies a frontier and answers with the page's own
        patch; the extraction is in Rust rather than in the page because applying a frontier
        *together* is the one obligation the transport carries.
  - [ ] **A page that can hold the other half.** This is all that is left, and doing the work
        above turned up two questions that were not visible before it:

        * **The page has to type-check its half without the data** — **answered and built.**
          `SchemaSource` offers a shape and no rows; `ServerMessage::Init` carries a
          `client_half` block with the manifest (re-emitted from what the server parsed, so
          the page compiles what the server compiled and not a file edited since) and one
          column list per source, taken from frames already loaded so it costs no re-read.
          `a_page_can_boot_from_the_opening_frame_alone` serialises a real `init`, throws the
          server away, and requires a page built from that message alone to draw exactly what
          the undivided app draws. The boot block is under 4 KB and the frontier is the rows,
          which is the honest shape of the cost: an opening frame is as big as the data,
          because cutting below the data means the data crosses once.

        * **A server that serves a split has to hand the browser an engine** — new, and the
          next thing to decide. `dagpane-serve` compiles `client.html` into the binary and
          deliberately does **not** embed the 1.1 MiB wasm module: `crates/cli/src/export.rs`
          argues that embedding would make every `dagpane` a megabyte larger for a command
          most runs never call, and put the build of one crate inside the build of another.
          So the module has to reach the page some other way, and the obvious candidate is
          `dagpane run --wasm <path>` matching `dagpane export --wasm` — which makes serving a
          split an opt-in with a flag rather than something a manifest can turn on by itself.
          That is a deployment decision and it is worth making deliberately.
        * **The stats bar needs a meaning under a split.** It is the product claim, live, and
          with two halves each running a pass there are two sets of counts. Showing one is a
          lie by omission and adding them is a different lie. Deciding what it says is a
          design question, not an implementation detail.

        Until then `dagpane run` and `dagpane serve` evaluate every cell on the server for a
        placed app, exactly as they would with the `place` lines deleted, and `dagpane check`
        says so. Flipping it is one line — `AppSession::resume_side` instead of `resume` in
        `crates/serve/src/lib.rs` — and it is deliberately not flipped, because a browser that
        cannot consume a frontier would render a placed app with its page-side panes simply
        missing.
* **A worker.** `send` is synchronous: the call *is* the pass, and it blocks the frame. At
  0.1–0.9 ms that is invisible; at the 100 000-row end of the row sweep it is not. Moving the
  module into a Web Worker is the fix and it is not written, because nothing measured yet
  needs it.
* **Anything a server was doing.** An exported app has no front door, no scheduled refresh, no
  HTTP or SQL sources and no eviction, because it has no server. `dagpane export` refuses a
  source it cannot inline rather than writing a bundle that breaks once opened.

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

Seven verbs was not an authoring surface. Evidence proved SQL + Markdown is one; Mosaic
proved params and selections are one. The eighth and ninth have shipped — a one-row
expression language and a join — and the gap is narrower, not closed: past it the author is
still writing a Rust closure, which is item 5, which is the item that makes the audience
worse. What is left on this list is the part that was always going to be hard.

In rough order of how much each adds against how much it risks the edge invariant:

- [x] **Derived columns and an expression sub-language.** `derive = { name = "margin",
      expr = "revenue - cost" }`, one row wide, over a hand-written parser in
      `dagpane-core/src/expr.rs`. The edge question was the whole design and it is answered
      lexically: **a bare name is a column and a cell reference is written `$name`**, so the
      edge set of an expression is the set of `$` tokens the lexer produced and the compiler
      reads it for nothing else. An over-declared edge is still the correct failure — an
      expression that names a control inside an `if` branch it will not take declares that
      control — and the digest short-circuit contains it, exactly as this item predicted.
      What shipped with it, because the schema had to be threaded through the pipeline to
      type-check an expression at all: `dagpane check` now names a bad column reference in
      *every* verb, before the app runs.
- [x] **Joins.** `join = { with = "accounts", left_on = "account_id", right_on =
      "account", how = "left" }`, with `inner`, `left`, `semi` and `anti`. This item was
      right that the edges were not the work — `with` is a cell name resolved by the same
      `bind_param` a filter's `param` uses — and right that the risk was in the semantics.
      Each of those risks is a loud failure rather than a quiet one: **a duplicated key on
      the right is refused** unless `multiple = true` (the default makes a join a lookup, so
      a total cannot double because a dimension table gained a row), a key of two different
      types is refused by `dagpane check`, a column name on both sides is refused unless
      `suffix` distinguishes them, and a null key matches nothing.
      `transform::join_schema` is one function with two callers — the checker and the
      transform — which is what stops a join that `check` passes from being one the run time
      refuses. `semi` and `anti` return `left.take_rows(…)` and allocate nothing but the hash
      map. `right` is `left` with the cells swapped; `full` is not built, and its one real
      decision is named in `How`'s documentation rather than guessed at.
- [ ] **Expressions in a filter predicate.** `derive` then `filter` already says everything
      a predicate expression would, at the cost of a named column. Worth doing only if
      authors are visibly paying that cost.
- [x] **SQL cells.** `sql = "select region, sum(amount) as revenue from sales where amount
      >= :min_amount group by region"` as a whole cell. ADR-0005 is the record; the two
      decisions are below.

**Why SQL was last, and what it cost to do it anyway.** The manifest's job is to produce
*edges*. Extracting the dependencies of a SQL statement without a real SQL parser means
matching table and column names out of text with a regular expression, and a regular
expression over SQL is wrong eventually — against a CTE, a subquery alias, a quoted
identifier, a comment containing a table name. A missed edge is a cell that does not
recompute when it should, which is a stale number on a page that looks like it is working:
the one failure mode this project exists to not have. **A wrong edge is a wrong app.**

The gate this item set was "a parser that can be shown to reject what it cannot resolve,
rather than guess", and it is met by **closing the grammar**: the parser accepts only the
statements that lower and refuses everything else by name, so the property is a consequence
of the grammar rather than a blocklist somebody has to keep complete. That is also why it
did **not** take the dependency this item anticipated. A third-party parser accepts all of
SQL and would have inverted the arrangement — a blocklist of features this runtime cannot
lower, which is never complete, which is the same "wrong eventually" one level up. The cost
is stated in ADR-0005 rather than hidden: this is not SQL, it is a dialect that fits on a
page.

The second decision is the one that keeps it honest. **A statement lowers to the nine verbs
and there is no second evaluator.** `where` becomes `filter`, `group by` becomes `group_by`,
a select-list expression becomes `derive`, and by the time anything runs there is no SQL
left — so nulls, type rules, division by zero and the join's duplicate-key refusal are the
ones already written down, and cannot acquire an exception. `crates/app/tests/sql.rs` holds
eight questions written once as SQL and once as steps, asserted to render identically.

The edges come out of the parse tree, resolved through the statement's aliases: a table name
inside a comment or a string literal is not an edge, and both cases are tested. A control is
`:name`, explicit, on the same terms as `derive`'s `$name`.

---

## 7. apps-per-core, and the concurrency numbers — measured

**Measured.** This header has been wrong twice, and both corrections are worth keeping because
the pattern is the same each time: the section's prose outlived the work.

It first said *"there is no benchmark in this repository and no number in any document here
measured in seconds"*, which stopped being true when `BENCHMARKS.md` gained the row sweep and
the round trip. It then said **"nothing here measures what it costs to host many apps or serve
many viewers"**, and kept saying it after `benches/fleet/` measured the memory, `benches/loadgen/`
measured apps-per-core on a pinned core, and both measured p99 under load. All five boxes below
are now ticked; the per-session figure closed the fourth, and what a session costs **while it is
being used** — memory that never comes back, and the wait it imposes on viewers doing nothing —
closed the fifth.

The lesson is cheap to state and was not free to learn: **a roadmap item is not done when the
work lands, it is done when the file that claims it is undone is edited.** Two rounds of work
were described here as unbuilt while the numbers sat in `BENCHMARKS.md`.

**And it was learned again, one round later, in files that were open at the time.** The round
that ticked the first box below left four other documents still calling that figure unmeasured,
and left `POSITIONING.md`'s score line — along with every other place that totals it up, this
file's own opening included — reading *zero of four fully met* after the verdict above it had
changed. All corrected. The habit that catches it costs nothing: **when a measurement lands or a
verdict moves, grep the tree for the word that used to be true.**

The dossier's argument is that the buyer is the platform team hosting hundreds of internal
apps and paying for a sticky-session-pinned, vertically-scaled container each —
`marimo-team/marimo#1831`, *"Support stateless for multi container scaling and
deployment"*, open since 2024-07-19 with no visible maintainer response, is the artifact of
that pain. The unit of value is then cost and blast radius per hosted app, in apps-per-core
and p99 under N concurrent viewers, **which is a number no incumbent publishes and this
project could win.**

The structure points the right way: the app, the graph and every loaded source are one
`Arc<App>` behind every connection; `Session` is `Send` and `Sync`, and every mutating entry
point takes `&mut self`, so the borrow checker is what stops a pass interleaving with another
pass or with a read.

This paragraph used to end *"none of that is a measurement"*, and singled out `Arc::ptr_eq` on
an untouched source as **asserted by a test rather than inferred from an RSS reading**. That is
now inferred from an RSS reading too, and the reading changed what the structure was taken to
mean: the sources are shared, and the frames a session computes are not. See the first box
below.

- [x] **Resident memory per additional session over one `Arc<App>`.** `benches/sessions/`.
      **179 kB** for the bundled example — and the number on its own is the least interesting
      part. The same pipeline over 200 000 rows costs **7 248 kB** a session while an
      *aggregating* pipeline over the same rows costs **145 kB**: 50× apart on identical data.
      So a session costs what its pipeline **materialises**, not what its app **sources**. The
      sources are shared — the 145 kB is the proof, where `Arc::ptr_eq` was the assertion — and
      the computed frames are per viewer, as they must be, since two filters give two answers.
      This section's own sentence about `Arc::ptr_eq` standing in for an RSS reading is what
      the rig was built to retire.
- [x] **p99 interaction latency under N concurrent viewers.** Two rigs, because the first one
      was not good enough and said so: `benches/fleet/` drives real browsers and **saturates
      before dagpane does**, so its dagpane rows are a lower bound and are marked as one;
      `benches/loadgen/` is protocol-level with a 67 µs floor and does not.
- [x] **apps-per-core against a Streamlit or marimo baseline.** **224 apps on one core**, 448
      sessions, 1 908 interactions/s inside a 250 ms p99 budget, four independent runs. The
      baselines come by service demand rather than by saturation, and `BENCHMARKS.md` says why
      that route is allowed and where it would not be.
- [x] **The caveat about 400 apps being 400 processes.** Stated, and then answered: it is true
      of `run` and `serve`, and `dagpane host` is why it is a choice. The number that replaced
      the word *cheaper* is **0.25 MB of marginal PSS per app**, against 94.8 MB for Streamlit
      and 106.6 MB for marimo.

- [x] **A busy session, as opposed to a held one.** `benches/sessions/busy.sh`. A pass allocates
      and **the process does not give the pages back** — the burst's peak becomes the floor, at
      every rung of all three ramps. Not a leak: the identical burst fired again costs about a
      tenth, and a window ten times longer raises the ceiling by about a quarter rather than by
      ten times, so the charge lands **once per viewer who has ever touched a control**. Per
      such viewer: **≈ 12 kB** on the bundled example, **≈ 1 449 kB** on the 200 000-row
      row-keeping pipeline, and on the aggregating one a slope whose sign changes between runs —
      so an app holding 200 000 rows adds nothing measurable, provided nothing downstream of its
      source keeps a row. And the half nobody was counting: a pass occupies a tokio worker for
      its duration, so **a viewer who is doing nothing waits about (busy viewers ÷ worker
      threads) passes** — a rule three apps spanning more than 300× in pass cost agree on, and
      one that holds when the denominator is moved with `taskset` rather than only the
      numerator. That last one had been reasoned from
      `crates/serve` for two rounds and was never a number.

      **One correction is worth carrying here rather than only in the changelog**, because it is
      the kind this section exists to catch. The bystander column mixed censored probes — those
      still owed an answer when the window shut, recorded at the clock time it shut — into the
      same median as completed ones. At the worst rung that turned 2 248.5 ms into 431.6 ms and
      produced a published row where the wait *fell* as the load doubled. The published diagnosis
      blamed the window; the window was not the problem. Over completed probes the three-second
      ramp agrees with the rule at every rung of all three apps. The aggregation was the error,
      and the wider window's real purchase is sample count.

      **What the bystander measurement does *not* support**, since a first draft of this box
      claimed it did: anything about whether that wait can reach the 25 s keepalive a hosted
      deployment leans on. At the top rung the bystander gets two answers in a three-second
      window, and a wait longer than the window is unobservable by construction — recorded
      censored and reported as a lower bound. A three-second aperture cannot return a number
      above about three seconds however bad the truth is, so it could not have falsified the
      claim it was cited for. **`busy.sh` now carries a rung with a thirty-second aperture and it
      answers the question properly**: of **75 of 81** probes answered inside it across two bursts
      and three processes the median is about 2 343 and 2 285 ms and the longest **4 905 ms** —
      roughly 5.1× under the keepalive on the worst single observation. The other six were cut
      short at window close, bounded below by their truncation and above by nothing, so the wait
      did not grow to fill the wider window *as far as the answered probes can say*. So **this
      load does not starve the keepalive**, which is a narrower claim than the one withdrawn and
      is the one the evidence carries. The rule remains the worrying half: the
      wait is pass × viewers ÷ workers, so a slower app or a smaller host walks toward that
      interval with nothing else changing.

**What is still not measured**, since the point of this section is to say so:

- [ ] **`--budget-mb` against the term that turned out to matter.** It counts **source bytes**,
      and the per-session figure above is not in it. A fleet of row-keeping apps with many
      viewers is sized against the smaller of its two costs, and nothing in the process notices.
      Making the budget see materialised frames is a design question, not a measurement: a
      frame's size is knowable, but charging it to a session makes eviction a decision about
      viewers rather than about apps, and that is a different policy from the one `crates/host`
      implements.
- [ ] **A host whose neighbours are known.** Every figure above is from a shared cloud
      container. The ratios survive that; the absolutes are for ordering decisions and not for a
      cost model, and `BENCHMARKS.md` repeats that beside each table rather than once.
- [ ] **The allocator the container actually ships — spot-checked, and the answer is that these
      figures do not transfer.** Every number above is a glibc build; the `Dockerfile` produces a
      static **musl** binary on `scratch`. One app, two repetitions, four rungs, so a spot check
      and not a result — and enough. On musl the retained memory is **negative**: the process
      ends a burst 15 MB *smaller* than it started it at four dragging viewers, and 235 MB
      smaller at sixty-four, where glibc keeps everything it took. And musl's pass is **1.7×
      slower with one viewer working and more than fourteen times slower with four** — the
      session count is held at sixty-five across both, so concurrency is the only thing that
      moved, which is what distinguishes a slower allocator from one that serialises. This
      runtime allocates heavily per pass and mallocng serialises where glibc's per-thread
      arenas do not. What stays open is the whole ramp on the shipped build,
      and the decision nobody has taken: **every published figure in `BENCHMARKS.md` is glibc's
      and the container is not.**
- [ ] **What a busy app does to its neighbours in one `dagpane host`.** Everything above is one
      process holding one app. The bystander rule says an idle viewer inherits the pass cost of
      the busiest viewer on the process; `host` puts *different* apps on that process, and
      nothing measures what one does to another. Nothing isolates their compute either, which is
      the design half of the same gap.

**`dagpane serve` ticked none of the boxes above, and it is worth saying which axis it moved.** It makes *one* app horizontally scalable on purpose rather than by accident — probes,
a drain that goes unready before it stops accepting, `SIGTERM`, declared origins, and a
manifest digest a rollout can compare across replicas. That is the **blast radius** half of the
dossier's claim: a replica can be added, replaced or removed without a viewer noticing, and a
half-finished rollout is now visible instead of silent.

The **cost** half is unchanged by it. Replicas of one app still each hold their own copy of that
app's sources, so N replicas is N × the data; `dagpane host` is the thing that amortises, and it
amortises across *apps* in one process rather than across replicas. That is now a measured
statement rather than a structural one — see the ticked boxes above — but `serve` is not what
measured it.

**Trigger: before any *general* performance claim appears in the README.** That is the whole
rule, and the word `general` is load-bearing since `BENCHMARKS.md` exists. The README says
*"No general performance claim"* and lists the counts as counts; a dated figure that names its
app, its machine and the command that produced it is allowed, and the first sentence that
implies speed, density or cost **as a property of this runtime** is blocked on this section,
not on review.

---

## What is deliberately not on this list

* **A notebook mode.** An editing surface is a different product with a different failure
  mode, and marimo already ships the good version of it.
* **A plugin system.** Eight crates and one dependency direction that CI greps for is the
  asset here; an extension point is a second public API to keep stable before the first one
  has a second user.
* **A hosted cloud.** The two things that made this impossible have both shipped — a front
  door (item 1) and many apps in one process (`crates/host`) — so what is left is running the
  fleet, and that is not a change to this repository.
