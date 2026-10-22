# ADR-0010: what a viewer holds is reported, not budgeted

**Status:** accepted.

## Context

`--budget-mb` is a ceiling on **source bytes** — the sum of what every resident app's loaded
sources hold — and eviction is the least recently opened app going when an admission would cross
it. `crates/host` states the reason: the sources are what a second *app* adds and a second
*viewer* does not, so they are what a budget over apps can honestly be written in.

`benches/sessions/` then weighed the term that reasoning set aside. A viewer costs what their
pipeline materialises on top of the sources they share: **7 248 kB** a viewer for a pipeline that
keeps 200 000 rows, **145 kB** for one that aggregates the same rows. A fleet of row-keeping apps
with many viewers was being sized against the smaller of its two costs, and nothing in the
process could see the larger one — `Host::session` handed a session out and kept nothing.

`ROADMAP.md` §7 filed it as a design question rather than a measurement: a frame's size is
knowable, but charging it to a session makes eviction a decision about viewers rather than about
apps, and that is a different policy from the one `crates/host` implements.

## Decision

### 1. Price it, allocation by allocation

A session's frames share storage with the app's sources and with each other. A projection clones
a column's `Arc`, `take` over a dictionary column keeps the dictionary, a slice is its parent at
an offset, and a derived column wraps the frame it extends. Summing `memory_size` over a viewer's
frames bills shared storage once for every frame that reaches it — so for an app whose cells
project a source, that sum says the source again per viewer, and it is false.

`Frame::allocations` reports each allocation behind a frame as an address and a size, and
`FrameLedger` counts each address once and skips what it was told is paid for. `SharedBaseline`
is an app's sources, computed once at admission; `SessionFootprint` is a session's slots counted
against it. The default `allocations` reports a frame as one allocation of `memory_size` bytes
at its own address: exact for a backend that owns its storage, and for one that shares and does
not override it, **high and never low**.

### 2. The host sees a session through a lease nobody can forget to return

The host hands sessions out and never holds one — a session is mutated on its connection's task
and nothing else may touch it — so it cannot weigh a session by looking at it. `Host::session`
and `Host::open_session` return a `SessionLease` beside every session, and the host keeps a
`Weak` to it. The connection calls `record` after each pass: an atomic store, with no lock on the
interaction path. A connection that ends drops its lease, and a lease nobody holds stops being
counted without anybody having to say so. **There is no way to get a session from a `Host`
without one**: a path that skipped it would be a viewer the process holds memory for and cannot
see, which is the gap this closes.

### 3. `/healthz` reports it beside the budget

Under `dagpane host`, each resident app's entry gains `sessions` and `session_bytes`, the payload
gains their totals, and `held_after_eviction` lists admissions that are no longer resident and
are still held by a connected viewer, with the source bytes they keep alive.

### 4. Nothing acts on it

The budget stays written in source bytes; admission and eviction are unchanged. The reasons, in
the order they decide it:

* **Evicting an app frees nothing its viewers hold.** A session holds its graph, and through the
  graph the sources. A budget that charged viewers to their apps and evicted apps to get back
  under it would evict, free nothing until those viewers left, and compile a second copy for the
  next viewer who asked — more memory rather than less, at the moment there was too little.
* **The only enforcement that frees memory acts on viewers.** Refusing the next one, or shedding
  one, is a policy about people: who is turned away, at the moment the process is busiest, and
  told what. That is a choice to make deliberately, and nobody has made it.
* **The figure is the frames, not the process.** It leaves out what a connection costs whatever it
  is looking at, and the transient a busy viewer leaves in the allocator. A limit enforced on it
  would be a limit on part of the memory, presented as the whole of it.

### 5. Matched by admission, not by key

An `AppKey` names an app and its manifest bytes, and that is not enough to say which copy a viewer
holds. A key evicted — by the idle sweep, by the budget, or by a deploy that is then rolled back —
and opened again is compiled again: a second copy of its sources under the *same* key, while a
viewer who connected before the eviction still holds the first. Every admission carries a number
unique over the host's life, and every lease the admission it was opened on. Matched by key, the
viewer who sat through a rollback would be booked against the reloaded copy, and the copy they
actually hold would be in neither list.

## How it is checked

* **The ledger.** A frame that shares nothing reports exactly its `memory_size`; a projection of
  a frame already paid for costs nothing; a filtered dictionary column is not billed for its
  dictionary; two slices of one array are one allocation. Replacing either override with the
  default fails these, and so does keying a buffer by its slice rather than by its allocation.
* **The host** (`crates/host/tests/sessions.rs`). A lease is counted while it lives and not after;
  a row-keeping session holds at least its rows' data and less than its source, an aggregating one
  under a hundredth of it; a moved control moves the figure; two viewers share one source and are
  billed for their own rows; an evicted version still held is still reported; and an earlier
  admission is not booked against the resident one.
* **The wire** (`crates/serve/tests/fleet.rs`). Real sockets: sessions 0 → 1 → 0, a figure that
  follows a `set` — deleting the record after a pass fails it — a viewer on a redeployed version,
  and a viewer who sat through a rollback, which matching by key fails.
* **The kernel** (`benches/sessions/ledger.sh`). What the ledger leaves out is **145–162 kB a
  viewer whichever app it is**, while the ledger itself runs from 1.3 kB to 7 034 kB, and the
  kernel's column repeats the held rig's 179 / 7 248 / 145 kB under `host`. Everything that
  varies with the pipeline is on the ledger's side of the line.

## Consequences

* The term that was invisible is a number an operator reads, per app — which is the question when
  one app is the reason — together with the copies the budget has let go of that viewers still
  hold, which `resident_bytes` had stopped counting the moment they left residency.
* `Host::session` returns `(Session, SessionLease)`. A caller that binds the lease to `_` drops it
  at once and gets a session the host does not count; `tests/isolation.rs` says so where it binds
  its own.
* A connection walks its session's cells and their buffers after each pass. It copies nothing and
  hashes addresses rather than contents.

### What is not built

* **A limit on what viewers hold.** `ROADMAP.md` §7 carries it as an open box, for whoever decides
  who gets turned away.
* **The figure under `run` and `serve`.** One app to a process and nothing in it that would read
  the figure; the process's own RSS answers the same question there.
* **The busy term.** What a viewer's pass leaves behind in the allocator is not a frame a session
  holds, so the ledger does not see it. `benches/sessions/busy.sh` is where it is measured.
