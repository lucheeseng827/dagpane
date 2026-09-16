# dagpane — positioning

*Written 2026-09-08, against the survey in COMPETITORS.md and against this checkout at
v0.1.0. Every claim about dagpane below was produced by a command in this repository; every
claim about anything else carries a date. The paragraph this file opens with is a deliberate
narrowing of the one the prior-art dossier offered, and §1 says which parts were cut and
why.*

---

## 1. The claim

Every reactive data-app framework in production today gets its dependency graph from source
text or from developer annotation. marimo statically analyses each cell's references and
definitions, which is why its own documentation concedes it cannot see mutation or attribute
assignment — *"tracking mutations reliably is impossible in Python"*. Streamlit, as of 1.63.0
(2026-09-01), lets you name a fragment and fire it from a callback with `st.rerun("filters")`,
which means you draw the boundary and you choose what it invalidates; its docs state plainly
that a fragment *"can't detect a change in input values"*. Dash, Shiny and Panel all require
the edges to be written out by hand, per callback. dagpane's claim is not that reactivity is
new — Shiny shipped a real reactive graph in 2012 and marimo shipped a real reactive DAG for
Python — it is narrower and it is this: **an app declares its edges once, and everything
after that is derived — which cells are stale, which of those actually recompute, which
recompute to a value that changes nothing below them, and which panes go on the wire. The
runtime prints that answer before the app is deployed.** `dagpane explain` on the bundled
11-cell example reports that moving one slider visits 8 cells, recomputes 6, reuses 1, never
looks at 3, and repaints 3 of 7 panes. The mechanism underneath is salsa's backdating at cell
granularity: a cell reuses its output when its inputs' digests match the ones it recorded, so
a cell that recomputes to the value it already held stops the pass at its own boundary. The
graph is immutable and shared — one `Arc<Graph>` behind every session, asserted by pointer
identity in `crates/core/tests/oracle.rs` — so a session is a vector of value slots, not a
kernel, not a thread, and not a process.

**Three things the dossier's version of that paragraph claimed have been cut, because
dagpane has not built them.** They are roadmap, not positioning:

* **Column-granular invalidation.** The dossier's strongest technical argument was
  comemo-style constrained memoization — a cell reading 3 of 200 columns not invalidating
  when column 47 moves. v0.1.0 invalidates at whole-value granularity. Change one cell of a
  `Table` and every cell reading that table recomputes. Nothing in this repository does
  sub-node invalidation and no README may say otherwise.
* **Engine placement decided at deploy time.** `cargo check -p dagpane-core --target
  wasm32-unknown-unknown` exits 0 and CI runs it. That is the entire claim. There is no
  `crates/wasm`, no wasm-bindgen, no client-side compute, and nothing has ever executed in a
  browser. Perspective 5.3.1 (2026-09-04, Apache-2.0, OpenJS Foundation) already ships
  symmetric client/server WASM compute with a memory64 build past 4 GB; dagpane does not
  compete there and must not imply it does.
* **Serialisable session state that removes the sticky-session requirement.** dagpane has no
  session state to serialise. A session is a WebSocket connection; closing the tab discards
  it; there is no store, no eviction, no TTL, no reconnection token. That does dodge marimo's
  sticky-session problem, but by having nothing worth preserving across a reconnect rather
  than by making state portable. It is a smaller claim and it is the true one.

---

## 2. The strongest argument against this module existing

**The reactive graph is the cheapest, smallest, most-solved part of a data-app framework,
and it is not why anyone picks one.**

marimo demonstrates that a sound reactive DAG over an interpreted language is a tractable
amount of work — AST reference/definition extraction plus a single-assignment invariant.
Shiny demonstrated the same thing in R in 2012. The reason 24,852,382 people a month
`pip install streamlit` and 2,420,457 install marimo is not the execution model. It is that
the body of a cell is `import pandas`, `import sklearn`, `import plotly`, `import boto3`, and
that a data scientist already knows how to write those four lines. **A Rust reactive core
asks that person to write Rust.** No amount of incremental-computation elegance survives
contact with that request.

The market has already answered this, in this exact lane, four times in the last twelve
months:

| crate | latest publish | lifetime dl | 90-day dl |
|---|---|---|---|
| `rustview` 0.1.8 | 2026-06-02 | 263 | 96 |
| `venus` / `venus-core` 0.1.2 | 2026-07-15 | 193 / 554 | 68 |
| `GORBIE` 0.18.1 | 2026-06-10 | 2,835 | 419 |
| `streamlit` (crate) 0.1.2 | 2026-01-06 | 51 | 16 |

**599 downloads across 90 days, between all four.** And the sharpest detail in that table is
`rustview`: it is *the full-re-execution design dagpane argues against*, already built, in
Rust, with Axum and a per-session vDOM and a JSON patch diff — and it has 96 downloads in 90
days. Whatever is holding a Rust data-app framework back, it is not the execution model,
because the one with the wrong execution model failed at the same rate.

Three supporting blows, each of which the project must carry rather than answer:

1. **The premise's first clause is stale.** "Streamlit reruns the whole script" stopped being
   the whole truth on 2024-04-05 and stopped being defensible as a headline on 2026-09-01.
   Leading with it dates the README to 2023.
2. **The premise's second clause is already delivered by the incumbents.**
   `marimo export html-wasm --mode run` is a deployable app with no server process at all,
   and marimo's server kernels are sub-threads. shinylive and `panel convert` ship the same.
3. **The GIL argument is on a visible decay curve.** PEP 779 was accepted and free-threaded
   builds became officially supported in Python 3.14 (October 2025), with single-thread
   overhead reported in single digits. Any positioning whose economics rest on "Python cannot
   do concurrency" has a shelf life measured in a few years, and this one does not rest on it.

There is no rebuttal to §2 in this file. The four conditions below are what would have to
become true for it to stop being decisive, and §3 grades them strictly.

---

## 3. The four conditions, and where v0.1.0 actually stands

All four, not any. The dossier was explicit about that, and the grades below are deliberately
harsh — a condition marked partly met is a condition not met.

### Condition 1 — the buyer is the platform team, not the analyst

The person with the pain is whoever hosts 400 internal Streamlit or marimo apps and pays for
a sticky-session-pinned, vertically-scaled container per app. `marimo-team/marimo#1831`
("Support stateless for multi container scaling and deployment"), open since **2024-07-19**
with no visible maintainer response, is the artifact of that pain. dagpane's unit of value
would then be cost and blast radius per hosted app, measured in apps-per-core and p99 under
N concurrent viewers — numbers no incumbent publishes.

**Not met.** The structure points the right way: the app, the graph and every loaded source
are one `Arc<App>` behind every connection, so a hundred viewers of the bundled 600-row app
are a hundred slot vectors over one table, and `Arc::ptr_eq` on an untouched source across
two sessions is asserted by a test rather than inferred from an RSS reading. But:

* **No apps-per-core number has been measured. No p99 has been measured. No memory figure has
  been measured.** Until one exists, this is a description of a data structure.
* **`dagpane run` serves exactly one manifest on one port.** Hosting 400 apps means 400
  processes. Each is a small static binary with no Python and no package manager, which is
  cheaper than 400 containers, but "cheaper" is a word and not a number, and the
  multiplexing story does not exist.
* The benchmark that would settle this — apps-per-core against a marimo or Streamlit baseline
  under the same load — is the single highest-value unbuilt thing in this module.

### Condition 2 — nobody writes Rust

The authoring surface must be declarative, with Rust as the invisible engine. The moment the
README shows a Rust closure as a cell body, the addressable market is Rust programmers who
want dashboards.

**Partly addressed, which is to say not met.** `examples/sales.toml` is a complete app —
sources, inputs, cells, panes — with no Rust in it, and the README can lead with it honestly.
The ceiling is exact and small: a cell's pipeline is drawn from **seven verbs** (`filter`,
`select`, `sort`, `limit`, `group_by`, `scalar`, `count`) over five widget kinds. There is no
SQL and no expression language, on purpose — the manifest's job is to produce *edges*, and a
dependency inferred wrongly from SQL text by a regular expression is a wrong app — but the
consequence is that the eighth verb an author needs is a `GraphBuilder::cell` closure in Rust.
Evidence proved SQL + Markdown is a viable BI authoring surface and Mosaic proved
params/selections is a viable interaction spec; seven verbs is not yet either of those.

### Condition 3 — the differentiator is sub-node invalidation, not node-level reactivity

Node-level reactivity is marimo's, shipped and free. Column-and-predicate-level invalidation
— comemo's constrained memoization applied to a columnar frame — is something neither marimo
(Python mutation is untrackable) nor Streamlit (fragments *"can't detect a change in input
values"*) can do, and is reachable only because Rust can make access observable.

**Not met, and not started.** dagpane invalidates at whole-value granularity. The dossier's
test for this condition was a measured result of the form "changed one column of a
200-column frame, recomputed 2 of 40 nodes"; the number this project can print today is
"changed one input, visited 8 of 11 cells, recomputed 6, repainted 3 of 7 panes", which is a
different and weaker claim. What v0.1.0 *does* have is the mechanism that makes sub-node
invalidation worth building — the digest short-circuit, so a cell that recomputes to an equal
value stops the pass — and a differential oracle (200 random DAGs × 12 interactions = 2,400
interactions, checked against a full-recompute oracle for correctness and economy after every
commit) that would catch a sub-node scheme getting it wrong. Those are foundations for
condition 3, not condition 3.

### Condition 4 — Perspective and Mosaic are cited as ancestors on page one

**Met for COMPETITORS.md, unverified for the README.** COMPETITORS.md §2 names Perspective
before any comparison and states that it already ships the symmetric client/server
architecture dagpane was going to claim; it names Mosaic as the peer-reviewed answer where an
app is SQL over DuckDB and says dagpane has nothing to add there. Whoever writes the README
inherits that obligation. A reader who finds Perspective before this project mentions it has
already decided what kind of project this is.

**Score: zero of four fully met, two of four partly.** That is the honest reading, and it is
why this file exists in the form it does rather than as a launch narrative.

---

## 4. The honest fallback: extract the engine, do not compete as a framework

If conditions 1 through 3 cannot be met, the recommendation the dossier reached is not to
ship a framework. The alternative is real and it is stronger than it sounds:

**Extract the engine only** — a Rust incremental-recompute core with column-granular
invalidation, exposed as a library and a PyO3 binding — and let marimo, Panel and Shiny for
Python be the front ends. That inherits the Python ecosystem instead of fighting it, has a
named upstream buyer, and does not require anybody to write Rust to benefit from it.

The code is already shaped for this and was shaped that way on purpose. `dagpane-core` is a
standalone crate with **one dependency** (serde), `#![forbid(unsafe_code)]`, no I/O, no
clock, no async runtime, and a manifest whose own comment says it is *"the one a downstream
project embeds directly (the engine without the server)"*. `cargo test -p dagpane-core` is a
complete test of the reactive semantics on a machine with no network and no browser, and
`cargo check -p dagpane-core --target wasm32-unknown-unknown` exits 0. Nothing about the
framework half of this module has to survive for the engine half to be useful to someone
else.

There is a sibling project in the same tradition that reached the same conclusion about its
own lane: that contributing the two things it did better to the incumbent tool would reach
more users than shipping a second tool competing for that tool's 329 downloads a quarter. The
argument has the same shape here. 599 downloads a quarter is the size of the prize for
winning the Rust data-app framework lane outright.

**This is written where anyone can read it, on purpose.** A competitor document that omits
the argument against building the thing is marketing; a positioning document that omits the
exit is worse, because positioning is where the decision actually gets made. Keeping it here
costs nothing except the discomfort of a reader finding it, and a reader who finds it learns
something true.

---

## 5. What would change this file

Each of these is a specific, checkable event. None has happened as of 2026-09-08.

* **A measured apps-per-core number** against a Streamlit or marimo baseline under identical
  load. Condition 1 turns from a data-structure description into a claim the moment this
  exists, and it is the cheapest of the four to produce.
* **Column-granular invalidation with a measured result** — "changed one column of a
  200-column frame, recomputed 2 of 40 cells", produced by `dagpane explain`. Condition 3.
  This is the one that requires new engine work rather than new measurement.
* **An authoring surface past seven verbs** that still produces edges statically. Condition 2.
  A SQL front end is the obvious candidate and the reason it is not built is written in
  `Cargo.toml` and in ADR-0002 — a dependency inferred wrongly from SQL text is a wrong app,
  and a wrong edge is worse than a verbose one. That constraint is the design problem, not an
  excuse to skip it.
* **Streamlit or marimo shipping value-equality backdating** — a rerun that produces an
  identical value not repainting downstream. COMPETITORS.md §3 records that nobody here has
  checked whether marimo already does this. If it does, the claim in §1 narrows again and
  this file gets rewritten — which has already happened once to this project's own framing
  and should happen again the moment the evidence says so.
* **`marimo#1831` closing.** The strongest live artifact in favour of this module existing is
  a two-year-old open issue in a competitor's tracker. If a maintainer fixes stateless
  multi-container scaling, condition 1's buyer loses their pain, and §2 becomes decisive
  without qualification.
