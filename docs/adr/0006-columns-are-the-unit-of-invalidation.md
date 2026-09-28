# ADR-0006: a column is the unit of invalidation, a rule about rows is finer still, and silence is whole

**Status:** accepted · **Date:** 2026-09

## Context

`ROADMAP.md` §3 held the same sentence for the life of this project: the only technical
differentiator this module has that survives contact with marimo is invalidation finer than a
node, Python cannot do it, and **it was not built**.

The engine compared whole values. A cell reading three columns of a 200-column frame
recomputed when any of the 200 moved. That is correct and it is expensive, and on a table
nobody designed — an exporter's scrape flattened one column per metric — it is most of the
work an interaction does.

The prior art is Typst's `comemo`: `#[track]` makes a type's accesses observable, `#[memoize]`
records which parts of an argument a computation touched, and a cached result survives a
change to the parts it did not read. The question was not whether to do that. It was where to
put the observation, and what happens when the observation is wrong.

## Decision

### 1. Accesses are recorded per step, not per cell read

The obvious reading of "make accesses observable" is to record a column index inside
`Frame::value_at`. That is wrong here, and not marginally: `value_at` is the innermost loop of
every verb, and a set insert per *cell* of a table would cost more than the recomputation it
saves.

What is recorded instead is one claim per pipeline step. `core::reads::ReadLog` is where a
compute says "my output depended on these columns of that input", and `app::reads` works the
answer out by tracking, through the nine verbs, where each column of the frame in hand came
from. Once per step, never once per row.

This is possible because the verbs are nine and closed — the same property ADR-0005 leans on.
An open transformation language would have to fall back on observing reads, and would pay for
it in the loop.

### 2. Row membership is a dependency, separately tracked

The rule most easily got wrong, and the one that would be silent.

A `filter` copies no data. Every value in its result was already there, so it changes no
column's *origins* — and an analysis that tracked only origins would conclude that a cell
downstream of `filter(region = "north")` does not depend on `region`. It does: `region`
decided which rows are there at all.

So the analysis carries two things: where each column came from, and which input columns
decided the row set. The answer is the union. `count` is the case that shows why they are
separate — it depends on the row set and on no column's data whatsoever, which over a wide
frame is the largest single win available.

### 3. Silence is the whole value, and that is the safety argument

Every input starts at "compared whole" and a compute must actively narrow it. A compute that
says nothing gets exactly the behaviour this engine had before any of this existed.

That ordering is the entire safety case. The failure mode of a granular scheme is
*under*-invalidation: a cell keeps a cached value after something it genuinely read has moved,
and the user sees a stale number on a page that looks correct. Nobody reports that. So the
default is the safe answer, and every narrowing is a positive claim somebody wrote down —
which is the kind of claim a test can be pointed at.

Three things follow, and all three are in the code rather than in this document:

* **A column set never covers a frame's shape.** Every granular key carries a digest of the
  row count and the full schema. A cell that reads no columns — `count` over an unfiltered
  table honestly is one — still wakes when a row appears or a column is added.
* **Anything unaccounted for widens back to whole.** A join key that does not resolve, a
  column index the frame does not have, an input that is not a frame: each returns the cell to
  a whole-value comparison. Coarse costs a recomputation. Narrow costs a wrong answer.
* **The analysis checks itself.** After every step, the columns the analysis believes in are
  compared against the columns the frame actually has. In a debug build a disagreement is a
  failed assertion; in a release build it surrenders every input. It caught its first real bug
  within minutes of being written — `semi` and `anti` joins add no right-hand columns, and the
  first draft of the join rule appended them anyway.

### 4. A predicate is recorded as a question and its answer, not as a column

The column rule above is still coarser than the truth for one verb. A cell that filters on
`amount >= 400` and totals `revenue` does not depend on `amount`'s **values** — it depends on
**which rows** `amount` selected. Move a value from 500 to 600 and the same rows survive, so
the answer cannot have moved.

So a `filter` records the predicate and the digest of the selection it produced, and a later
pass validates by asking the same question again. This is constrained memoization in its full
form: the key is not a value, it is a query and its result.

Two things keep it affordable. The column's digest is checked **first** — an unmoved column
cannot have moved its selection, so the overwhelmingly common case costs one comparison and
the predicate is never re-run. And when the column has genuinely moved, re-running the
predicate is one pass over one column, against a cell that would otherwise touch every column
of every surviving row.

**Two conditions, and they are correctness rather than tidiness.** A constraint is re-run
against the *input* frame, so it has to mean there what it meant when it was recorded:

* the column it read must be that input's own column, value for value — so a filter on a
  derived column is never a constraint;
* the rows it ran over must be that input's rows — so a filter *after* another is never a
  constraint either.

The second is the one that looks like fussiness and is not. A second filter's selection is a
list of positions in the *first* filter's output, and that list can be identical to a
selection over the whole frame while describing a different set of rows. Construct it — keep
rows {2,3,4}, have the second filter pick positions {0,1} within them, then move the column so
that rows 0 and 1 of the whole frame pass instead — and the recorded `[0, 1]` matches the
re-evaluated `[0, 1]` exactly, while the true answer has gone from two rows to none. The test
named for that sentence in `crates/app/tests/subnode.rs` is that case, and it is stale by two
rows the moment the rule is removed.

### 4b. `sort` carries the same constraint, about order instead of membership

This ADR previously ended §4 with: *"It is deliberately not extended to `sort`. The permutation
of a column whose values moved is rarely the same permutation, so the constraint would cost a
pass and almost never hold."* That was an assertion nobody had measured, and it is wrong.

A `filter` decides *which* rows survive. A `sort` decides *what order* they are in, and
likewise contributes no value: a cell that ranks by `latency` and charts host names depends on
the ranking, not on the latencies. So `sort` records the same kind of key — a question and its
answer — with `transform::ordering` in place of `transform::selection`, under the same two
conditions and with the same cheap check first.

**Why the old reason was wrong.** A ranking is far more stable than the values under it. Every
*order-preserving* change leaves it exactly intact — a uniform shift, a rescale by a positive
factor, a re-baseline, a unit conversion — and a single value moving disturbs it only if that
value crosses a neighbour. Measured, in
`a_top_n_pane_mostly_sleeps_through_a_single_edit_to_its_ranking_column`: a top-five pane over
forty single-row edits to its ranking column sleeps through 16 of them, and through 40 of 40
order-preserving ones. Whole-value granularity scores 0 on both.

**Why the old reason was worth writing down anyway.** It is now the most useful line in this
file: an unmeasured reason not to build something is not a decision, it is a guess wearing a
decision's clothes. The measurement took a morning; the sentence had stood for weeks.

**The two digests are tagged apart.** `selection_digest` is `0xc1` and `ordering_digest` is
`0xc2`, because the rows `[0, 2, 5]` a filter kept and the order `[0, 2, 5]` a sort produced
are different answers to different questions. The engine dispatches on the stored rule, so it
cannot cross them; the tags are the belt to that brace, and
`a_selection_and_an_ordering_of_the_same_rows_digest_differently` holds them apart.

**Where the "first row-changing step" rule is and is not load-bearing, stated because mutation
testing said so.** For a filter it is a correctness rule with the constructed staleness case
above behind it. For a sort, removing it does *not* produce a stale value on any case that
could be found: an ordering covers every row of the frame it ran on, so a permutation of a
filtered frame and a permutation of the input differ in length, and `ordering_digest` is
length-prefixed — the constraint breaks rather than wrongly holding. The search for a
counterexample ran through every row-changing verb (a filter or a limit that keeps everything
reduces to the identity; a `group_by`'s output columns are never passthroughs; a join's row
map is fixed by key columns that are already full dependencies, and being monotone it induces
the same order) and found none.

The rule is kept regardless, for two reasons. It is the same rule a filter obeys, so there is
one condition to reason about rather than two with a footnote; and dropping it would buy
nothing, because the length mismatch means such a constraint could never hold anyway. Making
it pay would mean replaying the pipeline's prefix at validation time, which is a different and
much larger design. `a_sort_after_a_filter_can_never_be_a_constraint` is labelled a cost test
for exactly this reason.

**What it does not buy.** The conditions are met far less often than the predicate ones.
Nineteen of the twenty bundled apps sort *after* a `group_by`, failing both at once — the
sorted column is derived, and the rows are no longer the input's — and every leaderboard in the
corpus displays the column it ranks by. Turning the constraint off changed not one count in
`examples/tools/verify.py` until `19-wide-telemetry` gained a cell written to the shape that
qualifies: rank the raw rows by one metric, report a different one over the top of them. That
shape is real, and it is narrower than the filter case.

### 5. The frame digest is composed from the column digests

`digest_frame` used to absorb one stream of bytes. It is now a Merkle composition: each
column's digest is taken on its own, and the frame's is taken over those. The columns become
separately comparable and a value that needs both still pays for one walk over its data.

The bytes this absorbs are not the bytes the old scheme absorbed. That is fine in a way worth
stating: a digest is only ever compared against another taken by the same process, never
against one written down earlier. What must not change is the rule — equal content, equal
digest — and composing a parent from its children is the standard way to keep that while
making the children comparable.

The in-tree `Table` keeps its own hand-written digest, because it reads `ColumnData` directly
and so hashes a text column without building a `String` per cell. Two hand-written encodings
of one thing drift; the test that holds them to each other already existed, and it failed the
moment this change landed, which is exactly what it is for.

## Consequences

**The claim is measurable and measured.** `dagpane explain --change-column CELL.COLUMN`
rewrites one column of one source and reports what it cost. On
`examples/apps/19-wide-telemetry.toml` — 122 columns, five of them read — moving a column
nothing reads recomputes **0 of 11 cells**, and the table did change. Moving `cpu0_user`, which
three cells filter on and a fourth ranks by, recomputes 4 of 11 at a floor where three rows
cross: the filters' rows moved and the ranking did not.

**A pass-through cell still depends on everything.** A cell whose output is a whole frame
genuinely depends on every column of it, so putting a filtered 122-column intermediate between
a source and the page defeats this entirely. That is not a limitation to work around, it is
the truth about that manifest; the example was rewritten to read the source per cell, which is
how anyone would write a page over a table that wide. It is worth knowing that the shape of a
manifest now has a cost it did not have before.

**Memory.** One digest per column per frame value: 16 bytes times the width, per frame-valued
cell, per session — about 2 KB per frame cell on the 122-column example. A source's is taken
once when the graph is built and shared by every session, as its whole-value digest always
was.

**A filter's cost now depends on the threshold, which is new and worth knowing.** On the
122-column example, with three cells filtering on `cpu0_user` and every value in it moved:
at a floor of 40 nothing recomputes, because the same 129 of 204 rows clear it; at 35, three
rows cross and four cells run. The same edit to the same column costs nothing or costs four
cells depending on where the slider is. That is the correct answer both times, and it means
"how expensive is this app" is no longer a property of the manifest alone.

**A second filter costs what a first one used to.** Only the first filter in a chain can be a
constraint, so `filter ... filter ... group_by` keeps a full dependency on the second column.
That is a real limit rather than an oversight, and §4 above is why.
