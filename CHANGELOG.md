# Changelog

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning: [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Every count in this file was produced by `dagpane explain` on the bundled example —
`examples/sales.toml`, 600 rows of synthetic sales, 11 cells, 7 panes — and **a count only
means something against a named app.** "8 of 11 cells" is a fact about that manifest and
about the interaction named beside it; the same slider move over a differently shaped graph
gives a different number, and a number quoted without its app is not a number. Where a
figure appears below, the command that produced it appears with it.

**Timings follow the same rule, and there are some now.** `dagpane-core` is clockless on
purpose — an engine that needs a clock to be tested behaves differently under a test harness
— so wall-clock microseconds are measured by `crates/serve` around a pass and reported per
interaction. Where a millisecond figure appears below it names the app, the interaction and
the bench that produced it, and `BENCHMARKS.md` holds the machine and the command. None of
them is aggregated into a claim about how fast this runtime is; that claim is not made
anywhere and no number here supports it.

## [Unreleased]

### Added

- **What a viewer costs while they are *using* it — and the memory does not come back.**
  `benches/sessions/busy.sh`, `ROADMAP.md` §7's last open box, and the exclusion the held rig
  wrote into its own method: *"an interaction allocates transiently and would be measured as
  noise."*

  **The instrument is the reason it is measurable.** A pass over the bundled example takes about
  a third of a millisecond, so a sampler reading RSS every 50 ms observes an interaction costing
  nothing roughly a hundred and fifty times in a hundred and fifty-one. The rig reads `VmHWM` —
  a high-water mark the kernel maintains on every page fault — and resets it with `clear_refs`
  per window.

  **A burst's peak becomes the process's floor**, at every rung of all three ramps. It is a
  plateau rather than a leak, and those are two readings that look alike and mean opposite
  things: the identical burst fired again costs about a tenth of the first, and a window ten times
  longer raises the ceiling by about a quarter rather than by ten times. So the charge lands
  **once per viewer who has ever touched a control** — ≈ 12 kB of it on the bundled example,
  ≈ 1 449 kB on the same pipeline over 200 000 rows, and on an aggregating pipeline over those
  same rows a slope whose sign changes between runs, which is flat.

  **The two 200 000-row apps are 50× apart held and differ in *shape* under load.** Past about
  four concurrent viewers — the worker-thread count here — the aggregating app is at its ceiling
  and sixteen times as many viewers do not move it, while the row-keeping app climbs from 37 MB
  to 126 MB. Pinning the server to one core with `taskset` drops the aggregating app's transient
  to **0 kB at every rung**, which is the mechanism stated as an experiment: what a burst has to
  fault in is the *second and later* concurrent pass's scratch buffer, and with one worker there
  is never a second.

  **And the half not paid by the viewer who is busy.** `apply` calls `session.commit()` inline in
  the connection's `async fn`, so a pass occupies a tokio worker for its whole duration. A viewer
  held back from the ramp, asking only for state it already holds, is answered in 0.5 ms when
  nobody is busy and waits **2.3 s** with 64 neighbours dragging a 200 000-row app — while the
  same 64 on the bundled example cost it **9.0 ms**. Divide by each app's own pass and three apps
  spanning more than 300× in pass cost collapse onto one rule:

  > a viewer who is doing nothing waits about **(busy viewers ÷ worker threads)** passes,

  which holds when the denominator is moved with `taskset` and not only the numerator. That is
  the tail-latency item that had been reasoned from `crates/serve` for two rounds and was never
  a number.

  **A conclusion first drawn from that number is withdrawn in the same breath.** It read 2.7 s
  against a 25 s keepalive as an order of magnitude of margin — and the 2.7 s is two probes seen
  through a **three-second** window, which records a longer wait as censored and reports a lower
  bound. Such a window cannot produce a number above about three seconds whatever the truth is,
  so it could not have falsified the claim it was cited for.

  **`busy.sh` gained a rung with a thirty-second aperture and it answers the question.** Across
  two bursts on three processes, **75 of 81** probes were answered inside the window — median
  about 2 343 and 2 285 ms, longest **4 905 ms**, roughly **5.1×** under the 25 s keepalive. The
  other six were cut short at window close: bounded below by their truncation and above by
  nothing, so they are consistent with the rest and cannot confirm it. Read at that size, the
  wait did not grow to fill the wider window. The conclusion comes back **narrower than it
  went**: this load does not starve the keepalive, rather than the keepalive being safe. The
  rule remains the worrying part, because the wait is pass × viewers ÷ workers: a slower app or
  a smaller host walks toward that interval with nothing else changing.

  **And a third correction, which reverses a diagnosis published one round earlier.** The rig
  was folding censored probes into the same median as completed ones, at the clock time the
  window shut — a value known to be too small. Where nothing is censored that changes nothing;
  at the worst rung of the worst app, where 3 of 11 probes never returned, it dragged the median
  from 2 248.5 ms down to **431.6 ms**, *below* that app's own thirty-two-viewer rung. That
  impossible row was published, and blamed on the three-second window being too narrow. It was
  not the window. Over completed probes the three-second ramp agrees with the rule at **every
  rung of all three apps** and matches the thirty-second measurement of the same rung to within
  4%; what the wider window buys is sample count — five completed probes become thirty-eight —
  and therefore the right to make the claim at all. `busy-session.mjs` now aggregates completed
  and censored apart, keeps the mixed figures under `_with_censored`, and prints what never came
  back as the rung's health warning.

  The rig needed three corrections before it could be believed, and each is a finding rather than
  tidying: a **fresh process per rung**, because the first version walked one process up the
  ladder and so measured every rung against the previous rung's peak; the **same burst twice**,
  to tell a plateau from a leak; and **three processes a rung, medianed**, because `peak − idle`
  depends on what the allocator was already holding free and two runs of one rung came back
  12 MB and 0 kB. Its guard earned its place too: a patch carrying no panes fails the run, and it
  did — a per-burst value cursor re-sent a viewer the value it already held, the engine reused,
  and without the check the rig would have reported that an interaction is free while reporting
  that it never caused one.

  **And a spot check that changes who those numbers describe.** Everything above is a glibc build;
  the `Dockerfile` ships a static **musl** binary on `scratch`. On musl the retained memory is
  *negative* — the process ends a burst 15 MB smaller than it started it at four dragging viewers,
  235 MB smaller at sixty-four — and a pass costs **1.7× more with one viewer working and more
  than fourteen times more with four**, on a constant sixty-five sessions so that concurrency is
  the only thing varying. That gap between the two is the finding: a build that is uniformly
  slower and one that serialises look alike at four dragging viewers and want different fixes.
  This runtime allocates heavily per pass and mallocng serialises where glibc's per-thread arenas
  do not. One app, two repetitions, four rungs: a spot check, and enough to say that **every
  published figure here is glibc's and the container is not.** `OPERATIONS.md` says so where an
  operator meets it and `ROADMAP.md` §7 carries what is left.

  `OPERATIONS.md` §Sizing now says to size for *viewers × (held + busy)*, and its advice to
  aggregate upstream is kept with a better reason and a correction: aggregating does not make a
  pass cheap — `filter` builds all 200 000 rows before `group_by` discards them — it makes the
  cost stop growing with viewers.

### Changed

- **A verdict moved and four documents that totalled it up did not.** `POSITIONING.md` §3's score
  line read *zero of four fully met* after condition 1's own verdict above it had been changed to
  met, and so did every other place that totals it up — `ROADMAP.md`'s opening among them, and
  one of them uses that score as half of a gate. The honest reading is **two of four fully met,
  two partly**, and all four now say so with a note about what they said before. The same sweep found per-session memory still called unmeasured in four more places a
  round after it was measured. `ROADMAP.md` §7 had already written the lesson down and it was
  learned again anyway, in files that were open at the time; the habit that catches it is one
  line — when a measurement lands or a verdict moves, grep the tree for the word that used to be
  true.

- **What one more viewer costs, measured — and it narrows a claim this project makes in nine
  places.** `benches/sessions/`, `ROADMAP.md` §7's first item, and the last of its four to be
  answered.

  The rig ramps *idle* sessions 0 → 512 against one `dagpane run`, reads `smaps_rollup` rather
  than `ps`, and takes the least-squares slope rather than a subtraction between two rungs —
  RSS grows in allocator steps and does not come back down. The bundled example: **179 kB per
  session**.

  **The comparison is the result, not that number.** The same pipeline over 200 000 rows costs
  **7 248 kB** a session; an *aggregating* pipeline over the same 200 000 rows costs **145 kB**.
  Same data, same process, same sockets — 50× apart. So a session costs what its pipeline
  **materialises**, not what its app **sources**.

  Both halves of that matter. **The sources really are shared**, and the 145 kB is the first
  measurement that says so — §7 had singled out `Arc::ptr_eq` as *"asserted by a test rather
  than inferred from an RSS reading"*, and this is the reading. **The computed frames are not
  shared**, and cannot be: two viewers with two filters must have two answers. That was never
  wrong, and it was never priced.

  So *"a hundred viewers of a 600-row app are a hundred slot vectors over one table, not a
  hundred copies of it"* — true, and read as a statement about the total, which it is not. Even
  on the bundled example a session costs **8.5× the CSV it is sharing**. The sentence has been
  narrowed everywhere it appears to say what is shared rather than to imply what is not.

  The operational consequence is in `OPERATIONS.md` under *Sizing*: **`--budget-mb` counts
  source bytes and none of this is in it**, so a fleet of row-keeping apps with many viewers is
  sized against the smaller of its two costs.

- **`dagpane host` is operated like `dagpane serve`.** The same probes on a listener of their
  own, the same unready-then-drain stop, and the same `SIGTERM`. What is not the same is the
  one decision worth stating:

  > **A hosting process is ready when it can compile and serve, not when every app it holds is
  > healthy.**

  The tempting answer is the other one, and it is wrong for a reason rather than a preference:
  one broken manifest is a fault *every* replica has, for the same reason, at the same moment,
  because the manifests are the shared thing. A readiness probe that walked the fleet would
  take every replica out of the pool together and turn one bad file into a total outage, while
  reporting something no amount of rescheduling can fix. `dagpane check` in front of the deploy
  is the gate for a broken manifest, and it fails before anything is listening.

  What does differ is the **description**. A single app's identity is fixed when it compiles; a
  fleet's changes while it runs, so `Health` is asked per request and `/healthz` lists what is
  resident *now* — each app with the digest of its manifest bytes and what it costs, against the
  byte budget. The fleet question one app answers ("do the replicas agree?") is then askable of
  every app at once, and a redeploy is visible as the digest changing rather than inferred.

  `an_app_that_cannot_compile_does_not_make_the_replica_unready` is the test, and the mutation
  that makes `/readyz` walk the fleet fails it.

- **A connection that survives being ignored, and a page that comes back.** Two halves of one
  property — a viewer should not have to touch anything — and the second half was a claim this
  project had already been making.

  **The server pings every 25 s.** A dashboard nobody is clicking is the ordinary case, not an
  idle one, but an AWS ALB, an nginx `proxy_read_timeout` and most ingress controllers reclaim
  a connection they have seen no bytes on for sixty seconds. So a viewer who is merely
  *reading* was disconnected, and the page left behind rendered perfectly and answered nothing
  with no error anywhere. `--heartbeat-seconds`, `0` to disable. It needs no client half: a
  browser answers a Ping in the transport and cannot send one from script, so the server is the
  only end that can start it.

  **The page reconnects on its own** — exponential, capped at 30 s, and jittered, which is the
  part that is about a fleet rather than one page: every viewer of a drained replica is
  reconnecting at the same instant, and without jitter they keep arriving together for as long
  as the outage lasts. It costs one pass and can land on a different replica, because a session
  *is* its input values and the viewer carries those in their URL fragment.

  One close is deliberately not retried: a socket that never spoke, on a page holding a token,
  is a refusal rather than a drop, and retrying it on a timer hammers a door that is not going
  to open and buries the message saying so.

  `crates/serve/tests/run.sh` is new and is what makes the client half checkable at all —
  nothing here had ever executed `client.html`; the existing check greps it. Real Chromium over
  the DevTools Protocol, driven from bare Node with **no npm dependency**, which is the same
  rule the wasm smoke test follows. Nine checks, and three mutations caught: never retrying,
  never clearing a pass that was in flight when the socket went, and retrying a refusal like
  any other close.

- **`dagpane serve` — the same app, deployed as any number of replicas.** `dagpane run` was
  written for a person at a terminal: it binds loopback, prints a URL, derives its `Origin`
  allowlist from the address it bound, and stops when that person stops it. A replica is none
  of those things — it is stopped by something that will not read output, sent traffic by
  something that only asks yes-or-no questions, and reached at a name it was never told. So
  the defaults are a separate command rather than a flag on that one, and `run` is unchanged.

  **Stopping is a sequence.** A replica that closes its listener the moment it is signalled is
  still in the load balancer's pool — the balancer finds out at its own pace, and every
  connection routed in the meantime is a reset. So a stop goes unready first (`/readyz` →
  `503`), waits `--drain-seconds` for the balancer to notice, and only then stops accepting.
  Measured against a real `SIGTERM` with `--drain-seconds 3`: `readyz=503` while the app port
  was **still serving `200`**, exit `0` after 3.004 s.

  **`/healthz` and `/readyz`, on `--admin-port` and not the app's.** The app's port has four
  endpoints and no others, `GET /{*path}` is a namespace an app's manifest controls, and the
  app's port is the one an ingress publishes — three reasons agreeing that the supervisor gets
  its own door. Leave it out of the Service.

  `/healthz` stays `200` **while draining**, which is the one part worth stating twice: a
  liveness probe that failed during a drain would have the supervisor conclude the process had
  hung and `SIGKILL` it, in the middle of the graceful stop it had just asked for.

  **`--origin`, which `OPERATIONS.md` said did not exist yet.** A container needs a wildcard
  bind to be reachable, and the allowlist derived from `0.0.0.0:8787` is an origin no browser
  sends — so the page loaded with `200`, the upgrade was refused with `403`, and the controls
  did nothing with nothing in the log to say why. Naming the origins you publish at
  **replaces** the derived list rather than extending it, and a trailing slash or a pasted path
  is refused at start-up naming the fix, because both look correct in an address bar and fail
  identically to a string that is nothing like it.

  **`/healthz` reports the digest of the manifest bytes this replica holds** —
  `dagpane_host::manifest_digest`, the identity `dagpane host` already routes and evicts by.
  `OPERATIONS.md` named the failure and called it a deployment problem: *two replicas holding
  different bytes are serving two different apps under one name*. It still is one. It was also
  invisible, with no symptom but viewers disagreeing with each other, and now it is one field
  to collect and one `sort -u` to count.

  `--origin` **drops the scheme's default port**, because a browser's `Origin` omits it:
  `https://x.example:443` is sent as `https://x.example`, and the comparison is exact, so the
  explicit spelling — the one somebody copies out of a config file — would otherwise declare an
  origin that can never match and refuse every upgrade. `https://x:80` keeps its port, because a
  default port for the *other* scheme is not a default port.

  `crates/serve/tests/replica.rs` drives the real server on two real listeners. Eight
  mutations, eight caught: sleeping before going unready, not sleeping at all, making
  `/healthz` mirror `/readyz`, shutting the probes down when the drain starts, letting declared
  origins extend rather than replace, letting a stopped replica report itself draining again,
  never telling the probe listener the app stopped, and making the fingerprint a constant.
  ADR-0009 is the argument.

- **A page can boot its half from the opening frame alone.** `ServerMessage::Init` carries a
  `client_half` block for a split app: the manifest, and one column list per `[[source]]`.

  The manifest is **re-emitted from what the server parsed**, not re-read from disk, so the
  page compiles exactly what the server compiled rather than a file somebody edited since —
  a manifest's bytes are its identity here, and two halves built from different bytes are two
  different apps wearing one name. The schemas come from frames already loaded, so producing
  them costs no re-read; `Source::schema` is explicitly not promised to be cheap and for a
  CSV it is a full parse.

  It carries the app's renderer scripts too, as bytes and not paths. `compile_with` demands a
  declared script by name — a missing one is the same `ManifestError::Renderer` a missing file
  is — and the manifest is re-emitted whole, so a split app with `[app] renderers` would hand
  the page a manifest naming scripts it was never given and fail at `compile_with` rather than
  draw anything. `a_page_whose_half_declares_a_renderer_gets_the_renderer` is the case.

  `SchemaSource` is what the page compiles against: a source that answers with a shape and a
  frame of no rows. It exists because `compile_with` must load every source to decide a CSV's
  column types, and the point of cutting below the data is that the page does not get the
  data. The compiler wants types, not rows, and types are a few dozen bytes for a table of any
  size.

  Two tests carry it. `a_half_compiled_from_shapes_is_the_same_half` requires a half compiled
  from shapes to be identical to one compiled from the CSV — cell for cell, id for id, same
  frontier, same drawn views once real rows arrive. `a_page_can_boot_from_the_opening_frame_alone`
  serialises a real `init`, discards the server, and requires a page built from that message
  and nothing else to draw what the undivided app draws.

  A stand-in whose shape is **wrong** does not produce a subtly different app — it fails to
  compile, which is the failure mode to want.

  The costs are asserted rather than hoped for: the boot block is under 4 KB and constant in
  the size of the data, while the opening frame is dominated by the frontier. An `init` for a
  split app is as big as the app's rows, because cutting below the data means the data crosses
  once — the same cost `dagpane export` pays.

- **Either half of a split app can now be run, and the protocol carries the frontier between
  them.** The mechanism landed last release checked a cut and reported it; this is the layer
  that would serve one.

  `AppSession::open_side` runs the server half or the page half over its own graph, with its
  own rendered-view cache — a pane repainted in the browser is not recorded on the server,
  which would otherwise make the stats block claim it sent something nobody saw. **A pane
  belongs to whichever side holds its cell**, so no `Pane` field says which side it is on and
  there is no second answer that could disagree with the cut.

  On the wire, `init` carries the **full** frontier and `patch` carries the delta — the same
  pairing `full_views` and `patch` already had, for the same reason. `BoundaryValue` is the
  wire shape and holds an `Outcome` rather than a `Value`, so a failure upstream of the cut
  arrives as a failure instead of a null the page would draw as a legitimately empty answer.
  Both fields are omitted from the encoding when empty, so an app that declared no placement
  sends byte-identical messages to the ones it sent before.

  `dagpane-wasm` takes `side: "client"` to open the page's half, and `dp_deliver` applies a
  server message and answers with the page's own patch. The extraction is in Rust and not in
  JavaScript deliberately: applying a pass's boundary values **together** is the one
  obligation the transport carries, and a page that unpacked a frontier itself could deliver
  half of one with no test in this repository seeing it. Absent `side`, the module runs the
  whole app — which is what a `dagpane export` bundle does, since it has no server to feed it.

  What this buys is pinned by
  `the_two_halves_together_show_what_one_undivided_session_shows`: two sessions talking over
  the real messages produce the same panes with the same rendered views as one undivided
  session, first paint and after every interaction. And
  `a_page_side_interaction_never_reaches_the_server` is the point of the whole feature as two
  assertions — the page repaints, and the server's epoch does not move.

  **`dagpane run` still evaluates every cell on the server for a placed app**, exactly as it
  would with the `place` lines deleted, and `dagpane check` says so. The switch is one line
  and is deliberately not flipped: a browser that cannot yet consume a frontier would render a
  placed app with its page-side panes simply missing, which is worse than ignoring the cut.
  `ROADMAP.md` §4 has what is left, including two design questions this work surfaced — the
  page has to type-check its half without the data, and the stats bar needs a defined meaning
  when two halves each run a pass.

- **Where a cell runs is a line in the manifest.** `place = "client"` on a `[[cell]]` or an
  `[[input]]`, and nothing else about the app changes — same verbs, same pipeline, same
  compiler. `examples/apps/20-placed.toml` is `examples/sales.toml`'s pipeline with four of
  them added.

  **Placement is monotone: a cell on the server may not read a cell in the page.** That one
  rule is what makes a cut a *frontier* — values cross once, in one message per pass — rather
  than a sieve costing a round trip per height boundary. A manifest that breaks it is refused
  at `check` time, by name:

  ```
  dagpane: `region_totals` runs on the server and reads `filtered`, which runs on the
  client: values cross the cut once and never come back. Move `region_totals` to the
  client, or `filtered` to the server.
  ```

  `dagpane check` reports the two numbers a deployment turns on — how wide the frontier is,
  and how many controls would need no network at all.

  **Nothing serves a split yet**, and `check` says so on every placed app: `dagpane run` and
  `dagpane export` evaluate every cell on their own side, so a placed app runs today exactly
  as it would with the `place` lines deleted. What exists is the mechanism and its proof —
  `Graph::split`, `Frontier`, `Split::deliver`, and the tests below. ADR-0008 argues the
  design and states plainly what is left; `ROADMAP.md` §4 lists it.

  The check that matters is the **split oracle**: two hundred generated graphs, each cut at a
  random admissible place, both halves driven through twelve interactions, and after every one
  the pair must agree **cell for cell** with an undivided session. It asserts its own coverage
  too, failing if too few seeds produced a real split. It has already earned its keep — it
  caught that a client seeded with only a *delta* starts with holes, which is why
  `full_frontier` exists beside `frontier`.

  Also new in `dagpane-core`: `Session::set_outcome`, so a boundary cell that failed crosses
  as a failure rather than as a null the page would draw as a legitimate empty answer.

- **The engine runs in a browser, and the client cannot tell.** `crates/wasm` holds an
  `AppSession`, takes a `ClientMessage` from a function call, and answers with a
  `ServerMessage` — which is what `dagpane-serve` does with a socket in the middle. The client
  gained a `transport`; everything above it is written once.

  ```sh
  $ dagpane export examples/sales.toml --out dist
      1.1 MiB  dist/dagpane.wasm
      4.7 KiB  dist/dagpane.js
     60.2 KiB  dist/index.html
      1.2 MiB  total
  ```

  A folder for any static host — served over `http://`, since a browser refuses modules and
  WebAssembly from a `file://` origin, and the page says so if you try.
  `crates/wasm/tests/run.sh` drives that bundle in a real WebAssembly engine and asserts the
  interaction costs **11 cells / 8 visited / 6 evaluated / 1 reused / 4 changed / 3 untouched /
  3 of 7 panes** — the same numbers CI asserts against the server, because a browser build
  that recomputed a different number would be a different product with the same name.

  **1.10 MiB raw, 318 KiB gzipped**, five exports and **no imports** — no `getrandom` backend,
  no wasi shim, no COOP/COEP, which is what `dagpane-core`'s clocklessness buys once there is a
  browser to spend it in. Against DuckDB-Wasm's 34.25 MB and DataFusion's 27 MiB, the payload
  argument `ROADMAP.md` §4 calls false for a query engine is true for a thin evaluator, and
  `BENCHMARKS.md` now has it measured rather than asserted.

  **No wasm-bindgen.** `cargo build --target wasm32-unknown-unknown --release` is the whole
  build: about forty lines of `extern "C"` over linear memory and about the same again of
  hand-written JavaScript, against a proc-macro tree and a CLI whose version has to match.

  **And the measurement came back sideways, which is the useful part.** §4's trigger was an
  interaction dominated by the round trip. It is — the wire is about half of every interaction
  on loopback — but wasm is **2.2–2.8× slower at the identical pass**, so the browser is
  *slower* on two of the three apps measured. What transfers is the *method* rather than the
  figure: a break-even is `wasm pass − server pass`, a property of the app's own pipeline, and
  **0.08–0.49 ms** is what these three apps came to on one machine. Typical round trips clear
  that, and `benches/roundtrip/` is how to check a fourth app rather than assume it.
  Client-side compute buys you the wire and charges 2.5× on the pass to do it; `BENCHMARKS.md`
  and ADR-0007 say so in those words. Not built here: a Web Worker, and everything a server
  was doing. (Per-cell placement was on this list and is now the entry above it — the
  mechanism, not a served split.)

- **`custom` panes — a new drawing is one JavaScript function, not a new server binary.** The
  runtime draws five things, and a sixth used to mean editing `PaneKind`, `View`, `render`, the
  manifest compiler and the client's `switch`.

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

  The engine keeps the part that can be wrong: it still decides which rows exist, whether
  anything moved, and what goes on the wire. A renderer is handed the answer and an element and
  is never on the path that produces one — so a custom pane whose view did not change is still
  absent from the patch, and a renderer cannot make the product claim untrue. `options` is
  opaque JSON rather than `Value`, because typing it would make every new option a change to
  `dagpane-app`, which is the cost the whole thing exists to remove.

  Three things stay the runtime's job because a renderer cannot do them: a named column that is
  gone is an error rather than a silently missing series, truncation is reported, and a
  renderer that throws takes out its own pane and nothing else. Scripts are read when the app is
  compiled, so `dagpane check` fails on a missing one and serving one is a map lookup rather
  than a file read a request steered. `examples/renderers/heatmap.js` is a working example.

- **Ordering constraints — a `sort`'s column is a question about order, not a dependency.** A
  top-N pane depends on the order its ranking column put the rows in and on the columns it
  totals. Move every value in the ranking column and the ranking is identical, so the pane is
  identical, so it does not run.

  ```console
  $ dagpane explain examples/apps/19-wide-telemetry.toml --set busy_cpu=35 --change-column metrics.cpu0_user
      ran      busy_scrapes         changed
      ran      memory_by_host       changed
      ran      hosts_present        same value — nothing below it ran
      reused   busiest_ten_memory   its inputs had not moved
    changed 1 column of a 122-column frame; recomputed 4 of 11 cells
  ```

  One column, one edit, one pass. Three cells *filter* on `cpu0_user` and their rows moved;
  one *ranks* by it and its order did not. Without the constraint that same edit recomputes 5
  of 11.

  The same two conditions as a predicate constraint, and the same cheap check first. What it
  is worth, measured rather than asserted:
  `a_top_n_pane_mostly_sleeps_through_a_single_edit_to_its_ranking_column` puts a top-five
  pane through forty single-row edits to its ranking column and it sleeps through 16; an
  order-preserving edit — a uniform shift, a rescale, a re-baseline — scores 40 of 40.
  Whole-value granularity scores 0 on both. `ROADMAP.md` §3 records what it does *not* buy:
  nineteen of the twenty bundled apps sort after a `group_by` and meet neither condition.

- **Predicate-range constraints — a filter's column is a question, not a dependency.** A cell
  downstream of `filter(amount >= 400)` depends on *which rows survived*, not on the values
  that picked them. So the engine records the predicate and the rows it selected, and compares
  that.

  ```console
  $ dagpane explain examples/apps/19-wide-telemetry.toml --set busy_cpu=40 --change-column metrics.cpu0_user
    changed 1 column of a 122-column frame; recomputed 0 of 11 cells

  $ dagpane explain examples/apps/19-wide-telemetry.toml --set busy_cpu=35 --change-column metrics.cpu0_user
    changed 1 column of a 122-column frame; recomputed 4 of 11 cells
    patch: 3 of 9 panes — busy_scrapes, tightest_host, memory_by_host
  ```

  Same column, same edit, three cells filtering on it. At a floor of 40 the same 129 of 204
  rows clear it before and after, so none of them can have a different answer. At 35, three
  rows cross.

  The cheap check runs first: an unmoved column cannot have moved its selection, so the common
  case still costs one digest comparison and the predicate is never re-run. Two conditions
  keep it sound — the filtered column must be the input's own, and the rows it ran over must
  be the input's rows — so a filter on a derived column is never a constraint, and neither is
  a filter after another. ADR-0006 §4 has the constructed case that makes the second of those
  a correctness rule rather than a tidiness one.

- **Sub-node invalidation — a column is the unit, not a table.** A cell that reads three
  columns of a hundred now sleeps through a change to the other ninety-seven.

  ```console
  $ dagpane explain examples/apps/19-wide-telemetry.toml --change-column metrics.disk_sdb_write_ops
    changed 1 column of a 122-column frame; recomputed 0 of 11 cells
    patch: 0 of 9 panes — nothing to send
  ```

  The table changed. Every cell was still visited — they all declare an edge to `metrics`
  and the engine cannot know better until it looks — and none ran. Moving `cpu0_user`, which
  three cells filter on, recomputes three; moving `net_eth0_rx_bytes`, which one cell sums,
  recomputes one and sends one pane.

  What a cell read is recorded as it runs, once per pipeline step rather than once per cell
  of the table, and an input nothing was said about keeps the whole-value comparison — so the
  default is always the old, safe behaviour and every narrowing is a claim somebody wrote
  down. ADR-0006 has the design, what it costs in memory, and the two things to know before
  relying on it.

  The counts on `examples/sales.toml` are unchanged, which is the assertion that none of this
  altered what the runtime does on an app that is not wide enough to need it.

- **`dagpane explain --change-column CELL.COLUMN`** — rewrite one column of one source frame
  and report what it cost, the way `--set` reports what turning a control costs. The edit is
  synthetic and the output says so. Given alongside `--set`, the controls settle in their own
  pass first and the column change is still the one thing measured — which is how you ask what
  a data change costs at a threshold other than the manifest's default.

- **`examples/apps/19-wide-telemetry.toml`** — a node exporter's scrape flattened one column
  per metric, 122 columns wide, of which the page reads five. The nineteenth example, and the
  only one whose lesson is about the data moving rather than about the controls. Its eleventh
  cell is the one app in the set that earns an ordering constraint: a top-ten by CPU reporting
  free memory, which never shows the column it ranks by.

- **SQL cells — a front end over the nine verbs, in a grammar small enough to close.**

  ```toml
  [[cell]]
  name = "by_region"
  sql = """
    select region, sum(amount) as revenue
    from sales
    where amount >= :min_amount
    group by region
    order by revenue desc
  """
  ```

  **It is not a second engine.** The statement is parsed, its tables are resolved, and it
  lowers to the same `Step` values a `[[cell.step]]` compiles to — a `filter`, a `group_by`,
  a `sort`. By the time anything runs there is no SQL left, so nulls, type rules, division by
  zero, the join's duplicate-key refusal and the digest short-circuit are the ones already
  written down and cannot acquire an exception. `crates/app/tests/sql.rs` holds eight
  questions written once as SQL and once as steps, asserted to render identically; that
  differential is the contract.

  **The edges come out of the parse tree.** `from sales` makes the cell depend on the cell
  `sales`; `:min_amount` makes it depend on that control. Nothing scans the text, so a table
  name inside a comment or a string literal is not a dependency and a quoted identifier is not
  a keyword — the two cases `ROADMAP.md` §6 named as the reason SQL was last, both tested.

  **The grammar is closed, and hand-written.** The parser accepts only what lowers, so "refuse
  what cannot be resolved" — the gate this item had to pass — is a property of the grammar
  rather than a blocklist somebody has to keep complete. That is why it took no dependency
  despite the roadmap anticipating one: a third-party parser accepts all of SQL and would have
  inverted the arrangement, and a blocklist of SQL features is never complete. ADR-0005 is the
  record, and it states the cost rather than hiding it — **this is not SQL**, it is a dialect
  that fits on a page.

  Refused by name, with what to write instead where there is something: `with`, `union`,
  `having`, `distinct`, window functions, subqueries, `right` and `full` joins, `cast`, `like`,
  a comma-separated `from`, `count(<expression>)`, chained comparisons, a multi-column
  `order by`, and a second join.

  ```console
  $ dagpane check app.toml
  dagpane: app.toml: cell `out`: `having` is not supported; filter the grouped cell in a
    second cell instead
    line 4: group by region having n > 1
                            ^
  ```

  Supported: `select` with expressions and `as`, `*`, one `join` (`inner`, `left`,
  `left semi`, `left anti`), `where`, `group by`, `order by`, `limit`, the five aggregates,
  and `case` / `between` / `in` / `is null` / `||`, which are desugared into the expression
  language. What the dialect cannot say and does not pretend to: there is no `skip_when`, and
  a `select` produces a table, so `scalar` and `count = true` stay steps.

- **`examples/apps/18-team-load.toml`** — delivery against deploys, five teams, every cell a
  `select`. `tools/verify.py` now gates nineteen manifests.

- **Two more expression functions: `is_null(x)` and `contains(haystack, needle)`.** `is_null`
  is the second function that sees a null and keeps going, and without it a null was something
  an expression could propagate but never *ask about* — which made "how many of these are
  missing?" unwriteable. `contains` is the substring test, matching the `filter` step's
  comparison. Both are reachable from `derive` as well as from SQL's `is null` and the
  `contains(...)` call.

- **`Expr` can be built from parts** — `Expr::column`, `Expr::binary`, `Expr::call` and the
  rest — so a front end that parsed something else produces an expression directly instead of
  rendering text for the parser to read back. The constructors also render the source, fully
  parenthesised, so an error about a lowered expression still quotes something a person can
  read.

- **Joins — the ninth verb.** `join = { with = "accounts", left_on = "account_id",
  right_on = "account", how = "left" }`, with `inner`, `left`, `semi` and `anti`.

  `with` names a cell, so the edge is written out exactly the way a filter's `param` is and
  resolved by the same function — `dagpane graph` draws both sides, and a `with` that names
  nothing is a compile error rather than an edge that quietly does not exist. That was never
  the hard part of a join. **Everything that is hard is about data that does not fit the join
  the author had in mind**, and each of those now fails loudly:

  * **a duplicated key on the right** silently multiplies rows and doubles a total. Refused,
    unless `multiple = true` says it was meant — so the ordinary join is a *lookup*, and the
    flag tells the next reader that the output may be longer than the left.
  * **a key that is `int` on one side and `text` on the other** would match nothing and leave
    an empty page with no explanation. Refused by `dagpane check`, where both schemas are
    known, and at run time where they are not.
  * **a column name on both sides** would leave every later step finding the first one.
    Refused unless `suffix` distinguishes them.
  * **a null key** matches nothing, including another null.

  `how` is required and has no default: `inner` is SQL's, and choosing it silently is how a
  page loses rows nobody asked it to lose. `semi` and `anti` add no columns and no rows — they
  are filters that happen to read another table — so they return `left.take_rows(…)` and
  allocate nothing but the hash map. `right` is `left` with the two cells swapped; `full` is
  not built, and `How`'s documentation names the one decision that would have to be made
  first rather than guessing at it.

  `transform::join_schema` is one function with two callers, the checker and the transform,
  which is what stops a join that `dagpane check` passes from being one the run time refuses.

  ```console
  $ dagpane explain examples/apps/17-service-risk.toml --set environment=staging
    6 cell(s) never looked at: slo, deploys, tier, slo_scoped, burn_by_service,
                               services_watched
    patch: 1 of 5 panes — risk
  ```

  Two sources, two controls, and each control reaches one side of the join. Filtering the
  deploy log cannot touch the SLO half of the page, and the graph says so before it runs.

- **`examples/apps/17-service-risk.toml`** — error budget against deploy activity, which
  needs both tables on one row and so could not be written before this. `tools/verify.py`
  now gates eighteen manifests.

- **Derived columns — the eighth verb.** `derive = { name = "margin", expr = "revenue - cost" }`
  computes a new column one row at a time, over a hand-written expression language in
  `dagpane-core/src/expr.rs`: literals, column references, `$parameter` references, arithmetic,
  comparisons, `and`/`or`/`not`, and thirteen functions. No aggregates, no joins, no user
  functions, no SQL.

  **A bare name is a column. An edge is spelled `$`.** `revenue - cost` reads two columns and
  depends on nothing new; `amount * (1 - $discount)` reads one column and one cell, and
  `discount` becomes an edge in the compiled graph. The compiler builds edges from the `$`
  tokens the lexer produced and reads the expression for nothing else, so the dependencies a
  derived column declares are visible by reading it and a `$` that resolves to nothing is a
  compile error rather than an edge that quietly does not exist. That is ADR-0001's rule
  applied to an expression, and the ADR carries the amendment saying so.

  What it costs at run time: one column. `derive` shares every column it was given through a
  new `frame::with_column` adapter rather than materialising a new frame, so it is the same
  `Arc` bump every other verb became.

  ```console
  $ dagpane explain examples/apps/16-margins.toml --set target_margin=70
    4 cell(s) never looked at: accounts, plan, costed, book_margin
    patch: 2 of 6 panes — clearing, by_verdict
  ```

  `costed` computes margin, margin percent and revenue per user for all 360 accounts and is
  **not looked at** when the floor moves, because none of those three expressions contains a
  `$`. The one that reads the slider is a cell of its own. That is the mechanic the new
  example app exists to show.

- **`dagpane check` names a bad column reference before the app runs**, in every verb and not
  just the new one. Sources are loaded by `compile`, so the schema at each step of each
  pipeline is known before a session exists; the checker threads it through `filter`,
  `derive`, `select`, `sort`, `group_by` and `scalar`, and reports the column that is missing,
  the columns that are there *at that point*, and a Damerau-Levenshtein suggestion.

  ```console
  $ dagpane check app.toml
  dagpane: app.toml: cell `net`, step `derive`: `net = amont * (1 - $discount)`:
    no column `amont` — did you mean `amount`?; the table here has `order_id`, `day`,
    `region`, `channel`, `units`, `amount`
  ```

  It is best-effort **by construction**: a cell whose `from` names a cell declared later in
  the file has no knowable schema, and everything downstream of one is left to the run time
  rather than guessed at. The run-time checks did not move, and they are the same code —
  `Expr::bind` against a different scope. All sixteen example apps compile unchanged.

- **`examples/apps/16-margins.toml`** — account margins, and the app `15-unit-economics`
  could not be. It is in `tools/verify.py`, which now gates seventeen manifests.

### Changed

- **The client's "fetches nothing" is now "fetches no third party",** and the server has a
  fourth endpoint. A renderer script is code the page did not ship with, so `GET /<renderer>`
  serves one from memory — the bytes were read when the app was compiled, so no request steers
  an `open(2)` and a path this process does not already hold is a `404`. The route is absent
  unless a manifest declares a renderer.

  The old claim was absolute and is not any more, so it is restated rather than quietly
  dropped: `the_client_is_one_self_contained_file` now enumerates **four** same-origin
  requests — `GET /auth`, a configured provider's token endpoint, a declared renderer script,
  and `./dagpane.js` in an exported bundle — and fails on a fifth. `compile_renderers` refuses
  a renderer path that is absolute, climbs with `..`, or contains a `:`, so neither `import`
  can name another origin; a renderer that fails to load takes out its own pane rather than
  the page. **The air-gapped deployment still renders**, which is the property the original
  rule was protecting. `ARCHITECTURE.md`, `OPERATIONS.md`, `crates/serve/README.md` and
  ADR-0007 say it in those words.

- **`dagpane export --wasm` says what the default is relative to.** With no flag it looks in
  `target/wasm32-unknown-unknown/release/` **relative to the current directory**, which
  resolves to a module only in a dagpane workspace that has just built one. The failure now
  prints the absolute path it tried and names the three cases that need `--wasm` — an
  installed binary, a shell below the workspace root, and a `CARGO_TARGET_DIR` pointing
  elsewhere — instead of leaving a relative path on screen for the reader to resolve against
  a working directory they cannot see from it.

- **"There is no benchmark in this repository" is narrowed to the claim that is still true.**
  `ROADMAP.md` §7 and `ARCHITECTURE.md` both said it in the present tense while
  `BENCHMARKS.md` sat beside them holding a row sweep, payload sizes, a fleet comparison and
  round-trip figures. What is actually still unmeasured is what §7 is about: apps-per-core,
  memory under N viewers, and latency against another runtime under identical load. The rule
  those documents were protecting survives with one word added — **no *general* performance
  claim** — so a dated figure that names its app, its machine and its command is allowed, and
  a sentence about how fast this runtime *is* remains blocked on §7.

- **A cell with no steps is an alias of whatever it reads, including a control.** It was
  documented as an alias and was one only for tables; a cell reading a slider produced a
  `CellError` at run time. It now passes the value through, and a cell that has a *step* and
  reads a control is a compile error naming both.

- **`Frame::memory_size` is unchanged, but `ColumnData` gained `with_capacity` and `push`,**
  replacing two private helpers in `transform.rs` that did the same thing. `push` promotes an
  `Int` into a `Float` column — the one conversion an expression needs, because `if(c, 1, 2.5)`
  is statically a float and evaluates to an int on the rows that take the first branch.

- **A missing column at run time now suggests the nearest one too**, through the same function
  the compile-time checker uses.

- **A cell's steps are normalised before they are compiled.** `StepSpec` (the TOML shape)
  becomes a `Step` — one verb, literals resolved, expressions parsed — and `compile_cell`
  works on that. It is what lets a SQL cell lower to the *same* steps a pipeline does rather
  than to a second kind of step that would need a second copy of every check below it.

- **`Agg` and `How` print the word the manifest spells.** An error saying `Sum` or `Anti` was
  telling an author about a word they did not type; both now `Display` in the `snake_case` the
  TOML uses.

- **An OIDC front door** — `crates/auth`, `--auth-jwks` and friends, and the PKCE flow in the
  bundled page. Without the flags nothing changes: the socket is open and the CLI warns about
  a non-loopback bind, exactly as before.

  **This process never talks to the identity provider.** Keys come from a file the operator
  supplies, so there is no discovery request to fail at start-up, nothing to redirect, and an
  air-gapped install works — the runtime still makes no outbound requests, even with a front
  door on it. Rotating keys is a copy and a restart.

  Getting a token is the page's job, which is what PKCE is for: a public client with no secret
  to keep. Verified end to end in a real browser against a stub provider — redirect out, code
  back, exchanged, code stripped from the URL, token in `sessionStorage`. **Never in a URL**:
  it travels in `Authorization: Bearer`, or in a `Sec-WebSocket-Protocol` entry for a browser,
  which cannot set headers on `new WebSocket()`. A query string is logged by every proxy in
  the path.

  **`alg: none` and `HS256`-signed-with-the-public-key are closed by construction.** The
  algorithm type has no variant for either, so they cannot be allowlisted; the JWKS loader
  skips symmetric keys, so there is no secret to HMAC against. Both attacks are mounted in
  `crates/auth/tests/verify.rs` and refused.

  A bad token is `401` and says the same thing whatever was wrong with it, so a caller cannot
  iterate towards one that works. A **good** token for another app is `403` — sending that
  person back to a login would be a loop that cannot succeed. The detail goes to the log.

  One flag has no default and the server refuses to start without it: `--auth-apps-claim` or
  `--auth-any-app`. Both wrong answers are silent ones.

  Not covered, and `SECURITY.md` says so: per-pane authorisation, an audit of who read what,
  and revocation before a token expires.

- **Durable sessions — and there is still no session store.** A session is its input values,
  so the viewer carries them: the client keeps them in the page's URL **fragment** and hands
  them back on the socket's query string, and `AppSession::resume` stages them before the
  first render. Reconnecting after a restart costs **one pass**, not a render at the defaults
  followed by a correction.

  Reload the page and your filters are still there. Send the URL to a colleague and they see
  what you see. A viewer who lands on a different replica after a deploy sees the state they
  left, because they brought it — there is nothing to run, replicate, evict or lose.

  A store would have been the obvious design and was rejected on ordering, not scale: a
  session id is a bearer token, and a store keyed by one — shipped before there is any
  authentication in front of it — is a way to read somebody else's session.

  **A resume is an ordinary `Set`**, through the same two predicates, with no privileged
  seeding path. A saved state naming a computed cell or an unknown one is dropped and
  reported, never applied. It differs from `Set` in one way only: `Set` is all-or-nothing,
  and a resume applies what it can and tells the client what it could not — so a manifest
  that renamed an input last week does not break every bookmarked link, and the viewer is
  told rather than shown different numbers under the same URL.

  The opening frame of a connection that carries no saved state is **byte-identical** to
  before, asserted by a test.

- `benches/loadgen/` — a protocol-level load generator, and the apps-per-core figure it
  found. Built because the browser rig's own result said it had to be: a Chromium page per
  viewer costs ~24 ms a round trip, which is larger than a dagpane interaction. This one
  costs **67 µs** over an in-process echo.

  **224 apps on one pinned core** — 448 concurrent sessions, 1 908 interactions/s, p99 75 ms
  inside a 250 ms budget, generator at 8.9% of its cores, reproduced three times. The server
  runs under `taskset` on one named CPU and the generator on the others, so *per core* is
  literal. Against the same workload, service demand measured by the browser rig puts marimo
  at 3.7 apps/core and Streamlit at 1.2; what licenses reading those as capacity is that the
  same model predicts 200 for dagpane where the ramp measured 224 — two rigs, no shared code,
  12% apart.

  Conditional on a heavy workload that `BENCHMARKS.md` states beside the number: two viewers
  per app moving a control every 200 ms, continuously, over 600-row apps.

  Every reply is held to an oracle, **including the absences** — a patch carries only panes
  whose view changed, so a generator that ignored a missing pane could not tell "nothing
  needed to change" from a missed invalidation. A mismatch fails the run and exits non-zero.

- `benches/fleet/` — the fleet benchmark, and the first result published from it.

  The same app written three times — dagpane, Streamlit, marimo — each idiomatically in its
  own runtime, driven by one browser that sets a control and waits until the number on
  screen matches an **independent oracle**, so a runtime that renders a wrong number fails
  rather than scoring well. Four arrangements, because dagpane is measured *both* ways: one
  process per app (how the baselines arrange themselves) and N apps in one process (what
  `crates/host` is for). The difference between those two is multiplexing alone.

  Marginal resident cost of one more app, as PSS: **0.25 MB** for `dagpane host`, 94.8 MB
  for Streamlit, 106.6 MB for marimo. 128 apps in one process hold 37 MB. Both RSS and PSS
  are reported, because summing RSS across a fleet of Python processes counts every shared
  page of libpython once per process — and where they differ, PSS is the honest one.

  **It did not produce an apps-per-core number, and says so where the number would be.** A
  browser per viewer on the same machine as the fleet saturates before dagpane does, so
  every point past sixteen apps measures the load generator. The driver floor is 24 ms and
  dagpane's p50 at small N is 22 ms, which means the only honest reading of those rows is
  "below this harness's resolution" rather than a latency. Both are stated in
  `BENCHMARKS.md` rather than rounded off.

- `crates/connect` — where a source's rows come from. One trait with **two** questions:
  `version()` is cheap and answers "has anything changed?" from metadata alone; `load()`
  reads the rows. A scheduled refresh asks the cheap one every tick and the expensive one
  only when it moved.

  Three implementations. `file` (CSV, and the format is an enum now rather than a hardcoded
  `csv:` field, so the next reader is an addition). `http` (a URL returning CSV) and `sql`
  (one read-only Postgres `SELECT`, no pushdown, no cursor) are **behind features that are
  off by default** — see *Changed* below.

  A `Version` is deliberately **not** a content digest, and the type says so at length: it
  is the source's own claim about its own metadata, comparable only against an earlier claim
  by the same source, and it can be wrong in one direction. Each implementation documents
  when. A server that sends no `ETag` and no `Last-Modified` reports a version that never
  repeats, so it reloads every tick rather than risking the stale-but-equal answer; a
  Postgres source versioned by row count alone cannot see an `UPDATE`, which is why `watch`
  exists and why the documentation says to name a column. A test asserts *both* halves of
  that — the blind source failing to notice, and the watching one noticing.

- `dagpane refresh <app>` — re-read the sources and print exactly what it cost.

  ```
  sales  unchanged (d75d49f577730d08)
  no cell moved: 0 of 11 cells visited
  ```

  Two filters, and each catches what the other cannot. The source's staleness check decides
  whether it is read; the engine's digest of what came back decides whether any cell
  recomputes. So `--force` over an untouched file reads 600 rows and *still* repaints
  nothing — a nightly export that produces identical bytes leaves a viewer's dashboard
  alone.

  `--watch <seconds>` re-reads on an interval and prints a line only when something moved.
  It exists because a single run on a freshly compiled app is *always* the unchanged case —
  compiling it read the sources a moment ago — so without it the command could only ever
  demonstrate half of what it measures. Watching the bundled example while its CSV is edited
  reports `9 of 11 cells visited`: the two control cells are not downstream of the source
  that moved, and are never looked at.

- Manifest: `[[source]]` takes `file = { path, format }`, `http = { url }` and
  `sql = { dsn, query, watch }` alongside the original `csv = "…"`, which is unchanged and
  is exactly `file = { path, format = "csv" }`. Exactly one, checked by the same rule that
  checks a control's kind. A manifest naming `http` or `sql` against a binary built without
  that feature fails at compile time, naming the feature — not at the first refresh, and
  never as a source that silently loads nothing.

- `App::sources` — the compiled app keeps the things that produced its rows, so it can read
  them again. Without it a compiled app is a photograph, and a refresh, a schedule and a
  control plane are all impossible for the same reason. **Breaking** for anything
  constructing `App` literally.

- `crates/host` — many apps in one process. An app is keyed by `(app id, manifest digest)`,
  so a redeploy is a *different key* rather than a mutation of the same one, and three
  properties fall out of that with no mechanism of their own: a viewer who reconnects after
  a deploy cannot be handed the graph that was deployed over, a rollback is a key change
  rather than a rebuild, and two processes holding one key hold the same graph. One
  `Arc<App>` per key however many viewers; every connection owns its own `Session` over it.
  Eviction is against a **measured** byte budget — see `Frame::memory_size` below — with the
  least recently opened app going first, an app larger than the whole budget refused rather
  than admitted at every other app's expense, and an idle sweep that only an explicit sweep
  performs. `tests/isolation.rs` carries thirteen tests, including a source that resolves
  one key and returns a different manifest's bytes: nothing is compiled, because the key
  names the bytes.

  No socket and no runtime in this crate, and CI greps for it. `crates/serve` owns the
  socket.

- `dagpane host <dir>` — every `*.toml` in a directory served from one process on one port,
  routed by the `Host` header. `sales.toml` is the app `sales`, reachable at any name whose
  first label is `sales`; a filename that is not a DNS label is skipped and reported at
  start-up rather than taking the process down. Editing a manifest on disk changes its
  digest, so the next request compiles the new one and the old graph is evicted — a redeploy
  with no deploy step. `--budget-mb` and `--idle-minutes` are the eviction policy.

- `dagpane_serve::serve_host` / `serve_host_on` — the same, as a library.

  The `Origin` check **generalises** here rather than carrying over. With one app it is the
  address the process bound; with many on one port there is no single address to derive one
  from, and the port a browser saw is the proxy's, not this process's. So the rule becomes:
  the `Origin` must name the `Host` the request is already asking for — the same "open it at
  its own address", stated for a server that has more than one. Without it, a page one party
  has open could open a neighbouring app's socket on the same port; that case does not exist
  for a single-app server and is a test now. An unrouted name gets a `404` at `/` rather than
  the client shell, which would otherwise be a page whose socket fails with nothing to read.

- `OPERATIONS.md` and `docs/RUNBOOK.md` — how to run this in front of other people, and what
  to do when it is already broken. Every message quoted in either was produced by this
  binary rather than written from memory, and the two facts most likely to cost an operator
  an afternoon are stated with the evidence: the `/ws` `Origin` check refuses a reverse proxy
  on a different hostname (`403`, page loads, controls dead), and `SIGTERM` is not handled —
  `SIGINT` shuts down gracefully, and systemd and Kubernetes both send `SIGTERM` by default.

- `examples/` — fifteen worked apps grouped by role, the data behind them, a Jupyter bridge
  and an object-storage refresh loop. `examples/tools/verify.py` asserts per app that an
  interaction both skips work and repaints something; it runs in the `claim` job.

### Changed

- **"No HTTP client anywhere" is now "one crate, optional, off by default".** The old rule
  was worth what it cost — a data-app runtime that can phone home is not one anybody
  self-hosts — and real sources made it impossible to keep literally. It became a shape
  instead: the clients live in `crates/connect` and nowhere else, both are `optional`, and
  `connect`'s `default` feature list is empty. CI checks those three and then checks the one
  that actually holds — that `cargo tree` on a default `dagpane-cli` resolves neither. A
  manifest rule can be satisfied while another crate's default feature turns the client on;
  the resolution cannot. `cargo install dagpane` still links no HTTP stack and no database
  driver.

- `dagpane_app::csv` moved to `dagpane-connect` and is re-exported from its old path, so
  `dagpane_app::csv::parse` still resolves.

- `Frame::memory_size` is now part of the `Frame` trait, required rather than defaulted. It
  was an inherent method on the Arrow backend; a caller admitting apps against a byte budget
  needs to ask it through `Arc<dyn Frame>`, and a default implementation could only guess —
  rows times a nominal width per column type — which is the one thing such a caller must not
  be handed. `Table` answers exactly (buffer capacity plus the heap behind every string),
  Arrow answers with `get_array_memory_size`, so a dictionary-encoded column reports its
  dictionary once rather than once per row. **Breaking** for anything outside this repository
  implementing `Frame`.

### Fixed

- **`ROADMAP.md` §7 and `POSITIONING.md` described measured work as unbuilt.** §7 was titled
  *"the concurrency numbers that do not exist"* and its body said *"nothing here measures what
  it costs to host many apps or serve many viewers"* — while `benches/fleet/` held the memory
  comparison, `benches/loadgen/` held **224 apps on one core** across four runs, and both held
  p99 under load. `POSITIONING.md` graded the same condition *"not met"* on the grounds that
  *"no apps-per-core number has been measured. No p99 has been measured. No memory figure has
  been measured"*, and asserted that *"the multiplexing story does not exist"* — which had been
  false since `crates/host` shipped.

  All four of §7's boxes are ticked, with what each one actually measured beside it, and a new
  list of what is genuinely still open underneath. §7 keeps both of its wrong headers on the
  record, because the pattern is the point: a roadmap item is not done when the work lands, it
  is done when the file claiming it is undone is edited.

- **`--idle-minutes` configured a sweep that never ran.** `Host::sweep_idle` has existed since
  the day `crates/host` did and was called only by that crate's own tests, so in the shipped
  binary an app left residency on a redeploy or under budget pressure and **never for being
  idle** — whatever the flag said. `OPERATIONS.md` and `ROADMAP.md` §1 both listed it as a known
  gap. `dagpane_serve::sweep_idle` is the caller; it lives in the crate that owns a clock,
  because the one that owns the map deliberately does not. It runs at half the idle window, so
  an app leaves somewhere between `--idle-minutes` and one and a half times it, and
  `--idle-minutes 0` keeps the old behaviour.

  The sizing advice that went with the gap was wrong as a result: *"size `--budget-mb` on the
  assumption that everything opened stays resident until the budget forces a choice"* was only
  honest while nothing was sweeping.

- **Evictions were recorded and never printed.** The log is in memory and bounded and nothing in
  the shipped binary read it, so an app leaving residency was invisible from outside except as a
  slower next request. The sweep prints one line per eviction, naming the key, the reason and the
  bytes freed.

- **`OPERATIONS.md` promised a reconnect that did not exist.** *"A restart drops every open
  page. Viewers reconnect automatically and get a fresh first render"* has been in the
  **Refreshing the data** section for three releases, and it was false: `client.html` answered
  a closed socket with a banner reading "Disconnected" and stopped. Every documented data
  refresh was therefore a reload on every open dashboard. The sentence is now true rather than
  edited.

- **`ClientMessage::Refresh` said it was what a client does after a reconnect.** It is not, and
  the reason is structural: a session *is* a connection, so a reconnect is a new session and
  its opening frame is an `init` carrying the viewer's values back off the query string — there
  is nothing left over to refresh. Nothing in this repository ever sent it. The message stays,
  for a client that keeps a socket across losing its own state; a page is not one.

- **ADR-0009 claimed a rolling update was invisible to viewers.** Half true as written: the
  drain closed sockets cleanly and the page's answer to a clean close was that banner, so
  "their next connection" meant a reload the viewer had to think of. The ADR now carries the
  correction beside the claim rather than a quiet edit.

- **`SIGTERM` is handled, in every mode.** It was not, and what that meant depended on the
  PID: as an ordinary process, immediate termination; **as PID 1** — which is how the published
  `scratch` image runs the binary — there is no default disposition to apply, so the signal was
  *ignored* and the process ran until the grace period expired and `SIGKILL` arrived.
  `OPERATIONS.md` measured 11 s for a `podman stop`, paid once per replica per rollout. Since
  systemd and Kubernetes both send `SIGTERM` by default, that was the ordinary path and not an
  edge case. Measured now: `SIGTERM` to `dagpane run`, exit `0` in 3 ms. The
  `KillSignal=SIGINT` and `STOPSIGNAL SIGINT` workarounds `OPERATIONS.md` recommended are no
  longer needed, and are harmless if you have them.

- **`examples/renderers/heatmap.js` printed `Infinity – -Infinity`** as its legend when a
  frame arrived empty or with a value column that was all nulls, because `min`/`max` were
  still their seeds. It now reads `no data`, keeping the row and column names — an empty
  result should look empty, not like a fault in the engine.

- `SECURITY.md` and this file both said the `/ws` upgrade does not check `Origin`. It has
  since the check landed; the text had not followed it. Corrected in both, with the residual
  weakness — a request with no `Origin` is allowed — stated rather than dropped.

## [0.1.1] — 2026-09-17

Two gates that had never run, and a container image that had never been built.

All three were found within minutes of publishing 0.1.0, and they share one cause: the
checks that would have caught them live on the public mirror, and the mirror had never
been synced. The first real publish ran them for the first time. Nothing in the engine
changed here — `fmt`, `clippy`, the full test suite, the architecture invariants and the
job that re-measures every count the README prints all passed on 0.1.0 and still do.

**0.1.0 has no container image.** Its build failed on the assertion below, so
`mancube/dagpane` carries no `0.1.0` tag and never will. 0.1.1 is the first published
image.

### Fixed

- **The Dockerfile could not build, and the check that was meant to catch that could not
  work.** The final builder stage asserted static linking by asking `ldd` for "not a
  dynamic executable" or "statically linked". That is glibc's wording and the builder is
  Alpine. musl's `ldd` prints the loader path for a static binary and a dynamic one
  alike, so the check was reading an answer that carries no information, and the `grep`
  simply never matched. Replaced with the thing that actually differs: a dynamically
  linked executable carries a `PT_INTERP` program header naming its interpreter, and a
  static one has nothing to name. `readelf` now answers that question, `binutils` is
  installed explicitly rather than relied on, and the output is printed instead of being
  swallowed by `grep -q`, so the next failure says what it saw.

- **The licence gate rejected a public-domain dedication.** `cargo-deny` refused
  `tiny-keccak`, which reaches the tree as `arrow-array` and `arrow-select` -> `ahash` ->
  `const-random` -> `const-random-macro`. Its licence is CC0-1.0, which is a dedication
  rather than a copyleft licence: it puts no condition on a binary that links the code,
  so allowing it satisfies the rule `deny.toml` already states rather than bending it. It
  arrived with the Arrow backend and went unnoticed because this gate had never executed.

## [0.1.0] — 2026-09-17

First public release, and the first publication of this source anywhere. The mirror
`lucheeseng827/dagpane` was created on 2026-09-10 and has carried nothing since but the
README it was initialised with: the sync publishes on a `dagpane-oss-v*` tag and no such
tag had been pushed, so nothing was ever mirrored. The code below is not new work — it
has been building and passing its gates in the private tree — but this is the first time
anyone outside it can read, clone or depend on it.

### Added

- **`dagpane` — a reactive runtime for data apps.** An interaction recomputes the cells
  that depend on it, and nothing else. `crates/core` is the engine: cells, declared edges,
  a build-time topological order, dirty propagation over the structural closure, and a
  trace saying which cells ran. Pure — no I/O, no async, no network, no clock, no `unsafe`,
  one dependency (serde).

- **Declared edges, and the three things that follow from them.** A cell names its inputs
  rather than having them traced at run time, so a cycle is a *build* error carrying the
  loop path, the graph is built once and shared immutably (`Arc<Graph>`) across every
  session, and `dagpane graph` can print the app before it runs. The cost is stated rather
  than hidden: a cell that reads an input only on a branch it did not take still declares
  it, and still re-runs when that input moves. See `docs/adr/0001-declared-edges.md`.

- **Height-ordered evaluation, which is what makes the pass glitch-free.** Every cell
  carries the longest path from any source, computed when the graph is built, and a pass
  evaluates in ascending height. Every edge runs from a lower height to a strictly higher
  one, so a cell's inputs are final before it runs: a diamond evaluates its join exactly
  once and never on a mixture of old and new values. No re-entrancy, no second pass, no
  recompute-until-stable loop. `docs/adr/0002-height-ordered-evaluation.md`.

- **A 128-bit content digest, taken once when a value is produced.** A cell reuses when its
  inputs' digests match the ones it recorded the last time it ran, which makes the decision
  two `u128` comparisons for a 600-row table instead of a walk over it. This is also what
  contains an over-declared edge: the cell recomputes, produces the value it already held,
  and nothing below it runs. Written in-tree, FNV-1a, seventy-nine lines with the comments
  stripped. `docs/adr/0003-digest-equality.md`.

- **Errors are values.** A failing cell holds a `CellError`; every cell below it holds
  `Upstream { cause, message }` naming the cell that actually failed rather than the
  immediate input. The pass finishes and the rest of the page renders, so one bad column
  takes out one number and leaves the slider that will fix it working.
  `docs/adr/0004-errors-are-values.md`.

- **A TOML manifest that compiles to a graph.** Sources (CSV), inputs with their widgets,
  cells as a pipeline of seven verbs — `filter`, `select`, `sort`, `limit`, `group_by`,
  `scalar`, `count` — and panes. No SQL and no expression language: the manifest's job is to
  produce *edges*, and a dependency inferred wrongly from SQL text by a regular expression
  is a wrong app. Unknown keys are refused rather than ignored.

- **A patch protocol, not a page.** `ServerMessage::Patch` carries only the panes whose
  *rendered view* differs from the one the viewer already has — a second check beyond the
  engine's, because a table pane sends its first 50 rows and a change in row nine thousand
  changes the cell and not the view. Every patch carries a `PassStats` block saying how many
  cells the pass looked at, ran, changed and never touched, so the claim is checkable in a
  browser's network tab.

- **The CLI: `check`, `graph`, `explain`, `run`.** `graph` prints text, Mermaid or JSON
  without executing a cell. `explain --set NAME=VALUE` runs one interaction and prints
  exactly what it cost, with `--json` for a CI gate that asserts on the numbers.

- **The server, and a client that is one file.** axum 0.8 and tokio in `crates/serve` only;
  one session per WebSocket connection; the whole front end compiled into the binary as a
  single HTML file with inline CSS and JS — no npm, no bundler, and nothing fetched at run
  time. A unit test enumerates every `http`-prefixed string in it and asserts there are
  exactly two, so a third one fails the build instead of quietly making an air-gapped
  deployment render blank.

- **The differential oracle**, `crates/core/tests/oracle.rs`. 200 pseudo-random DAGs of 8
  to 40 cells from a 64-bit LCG written in-tree (so a failing seed reproduces anywhere,
  forever), 12 random interactions each — 2,400 commits — and after every one of them two
  assertions: **correctness**, every cell's value is identical to what a fresh session
  computes from scratch with the same inputs; and **economy**, every cell the pass touched
  is inside `graph.closure(roots)`. A count assertion on a hand-drawn graph is a test of the
  author's expectations, and a scheduler that skips too much passes all of those.

- **Also asserted, because each is a property somebody could break while optimising:** two
  sessions over one `Arc<Graph>` share one allocation per untouched source (`Arc::ptr_eq`,
  not an RSS measurement, which is flaky on every runner); setting an input to the value it
  already holds produces an empty pass; a repeated identical failure does not re-wake the
  page; recovery clears a whole poisoned subtree in one pass.

- **A static musl binary and a `scratch` image.** No shell, no libc, no package manager, no
  Python and no Node — the front end is inside the executable. CI builds it, checks it is
  actually static, and runs `check` against the bundled app with it.

### Fixed

- **An error's digest covered its message and not its attribution, so a cell could go on
  naming a failure that had moved.** Two upstream cells failing with the same words digested
  alike; a downstream cell whose *cause* changed from one to the other compared equal,
  served its cached error, and kept naming the cell that was no longer the problem. The page
  showed a plausible message pointing at the wrong cell.

  Found by `tests/oracle.rs`, not by review and not by any hand-written test — every one of
  those used a single failing cell, and a single failing cell cannot exhibit it. The oracle
  found it because its `Divide` op fails on any zero input and 200 random graphs eventually
  put two of those under one join.

  Fixed by giving `CellError` its own `Digestible` impl, which tags the two variants apart
  and hashes `cause` before `message`. The regression test is
  `a_failure_that_moves_to_a_new_origin_stops_naming_the_old_one` in
  `crates/core/tests/reactive.rs`. The lesson recorded in ADR-0003 is not "hash more
  fields" — it is that anything the engine treats as a value must digest everything a
  reader will act on, and attribution is something a reader acts on.

  Found and fixed inside this development cycle, so no released version carries it.

### Measured

On this checkout, 2026-09-08, with the commands named:

```
$ dagpane explain examples/sales.toml --set min_amount=400
dagpane: Sales explorer — 11 cells, 7 panes
first render: 8 of 11 cells evaluated

set min_amount = 400
  epoch 2 — looked at 8 of 11 cells
    set      min_amount
    ran      filtered             changed
    ran      order_count          changed
    ran      region_totals        changed
    ran      channels             same value — nothing below it ran
    ran      top_orders           same value — nothing below it ran
    ran      revenue              changed
    reused   channel_count        its inputs had not moved
  3 cell(s) never looked at: sales, region, all_time_revenue
  patch: 3 of 7 panes — revenue, order_count, region_totals
```

The app is 11 cells — one CSV source, two controls, eight computed — and 7 panes. One
slider move visits 8 of them, runs 6, reuses 1, never looks at 3, and repaints 3 panes.
Two of the six that ran produced the value they already held, which is why nothing below
them ran. `.github/workflows/ci.yml`'s `claim` job reads those eight numbers out of `explain --json` and
fails the build when they move, because a document a reader can falsify in one command
costs more than the change that moved it.

`cargo test --workspace --locked`: **122 passed, 0 failed**, same checkout and date.
`cargo check -p dagpane-core --target wasm32-unknown-unknown` exits 0, and CI runs it.

### Known limits

Stated here as well as in the README, because a changelog is where people look for what
changed and not for what was never there.

- **No authentication.** `dagpane run` binds `127.0.0.1`; `--host` anything else prints a
  warning naming the consequence. A session is a connection — closing the tab discards it —
  and there is no session store, no eviction, no TTL and no reconnection token. `SECURITY.md`
  has the rest. The `/ws` upgrade does check `Origin` against the bound address, which stops
  another page in the same browser from opening the socket — but a client that sends no
  `Origin` at all is allowed, so loopback still means "only software on this machine can read
  it" and not "only you can read it".

- **Nothing runs in a browser.** `cargo check -p dagpane-core --target
  wasm32-unknown-unknown` passes and CI runs it. That is the entire WASM claim. There is no
  `crates/wasm`, no wasm-bindgen and no client-side compute; the check is a real constraint
  on the core — no filesystem, no clock, no threads — and nothing more.

- **The table is small on purpose.** `Vec<Option<T>>` per column, four column types, no
  chunking, no dictionary encoding. It is not Arrow and does not pretend to be. There is no
  polars, no duckdb and no arrow; `ARCHITECTURE.md` §6 names the seam a real engine plugs
  into and exactly which types change when one does, and there is deliberately no trait in
  the tree with one implementation and no second caller.

- **Invalidation is at whole-value granularity.** Change one cell of a `Table` and
  everything reading that table recomputes. Column-level provenance — the comemo idea named
  in `NOTICE` — is the gap this version has not closed.

- **The dirty closure is structural.** A pass *visits* every cell downstream of what
  changed, including the ones that turn out to reuse. Visiting is a digest comparison per
  input, so it is cheap, but it is not zero and `Trace::visited` reports it rather than
  folding it into `evaluated`.

- **No performance claim of any kind.** No apps-per-core figure, no p99 under concurrent
  viewers, no memory measurement, and no comparison in seconds against any other runtime.
  None has been measured. The only numbers this project prints are cell counts, pane
  counts, test counts and the trace.
