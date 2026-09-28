# Benchmarks

Every other number this project publishes is a **count**. This file is the exception, and
it exists to close one specific item: `ROADMAP.md` §2 asks for "the row count and column
width at which the current `Table` stops being the right answer" before the value model
can be reconsidered. Here is that measurement.

Reproduce it with the harness that produced it:

```sh
cargo build --release -p dagpane-cli
./benches/rows.sh
```

## The sweep

The bundled `examples/sales.toml` — eleven cells, seven panes, unchanged — over the same
six columns, with only the row count varying. **First render** is a cold `dagpane explain`:
read the CSV, infer column types, build the graph, compute every cell. **+1 interaction**
is the same with `--set min_amount=400`, so it is the whole cost of moving one control.

Median of three runs, on one x86-64 Linux machine, release profile:

| Rows | CSV | First render | +1 interaction | Peak RSS |
|---:|---:|---:|---:|---:|
| 600 | 0.04 MB | 7 ms | 8 ms | 10 MB |
| 10 000 | 0.4 MB | 55 ms | 67 ms | 10 MB |
| 100 000 | 3.7 MB | 1 000 ms | 768 ms | 67 MB |
| 1 000 000 | 37.8 MB | 8 471 ms | 10 216 ms | 637 MB |

The RSS column came from a separate run under GNU `time`; `benches/rows.sh` reports it
directly where `/usr/bin/time` is available and prints `-` where it is not, rather than
estimating.

## What it says

**The interactive ceiling is around 100 000 rows.** Below ten thousand every interaction
is comfortably inside a frame budget. At a hundred thousand a control move costs most of a
second, which is felt but usable. At a million it is ten seconds, which is not a dashboard.

**Reactivity does not rescue the expensive cell.** The interaction column tracks the first
render instead of falling away from it, and that is not a defect — it is what the claim
actually says. A pass recomputes the cells the interaction reaches and nothing else, so it
skips the cells that do not depend on `min_amount`. But `filtered` *does* depend on it, and
filtering a million rows costs a million rows of work however few other cells run. The
saving is in the cells you avoid, never in the one you cannot.

**Memory amplifies about seventeen times.** A 37.8 MB file becomes 637 MB resident. The
ratio settles as the row count grows — 25× at ten thousand rows, 18× at a hundred thousand,
17× at a million — so it is the per-row representation, not a fixed overhead. `ARCHITECTURE.md`
§6 and `ROADMAP.md` §2 both predict exactly this: a column is `Vec<Option<T>>`, an `i64`
costs sixteen bytes, and every text cell is an owned `String`. Two of these six columns are
four-value categoricals, so a million-row file holds two million `String`s that a dictionary
encoding would store as two million small integers and eight distinct values.

## Which engine can go behind the `Frame` seam

`ROADMAP.md` §2 has to choose a representation, and §4 cares what that choice costs a
browser. Both were open on the same missing check. Reproduce with:

```sh
rustup target add wasm32-unknown-unknown
./benches/wasm-engines/run.sh
```

Release profile, `opt-level = "z"`, LTO, stripped, `panic = "abort"` — the profile that
would actually ship. Measured 2026-09-08.

| candidate | builds for wasm32 | raw | gzipped |
|---|---|---:|---:|
| **`dagpane-wasm` — the whole engine, measured 2026-09-18** | **yes** | **1.10 MiB** | **318 KiB** |
| `arrow-array` + `-schema` + `-select` + `-ord` | yes | 1.6 MiB | **259 KiB** |
| the same, plus `parquet` | yes | 1.7 MiB | 297 KiB |
| Polars 0.44 (`lazy`, `parquet`) | yes | 6.3 MiB | 1.4 MiB |
| Polars 0.44 (`lazy`) | yes | 8.9 MiB | 1.7 MiB |
| DataFusion 54.1 | yes | 27 MiB | **5.9 MiB** |

**Everything builds.** The note this replaces said Polars did not, citing two upstream
issues; they are from 2024 and nobody had re-checked. What actually stops a naive build is
neither library — it is `getrandom` 0.3 declining to pick an entropy backend for a target
with no OS to ask. That is one line of `.cargo/config.toml`, it applies to every candidate
equally, and it has a consequence worth stating: it routes randomness through the host's
`crypto.getRandomValues`, so the artifact then needs a **JavaScript host**. A pure wasm
runtime would want a different backend.

The two Polars rows are **not** evidence that `parquet` makes it smaller. They link
different APIs — a lazy filter in one, the Parquet writer in the other — so they measure
different sets of reachable code. Read them as "both are between 6 and 9 MiB raw", not as a
comparison with each other.

So availability is not the question; size is, and it spans twenty-fold. Against §4's figures
for shipping a query engine into a browser (DuckDB-Wasm at 34.25 MB), DataFusion's 27 MiB is
the same order — that cost is real and paying it should be a decision, not a detail.

### What dagpane's own browser build weighs

The first row above is not a candidate for the `Frame` seam; it is the artefact this project
now ships. `dagpane-wasm` is the reactive engine, the nine verbs, the expression language, the
manifest compiler and the CSV reader, built for wasm32 under the same profile as everything
else in the table and with the Arrow backend **off** — see that crate's `Cargo.toml` for why
that is a size knob and not a behaviour one.

**1.10 MiB raw, 318 KiB gzipped.** Thirty-one times smaller raw than DuckDB-Wasm, nineteen
times smaller gzipped than DataFusion. `ROADMAP.md` §4 says the payload argument is false for
anything shipping a query engine and true only if the client stays a thin evaluator; this is
that claim measured rather than asserted.

Roughly a third of it is the TOML parser, which is there on purpose: the browser compiles the
**same manifest** the server compiles, through the same `compile_with`, so a misspelt column
is the same sentence in both places. Shipping a pre-compiled graph instead would be a second
representation of an app to keep in step with the first.

Reproduce with `./crates/wasm/tests/run.sh`, which prints both figures before it runs the
module.

---

## Is an interaction's latency the wire, or the pass?

`ROADMAP.md` §4 would not open client-side compute on "WASM would be interesting". Its trigger
is a measurement — *an interaction whose latency is dominated by the round trip, measured on a
real app* — and it says that if the round trip is not the cost, the item stays shut. Here is
that measurement, and it does not say what the item's framing assumed.

```sh
cargo build --release -p dagpane-cli
cargo build -p dagpane-wasm --target wasm32-unknown-unknown --release
node benches/roundtrip/roundtrip.mjs examples/sales.toml min_amount=400 300
```

300 interactions per app, medians, one x86-64 Linux machine, **loopback** — no DNS, no TLS,
no distance. Measured 2026-09-18.

| app | round trip | server pass | the wire | wasm pass | wasm vs native | break-even |
|---|---:|---:|---:|---:|---:|---:|
| `sales.toml` (11 cells, 600 rows) | 0.153 ms | 0.052 ms | 0.101 ms (66%) | 0.130 ms | 2.5× | **0.08 ms** |
| `15-unit-economics` (18 cells) | 0.165 ms | 0.085 ms | 0.080 ms (48%) | 0.235 ms | 2.8× | **0.15 ms** |
| `19-wide-telemetry` (122 columns) | 0.751 ms | 0.404 ms | 0.347 ms (46%) | 0.892 ms | 2.2× | **0.49 ms** |

**The wire is about half of every interaction**, on the friendliest wire that exists. That is
§4's trigger, met.

**And wasm is 2.2–2.8× slower at the identical pass.** So the "browser vs server" column that
an enthusiastic version of this table would print reads **1.18×, 0.70× and 0.84×** — the
browser is *slower* on two of the three apps. On loopback, moving an app into a browser is a
coin flip.

**The number that transfers off this machine is the break-even, and it is a threshold on the
WIRE rather than on the round trip.** A browser wins when `wasm pass < round trip`, and
`round trip = wire + server pass`, so it wins when `wire > wasm pass − server pass`. That
difference is the break-even column above: **0.08–0.49 ms of wire overhead**, independent of
the network. Stated on the round trip instead, the same threshold is `wasm pass` itself —
0.13–0.89 ms.

Quoting one as the other understates what the browser needs by roughly threefold, and an
earlier version of this table did exactly that. Loopback wire overhead here is 0.08–0.35 ms,
which is why the comparison is so close.

**Those thresholds belong to these three apps on this machine, and to nothing else.** A
break-even is `wasm pass − server pass`, so it is a property of the app's pass — a heavier
pipeline has a higher one, and there is no measurement here that bounds how much higher. What
transfers off this machine is the *method*, not the number: run `benches/roundtrip/` against
your app to get yours.

For scale rather than as a floor, typical round trips are ~0.5 ms within a datacentre, ~5 ms
within a city and 30–100 ms across a continent. Three of those comfortably exceed 0.49 ms and
one of them does not, which is the honest shape of it: **a split is worth measuring, not
assuming.**

So the item opens, and **not for the reason it was written for.** The browser does not win
because wasm is fast; it is measurably slower. It wins because a network is slower still, and
the honest way to say that is: *client-side compute buys you the wire, and charges you about
2.5× on the pass to do it.* On an app whose pass is already most of a frame budget — the
100 000-row region of the row sweep above — that trade is a bad one and this table is how to
tell.

### And Arrow alone is not the memory fix

One 1M-row column of a four-value categorical, which is exactly what `region` and `channel`
are in the bundled example (`./benches/wasm-engines/run.sh` prints this too):

| representation | resident |
|---|---:|
| `Vec<Option<String>>` — today | 27.2 MB |
| `arrow::StringArray` | 11.8 MB |
| `arrow::DictionaryArray` | 3.8 MB |

Adopting Arrow and keeping strings as `StringArray` recovers 2.3×. The 7.1× needs dictionary
encoding, which is a modelling decision on top of the dependency rather than something that
arrives with it. Whichever engine is chosen, the sweep above says the amplification is per-row
representation — so this is the row that has to change.

### On the whole example, not one column

`crates/frame-arrow` is that decision made. Its per-column heuristic encodes a text column
only when the distinct count pays for it, and on the bundled example's six columns:

```sh
cargo run --release -p dagpane-frame-arrow --example memory 1000000
```

| representation | column bytes |
|---|---:|
| `Vec<Option<T>>` — today | 139.2 MB |
| Arrow, plain strings | 66.3 MB |
| Arrow, encoding chosen per column | **34.3 MB** |

4.1× on the source columns, and it lands where you would expect: `day`, `region` and
`channel` encode (28, 4 and 4 distinct values), `order_id` and the numerics do not.

Read that against the row sweep with care — **they measure different things.** The 637 MB in
the sweep is process RSS for a whole session, including every derived cell the graph
materialises; the figures above are the source columns alone.

### What the process actually does

Sources are Arrow-backed by default now (`arrow-sources`, on unless you turn it off), so the
end-to-end figure can be measured rather than extrapolated. The same `explain --set` on the
same generated CSVs, once per representation:

| Rows | Lean RSS | Arrow RSS | Saved | Lean | Arrow |
|---:|---:|---:|---:|---:|---:|
| 100 000 | 70 MB | 43 MB | **39%** | 1 204 ms | 941 ms |
| 1 000 000 | 660 MB | 393 MB | **41%** | 12 994 ms | 10 628 ms |

**41%, not 4.1×**, and the gap between those two numbers is the honest part. The source is
Arrow-backed; the *derived* frames largely are not. `filter`, `sort` and `limit` return
whatever their input was, so a chain of those stays Arrow — but `group_by` aggregates into a
`Table`, and several of the bundled app's eleven cells are group-bys. The 4.1× is what one
representation costs; the 41% is what a whole session costs when only part of it has moved.

Running a little faster (about 18% at a million rows) was not the goal and is not claimed as
one — it is what falling out of `Vec<Option<String>>` construction happens to do here.

### Where the remaining 393 MB actually is — not where it looked

`group_by` now builds its output through the seam too, so no verb drops the representation
on the floor and a chain stays Arrow end to end (`crates/frame-arrow/tests/oracle.rs` asserts
it verb by verb). **It changed the resident figure by nothing at all** — 393 MB before and
after, at both sizes. That is not a failure of the change; it is what the bundled app is.
Its group-bys collapse a million rows into four regions and a handful of channels, and the
representation of a four-row frame is noise beside a million-row source. The change matters
for an app that groups by something high-cardinality; this one does not.

Chasing why led somewhere more useful. Peak RSS at a million rows, same binary:

| command | what it does | peak |
|---|---|---:|
| `check` | parse the CSV, build the graph, compute **nothing** | 393 MB |
| `graph` | the same, plus print the DAG | 393 MB |
| `explain --set` | parse, first render, one interaction | 393 MB |
| — | the source columns alone, measured directly | **34 MB** |

Every one of those is the same number, and the engine is not in it. **The whole remaining
peak is the CSV load.** `parse_columns` accumulates every raw field into a
`Vec<Vec<String>>` before inferring a type for any column, so a 1M × 6 file holds six
million live `String`s at once — and the columns those become occupy 34 MB. The reader, not
the representation, is what a large source costs today.

### The reader, streamed

Done. Inference genuinely has to see every value before naming a column's type, but it does
not need the *values* — only three booleans per column (does everything parse as an integer,
a float, a bool), and those fold one row at a time. So the reader makes two passes over the
text: the first carries those flags and discards every field, the second parses straight into
the typed column the first decided on. Peak becomes the output plus one row.

| Rows | Lean | Arrow, buffered reader | Arrow, streamed reader |
|---:|---:|---:|---:|
| 100 000 | 70 MB | 43 MB | **34 MB** |
| 1 000 000 | 661 MB | 393 MB | **295 MB** |

**661 MB to 295 MB, 55%**, and it runs *faster* despite parsing twice — 11.0 s to 9.6 s at a
million rows, because not allocating six million `String`s outweighs a second scan of text
that is already in memory.

The old buffered inference is kept as `#[cfg(test)] fn infer` and the new reader is checked
against it over a generated corpus, because type inference decides what every published count
is a count *of*. Two of those checks failed first time and both were the test's fault, not the
reader's: `split_line` trims, so feeding raw text to the old path compared inference against
inference-plus-splitter; and `NaN != NaN` made two identical float columns compare unequal.

### What is left, and where it is

`check` — load the file, build the graph, compute nothing — is 253 MB of the 295 MB. Loading
still dominates, and the arithmetic says why:

| | |
|---|---:|
| file text held by `read_to_string` | 38 MB |
| the Arrow columns it produces | 34 MB |
| `check` peak | **253 MB** |

The ~180 MB between them is the intermediate. `parse_columns` builds core's
`Vec<Option<T>>` columns and `ArrowFrame::from_columns` converts them, so both
representations are live at the moment of conversion — and the `Vec<Option<T>>` form of these
six columns measures 139 MB. It is not allocator retention: `MALLOC_MMAP_THRESHOLD_`,
`MALLOC_ARENA_MAX` and `MALLOC_TRIM_THRESHOLD_` move it by a megabyte between them.

So the next reduction is building the backend's arrays directly from the parse rather than
converting into them, which needs a builder on the seam — "give me somewhere to put a column
of this type" — so that `csv.rs` does not have to name a backend to avoid the copy. That is
a change to the trait, not to the reader, which is the opposite of where this section
started.

### The builder on the seam

So the reader now fills the backend directly. `FrameBuilder` is "give me somewhere to put a
column of this type": `begin_column(name, type, hint)` once per column, a `push_*` per row,
`finish()`. Text is pushed **borrowed** — that is the whole method, since an implementation
storing bytes contiguously copies into its own buffer and never allocates a `String` per
cell. `csv.rs` names no backend; `TableBuilder` is the reference implementation and
`ArrowFrameBuilder` the one that matters.

`ColumnHint` carries what the inference pass already learned — row count, and distinct count
when it stayed under the cap — because a streaming builder must commit to a physical layout
before seeing the data while the pass that chose the column's type has seen all of it. It is
advisory: `TableBuilder` ignores all of it and must produce identical content, which is what
the differential test asserts.

It did what it was built to do, and the number it did it to is not the headline one. Measured
back to back on one machine, same binary flags, 1M rows:

| | streamed reader | + builder seam |
|---|---:|---:|
| `check` — load, build the graph, compute nothing | 252 MB | **76 MB** |
| `explain`, group-bys removed from the manifest | 252 MB | **119 MB** |
| `explain`, the bundled app as published | 294 MB | **294 MB** |

The first row is the change, and it lands where §"What is left" predicted: 38 MB of text plus
34 MB of columns plus slack, with the intermediate gone. At 100 000 rows `check` goes 28 MB to
11 MB, the same shape.

The third row is the one to read twice. **The peak a user of this app actually pays did not
move at all.** `group_by` materialises its *inputs* — `column_from_frame` pulls every input
column into an owned `ColumnData`, `Option<String>` and all — so it rebuilds exactly the
representation the loader just stopped building, and it rebuilds it from a source the loader
now holds cheaply. The cost is absolute rather than additive: it is about what the old
loader intermediate cost, however cheaply the frame arrived. Take the group-bys out and the
win shows through undiminished, 252 MB to 119 MB.

Timings did not move either — 7.7 s against 7.9 s at a million rows, which is noise. This
change is a memory change and nothing else.

Two things follow. The first is that the same mistake was in the codebase twice, in two
files, and fixing the copy in the loader revealed the copy in the verb rather than removing
it; a peak is a maximum, so the second-largest cost is invisible until the largest one goes.
The second is that a benchmark reporting only the headline figure would have scored this
change at zero and it would have been reverted — the `check` column and the no-group-by
variant are what make it legible, and neither is the number anybody asks for.

The next target is therefore named precisely: `group_by` should read its inputs through the
trait rather than materialising whole columns, which is a change to `transform.rs` and needs
no new trait method — `value_at`, `is_null` and `compare_in_column` are already there.

One note on how this was nearly not measured at all. The first version of the builder test
compared a hinted build against an unhinted one through a helper that rebuilt the frame via
`ArrowFrame::from_columns` — which re-runs the encoding heuristic, so both sides came out
byte-identical and the test passed while exercising none of the new path. It is the failure
mode that matters most in a differential suite: a green test asserting nothing. Fixed by
having the builder expose `finish_arrow()` and testing the real build; the row count then had
to go from 600 to 20 000, because at 600 rows the fixed per-array overhead is larger than
anything the encoding decision changes.

### The verb, through the trait

`group_by` read its inputs by pulling every input column into an owned `ColumnData` first —
`Option<String>` per text cell and all. It now reads them one cell at a time through
`Frame::value_at`, `column_type` and nothing else, and materialises none of them. The output
still goes back through `same_kind`, so a chain over an Arrow frame stays Arrow.

| 1M rows | before | after |
|---|---:|---:|
| the bundled app | 294 MB · 7.9 s | **119 MB · 6.5 s** |
| group by a text key, sum a float | 290 MB · 4.5 s | **84 MB · 3.9 s** |
| group by a text key, count only | 288 MB · 4.2 s | **84 MB · 3.9 s** |
| no grouping columns, sum a float | 294 MB · 4.1 s | **85 MB · 3.8 s** |
| the same app with the group-bys removed | 119 MB | 119 MB |

**The app's peak is now exactly the peak of the app without any group-bys at all** — 119 MB
against 119 MB. The verb costs nothing above the rest of the pipeline, which is what "reads
its inputs" is supposed to mean. At 100 000 rows the app goes 33 MB to 16 MB.

Two things about that table are worth more than the headline.

Those two figures include one further megabyte or three from folding each numeric group
instead of collecting it — review pointed out that a `Vec<f64>` per group is a per-row buffer
of exactly the kind this change removed, and for a whole-table aggregate, which is one group,
it is one `f64` per input row. It shows where that buffer was the largest live thing (88 to
85 MB) and nowhere else, the app's own peak having moved to the sort in `top_orders`. Peaks
are maxima; that keeps being the lesson.

The third row is the one that had been quietly absurd: a whole-table `sum` over one float
column cost 294 MB, because the materialisation loop ran over `0..table.width()` and copied
all six columns of the source to read one of them. It was the most expensive shape in the app
and it needed the least data.

And it is **faster**, everywhere, by about 6–13%. That was not the expectation — reading a
text cell through `value_at` allocates a `String` per call, so the honest prediction was a
small regression bought with a large memory win. It went the other way, and the reason is the
same one the streamed reader found: the old path made *exactly the same allocations* and then
kept every one of them alive, so the copy was pure cost. Not allocating six million strings
outweighs any per-call dispatch, twice now.

### The whole sequence

| 1M rows, the bundled app | peak RSS | |
|---|---:|---|
| lean — `Vec<Option<T>>` throughout | 660 MB | where this started |
| Arrow-backed sources | 393 MB | dictionary encoding is 7.1× of it; Arrow alone is 2.3× |
| streamed CSV reader | 295 MB | two passes, no `Vec<Vec<String>>`, and faster |
| a builder on the seam | 294 MB | load 252 → 76 MB; the app's peak did not move |
| `group_by` through the trait | **119 MB** | the peak the builder had already earned |

**82% off, and every step of it measured on the shipped binary through the shipped manifest.**

The fourth row is the one to keep. It looked like a failure — a change that moved the headline
figure by one megabyte — and it was the change that made the fifth row possible: the load cost
had to go before the verb's cost was visible at all, because a peak is a maximum. Judged on the
number anybody asks for, it should have been reverted.

## What one more viewer costs — and the claim it narrows

`ROADMAP.md` §7's first item, and the last of its four to be answered. The two rigs below weigh
an **app**; this one weighs a **session**, which §7 named as the gap in those words:

> `Arc::ptr_eq` on an untouched source across two sessions is asserted by a test rather than
> inferred from an RSS reading.

```sh
./benches/sessions/run.sh
```

`benches/sessions/README.md` is the method. One `dagpane run`, sessions opened and left idle,
ramped 0 → 512, and the answer is the least-squares slope rather than a subtraction between two
rungs — RSS grows in allocator steps and does not come back down.

### The experiment

Three ramps, and **the comparison is the result**: the bundled 600-row app; the same pipeline
over 200 000 rows; and the same 200 000 rows with a pipeline that keeps no rows at all. Same
data in the last two, same process, same sockets — the only difference is whether anything
downstream of the source holds onto rows.

| app | source | per session | ratio to source |
|---|---:|---:|---:|
| 600 rows, keeps rows | 0.02 MB | 179 kB | 8.46× |
| 200 000 rows, keeps rows | 7.6 MB | **7 248 kB** | 0.935× |
| 200 000 rows, aggregates only | 7.6 MB | **145 kB** | 0.019× |

Rows two and three are **50× apart on identical data**.

### What it settles, in both directions

**The sources are shared, and this is the first measurement that says so** rather than the
pointer-identity test that has stood in for it. If a session copied them, row three would cost
about 7.6 MB. It costs 145 kB.

**The derived cells are not shared, and the docs were quiet about it.** This project has said,
in nine places, that *a hundred viewers of a 600-row app are a hundred slot vectors over one
table, not a hundred copies of it*. Every word of that is true and it is read as a statement
about the total, which it is not: the slot vector holds **computed frames**, a cell that keeps
rows holds about a table's worth, and each viewer has their own because each viewer filters
differently. Even on the bundled example a session costs **8.5× the CSV it is sharing**.

That is not a bug and there is nothing to fix in it — two viewers with two filters must have two
answers. It is a cost that was never priced, and the sentence above has been narrowed everywhere
it appears to say what is shared rather than to imply what is not.

**≈145 kB is the floor**: a socket's buffers and a slot vector with nothing materialised behind
them. The 600-row app's 179 kB is that floor plus its frames.

### What it does not say

One machine, shared and cloud — the ratios survive that and the absolutes are for ordering
decisions. Two pipeline shapes, chosen as the extremes; a real app sits between them. **Idle
sessions only** — an interaction's transient allocation is excluded, and it is not zero: the
next section is what it costs.

### The consequence an operator meets

**`dagpane host --budget-mb` counts source bytes, and none of the above is in it.** A fleet
sized on source footprint is sized against the term this shows can be the smaller one. See
`OPERATIONS.md`.

---

---

## What a viewer costs while they are *using* it

`ROADMAP.md` §7's last open box, and the exclusion the section above wrote into its own method.
"Not zero" is not something an operator can size against.

```sh
./benches/sessions/busy.sh
```

`benches/sessions/README.md` is the method. Sixty-four viewers held open so the held cost is in
the baseline and out of the answer, a ramp in how many of them drag a slider as fast as the
server will answer, a **fresh process per rung**, the same burst fired **twice**, and **three
processes a rung with the median taken**. All three of those are corrections to earlier versions
of the rig, and each is a finding about the thing being measured rather than tidying.

**The peak is not sampled.** A pass over the bundled example takes about a third of a
millisecond; reading RSS every 50 ms observes an interaction costing nothing about a hundred
and fifty times in a hundred and fifty-one. `VmHWM` is a high-water mark the kernel maintains
on every page fault, and `clear_refs` resets it per window.

### The burst's peak becomes the floor

| app | to hold a viewer | one pass in flight | 64 dragging | still resident once quiet |
|---|---:|---:|---:|---:|
| 600 rows, keeps rows | 179 kB | *see below* | 1 368 kB | **1 368 kB** |
| 200 000 rows, keeps rows | 7 248 kB | 16 636 kB | 129 084 kB | **126 240 kB** |
| 200 000 rows, aggregates only | 145 kB | 0 kB | 25 812 kB | **25 812 kB** |

The last two columns are the same number at **twenty of the twenty-four rungs** across the three
ramps: a second after the burst ends, the memory it needed is still resident. Four rungs return
something, and how much says what kind of exception each is — 0.07% and 0.2% at two of them,
2.2% at the row-keeping app's top rung, and **23%** at that app's two-dragging rung, which is
the only place in the set where a meaningful fraction comes back. An earlier run of the same
ramp returned 18% and 13% at two *different* rungs, so which rung gives memory back is not
itself stable; that a few do, and that most do not, is.

**One figure in these tables is not a measurement and is now labelled as one.** The bundled
example's *one pass in flight* has come out at **164, 244, 188, 92 and 92 kB** across five
independent runs of the same rung, and the three processes inside the last of those spread 128 kB
around a median of 92. The spread is larger than the number. That app's pass allocates a few tens
of kilobytes — a couple of dozen pages — so the reading is dominated by whatever the allocator
happened to round to, and no amount of repetition fixes a quantity that small against a
page-granular instrument.

Everything else in the same ramps is stable: that app's **ceiling** over the same five runs was
1 316, 1 324, 1 324, 1 368 and 1 368 kB, and the 200 000-row app's one-pass figure was 19 956,
17 532, 19 720 and 16 636 kB. The rule is the obvious one and worth stating because this rig
invites the mistake: **a transient of a few tens of kB is at the floor of what `VmHWM` can
resolve.** Read the small app's row for its shape, not its value.

**It is a plateau and not a leak**, and those are two readings that look alike and mean opposite
things. Firing the identical burst again costs about a **tenth** of the first in all three ramps,
so the process reached a working set rather than losing memory. Nor does it track the amount of
work: a window **ten times longer**, so ten times the passes, raises the ceiling by about a
**quarter** rather than by ten times. **The charge lands mostly per viewer who has ever touched a
control, not per interaction they make.**

| app | held, per viewer | extra, once they interact |
|---|---:|---:|
| 600 rows, keeps rows | 179 kB | ≈ 12 kB |
| 200 000 rows, keeps rows | 7 248 kB | **≈ 1 449 kB** |
| 200 000 rows, aggregates only | 145 kB | **flat** — see below |

Those are slopes over the upper half of each ramp. On the aggregating app a line is the wrong
model and the figure is quoted only to sit beside the row above it; see the next heading.

### The two 200 000-row apps part company differently this time

Held, they are 50× apart. Under load they differ in **shape** rather than in factor: past about
four concurrent viewers — the number of worker threads here — the aggregating app is at its
ceiling and sixteen times as many viewers do not move it, while the row-keeping app climbs from
37 MB to 126 MB over the same range. A pass on the aggregating app needs a *scratch buffer*, and
only as many are needed as there are passes at once; a pass on the row-keeping app leaves that
viewer holding a different allocation from the one it held before, which is per viewer.

So "aggregate upstream" survives as advice and earns a better reason. It does not make a pass
free — `filter` builds all 200 000 rows before `group_by` throws them away. It makes the cost
**stop growing with viewers**.

The two passes are not the same price and it is worth being exact, because an earlier draft
here said they were: one viewer dragging costs **≈ 16 MB** on the row-keeping pipeline, which
is left holding a fresh allocation of everything it materialised, against **0 MB** on the
aggregating one, whose scratch buffer of roughly 6 MB was already resident and free. What is
the same either way is the *scratch* a second concurrent pass needs, and that is what the
ceiling is made of.

The strangest cell in that table is the aggregating app's first dragging viewer: **0 kB** in all
three processes. Its pass wants a 200 000-row scratch buffer and one is already resident and
free — sixty-five sessions opened, each having run that pipeline once at connect, and the
allocator kept the pages. Nothing new faults in until a *second* pass wants a buffer at the same
time.

**"Reproducibly" is the word this sentence used to carry, and it has been taken back.** The
same app under `taskset` on the same four cores produced 0 kB, 11 596 kB and 0 kB across its
three processes. One dragging viewer against four workers is precisely the boundary where a
second concurrent pass either happens or does not, depending on whether the bystander's 50 ms
probe lands inside the dragger's pass — so the rung is bimodal and a median of three is the
statistic least able to say so. Below the boundary (one core) it is 0 kB in every process of
every run; above it (four dragging) it is 20 MB. At it, read the spread column.

### The half of the cost that is not paid by the viewer who is busy

`apply` calls `session.commit()` inline in the connection's `async fn`, so a pass occupies a
tokio worker for its whole duration. One viewer is held back from the ramp and never drags
anything; it asks for the state it already holds every 50 ms and times the answer.

| viewers dragging | 600 rows<br>(pass 0.5 ms) | 200k aggregating<br>(pass 25.6 ms) | 200k keeps rows<br>(pass 147.9 ms) |
|---:|---:|---:|---:|
| 0 | 0.4 ms | 0.3 ms | 0.4 ms |
| 16 | 1.8 ms | 75.7 ms | 497.9 ms |
| 32 | 4.1 ms | 195.7 ms | 919.9 ms |
| 64 | **9.0 ms** | **418.1 ms** | **2 248.5 ms** |

Every cell is a median over the probes that **came back**, and the ones that did not are
counted rather than folded in. That distinction is the whole of the next paragraph, and an
earlier version of this table got it wrong in a way worth printing.

The nought row is the control: all sixty-five sockets are open there too, so what the rows below
measure is the work rather than the connections.

**The last version of this table published 431.6 ms in the bottom-right cell, and the reason it
was wrong is the most useful thing in this section.** A probe still owed an answer when the
window shuts is recorded *censored*, at the time on the clock when the window shut — a number
known to be too small, by an unknown amount. The rig folded those into the same median as the
real observations. At rungs where nothing is censored that changes nothing; at the worst rung of
the worst app, where 3 of 11 probes never returned, it dragged the median from 2 248.5 ms down
to 431.6 ms — *below* the same app's thirty-two-viewer rung. Nothing in a server gets faster
when you double the load, and that impossible row is what the mistake looked like from outside.

**The diagnosis published alongside it was wrong too, and in the more interesting direction.**
That inversion was blamed on the three-second *window* — too narrow to see the rung — and the
thirty-second measurement was presented as the thing that rescued the rule. It was not the
window. Over completed probes the three-second ramp agrees with the rule at **every rung of all
three apps**, that rung included, and agrees with the thirty-second measurement of the same rung
to within 4%. The aperture was never the error; the aggregation was. `busy-session.mjs` now
aggregates the two apart and keeps the mixed figures beside them under `_with_censored`.

Divide each figure by its own app's pass and the three columns collapse into one:

| viewers dragging | 600 rows | 200k aggregating | 200k keeps rows | viewers ÷ workers |
|---:|---:|---:|---:|---:|
| 16 | 3.9 | 3.0 | 3.5 | 4 |
| 32 | 8.7 | 7.6 | 6.3 | 8 |
| 64 | 18.8 | 16.3 | 15.2 | 16 |

Each cell divides that rung's own pass cost, not the column header's: the pass drifts a little
across a ramp (143.8 to 147.9 ms on the row-keeping app) and dividing every rung by one figure
would put that drift into the ratio. The thirty-second re-measurement of the bottom-right rung
divides out to **17.6** — 2 343.1 ms over that run's own 133.5 ms pass — against 15.2 here, which
is the agreement the previous paragraph is about.

Three apps spanning **more than 300× in pass cost** agree, which is worth more than any absolute
above it:

> **A viewer who is doing nothing waits about (busy viewers ÷ worker threads) passes.**

So an idle viewer inherits the pass cost of the busiest viewer on the process, and pass cost is
a property of the manifest rather than of how many people are watching.

### The same app, one core against four

`DAGPANE_CORES` runs the server under `taskset`. tokio sizes its worker pool from the affinity
mask, so this moves the **denominator** of everything above rather than the numerator — which is
the only way to tell a rule from a coincidence. Same aggregating app, same ramp.

| viewers dragging | 1 core · 2 threads |  | 4 cores · 5 threads |  |
|---:|---:|---:|---:|---:|
| | transient | bystander | transient | bystander |
| 0 | 0 kB | 0.3 ms | 0 kB | 0.3 ms |
| 1 | **0 kB** | 16.0 ms | 0 kB * | 16.1 ms |
| 4 | **0 kB** | 96.4 ms | 20 712 kB | 10.1 ms |
| 16 | **0 kB** | 423.2 ms | 23 928 kB | 92.2 ms |
| 64 | **0 kB** | 822.9 ms ‡ | 26 548 kB | 420.1 ms |

**On one core the transient is zero at every rung, and that is the mechanism stated as an
experiment.** What a burst has to fault in is the scratch buffer of the *second and later*
concurrent pass; the first one's buffer is already resident, left behind by the sixty-five
sessions that each ran the pipeline once at connect. With one worker there is never a second
concurrent pass, so nothing new is ever needed — however many viewers are dragging.

**And the bystander's wait tracks the worker count, not the machine.** Against the rule
(busy ÷ workers) × pass:

| viewers dragging | 1 core: predicted / measured | 4 cores: predicted / measured |
|---:|---:|---:|
| 4 | 89 ms / **96.4 ms** | 25 ms / 10.1 ms |
| 16 | 365 ms / **423.2 ms** | 101 ms / **92.2 ms** |
| 64 | 1 443 ms / 822.9 ms ‡ | 405 ms / **420.1 ms** |

‡ **Half that rung's probes never came back** — six of twelve on one core — so its median is
over the six that were quick enough to be seen, and it is the one cell here not worth reading.
It is left in rather than blanked because a rung that loses half its samples should be visible
as such; the sixteen-viewer row above it is the one that tests the rule on one core.

It is close wherever there are more viewers than workers and optimistic below that, for the
obvious reason: with four workers and four busy viewers a bystander often finds one free.

So the trade is explicit. **A core buys throughput and it buys everybody else's latency; it
costs peak memory.** Four cores here serve 146 passes/s against one core's 40, answer the
bystander 4.6× sooner at sixteen dragging viewers — the last rung on one core where enough
probes come back to say — and hold 26 MB more at the peak.

\* **That starred cell is a median hiding a coin flip, and a previous draft of this file got it
wrong in the confident direction.** At a single dragging viewer the three 4-core processes
wanted **0 kB, 11 596 kB and 0 kB**. An earlier two-repetition run had produced 0 and 11 596 and
was averaged into a meaningless 5 798 kB; a three-repetition run then produced 0 in all three,
and this file concluded that repetition had settled it. It had not — the same split is back, and
the same 11 596 kB with it.

The bimodality is the answer rather than the noise. One dragging viewer with four workers is
exactly the boundary where a *second* concurrent pass may or may not happen, depending on
whether the bystander's 50 ms probe lands inside the dragger's pass. Below that boundary (one
core) there can never be a second, and the transient is 0 kB in every process of every run.
Above it (four dragging viewers) there always is, and the transient is 20 MB. At the boundary
the process gets one or the other, which is what a median of three is least able to tell you —
so the spread column, not the median, is what this rung is reporting.


### Opening the aperture: what the three-second window could not see

Every rung above uses a three-second window, and a bystander's wait longer than the window is
unobservable by construction. At sixty-four dragging viewers on the worst app that window answers
**5 of its 11 first-burst probes**, so the figure it prints rests on five samples. It was on
exactly that basis that an earlier draft of this work declared the 25 s keepalive safe — a claim
the measurement could not have contradicted.

So the same rung was run again at **thirty seconds**, ten times the aperture. A probe still owed
an answer when the window closes is recorded **censored** — a lower bound, not an observation — so
the two bursts and the two kinds of sample are kept apart below, because pooling them is exactly
the mistake the previous section is about.

**What the wider window bought was not a different answer.** Three seconds, over the probes that
came back, put that rung at **2 248.5 ms**; thirty seconds puts it at **2 343.1 ms**. They agree
to 4%, and both sit within a tenth of what the rule predicts from their own pass costs. What
thirty seconds bought is the right to say so: five completed probes became thirty-eight, and the
three-second repeat burst on the same processes gave 159 ms from three samples, which is the same
statistic saying nothing at all. **A rung with five samples is not wrong, it is unfalsifiable**,
and that is the honest reason to widen the window rather than the one first given here.

| 64 dragging · 200 000 rows · three processes | first burst | the identical repeat |
|---|---:|---:|
| probes issued | 41 | 40 |
| answered inside the window | **38** | **37** |
| p50 of those | **2 343 ms** | **2 285 ms** |
| longest of those | 3 923 ms | **4 905 ms** |
| cut short at window close | 3 | 3 |

**The wait did not grow to fill the window.** Ten times the room did not move the middle: the
three processes put their first-burst completed medians at 2 254, 2 310 and 2 547 ms, and the
repeat burst agrees at 2 285 ms. That is a statement about the 75 probes that came back; the six
that did not are bounded below by their truncation and above by nothing, so they are consistent
with this and cannot confirm it.

**Three things about how this was first written down here were wrong, and all of them were
mine.** The table published two rounds ago gave a "worst observed" of 3 718 ms. That was not a
worst and not an observation of one — it was the **median across the three processes of each
process's own worst**, relabelled as a maximum it never was. It was also the first burst only:
then as now, the longest wait of the pair comes from the **repeat** burst — 4 905 ms here — so
"at worst under four seconds" was under four partly because half the data was not in it. And the
statistic it was drawn from mixed censored lower bounds into its own median, which is the
correction two sections up and the one that took longest to find.

**What the measurement supports, at its own size**: at this load, on the slowest app here, a
bystander's median wait is about 2 343 ms, and the longest wait recorded across all **75 answered
probes** is **4 905 ms** — about **5.1× under** the 25 s keepalive on the single worst thing seen.
A further 6 probes were cut short at window close with lower bounds of 0.46–2.5 s; those are
consistent with the answered ones, and they are also the reason this cannot be stated as a bound.
A wait that never finished inside the window is a wait this rig did not measure.

So the claim is that **this load did not starve the keepalive**, not that it cannot be starved.
The rule is the thing that scales, and the rule says the wait is the app's pass times viewers over
workers — a bystander's median reaches 25 s at roughly ten times this pass cost, and its worst at
about 5.1×. Neither is far: the core ramp above measures a single core at **3.0×** four cores'
bystander wait on the aggregating app, on the host alone. A ceiling a few times above the worst
thing you measured is a margin worth knowing and not a guarantee.

**One thing the wider window did move**: the memory ceiling, from 129 084 kB to 163 304 kB. Ten
times the window buys about a **quarter** more memory, not ten times more — so the charge is
mostly, but not purely, per viewer who has interacted rather than per interaction they make.

### The allocator, and the build that actually ships

Everything above is a glibc build. The `Dockerfile` produces a **static musl binary on
`scratch`**, and musl's allocator is not glibc's — so "the burst's peak becomes the floor" is
exactly the kind of result that might be a property of the allocator rather than of this
runtime. It is.

**A spot check, and labelled as one**: the 200 000-row row-keeping app only, two repetitions
rather than three, four rungs rather than eight. It is enough to say the glibc result does not
transfer and not enough to replace it.

| 65 sessions, 4 dragging | glibc | musl |
|---|---:|---:|
| transient | 37 316 kB | 15 162 kB |
| **retained once quiet** | **+37 316 kB** | **−15 638 kB** |
| pass p50 | 124.8 ms | **1 763.3 ms** |

**On musl the memory comes back, and then some.** `retained` is *negative*: the process ends the
burst smaller than it started it, by 15 MB at four dragging viewers and by 235 MB at sixty-four —
and by 4 MB with only one viewer working. The most likely reading is that musl returns freed pages
eagerly but does the returning during later allocator traffic, so the burst is what finally cleans
up what opening sixty-five sessions left behind. Either way the headline above is glibc's, not
dagpane's.

**And musl is much slower here, but only under contention** — which is a different claim from
"musl is slower", wants a different fix, and needs a rung the spot check did not originally
have. It has one now: the session count is held at sixty-five throughout and only the number
*working* moves, so concurrency is the one thing varying. With **one** dragging viewer a pass
costs 260.8 ms against glibc's 152.1 ms — **1.7×**, the ordinary price of a simpler allocator.
Put **four** of the same sixty-five to work and it is **1 763 ms against 125 ms**, more than
**fourteen times** worse, and at sixty-four it is fourteen times still. This runtime
allocates heavily per pass — `filter` builds a whole new table — and mallocng serialises where
glibc's per-thread arenas do not.

**None of that is a benchmark note.** Every published figure in this file is a glibc figure and
the container is musl, so the one an operator runs is slower than the ones here under exactly
the conditions an operator cares about. Which allocator to ship is a decision this file does not
make; that the decision exists and was never taken deliberately is what the spot check found.

### What it does not say

One machine, shared and cloud, four cores. Three apps, of which two are the extremes of one
axis. And the musl spot check above is a spot check: one app, two repetitions, four rungs.

It is not a latency rig — `benches/loadgen/` is, protocol-level with a measured 67 µs floor, and
its figures are the ones to quote for throughput and p99. The bystander column is the one
latency question that rig cannot ask, because every viewer in it is generating load.

It is one process holding one app. What a busy app does to a **different** app in the same
`dagpane host` process is not measured, and the mechanism above says it is unlikely to be
nothing.

### The consequence an operator meets

Size for *viewers × (held + busy)*, not *viewers × held*; `OPERATIONS.md` §Sizing carries it.
And the bystander rule is the reason the CPU line there changed: cores are not only throughput,
they are the denominator under everybody who is not currently being served.

## The fleet: many apps, three runtimes, one machine

`ROADMAP.md` §7 and the roadmap's phase P2 ask for a second measurement — *apps-per-core, and
p99 interaction latency under N concurrent viewers, against a marimo and a Streamlit baseline
running the same app on the same hardware*. This is that harness and its first result.

```sh
cargo build --release -p dagpane-cli
python3 -m venv .venv && .venv/bin/pip install streamlit marimo playwright pandas
DAGPANE_BENCH_VENV=$PWD/.venv python3 benches/fleet/fleet.py --all --apps 1,2,4,8,16
```

`benches/fleet/README.md` is the method and what to distrust; `benches/fleet/results/` holds
the raw JSON, including the machine it came from. The short version of the method: the same
app written three times, each idiomatically in its own runtime, driven by one browser that
sets a control and waits until the number on screen is the number an **independent oracle**
says it should be — so a runtime that renders a wrong number fails rather than scoring well.

**Read the headline first, because it is not the flattering one.** This run did **not**
produce an apps-per-core figure. It produced a memory figure, a latency comparison at small
N, and a finding about the rig: the load generator saturates before dagpane does, so the
number the roadmap actually asks for is still not measured. §"What this does not settle"
below says what it would take.

### Where it ran

4 vCPU, 16.9 GB, Linux 6.18, **a shared cloud container**. The ratios below survive that
better than the absolutes do — all four arrangements take the same noise — but an absolute
figure that feeds a cost model needs a host whose neighbours are known, and this was not one.

### Resident memory, with every app holding a live session

| apps | dagpane<br>`run`, one process each | dagpane<br>`host`, one process total | Streamlit | marimo |
|---:|---:|---:|---:|---:|
| 1 | 5.8 MB | 5.8 MB | 157.7 MB | 168.9 MB |
| 2 | 11.5 | 5.9 | 315.1 | 336.7 |
| 4 | 23.0 | 6.6 | 630.5 | 671.0 |
| 8 | 45.6 | 7.6 | 1 258.6 | 1 340.3 |
| 16 | 91.0 | 9.6 | 2 517.6 | 2 680.4 |
| 32 | 182.6 | 13.6 | — | — |
| 64 | 365.8 | 21.4 | — | — |
| 128 | — | 37.2 | — | — |

Sum of RSS across the whole server process tree. Both dagpane columns are the same binary:
the difference between them is multiplexing and nothing else, which is what makes the
comparison to the right of them a comparison about runtimes rather than about two decisions
at once.

**Marginal cost of one more app**, as the slope over the measured range:

| | RSS per app | PSS per app | fixed cost |
|---|---:|---:|---:|
| dagpane, one process each | 5.68 MB | 0.95 MB | ~0 |
| **dagpane `host`, one process** | **0.25 MB** | **0.25 MB** | 5.6 MB |
| Streamlit | 157.3 MB | 94.8 MB | ~0 |
| marimo | 167.4 MB | 106.6 MB | 1.5 MB |

**PSS, not just RSS, and the difference is the finding.** Summing RSS over sixteen Python
processes counts every shared page of libpython, pandas and numpy sixteen times, so it
overstates what a fleet costs the machine and overstates it more as the fleet grows. PSS
divides each shared page by the number of processes mapping it. P2's criterion says RSS, so
RSS is above; **where they differ, PSS is the honest one**, and it is what the ratios use.

On marginal PSS, one more app costs **385× more in Streamlit and 433× more in marimo** than
in `dagpane host`. Against `dagpane run` — the same binary, arranged the way the baselines
arrange themselves — it is 100× and 112×. So roughly a quarter of the gap is multiplexing
and the rest is the runtime.

**What that ratio is sensitive to, stated because it is large enough to be doubted.** These
are 600-row apps, and a source is a source in any runtime: put a hundred megabytes of data
behind each app and the constant term stops mattering in all four columns. The figure is
about the cost of *holding an app*, which is the term that decides how many idle-to-moderate
dashboards a node can keep resident — the actual question a hosting plan asks. It is not a
claim about a data-heavy workload, and `benches/rows.sh` above is where that wall was already
measured.

### Interaction latency

One viewer per app, a control move every 300 ms, 20 interactions each. p50 / p99 in
milliseconds:

| apps | dagpane `run` | dagpane `host` | Streamlit | marimo |
|---:|---:|---:|---:|---:|
| 1 | 27 / 63 | 22 / 45 | 130 / 143 | 121 / 156 |
| 2 | 28 / 56 | 26 / 56 | 124 / 175 | 130 / 192 |
| 4 | 47 / 86 | 41 / 90 | 190 / 249 | 170 / 334 |
| 8 | 77 / 171 | 72 / 141 | 433 / 814 ⚠ | 405 / 819 ⚠ |
| 16 | 148 / 325 | 132 / 346 ⚠ | 845 / 1364 ⚠ | 796 / 1502 ⚠ |

⚠ **the load generator was saturated** — its CPU share or its dispatch lateness passed the
thresholds in `fleet.py`, so those latencies are a lower bound on what the *server* could
have done. That applies to the baselines' rows as much as to dagpane's: a saturated row is
evidence about the rig, not about anybody's runtime.

**The driver floor is 23.9 ms on this machine**, measured each run against a local page whose
entire server is a `textContent` assignment. It is inside every number in that table and it
is deliberately **not subtracted** — subtracting a median from a p99 is not arithmetic. For
Streamlit and marimo it is a rounding error. **For dagpane it is most of the number**: a p50
of 22 ms against a floor of 24 ms means the runtime is at or below the rig's resolution, and
the only honest reading of the dagpane rows at small N is *"too fast for this harness to
measure"*, not "22 ms".

That is worth being blunt about, because it cuts against this project: the latency comparison
above shows Streamlit and marimo at roughly 100 ms of real server work per interaction and
dagpane somewhere under the floor, and it **does not** tell you where under.

### What this does not settle — including the thing it was built to settle

**There is no apps-per-core number here, and this run cannot produce one.** The figure needs
the point at which p99 crosses a stated budget as apps are added, and at 16 apps the load
generator was already the bottleneck — a browser per viewer on the same 4-core machine as
the fleet. Every run past that point measures Playwright.

The fix is not a bigger machine, it is a different driver: a **protocol-level load generator**
that speaks the WebSocket directly, with no browser per viewer. That is cheap for dagpane and
expensive for the baselines, whose protocols are protobuf and a bespoke JSON dialect — which
is itself the reason the browser was chosen first: one driver that is fair to all three beats
three drivers that are each fair to one.

**That rig now exists** — `benches/loadgen/`, and the section below carries what it found. The
two are published separately and never averaged: one number came from a browser and one from
a socket, and saying so is cheaper than defending a figure that came from both.

Three smaller caveats:

- **Streamlit's caching is a choice this benchmark made.** `apps/streamlit_app.py` caches the
  CSV read with `@st.cache_data`, which every real Streamlit app does, and does not cache the
  derived frames, which is Streamlit's default execution model. Caching the derived frames
  too is a real third design that would land somewhere between the two runtimes. It is not
  what is measured, and `apps/streamlit_app.py` says so in its own header rather than
  quietly picking the flattering variant.
- **One viewer per app.** The concurrency axis the roadmap asks for is viewers *per app* over
  a shared graph, and this run varied apps instead. `--viewers` exists and was not used at
  scale for the same reason as above: the driver runs out first.
- **`--think 0` runs carry no latency.** The 32/64/128 points were taken with no think time,
  which makes every viewer due at once; their memory samples are valid and their latencies
  are driver queueing. The harness marks them `latency_reportable: false` rather than
  printing a number that looks like a measurement — a check that exists because an earlier
  version of this file did print one.

## Apps-per-core, at last — and how two independent methods were made to agree

The fleet section above could not produce this number: a browser per viewer saturates before
dagpane does, and its ~24 ms round trip is itself larger than a dagpane interaction. So a
second rig exists — `benches/loadgen/`, **dagpane only, protocol level, no browser** — whose
own round trip over an in-process echo is **67 µs at p50 and 123 µs at p99**, 350× finer than
the browser's. `benches/loadgen/README.md` is the method.

```sh
cargo build --release --manifest-path benches/loadgen/Cargo.toml
./benches/loadgen/apps-per-core.sh
```

The server runs under `taskset` on **one** named CPU and the generator on the others, so "per
core" is not a figure of speech and the tool is not fighting the thing it measures.

### The measurement

`dagpane host`, one process, 320 apps deployed, ramping how many are actively used. Two
viewers per app, each moving a control **every 200 ms** — a heavy workload, and the figure
below is conditional on it. Budget: p99 ≤ 250 ms.

| apps | sessions | p50 | p99 | throughput | schedule slip p99 | generator CPU |
|---:|---:|---:|---:|---:|---:|---:|
| 64 | 128 | 0.6 ms | 2.8 ms | 600/s | 2.1 ms | 3.5% |
| 128 | 256 | 1.0 | 14.1 | 1 178/s | 2.0 | 6.2% |
| 160 | 320 | 1.3 | 67.1 | 1 363/s | 2.1 | 6.6% |
| 192 | 384 | 5.9 | 89.7 | 1 767/s | 2.0 | 8.4% |
| **224** | **448** | **25.6** | **75.1** | **1 908/s** | **1.9** | **8.9%** |
| 256 | 512 | 169.8 | 355.8 ✗ | 2 133/s | **1 029.7** | 7.8% |
| 320 | 640 | 166.8 | 432.2 ✗ | 2 115/s | **2 880.3** | 6.9% |

**224 apps on one core**, at 448 concurrent sessions and 1 908 interactions per second, with
the generator using under 9% of the three cores it was given. **Four independent runs, all
224**: the table is the first, and the ladder was re-run three more times around the cliff.

The cliff is unmistakable and it is the server's: between 224 and 256 the schedule slip goes
from 1.9 ms to 1 030 ms while the generator's CPU *falls*. A tool at 8% is not the limit.

**Run-to-run variance at the ceiling, since it is visible in the committed files.** The 224
rung's p99 came back as 75, 105, 199 and 126 ms across the four runs, and its slip as 1.9,
1.8, 2.1 and 110 ms. Every one of them is inside the budget and the *next* rung failed every
time, which is why the ceiling is stable at 224 while the numbers at it are not. On a shared
container that spread is the neighbours; on a dedicated host it should narrow, and the
ceiling is the figure to carry across rather than any single p99 beside it.

**One app is one manifest is one dashboard**, which is the identity the roadmap's criterion
insists on — the figure is in the unit a plan would count, not a different one that happens
to be larger.

### The baselines, by a different route, and why it is allowed

Streamlit's and marimo's ceilings cannot be reached this way — the browser rig saturates
first, and a protocol driver for each would be fair to one runtime apiece. So their number
comes from **service demand**: server CPU-seconds per interaction, which the fleet rig already
samples on every run, and which bounds capacity without needing to saturate anything.

**Every row below is the four-app rung of `vm-20260917-133459.json`**, and that is not a
detail. Service demand is only service demand below saturation — above it, CPU per interaction
is measuring the queue — and four apps is the highest rung on which *none* of the four
runtimes had saturated. An earlier version of this table quoted each runtime's most flattering
rung, which put Streamlit's one-app figure beside marimo's sixteen-app one and compared two
different experiments.

| | CPU per interaction | implied apps/core at this workload | measured ceiling |
|---|---:|---:|---:|
| dagpane `host` | 0.50 ms | 200 | **224** |
| dagpane `run` | 0.62 ms | 161 | — |
| marimo | 27.00 ms | 3.7 | — |
| Streamlit | 81.50 ms | 1.2 | — |

The two dagpane rows differ by 0.12 ms, which is **one clock tick's worth of noise** on runs
this small — see the conditional below. Read them as one number, not as `host` beating `run`.

**The right-hand column is the point.** The model predicts 200 for dagpane and the ramp
measured 224 — agreement within 12%, from two rigs that share no code. That is what licenses
reading the marimo and Streamlit rows as capacity rather than as arithmetic. Without it they
would be a formula applied to a number.

**The committed fleet runs predate one fix to the rig.** Their interaction plan began at the
control's own default, so the first round of each viewer completed without a server round trip
— one sample in twenty at `--rounds 20`. It costs no server CPU and still counts as an
interaction, so it biases every row in this table in the same direction by the same fraction,
which is why the ratios above survive it and the absolute figures are a few per cent
optimistic for all four runtimes alike. `benches/fleet/fleet.py` no longer does it.

marimo costs about a third of what Streamlit does per interaction, which is what a dependency
graph is for and is the expected direction: it is the closer competitor and this measurement
says so.

### What this figure is conditional on, stated because the number is large

- **The workload.** Two viewers per app, one interaction each per 200 ms — ten interactions
  per second per app, continuously. A real dashboard is mostly idle. Halve the rate and every
  row above doubles; the ratios do not move.
- **Six hundred rows.** The service demand is the runtime's overhead plus the app's work, and
  this app's work is small. The row sweep at the top of this file is where a large source was
  measured, and it is the term that grows.
- **dagpane's service demand rests on 16 clock ticks.** `/proc` accounts CPU in 10 ms ticks,
  and dagpane's largest fleet run used 0.16 CPU-seconds in total — so ±1 tick is ±6%, and the
  smaller runs are worse. The baselines' figures rest on thousands of ticks and are precise.
  **dagpane's number is established by the ramp, not by the service-demand model**; the model
  is there to carry the method across to the runtimes whose ceiling could not be reached.
- **A shared container**, four cores, described in the fleet section. Pinning removes the
  denominator problem, not the neighbours.

### What it does not say

No cost per dashboard, no cost per core, no price. This is a capacity measurement on one
machine; turning it into money needs an instance type, a utilisation assumption and a
margin target, none of which are in this repository.

## What it does not measure

- **Not a real workload.** These are synthetic rows through the bundled manifest. §2 asks
  for a real app with a real source, so this narrows that item rather than closing it: it
  says where the representation breaks on data shaped like the example, not on data shaped
  like yours.
- **Not all of the concurrency numbers.** `ROADMAP.md` §7 wants three things: resident
  memory per additional session over one shared `Arc<App>`, p99 interaction latency under N
  concurrent viewers, and apps-per-core against another runtime. The fleet section above
  answers the third question's *memory* half and compares latency at small N; it does not
  produce apps-per-core, and it varied apps rather than viewers-per-app, so the first item
  is untouched. §7 stays open, and the rule it states still holds in its narrowed form: no
  *general* performance claim in the README until those exist. A dated figure that names its
  app, its machine and its command — everything in this file — was always allowed; a sentence
  about how fast this runtime is, is not.
- **A comparison with another runtime, now — and only in the fleet section.** That section
  measures Streamlit and marimo directly, on this machine, from apps this repository
  contains and whose idiomatic-ness is arguable in the open. Everywhere else in this file
  there are still no figures for any rival tool: the engine table above is a *dependency*
  evaluation — which library can sit behind this project's own seam and what it weighs — not
  a benchmark against a competing product, and it reports build size only.
- **One machine.** Absolute times will differ on yours; the shape of the curve and the
  memory ratio should not.
