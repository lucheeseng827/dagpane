# ADR-0011: a refresh is a new graph, and every viewer moves onto it

**Status:** accepted.

## Context

A manifest has been able to say `refresh_secs = 300` on a source since sources existed, and
nothing acted on it. `dagpane refresh` re-reads a manifest's sources in a process of its own;
a running server read them once, when the app compiled, and `OPERATIONS.md` called that a
design decision — *"it is what makes a hundred viewers a hundred vectors of values over one
allocation per source, and what makes a pass a pure function of its inputs."* Following data
that moved meant a restart.

Both properties are worth keeping, and the obvious implementation of a schedule breaks one of
them without failing a single test:

* **Staging the new table on each session** — `session.set("sales", rows)`, once per viewer —
  is the code `dagpane refresh` already has, generalised. It is correct on every value. It also
  hashes the table once per viewer, because a session weighs what it is given, and it leaves
  every viewer holding the graph they were opened on — which holds the rows the app compiled
  with. A page that sat through ten refreshes would pin the data from when it opened; a fleet of
  long-lived pages would pin one copy per refresh any of them had seen.
* **Recompiling the app** gives new viewers a clean graph, and gives live ones nothing: a
  session cannot move between two compiles, because nothing can show that two sets of closures
  compute the same thing, so every live viewer would either stay on the old rows or reconnect
  and recompute everything.

## Decision

### 1. The new rows are a new graph, built once

`Graph::with_sources` is this graph with some sources' starting values replaced. Everything
else is **shared**: every computation is the same `Arc`, every source not named keeps its
allocation, and the cells, edges and heights are this graph's. The new rows are weighed — their
digest, and per-column digests for a frame — once, there, exactly as a graph weighs any starting
value. A value whose digest equals the one its source holds is not a change: when every value
is like that, the answer is `None`, and an unchanged refresh costs every viewer nothing.

### 2. A live session moves onto it

`Session::rebase` moves a session onto a graph of the same shape — same cells, edges, declared
types, and computations by allocation — and stages each replaced source with the graph's
already-weighed value, adopting its allocation. The commit after it is an ordinary pass: what
reads the new rows recomputes, nothing else is visited, and the session lets go of the graph it
left. **A source the session set itself is kept** — by allocation, not by content, so a value it
has staged is its own even when it equals the default — which is what keeps a viewer's filter
across a refresh of the data under it. A committed value equal to the one it replaced changed
nothing and was never installed, so it is not the session's: the slot holds the graph's
allocation, and follows the next refresh. The same goes for rows that come back — A, then B,
then A again as a new copy, moved through before one commit: nothing recomputes, and the slot
takes the newest graph's copy, so the first one is freed and the next refresh is followed.

That is the whole of what the engine had to learn. A refresh is a new value of a source cell,
the same event as a viewer moving a control, and a pass is still a pure function of the values
in the session.

### 3. A refresh changes rows, never their shape

`refresh::read` refuses new rows whose columns differ from the ones the app holds — one gone,
renamed, reordered, or read as another type — and the app keeps the rows it had. Every cell was
checked against those columns when the manifest compiled; a change of shape is a deploy, with
`dagpane check` in front of it. For a restart this was a documented gap — *"a CSV that still
parses but has lost a column the app filters on compiles cleanly and fails at run time"* — and
a schedule that applied it would open the same gap with no restart anywhere near it.

### 4. The schedule lives where the clock is, and runs while somebody is looking

`dagpane-core` has no clock by design, `dagpane-app` has none, and CI forbids `dagpane-host` a
runtime. So `crates/serve` owns the schedule — one per app being viewed, per *admission* under
`host` — started by the first viewer with an immediate check, stopped when the last one leaves.
Reads run off the async workers; every connection moves onto the new generation between its
messages, never inside a pass the viewer asked for, and is sent a `Reloaded` frame holding only
the panes that moved in *their* session — or nothing.

Under `host`, `Host::reload` replaces the admission's resident slot under the same key and the
same admission: the next viewer opens on the new rows, the budget is spent against what the app
holds now, and growing evicts others as an admission would. `SessionLease::rebase` moves each
viewer's baseline onto the new rows, without which every viewer would be billed for the
refreshed table as their own.

### 5. New rows go only to a token that still holds

A token is checked when a connection opens, and not again — `dagpane-auth` makes no outbound
request, so there is nothing to re-check it against but its own `exp`. Before a schedule
existed, that also bounded what a connection could see: new data meant a restart, and a restart
meant coming back through the door with a token valid then. A schedule brings new data to an
open socket, so it would have quietly turned a short token lifetime — the control `SECURITY.md`
names for revocation — into no bound at all on whose page shows tomorrow's rows.

So whenever a connection would be moved onto a new generation — as it opens, if a refresh
landed between the door and the socket, and at every refresh after that — it asks
`Viewer::expired`, which uses admission's own arithmetic (`exp` plus the policy's leeway, in
whole seconds): a connection whose token has run out is closed with WebSocket code `4401`
instead, and the subject is logged. What it already holds was its to see, and it keeps that until it closes; only new data
is withheld. The bundled page reads `4401` as "sign in again" rather than as a server that went
away, so it says so once instead of retrying a door the same token cannot open.

## Consequences

* **One allocation per source, still** — per generation. The old rows are freed when the last
  viewer has moved, which is milliseconds after the read for every connection that is reading.
  For that moment both copies exist, so a scheduled source needs room for twice its rows;
  `OPERATIONS.md` says so. A connection whose client has stopped reading is stuck in a send,
  is moved only when the send completes, and keeps its rows until then — at most the write
  deadline, 30 s by default plus what a slow link would need for the frame, after which it is
  dropped. That deadline came in its own change, after this one, because it bounds every
  connection and not only refreshing ones.
* **The wire has a fourth server message**, `Reloaded`, sent unasked. It carries no `seq` and
  must not be taken for the reply to a `Set` in flight; the bundled client paints it and leaves
  its in-flight flag alone.
* **`Session::rebase` and `Graph::with_sources` are public**, and `SessionError::OtherGraph` is a
  new variant — a session offered a graph of another shape refuses it and changes nothing.
* **`dagpane_app::source_builder` is public**, because a refresh has to load in the
  representation the compile chose; a different one would digest the same and weigh differently,
  and `dagpane refresh`'s own copy of that choice is gone.
* **A schedule polls.** It is an interval, not a watcher, and a `sql` source is a query per
  interval while anybody is viewing. One second is the floor.
* **A connection closes when its token has expired and new rows arrive** — code `4401`,
  `dagpane_serve::TOKEN_EXPIRED` — and `dagpane_auth::Viewer::expired` is public. Behind no front
  door nothing changes.

### How it is checked

* `crates/core/tests/refresh.rs` — sharing, identical data, the session's own values, the old
  graph released, a graph of another shape refused, and moves made before one commit: two, rows
  that come back, and 200 random runs of one to four, each checked against a viewer opened on
  the newest graph. Six mutations, each caught.
* `crates/core/tests/oracle.rs` — 480 refreshes over random graphs and wide frames: a session
  that sat through every one agrees cell for cell with one opened on the newest graph, lets go
  of every graph it left, and wakes only cells that read the rewritten column.
* `crates/app/tests/refresh.rs`, `crates/host/tests/refresh.rs` — read once, many viewers, the
  shape refusal, the slot, the budget and the leases.
* `crates/serve/tests/refresh.rs` — the plan's exit criterion over real sockets: with viewers
  open, a change repaints each at their own filter and no change repaints none.
* `crates/serve/tests/door.rs` — under `run` and under `host`, a viewer whose token holds is sent
  the new rows and one whose token has expired is closed with `4401`; a unit test in
  `crates/serve/src/refresh.rs` holds the same rule at the moment a connection opens, a window
  no socket test can aim at; `crates/auth/tests` pins `Viewer::expired` to admission's
  arithmetic, to the second. Six mutations, each caught.
* `crates/serve/tests/reconnect.mjs`, in Chromium — the page says the session expired, keeps what
  it was sent, and does not retry. Removing the page's `4401` branch fails it.

### What is not built

* **A freshness label.** The page says *new data in sales* when a refresh moves something, and
  nothing when it does not; "these rows were read at 14:02" is a pane-level feature nobody has
  asked for yet.
* **An operator override.** The manifest's interval is the one used. A flag to turn schedules
  off, or to cap them, waits for somebody who needs it.
* **A schedule with no viewers.** A fleet's idle apps are not polled. A source with a cost per
  read is the case for it, and the case against.
