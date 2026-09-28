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

* ~~**Column-granular invalidation.**~~ **Built.** This entry read "nothing in this
  repository does sub-node invalidation and no README may say otherwise" for the life of the
  project. It is now false: a cell reading 3 of 200 columns does not invalidate when column
  47 moves, and `dagpane explain --change-column` prints what that saves on a 122-column
  example. The predicate half is built too: a filter change that does not move the result set
  costs nothing. ADR-0006 has the design, the two conditions a constraint rests on, and what
  it costs.
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

**Met on the numbers, and the numbers are in `BENCHMARKS.md`.** This paragraph read *"not
met"* while it said no figure existed; all three now do.

* **Apps-per-core: 224**, one core, 448 concurrent sessions, 1 908 interactions/s, p99 inside a
  250 ms budget, four independent runs. `benches/loadgen/`.
* **p99 under N concurrent viewers**, measured on two rigs — a browser rig that saturates
  before dagpane does and says so, and a protocol rig 350× finer that does not.
* **Memory: 0.25 MB per additional app** under `dagpane host` on marginal PSS, against 94.8 MB
  for Streamlit and 106.6 MB for marimo — 385× and 433×. `benches/fleet/`.
* **`dagpane host` is the multiplexing story**, and it is what makes that 0.25 MB figure a
  figure rather than a hope: 400 apps is one process. The line that used to stand here — *"the
  multiplexing story does not exist"* — has been false since `crates/host` shipped.

**What is still honest to say against it** is narrower and it is about a term nobody had
priced. A session's cost is what its pipeline **materialises**, not what its app sources: on
identical data, a row-keeping pipeline costs 7 248 kB per viewer and an aggregating one 145 kB,
50× apart. The sources really are shared — that is what the 145 kB proves — but a hundred
viewers of a row-keeping app are a hundred materialised frames, and `--budget-mb` counts
neither. So the apps-per-core figure is a figure about *apps*, and a plan that multiplies it by
viewers without reading §"What one more viewer costs" will be wrong in the direction that
costs money.

### Condition 2 — nobody writes Rust

The authoring surface must be declarative, with Rust as the invisible engine. The moment the
README shows a Rust closure as a cell body, the addressable market is Rust programmers who
want dashboards.

**Partly addressed, which is to say not met.** `examples/sales.toml` is a complete app —
sources, inputs, cells, panes — with no Rust in it, and the README can lead with it honestly.
The ceiling is exact and small: a cell's pipeline is drawn from **nine verbs** (`filter`,
`derive`, `join`, `select`, `sort`, `limit`, `group_by`, `scalar`, `count`) over five widget
kinds.
A cell may also be written as a `select`, in a **closed dialect that lowers into those same
nine verbs** rather than running on an engine of its own (ADR-0005). The edge rule did not
bend to admit it: a statement's tables come from its resolved parse tree and never from
scanning text, because a dependency inferred wrongly from SQL text by a regular expression is
a wrong app.

`derive` and `join` are the eighth and ninth, and they are the first two items on P5's list.
`derive` removes the ceiling that `margin = revenue - cost` had to be pre-computed somewhere
the app's author could not edit; `join` removes the one that two tables could sit on a page
and never meet on a row. Neither gives anything up: a bare name in an expression is a
**column** and a cell reference is written `$name`, a join's second table is a cell named in
`with`, and the compiler builds edges from those tokens and from nothing else. The vocabulary
computes and still declares.

What it does not remove: no aggregates inside an expression, no window functions, no `full`
outer join, no range or inequality join, and the tenth verb an author needs is still a
`GraphBuilder::cell` closure in Rust. Evidence proved SQL + Markdown is a viable BI authoring
surface and Mosaic proved params/selections is a viable interaction spec; nine verbs with a
one-row expression language and an equi-join is nearer to those than seven verbs was, and is
not yet either of them.

### Condition 3 — the differentiator is sub-node invalidation, not node-level reactivity

Node-level reactivity is marimo's, shipped and free. Column-and-predicate-level invalidation
— comemo's constrained memoization applied to a columnar frame — is something neither marimo
(Python mutation is untrackable) nor Streamlit (fragments *"can't detect a change in input
values"*) can do, and is reachable only because Rust can make access observable.

**Met.** The dossier's test for this condition was a
measured result of the form "changed one column of a 200-column frame, recomputed 2 of 40
nodes". The number this project prints today is:

```
changed 1 column of a 122-column frame; recomputed 0 of 11 cells
```

— on `examples/apps/19-wide-telemetry.toml`, for a column the page does not read, with the
whole table having genuinely changed. Moving a column three cells filter on recomputes three.
That is the form the dossier asked for, on a real app, out of the tool rather than out of a
benchmark harness.

The oracle that was described here as a foundation is now load-bearing: `crates/core/tests/oracle.rs`
gained 120 random graphs over wide frames with one column rewritten per interaction, asserting
both correctness against a full recompute and that no cell reading none of the moved column
ran. Four deliberate mutations of the mechanism were run against it and each is caught.

The predicate half — "column- **and predicate**-level invalidation", as the condition is
worded — is built as well. Three cells on that app filter on `cpu0_user`; move every value in
it and, at a floor of 40, nothing recomputes, because the same 129 of 204 rows clear the floor
before and after. At 35, three rows cross and four cells run. Same column, same edit: what is
compared is the question, not the column.

`sort` carries the same kind of constraint on *order* rather than on membership. An eleventh
cell on that app ranks the scrape by `cpu0_user` and averages memory over the top ten; it is
the one cell that sleeps through the 35 edit while the three filtering cells run, because
nudging every value leaves the ranking exactly where it was.

Two honest limits sit under that. Only the first row-changing step of a pipeline can carry a
constraint — a correctness rule with a constructed staleness case behind it rather than a
simplification — and the ordering constraint's conditions are met far less often than the
predicate one's: nineteen of the twenty bundled apps sort after a `group_by`, and every
leaderboard in the corpus displays the column it ranks by. ADR-0006 records both, and what
turning the feature off does and does not change.

### Condition 4 — Perspective and Mosaic are cited as ancestors on page one

**Met for COMPETITORS.md, unverified for the README.** COMPETITORS.md §2 names Perspective
before any comparison and states that it already ships the symmetric client/server
architecture dagpane was going to claim; it names Mosaic as the peer-reviewed answer where an
app is SQL over DuckDB and says dagpane has nothing to add there. Whoever writes the README
inherits that obligation. A reader who finds Perspective before this project mentions it has
already decided what kind of project this is.

**Score: two of four fully met — 1 and 3 — and two partly, 2 and 4.**

**That line read "zero of four fully met" for a round after condition 1's verdict above was
changed to met**, which is the same failure this file's own §3 preamble warns about from the
other direction: a grade is only honest while it is maintained, and a summary that outlives the
thing it summarises is worse than no summary, because a reader trusts it instead of reading.
The rule that would have caught it is the one `benches/` keeps relearning — **when a verdict
changes, grep for every sentence that totals it up.**

The score still is not four, and the two that are short are short for different reasons, which
an earlier draft of this paragraph flattened into "both are writing, not work."

**Condition 2 is product evidence, not prose.** §3 above says what is missing and it is not a
sentence: the tenth verb an author needs is still a `GraphBuilder::cell` closure in Rust, there
are no aggregates inside an expression, no window functions, no `full` outer join. Until a
non-Rust author can build the app they actually want, the condition is unmet by the code and no
amount of writing moves it.

**Condition 4 is writing**, and specifically the README naming its ancestors on page one, which
`COMPETITORS.md` §2 already does and the README inherits. That one really is a paragraph.

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
* **An authoring surface past nine verbs** that still produces edges statically.
  Condition 2. `derive` and `join` were the first two moves on this and they are the shape
  the rest has to take: they compute, and they still declare, because a cell reference is
  written `$name` or `with` and a bare name is a column — so the compiler builds edges from
  tokens rather than from a guess about what a name might be. The SQL front end is the third
  and it took the same shape: ADR-0005 records why its grammar is closed rather than borrowed,
  which is that a dependency inferred wrongly from SQL text is a wrong app and a wrong edge is
  worse than a verbose one. That constraint was the design problem, and it was not an excuse
  to skip it.
* **Streamlit or marimo shipping value-equality backdating** — a rerun that produces an
  identical value not repainting downstream. COMPETITORS.md §3 records that nobody here has
  checked whether marimo already does this. If it does, the claim in §1 narrows again and
  this file gets rewritten — which has already happened once to this project's own framing
  and should happen again the moment the evidence says so.
* **`marimo#1831` closing.** The strongest live artifact in favour of this module existing is
  a two-year-old open issue in a competitor's tracker. If a maintainer fixes stateless
  multi-container scaling, condition 1's buyer loses their pain, and §2 becomes decisive
  without qualification.
