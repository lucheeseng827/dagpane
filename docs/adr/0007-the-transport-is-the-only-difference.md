# ADR-0007: the transport is the only difference, and a drawing is not the runtime's business

**Status:** accepted · **Date:** 2026-09

## Context

Two things came due at once, and they turn out to be the same decision seen from two sides.

**`ROADMAP.md` §4** wanted client-side compute, and had spent four paragraphs explaining why
most versions of that idea are bad: a SQL engine in a browser is 30 MB, wasm32 caps at 4 GB,
threads need cross-origin isolation, and Perspective shipped the architecture already. The one
version it called defensible was *"the placement is a deployment decision rather than a
rewrite"*. It also set a trigger — an interaction whose latency is dominated by the round trip,
**measured** — and said in as many words that if the measurement came back the other way the
item stayed shut.

**A dashboard needed a sixth drawing.** The runtime draws five things: text, a metric, a table,
bars, a line. Adding a treemap meant editing `PaneKind`, `View`, `render`, the manifest
compiler and the client's own `switch` — five places across two crates — and then shipping a
new server binary to a user who wanted a different picture of their own data.

The second problem is what made the first one worth solving. An app that has to be rebuilt to
change a chart is an app with a maintainer in the loop; that is bearable when a server is in
the loop anyway and absurd when the whole thing is a folder of static files.

## Decision

### 1. The boundary is the protocol that already exists

`dagpane-serve` holds an `AppSession`, takes a `ClientMessage` off a socket and answers with a
`ServerMessage`. `dagpane-wasm` holds an `AppSession`, takes a `ClientMessage` from a function
call and answers with a `ServerMessage`.

Nothing new was designed. The client gained a `transport` — an object with `send` and `ready`
— and everything above it is written once. That is the whole of "the placement is a deployment
decision": there is no browser build of the app model, no second renderer, no parallel wire
format to keep in step.

The test that this is true rather than merely claimed: `crates/wasm/tests/run.sh` exports the
bundled example, drives it in a WebAssembly engine, and asserts the interaction costs **11
cells / 8 visited / 6 evaluated / 1 reused / 4 changed / 3 untouched / 3 of 7 panes** — the
same numbers `.github/workflows/ci.yml`'s `claim` job asserts against the server. A browser build that
recomputed a different number of cells would be a different product with the same name.

**The browser compiles the manifest.** It is handed the TOML and the bytes, not a graph. A
graph holds closures and cannot be serialised, and shipping a pre-compiled one would mean a
second representation of an app to keep in step with the first. `dagpane_app::Sources` is the
seam: a host offers a directory or it offers bytes, and `compile_with` does not learn which.
About a third of the wasm module is the TOML parser, and it is there on purpose — a misspelt
column is the same sentence in a browser as in `dagpane check`.

### 2. No wasm-bindgen

The interface is one JSON string in and one JSON string out. `wasm-bindgen` would bring a
proc-macro dependency tree, a `wasm-bindgen-cli` whose version has to match it exactly, and a
post-build step — into a repository whose client is *"one file, no build step, no package
manager"* and whose published numbers depend on a pinned dependency set. (The rest of that
sentence used to be *"nothing fetched at run time"*; §4 below is where this ADR weakens it,
so it is not quoted here as though it still stood.)

What replaces it is about forty lines of `extern "C"` over linear memory and about the same
again of JavaScript. `cargo build --target wasm32-unknown-unknown --release` is the whole
build. Every call that returns anything returns one pointer to a little-endian `u32` length
followed by that many bytes of UTF-8 JSON; the caller frees `4 + length`.

The cost is stated where it lives: `abi.rs` is the only module in the tree that is not
`#![forbid(unsafe_code)]`, its module docs enumerate every obligation a caller has, and
everything with logic in it is in `engine.rs`, which is ordinary safe Rust tested on the host.
If this ever needs to pass structured values rather than bytes — a frame shared with the page
without a copy — the trade changes and that comment is the first thing to revisit.

**The module imports nothing.** That is a consequence rather than a goal: `dagpane-core` is
clockless, I/O-free and deterministic because that is what made it testable without a browser,
and the dividend is a wasm artefact with no `getrandom` backend to configure, no wasi shim and
no COOP/COEP headers. `benches/wasm-engines/README.md` records a candidate that looked like it
could not build for wasm32 and was really just missing an entropy backend; this one has
nothing to miss.

### 3. A `custom` pane hands over the drawing and keeps the decision

```toml
[app]
renderers = ["renderers/heatmap.js"]

[[pane]]
cell = "latency_by_hour"
custom = { renderer = "heatmap", columns = ["day", "hour", "p99"],
           options = { low = "#f7f4ea", high = "#2f6f4f" } }
```

```js
window.dagpane.renderer("heatmap", (view, options, el) => { … });
```

A new visualisation is one JavaScript function and one manifest stanza. No Rust.

**What the engine keeps** is the part that can be wrong. It still decides which rows exist,
still decides whether anything moved, and still decides what goes on the wire — a renderer is
handed the answer and an element, and is never on the path that produces one. So a custom pane
whose view did not change is still absent from the patch, and a renderer cannot make the
product claim untrue. `a_custom_pane_is_still_absent_from_a_patch_when_its_view_did_not_move`
is that property, pinned.

**What it hands over** is genuinely opaque. `options` is JSON, not `Value`: typing it would
mean every option a new drawing wants is a change to `dagpane-app`, which is the cost this
whole variant exists to remove. The data is typed and schema-carrying either way — a
projection of a frame with its true height beside it, or the scalar as it stands — so a
renderer never has to guess what a column is.

**Three things are still the runtime's job**, because a renderer has no way to do them:

* **A named column that is gone is an error, not an omission.** A chart drawing three of its
  four series because somebody renamed a column looks fine and is wrong.
* **Truncation is reported.** `total_rows` travels with every custom view exactly as it does
  with a table.
* **A renderer that throws takes out its own pane and nothing else** — the same rule a cell in
  error follows.

### 4. Renderer scripts are read when the app is compiled

Not at request time. `dagpane-serve` holds the bytes and serving one is a map lookup, so there
is no `open(2)` a request can steer — which is what lets this route exist beside a server whose
job is to be safe to point at a directory. It also means `dagpane check` fails on a `custom`
pane whose script is missing, rather than a page that loads with a blank card in it.

Three refusals, all about the path and none about the code: it must be relative, it must not
climb out of the app's directory, and it must not be a URL. The third is the one worth writing
down, because `https://cdn.example/x.js` is **not** an absolute path — `is_absolute()` is false
for it and it contains no `..` — while the client resolves it with
`new URL(path, document.baseURI)`, where it is absolutely another origin. On a server the file
read catches it; in a browser, whose host supplies scripts by name, nothing would.

This weakens a rule that used to be absolute: the client fetched **nothing**. It now loads code
an app declared. `the_client_is_one_self_contained_file` enumerates the two `fetch`es and the
two `import`s the page is allowed and fails on a third, which is the shape that rule has to
take now rather than a rule quietly deleted.

## Consequences

**The measurement came back sideways, and it is the most useful thing here.**
`benches/roundtrip/` runs three apps, 300 interactions each, against a real socket and then
against the same pass in wasm:

| app | round trip | the wire | wasm pass | wasm vs native | break-even |
|---|---:|---:|---:|---:|---:|
| `sales.toml` | 0.153 ms | 0.101 ms (66%) | 0.130 ms | 2.5× | **0.08 ms** |
| `15-unit-economics` | 0.165 ms | 0.080 ms (48%) | 0.235 ms | 2.8× | **0.15 ms** |
| `19-wide-telemetry` | 0.751 ms | 0.347 ms (46%) | 0.892 ms | 2.2× | **0.49 ms** |

The wire is about half of every interaction, on the friendliest wire that exists — so §4's
trigger is met. **And wasm is 2.2–2.8× slower at the identical pass**, so on loopback the
browser is *slower* on two of the three apps. The figure that transfers is the break-even:
`wasm pass − server pass`, which does not depend on the network and is 0.08–0.49 ms **for
these three apps on this machine**. It does depend on the app — a heavier pass raises its own
threshold — so the transferable thing is the method, and `benches/roundtrip/` is it. Typical
round trips clear this range, which is the reason to expect the trade to pay and not a bound
this measurement establishes.

So the honest sentence is not "the browser is faster". It is **"client-side compute buys you
the wire and charges about 2.5× on the pass to do it"** — a good trade on a real network, a bad
one for an app whose pass is already most of a frame budget.

**What is not built**, and saying so is the point of an ADR:

* **Per-cell placement.** §4's defensible version was the placement of each *cell*. This moves
  the whole graph. The seam exists; splitting a graph across a wire with glitch freedom
  preserved across the cut is a separate piece of work with its own correctness argument.

  *Since superseded in part:* **ADR-0008** makes that argument and builds the mechanism — a
  monotone cut, one frontier message per pass, and a split oracle that checks it. Nothing
  serves a split yet, so the sentence above is still true of what `dagpane run` and
  `dagpane export` do.
* **A worker.** `send` is synchronous — the call *is* the pass, and it blocks the frame. At
  0.1–0.9 ms that is invisible; at the 100 000-row end of `BENCHMARKS.md`'s row sweep it is
  not. Nothing measured so far needs the fix, so it is not written.
* **Everything a server was doing.** No front door, no scheduled refresh, no HTTP or SQL
  sources, no eviction. `dagpane export` refuses a source it cannot inline rather than writing
  a bundle that breaks once opened.

**An exported app is as big as its data.** The rows go into the page, so they need no second
fetch — but the export is still a **multi-file bundle** whose files deploy together: the page,
the glue, the module and any renderer scripts. `dagpane export` prints the total for exactly
this reason — 1.2
MiB for the bundled example is fine and a 40 MB CSV is not, and the row sweep says where the
line is.
