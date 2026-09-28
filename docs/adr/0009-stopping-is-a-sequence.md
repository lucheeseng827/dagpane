# ADR-0009: stopping is a sequence, so `serve` is not `run` with a flag

**Status:** accepted.

## Context

`OPERATIONS.md` has said this for three releases:

> The process is therefore horizontally scalable by accident — any number of replicas behind a
> load balancer are correct, because there is nothing to share and nothing to stick.

Every word of that is true and none of it was enough. A session is a connection and the viewer
carries their own state, so replicas genuinely share nothing; what was missing is everything
that happens *around* a replica rather than inside it. The same document listed the gaps,
measured, in three different sections:

* **`SIGTERM` was not handled.** The published image is `scratch` plus a static binary, so the
  binary is PID 1 — and PID 1 has no default disposition, so an unhandled `SIGTERM` is not a
  termination, it is *ignored*. A `podman stop` was measured at 11 s, waiting out the grace
  period for a `SIGKILL`. Both systemd and Kubernetes send `SIGTERM` by default, so that was
  the ordinary path.
* **Nothing could be asked whether it was ready**, so `GET /` returning `200` was the
  recommended probe. It is a static constant: it answers `200` right up to the `SIGKILL`.
* **A container's `Origin` allowlist was one no browser sends.** `--host 0.0.0.0` is required
  for a container to be reachable, and the allowlist is derived from the address bound, so
  the page loaded with `200` and the socket was refused with `403`. The document's own words:
  *"until an origin flag exists"*.

Each was filed as a deployment note. Together they are one thing: **`dagpane run` was written
for a person at a terminal, and a replica is not that.** A person decides when to stop, sees
the URL in the output, and knows what hostname they typed. A replica is stopped by something
that will not read output, is sent traffic by something that will only ask yes-or-no
questions, and is reached at a name the process was never told.

## Decision

### 1. A stop is three steps, and the order is the whole thing

```
1. go unready      /readyz answers 503; the listener stays open, in-flight work untouched
2. wait            --drain-seconds, the window the balancer has to notice
3. stop accepting  and let what is in flight finish
```

A replica that closes its listener when it is signalled is **still in the balancer's pool**.
The balancer finds out at its own pace, and every connection it routes in the meantime is a
reset. That is the bug, it is entirely a bug of ordering, and it is invisible in any test that
stops one process and checks the exit code.

Step 2 is the part with nothing to derive it from: how long a balancer takes to notice is a
property of the balancer. So it is a number the operator sets, and the guidance is a formula
rather than a default to trust — longer than the health interval times the unhealthy
threshold.

### 2. Liveness and readiness are different questions with different answers

`/readyz` fails during a drain. `/healthz` **passes**. Wiring both to the same answer is the
mistake that undoes step 1 entirely: a liveness probe that went red during a drain would have
the supervisor conclude the process had hung and `SIGKILL` it, in the middle of the graceful
stop it had just asked for. Liveness asks *is this alive*; readiness asks *should it be sent
work*. A draining replica is alive and should not be sent work.

### 3. The probes are on a listener of their own

Not a fifth route on the app's port, for three reasons that agree:

* **The app's port has four endpoints and no others**, which is a claim `OPERATIONS.md` makes
  and a reader can check. A health surface there makes it five.
* **`GET /{*path}` serves an app's declared renderer scripts**, so `/healthz` on that port is
  a name collision with a namespace the manifest controls.
* **The app's port is what an ingress publishes.** The supervisor is not the public.

The cost is one more flag and one more container port. What it buys is that "do not publish
this" is enforceable by leaving a port out of a `Service`, rather than by a routing rule
somebody has to write correctly.

### 4. Declaring origins replaces the derived list, and is checked at start-up

`--origin https://panels.example.com`, repeatable. When any is given, the allowlist is
**exactly** those — the bound address is not silently kept. An operator who has said where the
app is published has said it; extending the list with an address no browser sends would only
preserve the case where somebody rewrote `Origin` at a proxy to match a wildcard bind, which is
a thing to stop doing rather than a thing to protect.

The value is compared as a string, so *nearly right* and *nothing like it* fail identically —
and the symptom is the same inert page either way. The two nearly-right spellings are a
trailing slash and a pasted path, both of which look correct in an address bar. They are
refused at start-up, naming the string and the fix, because a refusal in a terminal an operator
is looking at is cheap and a `403` found by whoever was sent the link is not.

### 5. A replica says which bytes it holds

`/healthz` reports `dagpane_host::manifest_digest` of the manifest it compiled — the same
identity `dagpane host` already routes and evicts by, so the two modes agree about what an app
*is*. `OPERATIONS.md` named this failure and then called it somebody else's:

> a manifest's bytes are its identity, so two replicas holding different bytes are serving two
> different apps under one name. That is a deployment problem, not a runtime one.

It is still a deployment problem. It was also, until now, an **invisible** one: a half-finished
rollout had no symptom except viewers disagreeing with each other about what the data said.
Collecting one field from every replica and counting the distinct values is the check, and the
runtime's only job was to have a field to collect.

### 6. `run` and `serve` are separate commands

Every default they need disagrees:

| | `run` | `serve` |
|---|---|---|
| bind | loopback | every interface |
| origins | derived from the bind | the names you publish at |
| stop | close now | drain, then close |
| probes | none | two, on their own port |
| output | a URL for a person | an identity for a rollout |

A single command with a `--cluster` flag would mean every one of those has to be read twice to
know which way it went, and the laptop default is the dangerous one to inherit by accident: a
container that binds loopback is a container nothing can reach, and it fails at the worst
moment with the least information. Two commands make the choice once, at the point where it is
already being made.

`SIGTERM` is the exception that is not one: **every** mode handles it now, `run` and `host`
included. That was never a deployment preference, it was a signal being dropped.

## How it is checked

`crates/serve/tests/replica.rs` binds two real listeners, runs the real `serve_replica`, and
asks it what a kubelet and a load balancer ask. The one substitution is the trigger —
`Lifecycle::request_stop` stands in for `SIGTERM`, and it is the same code path from there,
because `kill(2)` is not the property worth asserting and what `/readyz` says *while the
listener is still open* is.

**Eight mutations, eight caught.** Sleeping before going unready, not sleeping at all, making
`/healthz` mirror `/readyz`, shutting the probes down when the drain starts, letting declared
origins extend rather than replace, letting a stopped replica report itself draining again,
never telling the probe listener the app stopped, and making the fingerprint a constant each
fail a different named test. The fourth and seventh are the two that would leave a supervisor
unable to see the drain it asked for.

The signal handling itself is checked the only way it can be — against a real process and a
real `kill -TERM`. The transcript is in `OPERATIONS.md` under *Lifecycle and signals*, and the
two lines worth reading twice are `readyz=503` and `app port still serving 200` on the same
line of it.

## Consequences

**A rolling update stops being visible to viewers**, which is the point. A viewer on a replica
that is going away finishes their interaction, and their next connection lands on a replica
that is not going away — and since a session *is* its input values and the viewer carries
those in their URL fragment, landing somewhere else costs one pass and shows the same view.
That property was already true and had nothing to make use of it.

> **Correction, and it is this ADR's own claim being narrowed.** As written above that
> sentence was half true, and the missing half was on the other side of the wire. The drain
> closes sockets *cleanly*, and what `client.html` did with a clean close was show a banner
> reading "Disconnected" and stop. "Their next connection" was therefore a **reload the viewer
> had to think of**, which is not a rolling update nobody notices.
>
> The page now reconnects on its own, with a capped and jittered backoff, and the claim holds
> as written. See *Coming back* below. The general shape of the mistake is worth keeping: a
> property that needs both ends was checked at one end, and the test that would have caught it
> could not exist because nothing in the repository ran the client.

**`terminationGracePeriodSeconds` now has to exceed `--drain-seconds`**, or the supervisor
kills the process in the window it was told to wait. That is a new way to misconfigure a
deployment, and it is the same shape as every other timeout pair: the outer one is longer.

**A rollout can assert on identity**, which is a check that did not exist rather than one that
got easier.

**Replicas of one app still each hold that app's data.** Nothing here amortises anything across
replicas — N replicas is N × the sources, and `dagpane host` remains the only thing that shares
across *apps*. ROADMAP §7 is where that number would go, and it is still not measured.

### Coming back

A closed socket is the ordinary case rather than an error: a rolling update closes every one of
them on purpose, and so does a scale-in, a spot reclamation, a laptop lid, and a proxy that
reclaimed a connection somebody was only reading. So the page retries — exponential, capped at
30 s, and **jittered**, which is the part that is about a fleet rather than about one page.
Every viewer of a replica that just drained is reconnecting at the same instant; without jitter
they arrive together, retry together, and keep arriving together for as long as the outage
lasts.

Reconnecting is cheap here for the same structural reason the drain is: **a session is a
connection, and the viewer holds what makes it theirs.** A reconnect is the same `connect()`
as the first one, it carries the control values off the page's URL fragment, and the server
stages them before the first render. One pass, same view, possibly a different replica.

Two closes are not the same, and the difference is the only branch in it. A socket that **never
spoke**, on a page holding a token, is a refusal: retrying that on a timer hammers a door that
is not going to open and buries the one message that tells the viewer what to do. Everything
else is a transport event that will clear on its own.

The quieter half is a **WebSocket Ping every 25 s**, and it needs no client code at all: a
browser cannot send a Ping from script, and answers one in the transport with no JavaScript
involved. It exists because *a dashboard nobody is clicking is the ordinary case, not an idle
one*, and a load balancer counting sixty seconds of silence does not know the difference.

`crates/serve/tests/run.sh` is what makes any of the above checkable. It drives the real client
in real Chromium over the DevTools Protocol — from bare Node, with no npm dependency — kills
the server, and requires the page to come back with its controls still working. **Three
mutations, three caught**: never retrying, never clearing a pass that was in flight when the
socket went, and retrying a refusal like any other close. The second is the one worth having:
`inFlight` is cleared only by a reply, so a socket that dies mid-pass leaves it set and `flush`
refuses to send forever — a page that reconnects, repaints, and then silently ignores every
control. That is worse than the banner it replaced, because it looks like success.

### What is not built

**`dagpane host` has probes and a drain too, and the readiness question it raised has an
answer.** This section used to say the question had more than one and that inventing an answer
before anything asked would be inventing requirements. The parenthesis it dismissed the question
with — *a fleet where one app fails to compile is not unready* — turned out to be the whole
answer, and worth stating as a rule rather than an aside:

> **A hosting process is ready when it can compile and serve, not when every app it holds is
> healthy.**

The argument for it is what makes it not a matter of taste. One broken manifest is a fault
*every* replica has, for the same reason, at the same moment — the manifests are the shared
thing. So a readiness probe that walked the fleet would take every replica out of the pool
together and turn one bad file into a total outage, while reporting a fault that no amount of
rescheduling can fix. The gate for a broken manifest is `dagpane check` in front of the deploy,
which fails before anything is listening.

What differs from one app is the *description*, not the readiness. A single app's identity is
fixed when it compiles; a fleet's changes while it runs, so `Health` is asked per request and
`/healthz` lists what is resident now — each app with the digest of its manifest bytes, against
the byte budget. The fleet question one app answers ("do the replicas agree?") is then askable
of every app at once.

The drain is identical, because what is drained is identical: connections, which belong to no
app in particular.

**The idle sweep now runs.** Not strictly a lifecycle matter, but it arrived here because it is
the same kind of gap: `Host::sweep_idle` existed from the day `crates/host` did, was called only
by that crate's own tests, and so `--idle-minutes` configured a sweep that never ran. An app left
residency on a redeploy or under budget pressure and never for being idle, whatever the flag
said. `dagpane_serve::sweep_idle` is the caller — it needs a clock, which is why it lives in the
crate that owns one — and it prints every eviction, which closes the other half: the log was in
memory, bounded, and nothing read it.

**Nothing serves a split.** `serve` runs the same session code `run` does, so a placed app
evaluates every cell on the server here too, and `dagpane check` says so. ADR-0008 and
ROADMAP §4 are unchanged by this: a deployment shape is not the page that holds the other half.

**There are no metrics.** Two endpoints that answer health and identity are not telemetry, and
`OPERATIONS.md` still says there is no metrics endpoint, because there is not one.
