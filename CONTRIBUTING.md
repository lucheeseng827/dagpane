# Contributing

## The short version

```console
$ cargo test --workspace                       # everything. 122 tests on this checkout
$ cargo test -p dagpane-core                   # the whole reactive semantics, no browser
$ cargo test -p dagpane-serve --test socket    # the real wire, over a real socket
$ cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
```

All four have to pass. The second is the one that decides whether a change is correct — it
is a complete test of the engine on a machine with no network, no browser and no async
runtime, and it is that way on purpose. The third is the one people forget, and it is the
only place the client's JSON, the widget checks and the patch diff are exercised together
against something listening on a port.

## If you touch the engine

`crates/core` is where a bug is **silent**. A miscomputed layout is visible; a missed
invalidation is a stale number on a page that looks like it is working, and nobody files
that. Three consequences, and they are rules rather than preferences:

**1. `tests/oracle.rs` stays green, and if you change what a pass is allowed to skip you
add a case to it.** Any change to `session.rs`, `graph.rs` or `digest.rs` is a change to
that question. The oracle generates 200 pseudo-random DAGs of 8 to 40 cells, runs 12 random
interactions on each, and after every one of the 2,400 commits asserts both halves:
*correctness* — every cell's value equals what a brand-new session computes from scratch
with the same inputs — and *economy* — every cell the pass touched is inside
`graph.closure(roots)`.

**2. A count assertion on a hand-drawn graph proves nothing on its own.** Every other test
in `crates/core` asserts a number about a graph the author drew, which makes it a test of
the author's expectations. A scheduler that skips too much passes all of them and is
silently wrong. That is the whole reason the oracle exists, and it has already earned its
place: the error-attribution digest bug in `docs/adr/0003-digest-equality.md` was found on
a random graph after every hand-written test had missed it, because each of those used a
single failing cell and a single failing cell cannot exhibit it. Keep the hand-written
tests — they say what a property *means* and their failures are readable — but do not
mistake them for the check.

**3. The generator's own health is asserted, not assumed.** `oracle.rs` ends by checking
that the corpus produced 2,400 interactions, more than 200 value-equality short circuits
and more than 50 errors. A differential test that never exercised the short circuit or the
error path would be green and worthless, and it would stay that way silently if the op
weights were edited. If you change the generator, keep those assertions honest.

If a change makes the oracle slow enough to be tempting to shorten, shorten the *graphs*,
not the number of seeds. The bug it found needed two failing cells under one join, which is
a thing you get from many graphs rather than from big ones.

## The purity rules, and that CI greps for them

These are properties of the dependency graph, so `cargo test` cannot see them. The lockfile
pins them; it does not defend them. `.github/workflows/ci.yml`'s `invariants` job greps, which also
catches the case where somebody adds the dependency before writing the code that would fail
a test.

* **`crates/core` has no I/O, no async, no clock, no HTTP client and no `unsafe`.** One
  dependency, serde. The banned list in CI is `tokio`, `axum`, `hyper`, `reqwest`, `ureq`,
  `std-async` and `futures`. This is what makes `cargo test -p dagpane-core` a complete test of the reactive
  semantics with nothing listening on anything, and it is what keeps
  `cargo check -p dagpane-core --target wasm32-unknown-unknown` passing — which is the
  entire WASM claim and is worth exactly what it costs.
* **`crates/app` never depends on a server.** No `tokio`, no `axum`, no `hyper`. The patch
  protocol has to be testable with no socket in the process.
* **No HTTP client anywhere in the workspace.** Dev-dependencies are exempt: `crates/serve`
  uses a WebSocket *client* to test its own wire.
* **The bundled client fetches nothing and pastes no markup.** No `script src=`, no
  `@import`, no `fetch(`, no `importScripts`, no CDN host, no `insertAdjacentHTML`, no
  `document.write` — and a unit test in `crates/serve/src/lib.rs` also enumerates every
  `http`-prefixed string in the file and asserts there are exactly the two that belong
  there. CI repeats a subset of that as a grep, because a grep fails before a compile does.
* **Every crate carries `#![forbid(unsafe_code)]`.** There is no `unsafe` block in this
  project and there is no reason for the first one to arrive without a discussion.

## If you change the counts

The published numbers are the product. `.github/workflows/ci.yml`'s `claim` job runs
`dagpane explain examples/sales.toml --set min_amount=400 --json` and compares eight
numbers — 11 cells, 8 visited, 6 evaluated, 1 reused, 4 changed, 3 untouched, 3 panes sent
of 7 — against what `README.md` and `ARCHITECTURE.md` §3 print. It reads the same `Trace`
the server ships to a browser, so it asserts the product rather than a test double.

When that job fails, one of two things is true and neither is fixed by editing the job:
either the change is wrong, or the documents are now wrong and have to be rewritten in the
same commit. A number in a document that no longer matches the binary is worse than no
number, because a reader can check it in one command.

No new number goes into a document unless a command in this repository prints it. There is
no benchmark here, no figure measured in seconds, and no claim about apps per core, memory
under N viewers or latency against anything else. `COMPETITORS.md` §4 keeps the standing
list of sentences this project does not write; read it before writing a comparison.

## No new dependency without an argument in the pull request

The workspace has a short dependency list on purpose, and the header of the root
`Cargo.toml` argues for each *absence* — no polars/duckdb/arrow, no signal or reactivity
crate, no tokio below `serve`, no HTTP client anywhere. Read those paragraphs before adding
something; if your dependency contradicts one, the paragraph is what you have to answer.

## The vocabulary is not decoration

The engine, the CLI, the wire and these documents use one set of words, and mixing them is
how a document starts describing a runtime that does not exist:

* **cell** — a node in the graph. A source cell holds a value; a computed cell has declared
  inputs and a `Compute`.
* **pane** — a view of a cell on the page. Panes are not cells: one cell can have two
  panes, and a pane's view can be unchanged when its cell moved.
* **input** / **control** — a source cell a widget sets. "Input" is also the word for an
  edge's tail (`a cell's inputs`), and that ambiguity is deliberate: they are the same
  relation.
* **pass** — one recompute, from a `commit` to the patch it produces.
* **epoch** — a pass number. Every slot records the epoch at which its value last changed,
  which is how a patch is built.
* **closure** — the cells structurally downstream of what changed. `graph.closure(roots)`.
  What the pass is allowed to touch.
* **reused** — the cell's compute did **not** run, because every one of its inputs held the
  same value as when it last ran.
* **short-circuited** — the cell's compute **ran** and produced the value it already had, so
  nothing below it ran. `Trace::short_circuited()` counts these; `explain` prints
  `same value — nothing below it ran`.

Reused and short-circuited are different events with different costs and must never be
written as if they were one. Never write **re-render**, **reactive update** or **refresh**
for a pass — the first two describe a UI framework this is not, and `refresh` is already a
method on `Session` that means something specific.

## Style

Match what is there. Concretely:

* **Comments say why, not what.** If a constant has a number in it, the comment says how
  the number was chosen and what happens if it moves. `default_max_rows` is 50 because a
  user is always told what they are not being shown; that is the shape.
* **Record the bug in the code that fixes it.** `CellError`'s `Digestible` impl, the
  `changed: bool` on `StepOutcome::Evaluated`, `Slot::valid` beside `Digest::EMPTY` — each
  carries a paragraph naming the failure it prevents. They are there so nobody
  re-introduces one while tidying, and deleting one should feel wrong.
* **Test names are sentences.** `a_diamond_join_never_sees_a_mixed_state`, not
  `test_diamond`. A failing name should say which property broke without opening the file.
* **An ADR for a decision, not for a change.** There are four in `docs/adr/`, each stating
  what breaks if it is reversed. A pull request that reverses one edits the ADR.
