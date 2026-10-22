# Security policy

A runtime whose whole product is *what it did not recompute* has a failure mode that is
worse than a crash: a pane showing a number the app no longer computes, on a page that
looks like it is working. Nobody reports that as a bug, because nothing looks wrong. Most
of this document is about it.

The other half of the posture is one sentence and it is stated before anything else, in
case a reader stops here: **authentication is off unless you turn it on.** `dagpane run`
binds `127.0.0.1`, `--host` anything else prints a warning naming the consequence, and
`--auth-jwks` puts a front door on. Without that flag, anyone who can reach the address can
read every pane of the app.

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

The client touches `innerHTML` nowhere — the stats bar and a `rejected` message are built
the same way as everything else — and `the_client_never_writes_server_text_as_markup` fails on
any line that names `.innerHTML`, so adding one fails a test rather than waiting for a
reviewer. That was already so in 0.1.1. A way to get server text parsed as markup anyway is a
finding.

**One viewer's value reaching another viewer's page.** A session is a vector of value slots
beside a shared, immutable `Arc<Graph>`; sources are shared by `Arc` and a session that
sets one gets its own slot. `sessions_over_one_graph_do_not_share_values` and
`two_browsers_do_not_share_a_session` assert it from opposite ends. A path by which a
`set` on one connection changes what another connection is shown is a finding.

## Known weaknesses, stated

These are documented, not findings. A report that one of them exists gets this section back.

**Authentication is optional, and off is still the default.** `--auth-jwks` puts a front
door on: tokens are verified against a JWKS the operator supplies as a file, with the issuer,
the audience, the algorithm allowlist and the per-app rule all named on the command line.
Without it there is no login and no authorisation, and the CLI prints, for any bind that is
not loopback:

```
dagpane: warning — binding 0.0.0.0:8787, which is reachable from outside this machine,
         with no authentication. Anyone who can reach that address can read every
         pane of this app. Pass --auth-jwks to put a front door on it, or put one in
         front. See SECURITY.md.
```

What the door does **not** cover, stated because the gap is the interesting part:

* **Per-pane authorisation.** Access is per app. A viewer who may open an app sees every
  pane of it.
* **An audit of who read what.** A refusal is logged with the subject; an accepted
  connection is not.
* **Revocation before a token expires.** There is no introspection call and no deny list —
  again, because this process makes no outbound requests. `--auth-leeway-secs` is small and
  the practical control is a short token lifetime at the provider.
* **A connection that outlives its token.** A token is checked when the socket opens, and
  not again. A page open when its token expires keeps what it was sent and can go on moving
  controls over the rows it holds until it closes. What it is not sent is *new* data: when a
  scheduled refresh reads new rows, a connection whose token has expired is closed with code
  `4401` instead, and the bundled page asks the viewer to sign in again. A short token
  lifetime bounds which data a viewer can see, not how long a page stays open.
* **`/` itself.** The page is served without a token, because the page is where the sign-in
  starts. It carries no data: every value arrives over the socket, which is the thing that
  checks. Under `dagpane host` the request does more than serve a file — it compiles the app
  its name routes to if that app is not resident, and may evict others to make room, before
  any token is asked for. *`dagpane host` and DNS rebinding*, below, says what bounds that.

The bundled `Dockerfile`'s header runs the image as `dagpane serve` with an `--origin`, and
`serve` binds every interface by default, because inside a container that is the only useful
choice — so the warning above is printed unless `--auth-jwks` is passed. The header names both
answers: pass `--auth-jwks`, or put something that authenticates in front, before the container
is reachable by anyone you would not show the data to.

**The token never goes in a URL.** It travels in `Authorization: Bearer` for anything that
can set a header, and in a `Sec-WebSocket-Protocol` entry for a browser, which cannot. A
query string is logged by every proxy in the path, and a credential in an access log is a
credential — so `?t=` is refused even though it would have been the easiest thing to write.
In the browser the token lives in `sessionStorage`: it dies with the tab, and it is never
written to the fragment beside the saved state.

**Two JWT attacks are closed by construction rather than by a check.** `alg: none` and
`HS256` signed with the verifier's own public key are the two that have cost people their
systems, and both rely on a verifier trusting the token's own header. `dagpane_auth::Algorithm`
has no variant for either, so an operator cannot allowlist them; the JWKS loader skips `oct`
keys, so there is no secret in the key set to HMAC against. `crates/auth/tests/verify.rs`
mounts both attacks and asserts the refusal.

**A refusal tells the caller almost nothing.** Wrong audience, expired, unknown key and a
bad signature all produce the same `401` and the same sentence; only "you may not open this
app" is distinguished, as a `403`, because that viewer has authenticated and sending them
back to a login would be a loop that cannot succeed. The detail goes to the server log, where
the operator is rather than where a prober is.

**A viewer's control values travel in a URL.** A session is durable because the viewer
carries it: the client keeps its input values in the page's **fragment** and hands them to
the server on the socket's query string. Two consequences, and the first is the one that
matters:

* A **fragment** is never sent to a server, so it does not appear in an access log, a
  referrer header or a proxy's request log. That is why the durable half lives there.
* The **query string** on the `/ws` upgrade does carry them, and anything terminating TLS or
  proxying in front of this can log it. If an app has a free-text input and a viewer types
  something sensitive into it, that string can end up in a reverse proxy's log. There is no
  mechanism here that stops it: do not put a secret in a control, and if an app's controls
  are sensitive, turn off query logging on whatever sits in front.

What a saved state **cannot** do is reach anything a `Set` could not. It goes through the
same two predicates — the widget must accept the value, and the cell must be a source of a
compatible type — because a resume is deliberately not a privileged path. `total` is a
computed cell and a saved state naming it is dropped and reported, not applied;
`crates/serve/tests/resume.rs` asserts both. The size of a saved state is capped
(`dagpane_app::resume::MAX_ENCODED_BYTES`) and an oversized one is refused whole rather than
truncated, so a viewer never gets half a remembered state with no way to see which half.

**Loopback is a boundary against a browser page, and not against anything else.** For a
single app (`run`, and `serve` without `--origin`) the `/ws` upgrade checks `Origin` against an
allowlist derived from the bound address — both schemes, plus `localhost` when the bind is
loopback — and answers `403` otherwise. That closes the case this section previously said was
open: a page a viewer happens to have open elsewhere in the same browser can no longer
connect to `ws://127.0.0.1:8787/ws` and read every pane, because a page cannot suppress its
own `Origin`.

What remains true, and is why this is still listed here rather than deleted:

* A request with **no** `Origin` is allowed, because that is a non-browser client — `curl`,
  a script, the integration tests. Anything that can open a socket on the machine can still
  read every pane. "It only binds loopback" means "only software on this machine can read
  it", never "only you can read it".
* `GET /` is **not** origin-checked at all. For a single app it serves a static page and no
  data, but it is not a boundary either; under `dagpane host` it also compiles the app its
  name routes to, which *`dagpane host` and DNS rebinding*, below, bounds.
* This is an anti-CSRF measure and not authentication. Where `--auth-jwks` is configured it
  complements authentication and does not replace it; without that flag it is the only check
  the socket makes.

It also has a deployment consequence worth stating where operators will find it: behind a
reverse proxy on a different hostname the browser sends that hostname, which never matches
the bound address, so the page loads and the socket is refused. `OPERATIONS.md` has the
verified matrix and the supported shapes, and `docs/RUNBOOK.md` §1 is the symptom. The shape
that needs no rewrite is `dagpane serve --origin <the URL users type>`: the declared origins
replace the derived allowlist, so the proxy forwards `Origin` as the browser sent it. `run` has
no `--origin`, and its two shapes are a loopback tunnel or a proxy that allowlists `Origin` and
then rewrites it. Under `dagpane host` the socket compares `Origin` with the `Host` header
instead, so most of that trap goes, and another takes its place: the name has to be declared.
Behind a proxy that forwards `sales.example.com`, every app answers `404` until the process is
started with `--domain example.com`.

**`dagpane host` and DNS rebinding.** `dagpane host DIR` serves every manifest in a directory
on one port and picks the app by the `Host` header. Until ADR-0012 (October 2026) it picked by
the **first label of any name** — `sales.example.com`, `sales.localhost`, a bare `sales` and
`sales.<anything>` all reached `sales.toml` — and its socket's `Origin` check asked only that
`Origin` name the same host as `Host`. Together those let a page the viewer merely opened read
every pane. A page at `http://sales.attacker.example:8787` whose owner then points that name at
`127.0.0.1` — DNS rebinding — is still, to the browser, talking to its own name: it sends
`sales.attacker.example:8787` as `Host` and `http://sales.attacker.example:8787` as `Origin`,
the two agree, and the first label routed it to `sales`. `GET /` for that name compiled the
app as well, and could evict others to fit it. `run` and `serve` were never open to this,
because their allowlist is the bound address or `serve`'s declared `--origin`s, and a rebound
page's `Origin` is its own name. No release had the hole: `dagpane host` first shipped in
0.2.0, with the rule below. But `main` was public with it before that, so a build from `main`
from before `--domain` existed has the hole.

**The rule now: a name is answered only if it is exactly one label in front of a declared
domain** — `<app>.localhost` always, and `<app>.<d>` for each `--domain d`. One trailing dot is
ignored, so `sales.localhost.` is `sales.localhost`. Membership is exact equality on everything
after the first dot, never a suffix test, so `salesexample.com`,
`sales.localhost.attacker.example`, `x.sales.localhost`, a bare `sales`, `localhost` itself and
an address (`127.0.0.1`, `[::1]`) are all refused. A refused name is answered `404` with
`no app is served at <name>: this server does not answer to that name`, and nothing is read,
compiled or evicted for it: the check is the first thing `DirAppSource::resolve`
(`crates/host/src/source.rs`) does, and `Host::open` and `Host::open_session` resolve before
they do anything else. (At `/ws` the `Origin` check and any token check come first, and neither
touches an app.) A declared name with no manifest behind it, such as `nobody.localhost`, still
gets `no app is served at <name>`, so the two refusals point at different fixes.

The boundary is the contract on `AppSource::resolve`, and it binds every source, not only the
directory one: **a source must answer only names its operator declared.** An exact table, as
`MemAppSource` keeps, is bounded by construction; a pattern — a label, a suffix, a fallback app
— must be bounded to declared names and refuse before any I/O. A name answered that nobody
declared, or a manifest read, compiled or evicted for a refused one, is a finding.
`docs/adr/0012-a-name-is-served-only-where-it-was-declared.md` records why the check lives in
the source and not beside it.

**`--domain` is how `host` is published under a name.** `dagpane host DIR --domain example.com`
makes `sales.toml` answer at `sales.example.com` as well; the flag repeats, and `localhost` is
always answered in addition. Case, one trailing dot and a leading `*.` are accepted as spellings
of the same domain — `*.example.com` is `example.com`, and either means exactly one label in
front of it. Anything else is refused at start-up, with the fix or the reason named, before
anything is bound or any `http://` printed: a URL, a comma-separated list, a path, an IP
address, a port, a leading dot, any other `*`, a non-ASCII name (write its punycode form), an
invalid label, a name over 253 bytes, a numeric last label (which a browser reads as an IPv4
address), a name ending in `.local`, and a single label other than `localhost`, `internal` or
`test`. So `--domain com` and `--domain uk` are refused because anyone can register a name
directly under a top-level domain, and `--domain local` or `--domain <machine>.local` — what
`hostname` prints on a Mac — because `.local` is multicast DNS, which any machine on the network
may answer for. A forgotten `--domain` shows as every app answering
`404`; stderr names the refused domain once, with the flag that would declare it when it could
be one, and writes at most 32 such lines in all, so whoever chooses the names cannot choose how
much is written.

**Declaring a domain trusts every name directly under it**, because whoever controls a name's
DNS can point it at this machine. Name a zone whose DNS you control. Never declare a shared
wildcard service whose DNS somebody else answers — `localtest.me`, `lvh.me`, `nip.io`,
`sslip.io` — nor a public suffix such as `duckdns.org`, `github.io` or `co.uk`, under which
strangers register names: either reopens this hole for every name beneath it. The parser
refuses a top-level domain; it cannot recognise a multi-label public suffix, so that part of the
rule is this paragraph and not a check.

**Why `localhost` is always answered.** Chrome and Firefox resolve `*.localhost` to loopback
themselves (RFC 6761 §6.3), without asking DNS, so no DNS answer can send a `.localhost` name
anywhere else and a rebinding page cannot arrive under one. A client that forwards `.localhost`
names to network DNS loses that, and a front door (`--auth-jwks`) removes the dependence on it.
It is a guarantee about where a `.localhost` name leads, not about who serves pages there: any
local server can, which is what the next paragraph is about.

**The `Origin` check at the host socket is CSRF defence, not the rebinding defence.** `Origin`'s
scheme must be `http` or `https` and its host must equal the request's `Host` name, as before —
and a rebound page passes that by construction, which is why the boundary is the name. One thing
is new: **under `localhost` the port must agree as well.** When the name is `localhost` or ends
in `.localhost` and the `Host` header carries a port, the `Origin`'s port — the one it names, or
80 or 443 by its scheme — must equal it. A browser resolves `sales.localhost` to loopback
whoever is serving it, so any other local server — a dev server on `:3000`,
`python -m http.server` — serves pages at `sales.localhost:3000`, and with ports ignored such a
page could open `sales.localhost:8787`'s socket and read every pane. Before ADR-0012 it
could; that was reproduced against the built binary. Two limits, both deliberate:

* **When `Host` carries no port, only the name is compared**, because there is nothing to agree
  with. A proxy forwarding nginx's `$host` drops the port, and a browser leaves it out at its
  scheme's default — so the rule protects `<app>.localhost:<port>`, the URL `dagpane host`
  prints on any port but 80, and not a `.localhost` name served on port 80 or 443, or through
  such a proxy.
* **Names under a `--domain` stay port-blind**, because behind a proxy the `Origin` carries the
  public port and `Host` carries this process's: eggrd sends `Host: sales.example.com:8787` with
  `Origin: https://sales.example.com`. Any port of a declared name is trusted — one more reason
  to declare only a zone whose DNS is yours.

An absent `Origin` is still allowed: that is a non-browser client, and a page cannot suppress
the header.

What this does not cover, stated because the gaps are the interesting part:

* **The probe listener has no `Host` check**, and `dagpane host` binds it on every interface by
  default (`--admin-bind 0.0.0.0`, port 9787). A rebinding page aimed at that port can read
  `/healthz` — the resident apps' names, their manifest digests, byte and session counts — though
  no pane data. Keep the port unreachable from browsers: the start-up line already says not to
  publish it, and `--admin-bind 127.0.0.1` keeps it from browsers on other machines but not from
  one on this machine.
* **`GET /` still compiles before any token.** Under `host`, serving the page opens the app —
  compiling it if it is not resident, evicting others if the budget needs the room — and a token
  is checked only at `/ws`. The rule above confines that to the directory's apps under declared
  names; anyone who can reach the port and send one of those names can still make the process
  compile and evict. `/auth`, which tells the page whether a token is needed, answers any name
  too, and holds no app data.
* **A token is checked against the `Host`'s first label**, not against the app the source routed
  to. Under `DirAppSource` the two are the same label by construction; a source that routed a
  name to a differently named app would make them differ.

**No rate limit.** A connection is a sequential loop — read a message, run a pass, send a
patch — so one client cannot overlap passes with itself, but nothing bounds how fast it
sends, how much work each pass costs, or how many connections exist. `refresh` serialises
every pane of the app and costs no recomputation, which makes it the cheapest message to
send and one of the more expensive to answer. There is also no configured limit on inbound
message size: the upgrade is taken with the WebSocket layer's defaults and this document
states no byte figure because the code sets none. How slowly a client *reads* is bounded
whether or not it is being sent something, with both deadlines on as they are by default: a
frame not written within its allowance — `--write-timeout-seconds`, 30 s by default, plus what
a 128 kbit/s link needs for the frame — drops its connection, so a connection stuck sending to
a client that stopped reading holds its session, and under a scheduled refresh the rows it had,
for no longer than that. On a connection with nothing to send, where no write waits, a ping not
answered within its own allowance — `--ping-timeout-seconds`, 30 s by default, plus what that
link needs for everything sent since the last answer — drops it the same way, and so does a
client that has gone without a word; only a Pong echoing the ping counts, which a client can
send only after reading everything before it, and the ping's payload is keyed per connection so
it cannot be echoed by counting. Neither bounds a client that does read: it answers pings and
is kept, like any viewer, however little it asks for. How many connections one client may hold
is the proxy's to limit, beside the rate limit.

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
the `textContent` discipline above, and it is not in 0.2.0.

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

**There is no HTTP client and no database driver in a default build, and CI checks for
both.** A data-app runtime that can phone home is not one anybody self-hosts, and that should
be checkable from `cargo tree` rather than from this paragraph. Through 0.1.1 there was none
anywhere. From 0.2.0 one crate may hold them, `crates/connect` — `ureq` behind its `http`
feature, `postgres` behind `sql`, both off by default — and CI fails if either appears in any
other manifest, is not optional there, or is resolved by a default build of `dagpane-cli`,
`dagpane-serve` or `dagpane-app`. The published image is a default build. The check exempts
dev-dependencies, because `crates/serve` uses a WebSocket *client* to test its own wire.

**The bundled client fetches nothing it was not told to.** It is one file compiled into the
binary: no `<script src=`, no stylesheet `href`, no `@import`, no `importScripts`. Through
0.1.1 it made no request but its socket; 0.2.0's front door, custom panes and `export` added
four, and `the_client_is_one_self_contained_file` lists them and fails on any other. Two
`fetch`es: `/auth`, on the server that served the page, and the token endpoint of a browser
sign-in the operator configured. Two `import`s: a renderer script the manifest declared —
resolved against the page, and refused by `compile_renderers` if it is absolute, climbs with
`..` or contains a `:` — and `./dagpane.js` beside a page `dagpane export` wrote. The same
test enumerates every string beginning `http` and asserts there are exactly four: two in the
message a page opened from `file://` shows, naming `http://` and `python3 -m http.server`;
`https:`, compared against `location.protocol` to choose `ws://` or `wss://`; and the SVG
namespace that `createElementNS` requires. A new one fails the build rather than quietly
making an air-gapped deployment render blank.

`dagpane-core` has one dependency, serde. Eight of the workspace's nine crates carry
`#![forbid(unsafe_code)]`. The ninth, `crates/wasm`, does not, and its `abi.rs` is the one
place in the tree with `unsafe` in it: the WebAssembly boundary an exported page calls, where
a pointer and a length from the page become a slice — one private `unsafe fn` and four
`unsafe extern "C"` exports, with the contract the caller must keep stated at the top of the
file. That module is compiled only for `wasm32`, so it is in an exported bundle's
`dagpane.wasm` and not in the `dagpane` binary or the image. `cargo-deny` (advisories, bans,
licences, sources) and `cargo audit --deny warnings` run on every commit.
