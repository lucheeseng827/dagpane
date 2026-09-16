# ADR-0003: Values are compared by a 128-bit content digest, taken once when the value is produced

**Status:** accepted · **Date:** 2026-09

## Context

The engine has to answer one question, over and over: *did this input change?* The values
range from a `bool` to a whole `Table`, and the answer decides whether a compute runs.

Comparing values is O(n) in the size of the value, per input, per dependent, per pass. A
join with three inputs, each holding a 600-row table, walks all three tables before deciding
not to run — which is the wrong shape, because the whole point of the decision is to avoid
touching the data.

Comparing digests is O(1) — two `u128`s — but it costs one hash pass over the value each
time a value is produced.

That asymmetry decides it. A value is produced **once** and compared **once per dependent
per pass**, forever. Hashing at production is paid once, over bytes the cell just built and
still has in cache. Comparison is paid every time, by everything downstream, for the life of
the session. And for a source the production cost is paid once for the whole process:
`Kind::Source` stores `initial: Arc<Outcome>` beside `digest`, both taken in
`GraphBuilder::source`, so a loaded CSV is hashed at build time and not once per viewer.

## Decision

FNV-1a over 128 bits, written in-tree: `Digest(u128)`, `Hasher(u128)`, and a `Digestible`
trait. Seventy-nine lines with the comments stripped. `OFFSET_BASIS` and `PRIME` are the
reference specification's values, regrouped in fours so a reader can compare them digit by
digit against it.

Every produced outcome is digested immediately. `Session::evaluate` does
`let digest = outcome.digest();` and writes it into the slot beside `input_digests` — the
digests of that cell's inputs the last time its compute ran. The reuse decision is then one
comparison of two `Vec<Digest>`, and it is the entire decision:

```rust
if self.slots[id.index()].valid && self.slots[id.index()].input_digests == current {
```

**Why in-tree rather than a hashing crate.** `std::hash::Hasher` is 64-bit, and its `write_*`
methods are free to encode integers however an implementation likes, so a digest taken
through it is a property of the platform rather than of the value. `DefaultHasher`'s
algorithm is explicitly not specified and is free to change between releases; today the
engine only ever compares two digests taken by the same process, so that instability would
not break correctness — but `Digest` already derives `Serialize` and `Deserialize` and
implements `Display`, so it is one commit away from leaving the process, and a number that
silently changes meaning across a toolchain bump has no business on that path. Above all,
this is the one place in the project where a bug is **silent**: a digest that collides shows
a user a stale number on a page that looks like it is working. Seventy-nine lines a reviewer
reads in full are worth more here than a faster function they will not.

Everything hashed goes through `Hasher::bytes` with an explicit big-endian encoding. Two
details do the structural work, and each has a unit test:

* **A one-byte type tag before every composite**, so `Value::Text("1")` and `Value::Int(1)`
  cannot digest alike (`tags_separate_types`).
* **Length-prefixed strings**, so `["ab", "c"]` and `["a", "bc"]` differ
  (`length_prefixing_separates_concatenations`).

`Table::digest_into` hashes column names and column types as well as the data, because a
client renders the header from the schema — a rename or a retype is a change to the value
even when every cell in the table is identical.

**Floats are canonicalised before hashing**, because the engine's contract is "an equal value
does not propagate" and IEEE equality disagrees with bitwise equality at exactly two points:

* every NaN hashes as one canonical NaN, so a cell that recomputes NaN from a different NaN
  bit pattern does not wake its dependents
  (`nan_is_canonical_so_it_does_not_wake_dependents`);
* `-0.0` hashes as `0.0`, because `-0.0 == 0.0` and a user who has seen `0` twice has not
  been shown a change (`negative_zero_equals_zero`).

## Consequences

**This is what contains a declared over-declared edge (ADR-0001).** The cell recomputes,
produces a value whose digest matches the one it already held, and nothing below it runs.
`Trace::short_circuited()` counts exactly those cells, and
`a_recomputation_to_the_same_value_stops_the_pass` asserts the downstream compute runs once
and never again, however many times the input above moves.

**On the bundled example it is visible in one command.** `--set min_amount=400` re-runs
`channels` — the set of channels present, which does not change when the floor rises — and
`top_orders`, whose ten largest orders are all above the new floor. Both produce the value
they already held, `channel_count` reuses on the strength of it, and the patch is 3 of 7
panes.

**Collisions are possible, and the consequence is a missed recompute.** A chance collision at
128 bits is about 2^-128 per comparison and is not a risk this project manages. FNV-1a is
**not** collision-resistant: anyone who can choose a cell's exact output bytes can construct
a second value with the same digest, and the result would be a stale figure on a page that
looks fine. dagpane's threat model does not include such an adversary — a cell's output is
produced by the app author's own manifest, and `dagpane run` binds 127.0.0.1 with no
authentication and no session store. If that ever stops being true, `digest.rs` is the one
file that changes: nothing above it names the algorithm, only `OFFSET_BASIS`, `PRIME` and
`Hasher::bytes` do, and every `Digestible` impl is written in terms of `tag`, `str`, `i64`,
`f64` and `bytes`.

**The bug this decision's first version had, found by the differential oracle.** `CellError`
originally had no `Digestible` impl of its own and an error digested only its **message**. Two
upstream cells failing with the same words therefore digested alike, so a downstream cell
whose *cause* moved from one to the other compared equal, served its cached error, and went
on naming the cell that was no longer the problem. No hand-written test caught it — every one
of them used a single failing cell, and a single failing cell cannot exhibit it.
`crates/core/tests/oracle.rs` found it, because its `Divide` op fails on any zero input and
200 random graphs eventually put two of those under one join. The fix is the `Digestible`
impl on `CellError`, which tags the two variants apart and hashes `cause` before `message`;
the regression test is `a_failure_that_moves_to_a_new_origin_stops_naming_the_old_one`. The
lesson recorded here is not "hash more fields" — it is that anything the engine treats as a
value must digest **everything a reader will act on**, and attribution is something a reader
acts on.

**A value the engine cannot digest cannot be a `Value`.** `Value` has seven variants and
`Table` four column types, and the shortness of both lists is a consequence of this ADR as
much as of scope: every member has to be hashable here, renderable in a browser, and
expressible in a manifest without a type grammar.

**The digest is over the value, not over the computation.** Two cells that arrive at the same
table by different routes hold the same value as far as the engine is concerned. That is
intended, and it is what makes `skip_when` work in the bundled manifest: a filter step that
is switched off returns the table it was given, which digests as the table it was given, so
setting the region dropdown back to `all` runs `filtered` and stops there.

**`Digest::EMPTY` is a real digest, not a sentinel.** It is the digest of an empty byte
stream and also what a slot carries before it has ever been computed, which is why `Slot`
tracks `valid` separately: a value whose digest happened to equal the offset basis would
otherwise read as uncomputed forever.

**A second, independent consumer.** `dagpane-app`'s `view_digest` hashes a pane's serialised
JSON with the same `Hasher` to decide whether the pane goes on the wire. The two decisions are
deliberately separate: a table pane sends its first fifty rows, so a change in row nine
thousand changes the cell and not the view, and building the patch from the trace alone would
send that pane anyway and quietly make the engine's numbers look better than the wire does.
