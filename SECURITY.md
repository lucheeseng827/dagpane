# Security policy

A runtime whose whole product is *what it did not recompute* has a failure mode that is
worse than a crash: a pane showing a number the app no longer computes, on a page that
looks like it is working. Nobody reports that as a bug, because nothing looks wrong. Most
of this document is about it.

The other half of the posture is one sentence and it is stated before anything else, in
case a reader stops here: **there is no authentication in v0.1.0.** `dagpane run` binds
`127.0.0.1` and `--host` anything else prints a warning naming the consequence. Anyone who
can reach the address can read every pane of the app.

## Reporting

Open a private security advisory on the repository, or email the maintainer. Include what
you did, what happened, what you expected, and a version or commit. A proof of concept
helps and is not required.

For anything in the engine, a **seed** is worth more than a paragraph.
`crates/core/tests/oracle.rs` generates its graphs from a 64-bit LCG written in-tree for
exactly this reason: a seed printed in an assertion message reproduces on any machine, on
any toolchain, for the life of the project. A failing seed and the interaction sequence is
a complete report.

**Do not open a public issue for a vulnerability.** Do open one for anything in this
document that is wrong or unclear.

## What is in scope, most interesting first

**A client value that reaches a compute without passing its widget's `accepts` check — or
any way to set a computed cell from outside.** This is the highest-severity finding in the
project, because everything downstream of a cell trusts the value in it. Two independent
gates have to fail for one to exist, and a report should say which:

* `AppSession::set` (`crates/app/src/session.rs`) looks up each name in the app's widget
  table — an unknown name is refused with `is not an input of this app` — then calls
  `Widget::accepts`, which bounds a slider to its `min..=max`, a select to its declared
  options, and each of the rest to its type. Every value in a batch is checked **before any
  of them is staged**, so a batch carrying one bad value changes nothing at all; a
  half-applied interaction would leave the viewer's controls and the session disagreeing
  with no way to tell which is right.
* `Session::set_id` (`crates/core/src/session.rs`) refuses a `Kind::Computed` cell with
  `SessionError::NotAnInput` and refuses a value whose type is not compatible with the
  source's declared initial value. The engine does not rely on the app layer having
  checked, and the app layer does not rely on the engine.

`crates/serve/src/lib.rs` turns either refusal into `ServerMessage::Rejected` and keeps the
connection open. Tests: `a_computed_cell_cannot_be_set_from_outside`,
`the_wrong_type_is_refused_at_the_edge_not_inside_a_cell`,
`a_slider_refuses_a_value_outside_its_range`, and, over a real socket,
`a_value_the_widget_could_not_produce_is_rejected_and_the_session_survives`.

**A stale pane.** Anything that makes the runtime serve a cached value when an input it
depends on changed. This is the missed-invalidation class, it is a correctness bug that
looks like a working page, and it is the reason `crates/core/tests/oracle.rs` exists — 200
pseudo-random DAGs, 12 interactions each, and after every one of the 2,400 commits an
assertion that every cell holds exactly what a fresh session computes from scratch with the
same inputs. There are two distinct places it can come from and both are in scope:

* the engine reusing when it should not — `session.rs`'s comparison of a cell's current
  input digests against the ones recorded when it last ran, `graph.rs`'s edges, heights and
  closure, or `digest.rs` itself;
* the app layer deciding a pane's rendered view is unchanged when it is not —
  `view_digest` in `crates/app/src/session.rs` hashes a pane's serialised JSON to decide
  whether it goes on the wire, which is a second, independent consumer of the same hasher.

This class has already produced one real bug, and it was found by the oracle rather than by
review. An error's digest originally covered only its *message*, so two upstream cells
failing with the same words digested alike and a cell below them served its cached error
while naming the cell that was no longer the problem. Every hand-written test used a single
failing cell, and a single failing cell cannot exhibit it. The fix is the `Digestible` impl
on `CellError`, which hashes cause and message; the regression is
`a_failure_that_moves_to_a_new_origin_stops_naming_the_old_one`.

**Script injection through data or a manifest.** Column names, cell values, pane titles,
the app title and subtitle, and cell error messages all reach the browser, and all of them
come from a CSV or a TOML file that whoever deployed the app may not fully control. The
client (`crates/serve/src/client.html`) builds every node with `createElement` and
`textContent` — and `createElementNS` for the two SVG chart shapes — and never pastes
markup. A unit test asserts the file contains no `insertAdjacentHTML` and no
`document.write`, and CI greps for the same strings before a compile.

Two `innerHTML` assignments exist, both in the stats bar, and a way past either is a
finding:

* `showStats` concatenates the `PassStats` counters, which are `usize` and `u64` in the
  wire type. A route by which a non-number reaches one of those fields on the wire is in
  scope.
* the `rejected` branch concatenates `escapeHtml(msg.message)`, which round-trips the
  string through a detached node's `textContent`.

**The guard test is weaker than it reads, and this document says so rather than leaving it
to be discovered.** `the_client_never_writes_server_text_as_markup` asserts
`!CLIENT.contains(".innerHTML = ") || CLIENT.contains("escapeHtml")` — the second clause is
satisfied by the helper existing anywhere in the file, so a *new* unescaped `innerHTML`
assignment would not fail it. Reviewing an added `innerHTML` is a human job today.

**One viewer's value reaching another viewer's page.** A session is a vector of value slots
beside a shared, immutable `Arc<Graph>`; sources are shared by `Arc` and a session that
sets one gets its own slot. `sessions_over_one_graph_do_not_share_values` and
`two_browsers_do_not_share_a_session` assert it from opposite ends. A path by which a
`set` on one connection changes what another connection is shown is a finding.

## Known weaknesses, stated

These are documented, not findings. A report that one of them exists gets this section back.

**No authentication, and no plan for one in v0.1.0.** There is no login, no token, no
per-pane authorisation and no audit of who read what. The deployment answer is that
`dagpane run` binds loopback and the CLI prints, for anything else:

```
dagpane: warning — binding 0.0.0.0:8787, which is reachable from outside this machine.
         There is no authentication in this version: anyone who can reach that
         address can read every pane of this app. See SECURITY.md.
```

The bundled `Dockerfile` needs `--host 0.0.0.0` to be useful at all, and says in its header
to put the container behind something that authenticates before it is reachable by anyone
you would not show the data to.

**Loopback is not a boundary against a browser.** The `/ws` upgrade does not check the
`Origin` header. WebSocket connections are not subject to the same-origin policy and there
is no preflight, so any page a viewer visits — in the same browser, on the same machine —
can open `ws://127.0.0.1:8787/ws`, receive the `init` message, and read every pane. The
default port is 8787 and guessing it is not a defence. An origin allowlist on the upgrade
is the cheapest fix and it is not in v0.1.0; until it is, "it only binds loopback" means
"only software on this machine can read it", not "only you can read it".

**No rate limit.** A connection is a sequential loop — read a message, run a pass, send a
patch — so one client cannot overlap passes with itself, but nothing bounds how fast it
sends, how much work each pass costs, or how many connections exist. `refresh` serialises
every pane of the app and costs no recomputation, which makes it the cheapest message to
send and one of the more expensive to answer. There is also no configured limit on inbound
message size: the upgrade is taken with the WebSocket layer's defaults and this document
states no byte figure because the code sets none.

**A text input is unbounded.** `WidgetKind::Text` accepts any string of any length; the
session holds it for the life of the connection and every cell downstream recomputes over
it. `accepts` bounds sliders, numbers, selects and checkboxes; it does not bound length.

**The manifest is trusted input from whoever deploys the app.** It is not viewer input, and
nothing on the wire can change it. A `csv =` path is resolved against the manifest's own
directory — never the process's working directory, so an app behaves the same whichever
directory it is started from — and is not otherwise constrained: a manifest naming
`../../something.csv` reads that file into a table at start-up. That is not an escalation,
because whoever can write the manifest already decides what the app computes and which
panes show it. Treat a manifest like a program: it is one.

**The digest is FNV-1a over 128 bits and is not collision-resistant.** A chance collision
is about 2^-128 per comparison and is not a risk this project manages. An *adversarially*
chosen collision is achievable by anyone who can choose a cell's exact output bytes, and
the consequence would be a stale figure on a page that looks fine. dagpane's threat model
does not include that adversary, and `docs/adr/0003-digest-equality.md` records both the
reasoning and the fact that `digest.rs` is the one file that changes if it ever does — no
code above it names the algorithm.

**A compute that returns an error is contained; a compute that panics is not.** Errors are
values: a failing cell holds a `CellError`, cells below hold `Upstream { cause, message }`
naming the cell that actually failed, the pass finishes and the rest of the page renders.
There is no `catch_unwind` anywhere, so a closure that panics unwinds out of the pass and
takes that connection's task with it. Every cell a manifest can build returns `Result`; a
cell written against the Rust API is the author's own code.

**No security headers.** `GET /` returns the client page and nothing else — no
`Content-Security-Policy`, no `X-Frame-Options`. A CSP would be a cheap second layer behind
the `textContent` discipline above, and it is not in v0.1.0.

## What is not a finding

* **A cell error rendered in a pane.** Errors are values here on purpose; one bad column
  should take out one number and leave the page, and the slider that will fix it, working.
* **An over-declared edge causing a recomputation.** A cell that reads an input only on a
  branch it did not take still declares it and still re-runs. The digest comparison stops
  the damage at that cell's own boundary — it produces the value it already held and
  nothing below it runs — so the cost is one recomputation and not a cascade. `explain`
  prints it as `same value`. It is a cost, stated in the README and in ADR-0001.
* **The app showing the data it was pointed at.** Every pane in the manifest is a pane the
  deployer wrote.

## The dependency surface

**There is no HTTP client anywhere in the build, and CI greps for one.** A data-app runtime
that can phone home is not one anybody self-hosts, and that should be checkable from
`cargo tree` rather than from this paragraph. The grep exempts dev-dependencies, because
`crates/serve` uses a WebSocket *client* to test its own wire.

**The bundled client fetches nothing at run time.** It is one file compiled into the
binary: no `<script src=`, no stylesheet `href`, no `@import`, no `fetch(`. A unit test
enumerates every string in it beginning `http` and asserts there are exactly two — `https:`,
compared against `location.protocol` to choose `ws://` or `wss://`, and the SVG namespace
that `createElementNS` requires — so a third one appearing fails the build rather than
quietly making an air-gapped deployment render blank.

`dagpane-core` has one dependency, serde. All four crates carry
`#![forbid(unsafe_code)]`, so there is no `unsafe` block in this project to review.
`cargo-deny` (advisories, bans, licences, sources) and `cargo audit --deny warnings` run on
every commit.
