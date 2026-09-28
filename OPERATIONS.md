# Operating dagpane

How to run this in front of other people, what it costs, and what it will not do for you.
`docs/RUNBOOK.md` is the symptom-first companion: reach for it when something is already
wrong.

Everything below was checked against `dagpane 0.1.1` on this tree rather than inferred from
the source. Where a number comes from `BENCHMARKS.md` it says so, because those were measured
on one machine and yours is not that machine.

---

## What you are deploying

One statically linked binary that reads files and serves HTTP. It serves **one** app on one
machine (`dagpane run`), **one app as any number of replicas** (`dagpane serve`), or **many
apps on one port** (`dagpane host`), and which of those you run is the only structural choice
in this document. Everything below holds whichever it is:

* **No database, no cache, no broker, no sidecar** it depends on.
* **No writes.** It opens your sources read-only and never writes to disk.
* **No server-side session state.** A session *is* a WebSocket connection: no session store,
  no reconnection token, nothing to back up. The viewer's own control values live in their
  page's URL **fragment** — which a browser never puts on the network — and the client
  replays them on connect, so a reload returns to the same filtered view and a filtered
  dashboard is a link somebody can be sent, with nothing kept here.
* **No outbound network in the build you are running.** The published image is compiled with
  default features, so it links no HTTP client and no database driver at all. Build `http` or
  `sql` in and `crates/connect` is the one place either may appear. The front door never makes
  a request whichever way you build it: it verifies tokens against a JWKS **file**.
* **Authentication is optional, and off unless you turn it on** with `--auth-jwks`. Read the
  front door section before the topology section either way — what it covers and what it does
  not is the constraint the rest of this bends around.

Four endpoints, and no others:

| endpoint | what it is |
|---|---|
| `GET /` | the single client page, served **without** a token, because the page is where signing in starts. It carries no data, and it fetches nothing off this host but the three rows below |
| `GET /auth` | what the page needs before it has a token: whether one is required, and the browser sign-in parameters if you configured them. JSON, and no secret in it |
| `GET /ws` | the WebSocket upgrade. One connection, one session — and the place the token is actually checked |
| `GET /<renderer>` | a renderer script an app declared in `[app] renderers`, and nothing else. Served from memory — the bytes were read when the app was compiled, so no request steers an `open(2)`, and a path this process did not already hold is a `404`. Absent unless a manifest declares one |

The renderer route is the only one that arrived after `0.1.0`, and it is the reason the page's
old *"fetches nothing"* is now *"fetches no third party"*. It is a map lookup over bytes fixed
at compile time, not a static file server: pointing this process at a directory still does not
put that directory on the network.

The process is therefore horizontally scalable by accident — any number of replicas behind a
load balancer are correct, because there is nothing to share and nothing to stick. What they
must share is the *same snapshot of the data*, and under `dagpane host` the same manifest
directory: a manifest's bytes are its identity, so two replicas holding different bytes are
serving two different apps under one name. That is a deployment problem, not a runtime one —
but it is now one you can **see**, which it was not before. See **Refreshing the data**.

**`dagpane serve` adds a second listener, and it is not on the app's port.** Two more
endpoints, for the supervisor rather than the public:

| endpoint | what it is |
|---|---|
| `GET /healthz` | liveness, plus this replica's identity: the app name, the **digest of the manifest bytes it holds**, its cell and pane counts, its state and its version. `200` for as long as the process runs, **including while it drains** — see below on why |
| `GET /readyz` | readiness: `200` while serving, `503` the moment a drain begins. This is the one a load balancer polls |

`dagpane host` takes the same two, and answers the first of them differently: `/healthz` lists
the apps it is **currently** holding, each with the digest of its manifest bytes and what it
costs, against the byte budget. A fleet's residency changes while it runs — apps arrive on first
request and leave under budget pressure or the idle sweep — so that is asked per request rather
than snapshotted at start-up.

**Readiness under `host` is whether the process can compile and serve, not whether every app it
holds is healthy.** A fleet of four hundred manifests where one is broken is not an unready
replica: that app's viewers get its error and the other three hundred and ninety-nine are fine.
Failing the probe on it would pull a working replica out of the pool to report a fault that
*every* replica has, for the same reason, at the same moment — so every replica goes unready
together and one bad file takes the whole service down. Put `dagpane check` in front of the
deploy; that is the gate for a broken manifest, and it is the one that fails before anything is
listening.

They are on `--admin-port` (default `9787`) rather than the app's port for three reasons that
all point the same way: the app's port has four endpoints and that is a claim worth keeping, a
`/healthz` there would collide with the renderer route's namespace, and the app's port is the
one an ingress publishes. **Leave the admin port out of the Service, the target group or the
upstream.** It carries no row of anybody's data, but it is not a page to publish.

The digest is the one `dagpane host` already routes and evicts by, so the two modes agree on
what an app's identity is. Checking that a fleet agrees is one line:

**Under `dagpane serve`**, where the process holds one app and `.fingerprint` is that app:

```console
$ for ip in $(kubectl get endpointslices -l kubernetes.io/service-name=sales \
      -o jsonpath='{.items[*].endpoints[*].addresses[0]}'); do
    curl -s "http://$ip:9787/healthz" | jq -r .fingerprint
  done | sort -u
732b83079d170591
```

**Under `dagpane host` there is no `.fingerprint`**, because there is no single app: the payload
carries an `apps` array, one entry per resident app with its own digest. The same check is then
per app, and a replica holding a *different set* of apps is as interesting as one holding
different bytes — so compare the whole pairing rather than a single field:

```console
$ for ip in $(kubectl get endpointslices -l kubernetes.io/service-name=panels \
      -o jsonpath='{.items[*].endpoints[*].addresses[0]}'); do
    curl -s "http://$ip:9787/healthz" | jq -cr '[.apps[] | "\(.app)@\(.manifest)"] | sort'
  done | sort -u
["sales@732b83079d170591","weekly@4c19aa02b1e7d350"]
```

**Residency is not deployment, and this is the one place that distinction bites.** A host
compiles an app on its first request, so two replicas legitimately differ while one has been
asked for something the other has not. What must not differ is the digest **for an app they both
hold** — so a second line whose app *names* differ is a warm-up, and one where the same name
carries two digests is the failure. The `jq` above shows both; only the second is a problem.

One line out is a finished rollout. Two is two apps under one name, which previously had no
symptom at all except viewers disagreeing with each other about what the data said.

---

## The constraint that catches everyone: the Origin check

**Symptom when you get this wrong: the page loads and the controls do nothing.**

The `/ws` upgrade checks the browser's `Origin` header against an allowlist derived from **the
address the process bound**, and nothing else. There is no flag to change it. This is an
anti-CSRF measure — without it, any page open in the same browser could connect to
`ws://127.0.0.1:8787/ws` and read every pane of your app — and it is *not* authentication.

Verified against a running `dagpane 0.1.1` bound to `127.0.0.1:8802`:

| `Origin` sent | result |
|---|---|
| *(none — `curl`, scripts, tests)* | `101 Switching Protocols` |
| `http://127.0.0.1:8802` | `101 Switching Protocols` |
| `https://127.0.0.1:8802` | `101 Switching Protocols` |
| `http://localhost:8802` | `101 Switching Protocols` |
| `https://panels.example.com` | **`403 Forbidden`** |
| `https://evil.example` | **`403 Forbidden`** |

```
this app does not accept WebSocket connections from https://panels.example.com;
open it at its own address
```

The fifth row is the deployment case. Put this behind nginx, an ALB, or a Kubernetes Ingress
on a real hostname and the browser sends *that hostname* as the Origin — which is never the
bound address — so `GET /` returns the page with `200` and the upgrade is refused with `403`.
The page renders its first paint and then sits there, inert, with nothing in the server log to
explain it.

**`--host 0.0.0.0` makes this unconditional, which is worse than it first looks.** The
allowlist is derived from the bound address literally, so binding `0.0.0.0:8787` allows exactly
`http://0.0.0.0:8787` and `https://0.0.0.0:8787` — origins no browser ever sends. Since
`--host 0.0.0.0` is required for a container to be reachable at all, **every containerised
deployment needs one of the two shapes below**, even reached over plain `localhost` with no
proxy in the picture. Verified against a process bound `0.0.0.0:8813`:

```console
$ curl ... -H 'Origin: http://127.0.0.1:8813' http://127.0.0.1:8813/ws
403
$ curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8813/
200
```

That `200` on the page beside a `403` on the socket is the whole trap in two lines.

Both `http://` and `https://` of the bound address are allowed, because the process cannot
know whether something in front of it terminates TLS. `localhost` is allowed alongside a
loopback IP because a browser sends whichever the user typed. Neither helps a proxy on a
different hostname.

**`dagpane serve --origin` is the flag this section used to say did not exist.** Name the
origins you publish the app at, once per name, and they replace the list derived from the bound
address outright:

```sh
dagpane serve /app/app.toml --origin https://panels.example.com
```

Verified against a process bound `0.0.0.0` with that flag — the four rows that matter:

| `Origin` sent | result |
|---|---|
| `https://panels.example.com` | `101 Switching Protocols` |
| a second `--origin` you also declared | `101 Switching Protocols` |
| `https://evil.example` | `403 Forbidden` |
| `http://0.0.0.0:8787` — the address it bound | **`403 Forbidden`** |

That last row is the design: declaring origins **replaces** the derived allowlist rather than
extending it. Once you have said where the app is published, the address it happened to bind is
not one of those places.

It is scheme, host and port, exactly as a browser sends them — **no trailing slash and no
path**, which are the two ways to get this wrong and which look right in an address bar. Both
are refused at start-up, by name, rather than at the first upgrade:

```console
$ dagpane serve app.toml --origin https://panels.example.com/
dagpane: `https://panels.example.com/` has a trailing slash and an `Origin` header never
does — a browser sends `https://panels.example.com`. Drop the slash
```

A wildcard bind with no `--origin` at all prints a warning at start-up saying the page will
load and the controls will do nothing, because that is exactly what will happen.

**Without that flag — under `dagpane run`, or on an older binary — the supported shapes are:**

1. **Reach it at the address it bound** — and note that this means the process must have
   bound a *loopback* address. An SSH tunnel (`ssh -L 8787:127.0.0.1:8787 host`) is the least
   work and keeps the no-auth story honest: the remote process binds `127.0.0.1:8787`, the
   browser's origin is `http://127.0.0.1:8787`, and that matches.

   **A tunnel does not rescue a `0.0.0.0` bind.** Forwarding a local 8787 to a process bound
   `0.0.0.0:8787` still sends `Origin: http://127.0.0.1:8787` while the allowlist holds only
   `http://0.0.0.0:8787`, so the upgrade is still refused. A wildcard bind needs option 2.
2. **A proxy that validates the incoming `Origin`, then rewrites it** to the bound address on
   the upgrade request. Both halves matter: an unconditional rewrite forwards *every* origin
   as the allowed one, which hands back exactly the CSRF the check exists to stop — any page
   in a viewer's browser could then reach your panel through your proxy. So allowlist first,
   rewrite second:

   ```nginx
   # The origins YOU serve this panel on. Anything else never reaches the upstream.
   map $http_origin $dagpane_origin_ok {
       default                          0;
       "https://panels.example.com"     1;
   }

   location /ws {
       # An absent Origin is a non-browser client, which is not the case this defends against.
       if ($http_origin != "") { set $dagpane_check "$dagpane_origin_ok"; }
       if ($dagpane_check = "0") { return 403; }

       proxy_pass http://127.0.0.1:8787;
       proxy_http_version 1.1;
       proxy_set_header Upgrade    $http_upgrade;
       proxy_set_header Connection "upgrade";
       # REQUIRED: without this the upstream refuses and the page loads but never updates.
       proxy_set_header Origin     "http://127.0.0.1:8787";
   }
   ```

   Authentication at the proxy does **not** substitute for that allowlist: a CSRF request
   carries the viewer's own cookies, so an authenticating proxy will happily let it through.
   The two controls answer different questions — *who is this* and *which page asked* — and
   you need both. See the next section for the first one.

**Under `dagpane host` the rule generalises, and most of the trap goes with it.** With many
apps on one port there is no single bound address to build an allowlist from, so the check
compares the `Origin` against the **`Host` header the request already carries**: a page at
`https://sales.example.com` may open `sales`'s socket, and a page at anything else may not.
That is the same rule — *open it at its own address* — stated for a server that has more than
one, and it is the version that survives a proxy. Forward `Host` unchanged and there is no
`Origin` to rewrite:

```nginx
location / {
    proxy_pass http://127.0.0.1:8787;
    proxy_http_version 1.1;
    proxy_set_header Upgrade    $http_upgrade;
    proxy_set_header Connection "upgrade";
    # This one header both routes the request and satisfies the Origin check.
    proxy_set_header Host       $host;
}
```

[`docs/PUBLIC-HTTPS.md`](docs/PUBLIC-HTTPS.md) is this shape done with eggrd, which **cannot**
forward `Host` unchanged — it sets one from its own upstream URL, so the recipe there is to
make that URL's hostname the public name. It also carries the TLS and abuse settings, and the
one eggrd default that breaks every dashboard until it is changed.

The port is deliberately not compared, because a browser's `Origin` carries whatever port it
was given — the proxy's — and never this process's. The hostname is compared in full and
case-insensitively; the app is the **first label** of it, so `sales.example.com` and
`sales.internal` both route to `sales.toml`.

Test it from the shell before you hand out the URL. The page returning `200` proves nothing:

```console
$ curl -s -o /dev/null -w '%{http_code}\n' \
    -H 'Connection: Upgrade' -H 'Upgrade: websocket' \
    -H 'Sec-WebSocket-Version: 13' -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' \
    -H 'Origin: https://panels.example.com' \
    https://panels.example.com/ws
101
```

`101` means the upgrade would succeed. `403` means you have shipped a dead page.

---

## The front door, and the three things it does not do

**It is optional and off by default.** With no `--auth-jwks`, there is no login and no
authorisation, `dagpane run` binds `127.0.0.1`, and anything else prints:

```
dagpane: warning — binding 0.0.0.0:8787, which is reachable from outside this machine,
         with no authentication. Anyone who can reach that address can read every
         pane of this app. Pass --auth-jwks to put a front door on it, or put one in
         front. See SECURITY.md.
```

`dagpane host` prints the same thing about *every app below*, which is the case where it
matters more.

### Turning it on

The process **verifies** OIDC id tokens; it never obtains one and never talks to your
provider. You supply the JWKS as a file:

```sh
dagpane host ./apps \
  --auth-jwks idp-jwks.json \
  --auth-issuer https://idp.example.com/ \
  --auth-audience dagpane \
  --auth-alg RS256 \
  --auth-apps-claim dagpane_apps
```

That is the operational property rather than a limitation: there is **no discovery request to
fail at start-up**, nothing to redirect, and an air-gapped install works. Rotating keys is
copying a file and restarting. The flags are flat rather than one opaque string because OIDC
genuinely has this many inputs and you want to know which one you got wrong — and a policy
that cannot work is refused at start-up rather than refusing every viewer later, which looks
identical from outside and is far harder to diagnose.

Two flags you must choose between, with no default: `--auth-apps-claim NAME` (a token's claim
lists the apps its holder may open; `*` is all of them) or `--auth-any-app` (any valid token
opens anything this process serves — right for a single app behind something that already
decided who may reach it, wrong for a fleet). Given neither, the process will not start.

For a browser sign-in, add `--auth-authorize-url`, `--auth-token-url` and `--auth-client-id`
and the bundled page runs PKCE itself — a public client with no secret, which is what a page
is. Without them the page says a token has to arrive another way, which is the right answer
when an authenticating proxy in front is already supplying one. The token is never put in a
URL: it travels in `Authorization: Bearer`, or in a `Sec-WebSocket-Protocol` entry for a
browser, which cannot set headers on a WebSocket. In the page it lives in `sessionStorage`
and dies with the tab.

**Note what the socket's query string does carry.** The viewer's saved control values ride on
the `/ws` URL as `?s=…` — not the token, and not on the page's own URL. If you log full
request URLs at the proxy, you are logging which filters people chose.

### What it does not cover

Stated because the gap is the interesting part, and it is the same list as `SECURITY.md`:

* **Per-pane authorisation.** Access is per **app**. A viewer who may open an app sees every
  pane of it. Split data that some viewers must not see into separate apps, do not rely on a
  pane.
* **An audit of who read what.** A refusal is logged with the subject. An accepted connection
  is not logged at all.
* **Revocation before a token expires.** There is no introspection call and no deny list —
  again because this process makes no outbound request. `--auth-leeway-secs` defaults to 5,
  and the practical control is a short token lifetime at your provider.

### Whether the door is on or off, these stay yours

Three consequences an operator owns:

* **With no `--auth-jwks`, whatever is in front must authenticate**, and must do so before the
  request reaches the port — an OAuth2 proxy, an identity-aware proxy, an SSO ingress, or a
  network boundary you actually trust. This is now one of two answers rather than the only
  one, and it remains a perfectly good answer where a proxy already does it.
* **Sources are shared across every session, and the front door does not change that.** There
  is no per-viewer filtering and no way to add one from outside. Everyone who gets through
  sees the same rows, so **do not put rows in the file that some viewers should not see.**
  Split the data into separate apps and let the apps claim decide, which under `dagpane host`
  is a directory of manifests rather than a second deployment.
* **No rate limit and no inbound message size limit.** A connection is a sequential loop, so
  one client cannot overlap passes with itself, but nothing bounds how fast it sends or how
  many connections exist. `refresh` costs no recomputation and serialises every pane, which
  makes it the cheapest message to send and one of the more expensive to answer. If the port
  is reachable by anyone you would not hand a shell to, put a rate limit in the proxy.

`SECURITY.md` has the full list of known weaknesses and the threat model. Read it before the
first deployment, not after the first incident.

---

## Lifecycle and signals

**`SIGTERM` and `SIGINT` both stop every mode gracefully.** Verified by signalling a running
process:

| signal | behaviour | exit |
|---|---|---|
| `SIGINT` | **graceful** — `axum` stops accepting, in-flight requests finish | `0` |
| `SIGTERM` | **graceful, identically.** Under `dagpane serve` it runs the drain below first | `0` |

That second row used to read *not handled*, and the consequence was worse than it sounds. The
published image is `scratch` plus a static binary, so the binary is **PID 1** — and PID 1 has
no default disposition to apply, so an unhandled `SIGTERM` is not a termination, it is
*ignored*. The process ran until the stop grace period expired and `SIGKILL` arrived: a
measured 11 s for a `podman stop` that should have taken milliseconds, paid once per replica
per rollout. Since systemd and Kubernetes both send `SIGTERM` by default, that was the normal
path and not an edge case. Measured now, on the same shape: `SIGTERM` to `dagpane run`, exit
`0` in 3 ms.

**`KillSignal=SIGINT` and `STOPSIGNAL SIGINT` are no longer needed.** They are harmless if you
already have them. Leave `TimeoutStopSec` / `terminationGracePeriodSeconds` comfortably above
your drain window.

### Draining, under `dagpane serve`

A stop that is graceful to *this* process can still be visible to viewers, and the reason is
ordering. A replica that closes its listener the moment it is signalled is still in the load
balancer's pool — the balancer finds out at its own pace, and every connection it routes in
the meantime is a reset. So `dagpane serve` stops in three steps:

1. **Go unready.** `/readyz` starts answering `503`. The listener is still open and in-flight
   work is untouched.
2. **Wait `--drain-seconds`** (default `5`). This is the window the balancer has to notice.
3. **Stop accepting**, and let what is in flight finish.

Measured against a real `SIGTERM`, with `--drain-seconds 3`:

```
before: readyz=200  healthz=200
during: readyz=503  healthz=200  state="draining"  app port still serving 200
exit=0 elapsed=3.004s
after:  both ports refused
```

Two things in that transcript are the point. The app port **still serves 200 while `/readyz`
says 503** — that is the drain working rather than a contradiction. And `/healthz` **stays
200**: a liveness probe that went red during a drain would have the supervisor conclude the
process had hung and `SIGKILL` it, turning the graceful stop into exactly the ungraceful one
the drain exists to avoid. Liveness answers *is this alive*; readiness answers *should it be
sent work*. Wire them to the two different endpoints.

Set `--drain-seconds` above your balancer's health interval times its unhealthy threshold —
that product is how long the balancer may take to notice. Too short and the last connections
routed here are reset; too long and every rollout pays it per replica.

**`dagpane host` drains the same way**, because the thing being drained is the same thing:
connections, which belong to no app in particular. `dagpane run` stops gracefully but does not
drain — it is not behind a balancer, so there is nothing to wait for.

### The other half: the page comes back

A drain is only worth something if the viewer's page reconnects, and until recently it did not
— a clean close produced a banner reading "Disconnected" and nothing else, so a rolling update
was invisible on the server and a reload on every open dashboard.

**The page now retries on its own**: exponential backoff, capped at 30 s, jittered so that
every viewer of a drained replica does not arrive back in the same instant. It carries its
control values on the socket's query string, so it comes back to the view it left in one pass,
on whichever replica answers. Nothing to configure.

**One close is not retried**, on purpose: a socket that never spoke, on a page holding a token,
is a refusal rather than a drop. That page says so and offers a sign-in, instead of hammering a
door that is not going to open.

### The quiet connection, and the timeout in front of it

**A dashboard nobody is clicking is the ordinary case, not an idle one.** Whatever is in front
of this process does not know that: an AWS ALB, an nginx `proxy_read_timeout` and most ingress
controllers reclaim a connection they have seen no bytes on for **60 seconds**. So a viewer who
is merely *reading* gets disconnected, and — before the reconnect above — stayed that way.

So the server sends a **WebSocket Ping every 25 s** by default, which is under that 60 and
leaves room for one tick to be missed. `--heartbeat-seconds` changes it and `0` turns it off.
It needs nothing from the page: a browser answers a Ping in the transport with no JavaScript
involved, and cannot send one from script even if it wanted to — the server is the only end
that can start this.

**Raise the timeout in front of it as well, not instead.** The two settings answer different
questions, and a proxy configured to reap at 20 s will still reap a connection pinged at 25.

Restarting is the *normal* way to pick up new data, so this is not a rare path. See below.

---

## Sizing

**`--budget-mb` counts source bytes, and that is not all a viewer costs.** Measured
(`BENCHMARKS.md`, *What one more viewer costs*): a session holds the frames its pipeline
materialises, and on identical 200 000-row data a row-keeping pipeline costs **7.2 MB per
viewer** while an aggregating one costs **145 kB** — 50× apart, and neither is in the budget.
The sources genuinely are shared; it is the computed frames that are not, and they cannot be,
because two viewers with two filters must have two answers.

**If you run the published container, read the allocator note first.** Everything below is
measured on a glibc build and the image is a static **musl** binary. A spot check
(`BENCHMARKS.md`, *The allocator, and the build that actually ships*) says the two do not behave
alike: on musl the memory a burst takes **comes back** rather than becoming the floor, and a pass
under load costs **more than ten times** what it costs on glibc. So the memory guidance below is
conservative for the container and the CPU guidance is optimistic for it. Build with glibc if you
want these numbers to describe your process.

**And a viewer who is *using* the page costs more again, permanently.** Measured
(`BENCHMARKS.md`, *What a viewer costs while they are using it*): a pass allocates, the process
does not give the pages back, and firing the same interaction again is nearly free — so the
charge lands **once per viewer who has ever touched a control**, not per interaction. It is
about **7%** on top of the held figure for the bundled example and **20%** for the 200 000-row
row-keeping app. Budget for it the way you would budget for the viewer arriving at all.

What to do about it, in the absence of a budget that sees either term:

* **Work out both figures for your own app** — `./benches/sessions/run.sh` for the held cost,
  `./benches/sessions/busy.sh` for the rest — and hold `--budget-mb` plus *viewers × (held +
  busy)* under the memory you have.
* **Prefer pipelines that aggregate before they reach a pane**, and for a sharper reason than
  this page gave before. A `group_by` upstream of a table pane is the difference between the two
  held numbers above. It does **not** make a pass free — the aggregating app still builds all
  200 000 rows before it throws them away. What it does is make that cost **stop growing with
  viewers**: past about as many concurrent viewers as the process has worker threads, the
  aggregating app is at its ceiling and sixteen times more viewers do not move it, while the
  row-keeping app climbs all the way.

  The two pipelines' passes do not cost the same and an earlier draft of this bullet said they
  did. Measured, one viewer dragging: **≈ 16 MB on the row-keeping pipeline** — it is left
  holding a fresh allocation of everything it materialised — against **0 MB on the aggregating
  one**, whose scratch buffer was already resident and free from the sessions opening. Per
  extra viewer who has interacted: **flat against ≈ 1 449 kB**.
* **An app whose panes show raw rows is the expensive shape**, and it is expensive per viewer
  rather than per app, so it gets worse exactly when the app succeeds.

From `BENCHMARKS.md` — median of three runs, one x86-64 Linux machine, release profile, six
columns with only the row count varying. Treat as an order of magnitude, not a promise:

| rows | CSV | first render | one interaction | RSS |
|---:|---:|---:|---:|---:|
| 600 | 0.04 MB | 7 ms | 8 ms | 10 MB |
| 10 000 | 0.4 MB | 55 ms | 67 ms | 10 MB |
| 100 000 | 3.7 MB | 1 000 ms | 768 ms | 67 MB |
| 1 000 000 | 37.8 MB | 8 471 ms | 10 216 ms | 637 MB |

**The interactive ceiling is around 100 000 rows.** Below ten thousand, every interaction is
imperceptible. At a million, a filter over the source cell costs a million rows of work however
few other cells run — the graph saves you the cells that did not change, not the scan itself.

**Memory amplifies about seventeen times** over the CSV on disk, and the ratio settles as rows
grow (25× at ten thousand, 17× at a million), so it is the per-row representation rather than
fixed overhead. Size the container against RSS, not the file. The `frame-arrow` backend cuts
this by roughly 40% — it is behind a default-on feature — and `ARCHITECTURE.md` §6 names the
seam a real engine would plug into.

Practical limits to set:

* **Memory limit:** at least 20× the total size of the CSVs, plus headroom, **plus the two
  per-viewer terms in *Sizing* above** — that 20× is a figure for a process with one viewer and
  it is the smaller number the moment the app is popular. A limit below the load-time peak means
  the process is killed during start-up, which reads as a crash loop.
* **CPU:** more cores buy concurrency across viewers, never speed for one — a pass is sequential
  per connection and always will be. **What this line used to say next was "one core is enough
  below the ceiling", and that is only true of a process with one viewer.** A pass runs inline on
  a tokio worker, there are as many workers as the affinity mask allows, and measurement
  (`BENCHMARKS.md`, *What a viewer costs while they are using it*) gives the rule:

  > a viewer who is doing nothing waits about **(busy viewers ÷ worker threads) passes**.

  Three apps spanning more than 300× in pass cost agree on it at every rung measured, and pinning
  the server to one core makes it between four and five times worse at the same viewer count. So
  cores are the denominator under everybody who is *not* currently being served: a single-core
  container with a 150 ms pass and a dozen busy viewers leaves everyone else waiting nearly two
  seconds. Size CPU against the viewers you expect to be active at once, not against the app's own
  pass alone.

  **A core is not free in the other direction**, and it is the one place the two costs on this
  page pull against each other: on one core the transient memory above is **zero** at every
  viewer count, because there is never a second concurrent pass wanting a second scratch buffer.
  More cores answer everyone sooner and hold more at the peak. The trade is real and it is small
  — tens of megabytes against seconds.
* **Replicas:** any number. Nothing is shared, nothing is sticky — provided every replica
  mounts the same snapshot.

**Many apps on one process** is sized differently, and `BENCHMARKS.md` has that measurement
too: **224 apps on one core**, at 448 concurrent sessions and 1 908 interactions per second,
against 200 predicted by a service-demand model from a second rig sharing no code with the
first. Read it as capacity for small apps rather than a promise for yours — the ceiling moved
with the app, and the figure is the bundled example's.

`--budget-mb` is the number to think about, and it is written in **source bytes**: the sum of
what every resident app's loaded sources hold, which excludes the graphs, the sessions and the
allocator's slack. It is a figure for comparing and summing apps, not an RSS prediction, so
set the container's memory limit well above it. An app whose own sources exceed the whole
budget is **refused** rather than admitted and then evicted — admitting it would evict
everything else first and still fail, turning one oversized deploy into an outage for every
app on the node.

---

## Configuration surface

There is deliberately almost none. No config file, no environment variables read by the
binary, no feature flags at run time.

```
dagpane check   <manifest>                  # exit 0 ok, 1 on any error
dagpane graph   <manifest> [--format text|mermaid|json]
dagpane explain <manifest> [--set NAME=VALUE ...] [--change-column CELL.COLUMN] [--json]
dagpane refresh <manifest> [--force] [--watch SECONDS] [--json]
dagpane run     <manifest> [--port 8787] [--host 127.0.0.1] [--auth-* ...]
dagpane serve   <manifest> [--port 8787] [--bind 0.0.0.0] [--auth-* ...]
                           [--admin-port 9787] [--admin-bind 0.0.0.0]
                           [--origin URL ...] [--drain-seconds 5]
                           [--heartbeat-seconds 25]
dagpane host    <dir>      [--port 8787] [--bind 127.0.0.1] [--auth-* ...]
                           [--budget-mb 512] [--idle-minutes 60]
                           [--admin-port 9787] [--admin-bind 0.0.0.0]
                           [--drain-seconds 5] [--heartbeat-seconds 25]
```

**`run` takes `--host` while `serve` and `host` take `--bind`, and all three mean the same
thing.** That is an inconsistency in the binary rather than a typo here; `dagpane <command>
--help` is authoritative, and this list was wrong about it on both sides of a merge before
anybody checked. `serve` follows `host` because the new command is the one that can still pick.

**`run` and `serve` are the same app deployed two ways, and every default they disagree about
is a default one of them would get wrong.** `run` binds loopback, derives its origin allowlist
from that, prints a URL, and stops when you stop it — right for a laptop, wrong for a
container that nothing can reach on loopback. `serve` binds every interface, takes the origins
you publish it at, answers probes on a port you do not publish, and drains before it stops —
right for one replica of several, and needlessly ceremonious for a laptop. Neither is a mode
flag on the other, because a mode flag would mean every one of those defaults has to be read
twice to know which way it went.

`--auth-*` is the flat group in the front door section, shared by `run`, `serve` and `host`
— a replica takes a front door exactly as the other two do, and a public one should have one.
There are
**no environment variables read by the binary** — every input is an argument or the manifest.

`explain --change-column CELL.COLUMN` is the measurement behind column- and
predicate-granular invalidation: it rewrites one column of one source frame and reports what
that cost, the way `--set` reports what turning a control costs. Given alongside `--set`, the
controls settle in their own pass first and the column change is the one measured — which is
how you ask what a data change costs at a threshold other than the manifest's default.

Everything else is the manifest, which is **trusted input from whoever deploys the app** — a
`csv =` path resolves against the manifest's own directory and is not otherwise constrained.
Treat a manifest like a program, because it is one: whoever can write it decides what the app
computes and which panes show it. Under `dagpane host` that goes further: a manifest's bytes
*are* the app's identity, so write access to the directory is deploy access.

**A `sql` source's DSN is in the manifest, and that is a stated limitation rather than a
recommendation.** A manifest is a file people commit. Until there is somewhere else to put a
credential, connect as a role that can do nothing but `select`.

`check` exiting non-zero is the gate to put in front of every deployment. It catches malformed
TOML, a CSV that is not there, a filter naming an input that does not exist, a pane naming a
cell that does not exist, duplicate names and cycles — before the process serves anything.

---

## Refreshing the data

**A running server does not re-read its sources.** They are loaded once, when the app is
compiled, and shared immutably across every session. There is no file watcher, no hot reload,
and nothing in the process on a timer. That is a design decision rather than a gap: it is what
makes a hundred viewers a hundred vectors of values over one allocation per source, and what
makes a pass a pure function of its inputs. **What is shared is the source.** The frames a
session computes are its own — see *Sizing* on what that costs, because it is the term
`--budget-mb` does not count.

Two things commonly mistaken for an exception, so both are stated plainly:

* **`refresh_secs = N` on a source is declarative, and nothing in the shipped binary acts on
  it.** It records how often the data is *expected* to move, for whatever owns a clock. You
  own the clock.
* **The client's refresh button re-renders; it does not reload.** It resends the panes the
  session already holds, which is why its stats line reports `visited: 0`.

So a data refresh is a **restart**, and the supported shape is:

```
sync  →  check  →  swap the snapshot  →  restart
```

`examples/bucket/serve.sh` implements exactly that loop against S3, GCS, Azure Blob, rclone or
a local directory, and `examples/bucket/README.md` documents it. The order is the point: a
snapshot that will not compile never reaches a viewer, an unchanged bucket costs no restart,
and rollback is a symlink.

Know the gap the gate leaves, because it is not obvious. `check` compiles the manifest and
loads the sources; it does not look inside a column. A CSV that still parses but has lost a
column the app filters on **compiles cleanly and fails at run time** — the failing cell holds
its error, cells below name it, and the rest of the page keeps working. That is errors-as-values
behaving correctly (ADR-0004), and it is a good failure, but it is not one the promotion gate
catches. Assert the schema in the job that writes the data, where the producer is.
`MIN_ROWS` in `examples/bucket/serve.sh` covers the other case `check` cannot see: a CSV that arrived as a
header with no rows under it.

**`dagpane refresh` is the gate for that loop**, and it runs in its own process against the
manifest rather than against a running server. It asks each source the cheap staleness
question — a file's mtime and length, an `ETag`, a row count — re-reads only what moved, and
reports what that cost. A source that could not be read is a **non-zero exit**, because the
thing that runs this is a cron job and a cron job that cannot tell a failed refresh from a
successful one is one nobody notices has stopped.

```sh
dagpane refresh app.toml --json      # what would a refresh actually cost right now?
dagpane refresh app.toml --force     # read regardless: the cases a staleness check cannot see
```

`--force` is for exactly those cases — a file rewritten within its filesystem's timestamp
granularity at the same length, a server that sends no validator, a table whose row count did
not move. For a `sql` source, naming `watch = "updated_at"` closes the common one, because a
row count is unchanged by every `UPDATE`.

**Under `dagpane host` a *manifest* change needs no restart, and a data change still does.**
The manifest's bytes are the app's key: the directory is read on every request, so changed
bytes mean a new key, a recompile — which re-reads that app's sources — and the old graph
evicted immediately, while viewers of the other apps notice nothing. Identical bytes mean the
compiled graph is served as it is, so rewriting the CSV underneath alone changes nothing. If
you want a data swap to take effect under `host` without restarting the process, make the
manifest name the snapshot, so promoting one edits a byte.

A restart drops every open page. Viewers reconnect automatically and get a fresh first render
— and, since the client replays the control values held in its own URL, back to the view they
were looking at rather than to the manifest defaults. Set the refresh interval against how
often the data genuinely moves.

---

## Observability

Be realistic about what you get. There is no metrics endpoint, no structured log, no request
log and no trace exporter — `dagpane serve`'s two probe endpoints are health and identity, not
telemetry. What the process emits is:

```
dagpane: Weekly business review — 14 cells, 9 panes
dagpane: http://127.0.0.1:8787
```

…on start-up, plus the no-auth warning on a non-loopback bind. `dagpane host` prints the app
count, the budget and the idle setting, then a line per app it found. With a front door
configured you also get one line naming the key count, the issuer, the audience and the access
rule — which is worth reading, because it is the only confirmation that the policy you meant
is the policy that loaded.

**Almost nothing is printed per connection, per interaction or per error.** A cell that fails
sends its error to the pane that shows it and writes nothing to stdout. The one exception is
the front door: a **refused** connection writes the reason to stderr, naming the app and the
detail that is deliberately kept out of the HTTP body.

```
dagpane: refused a connection to "sales": jane@example.com has no access to app "sales"
```

An **accepted** connection is not logged, which is the audit gap `SECURITY.md` names. If you
need to know who opened what, that has to come from the layer in front.

Two gaps that were here for three releases and are now closed, stated because the sizing advice
that went with them was wrong:

* **`dagpane host` now prints every eviction.** One line per app that leaves residency, naming
  the key, the reason and the bytes freed. The log was in memory and bounded and nothing in the
  shipped binary read it, so an app disappearing was invisible from outside except as a slower
  next request.

  ```
  dagpane: evicted sales@732b83079d170591 — Idle after 3600 s idle, 43140 byte(s) freed
  ```
* **`--idle-minutes` now configures a sweep that runs.** `Host::sweep_idle` existed from the
  day `crates/host` did and was called only by that crate's own tests, so in the shipped binary
  an app left residency on a redeploy or under budget pressure and **never for being idle** —
  whatever the flag said. The sweep runs at half the idle window, so an app leaves somewhere
  between `--idle-minutes` and one and a half times it. `--idle-minutes 0` turns it off, which
  is the behaviour every release before this one had.

  The old advice here was to *"size `--budget-mb` on the assumption that everything opened stays
  resident until the budget forces a choice."* That is no longer the assumption to size against,
  and it was the honest one only because the sweep was not running.

What you can actually monitor:

* **Liveness and readiness, under `dagpane serve`:** `GET /healthz` and `GET /readyz` on the
  admin port. These are the real ones — `/readyz` goes `503` the moment a drain begins, which
  is what takes a stopping replica out of the pool before its listener closes. `/healthz` also
  reports the digest of the manifest bytes this replica holds, which is how you check that a
  rollout finished rather than half-finished.
* **`dagpane host` answers the same two probes**, on its own `--admin-port`. `/readyz` is
  whether the process can compile and serve — not whether every app it holds is healthy, for
  the reason the endpoint section gives — and `/healthz` lists the resident apps with their
  digests and byte footprints, which is the fleet's version of the identity check above.
* **Under `run` there are no probes**, and the best available substitute is
  `GET /` returning `200`. It is served from a static constant, so it proves the process is up
  and not that the graph is healthy.
* **A real readiness check before the port is open** is `dagpane check <manifest>` at start-up
  — as a container init step or a systemd `ExecStartPre`. Non-zero means do not promote. It is
  still worth having alongside the probes: it fails the rollout before anything is listening,
  where `/readyz` can only report a process that already compiled.
* **Whether an interaction is still cheap** is `dagpane explain --json` in CI, asserting on
  counts. That is the signal that catches a change making the page expensive, and it catches it
  before deployment rather than after. `examples/tools/verify.py` is a worked version.
* **Process-level** RSS, CPU and restart count from whatever already watches your processes.
  Given the memory profile, RSS is the number most worth alerting on.
* **Not a connection count.** Nothing here counts open sockets or reports them anywhere, so
  "how many people are looking at this" is a question for the layer in front. The heartbeat
  keeps connections alive; it does not tally them.

If you need per-interaction visibility in production, the `PassStats` the server already sends
every client is the data you want — it is on the wire, not in a log.

---

## Deployment shapes

**One person, one laptop.** `dagpane run app.toml`. Loopback, no proxy, nothing to configure.

**Several replicas behind a load balancer.** `dagpane serve app.toml --origin https://<name>`.
Every interface, probes on `9787`, and a drain before it stops. Replicas share nothing and
stick to nothing, so the count is yours to pick — what they must share is the same manifest
bytes and the same snapshot of the data, and `/healthz` is where you check the first of those.

**A team, one host.** Run it as a systemd unit bound to loopback, and reach it over an SSH
tunnel or through an authenticating proxy that rewrites `Origin`. The bucket refresh loop can be
the unit's `ExecStart` directly. No `KillSignal=` is needed — `SIGTERM`, which is what systemd
sends by default, now stops it gracefully.

**Many apps, one process.** `dagpane host ./apps` routes on the first label of the `Host`
header, so `sales.toml` is served at anything starting `sales.`. One port, one compiled graph
per app however many viewers, and a byte budget deciding what stays resident. This is also the
shape where the `Origin` trap does not bite, because the check compares against `Host` — see
that section. Editing a manifest is the deploy: the next request for it compiles the new bytes
and evicts the graph it replaced, with the other apps untouched. Put `dagpane check` in front
of that edit, not after it.

**Containers.** The published image is `scratch` plus one static musl binary — no shell, no
package manager, no libc, and `linux/amd64` only. There is nothing in it to run a sync with, so
mount the app directory rather than baking it in:

```sh
docker run --rm -p 8787:8787 -v "$PWD/app:/app:ro" \
  mancube/dagpane:0.1.1 serve /app/app.toml --origin http://localhost:8787
```

`serve` binds every interface already, which is what a container needs. The `--origin` is not
optional decoration: **without it the Origin allowlist is `http(s)://0.0.0.0:8787`, which no
browser sends**, so the socket is refused even over plain `localhost` and the page loads and
then does nothing. The start-up warning says so, and `make origin-probe URL=...` exits non-zero
on `403` so it works as a deployment gate.

The no-auth warning still applies to any non-loopback bind. Put the authenticating layer in
front before the port is.

**Kubernetes.** An init container syncs the bucket into an `emptyDir` and runs `dagpane check`
as its last step — a non-zero exit stops the rollout and the previously running pods carry on
serving. The dagpane container mounts that volume read-only and runs `dagpane serve`. Because
sources load at start-up, a data refresh is a rollout: a `CronJob` that syncs and then restarts
the Deployment. Set `runAsNonRoot` and any UID; the binary is static and needs nothing from the
filesystem beyond the mounted app directory.

The image has no shell, so `exec` probes are not available — use HTTP probes against the admin
port, and **wire the two probes to the two different endpoints**:

```yaml
args: ["serve", "/app/app.toml", "--origin", "https://panels.example.com",
       "--drain-seconds", "10"]
ports:
  - { name: http,  containerPort: 8787 }
  - { name: admin, containerPort: 9787 }   # NOT in the Service
livenessProbe:
  httpGet: { path: /healthz, port: admin }
readinessProbe:
  httpGet: { path: /readyz,  port: admin }
  periodSeconds: 2
terminationGracePeriodSeconds: 30          # > drain-seconds, with room for in-flight work
```

`--drain-seconds` should exceed `periodSeconds × failureThreshold` — that product is how long
the kubelet may take to see the `503`. Pointing **both** probes at `/readyz` is the mistake
worth naming: the drain would then fail the liveness probe too, the kubelet would conclude the
process had hung, and it would `SIGKILL` the pod in the middle of the drain it asked for.

Leave the admin port out of the `Service` and out of any `Ingress`. The kubelet reaches a
container port directly and does not need one.

---

## Upgrading and rolling back

The binary and the app are independent artefacts, and the app is the one that changes daily.

* **Rolling back the data** is `ln -sfn` at a previous snapshot plus a restart —
  `examples/bucket/serve.sh` keeps the last `KEEP` snapshots by content digest for this.
* **Rolling back the binary** is a tag change. There is no on-disk state and no schema, so
  moving between patch versions in either direction is just a restart.
* **Before upgrading the binary**, run `dagpane check` and `dagpane explain --json` on your real
  manifests with the new binary and compare the counts. A change that alters what an
  interaction costs is exactly what those counts exist to catch — the project gates its own
  published counts the same way, in `.github/workflows/ci.yml`.

## See also

* `docs/RUNBOOK.md` — symptom first, when something is already wrong.
* `SECURITY.md` — the threat model and the full list of known weaknesses.
* `BENCHMARKS.md` — where the sizing numbers came from, and how to reproduce them.
* `ARCHITECTURE.md` — why sources load once, §2 on what CI greps for, and §6 on the frame
  seam and its two backends.
* `docs/CUSTOMISING.md` — every manifest option, including which source forms need which
  build features.
* `examples/bucket/README.md` — the refresh loop, working.
