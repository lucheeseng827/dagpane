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

## What it does not measure

- **Not a real workload.** These are synthetic rows through the bundled manifest. §2 asks
  for a real app with a real source, so this narrows that item rather than closing it: it
  says where the representation breaks on data shaped like the example, not on data shaped
  like yours.
- **Not the concurrency numbers.** `ROADMAP.md` §7 wants resident memory per additional
  session over one shared `Arc<App>`, p99 interaction latency under N concurrent viewers,
  and apps-per-core against another runtime. None of that is here. §7 stays open, and the
  rule it states — no performance claim in the README until those exist — still holds.
- **Not a comparison with another runtime.** There are no figures for any rival tool in
  this file, measured or quoted. The engine table above is a *dependency* evaluation — which
  library can sit behind this project's own seam and what it weighs — not a benchmark against
  a competing product, and it reports build size only, never anyone's performance.
- **One machine.** Absolute times will differ on yours; the shape of the curve and the
  memory ratio should not.
