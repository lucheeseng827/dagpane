# Runbook

Symptom first. `OPERATIONS.md` is the companion for deciding how to run this; reach for this
file when it is already running and something is wrong.

Every command here is one you can paste, and every quoted output was produced by `dagpane`
built from this tree rather than written from memory.

---

## First five minutes

```console
$ dagpane check /path/to/app.toml            # does the app still compile?     exit 0 = yes
$ curl -s -o /dev/null -w '%{http_code}\n' http://HOST:PORT/          # is the page served?
$ curl -s -o /dev/null -w '%{http_code}\n' \
    -H 'Connection: Upgrade' -H 'Upgrade: websocket' \
    -H 'Sec-WebSocket-Version: 13' -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' \
    -H "Origin: https://THE-HOSTNAME-USERS-TYPE" \
    https://THE-HOSTNAME-USERS-TYPE/ws       # 101 = healthy.  403 = §1 (§15 under dagpane host).  404 under dagpane host = §15.  Anything else = §2
$ dagpane explain /path/to/app.toml --set SOME_INPUT=VALUE    # does an interaction still work?
```

Those four answers separate almost everything below. The third is the one people skip, and it
is the one that catches the most common production failure in this version.

| what you see | go to |
|---|---|
| page loads, controls do nothing | [§1](#1-the-page-loads-and-the-controls-do-nothing) |
| nothing is listening, whether or not a URL was printed | [§2](#2-it-stopped-at-start-up-before-printing-a-url) |
| process will not start | [§3](#3-the-process-refuses-to-start) |
| one pane shows an error, the rest are fine | [§4](#4-one-pane-shows-an-error-and-the-rest-of-the-page-is-fine) |
| every pane is empty or wrong | [§5](#5-every-pane-is-empty-or-shows-zero) |
| interactions are slow | [§6](#6-interactions-are-slow) |
| killed during start-up, restart loop | [§7](#7-killed-during-start-up-or-looping-on-restart) |
| the numbers are stale | [§8](#8-the-numbers-are-stale) |
| the refresh loop keeps refusing | [§9](#9-the-refresh-loop-keeps-refusing-snapshots) |
| the process vanished during a deploy | [§10](#10-the-process-vanished-during-a-deploy) |
| the wrong people can read it | [§11](#11-the-wrong-people-can-read-it) |
| a number looks suspiciously unchanged | [§12](#12-a-number-did-not-change-and-i-do-not-know-if-that-is-a-bug) |
| the log says a connection was dropped | [§13](#13-the-log-says-a-connection-was-dropped) |
| the log says a ping went unanswered | [§14](#14-the-log-says-a-ping-went-unanswered) |
| dagpane host answers 404 for every app | [§15](#15-dagpane-host-answers-404-for-every-app) |

---

## 1. The page loads and the controls do nothing

By far the most likely failure the first time this goes behind a proxy.

**Confirm.** `GET /` returns `200` and the page paints, but moving any control changes nothing
and the counts above the panes never move. Run the upgrade probe from **First five minutes**
with the hostname users actually type. A `403` confirms it:

```
this app does not accept WebSocket connections from https://panels.example.com;
open it at its own address
```

**Cause.** The `/ws` upgrade checks the browser's `Origin` against an allowlist. Under
`dagpane run` that allowlist is derived from **the address the process bound**, and nothing
else; under `dagpane serve` it is too, unless `--origin` replaces it. A proxy, ingress or load
balancer on a real hostname means the browser sends that hostname, which never matches the
bound address. It is an anti-CSRF measure and it is working as designed. `run` has no flag to
change it; `serve` has `--origin`.

Under `dagpane host` the check is different — the `Origin` is compared with the request's own
`Host`, not with the bound address — so the fixes below do not apply and rewriting `Origin` does
not help: a `403` there means the proxy is not forwarding the hostname users type as `Host`,
and [§15](#15-dagpane-host-answers-404-for-every-app) has the fix.

**If the process bound a wildcard with no `--origin` — `run --host 0.0.0.0`, or `serve` at
its default bind — this fires unconditionally.** The derived allowlist is the bound address
literally, so `0.0.0.0:8787` allows exactly `http://0.0.0.0:8787` — which no browser ever
sends. Every containerised deployment without `--origin` hits this, proxy or not, because a
wildcard bind is what makes a container reachable at all, and it is where `serve` binds by
default. `serve` warns about it at start-up; `run` does not. Verified:

```console
$ # process bound 0.0.0.0:8813, reached over plain localhost, no proxy anywhere
$ curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8813/         # the page
200
$ curl ... -H 'Origin: http://127.0.0.1:8813' http://127.0.0.1:8813/ws     # the socket
403
```

There will be **nothing in the server log** about the refused upgrades, so under `run`, and
under `serve` once its start-up warning has scrolled away, nothing says why.

**Fix — pick one.**

* **Under `serve`, pass `--origin` with the URL users type** — scheme, host and port, no
  trailing slash, once per name the app is published at:

  ```sh
  dagpane serve app.toml --origin https://panels.example.com
  ```

  The declared origins **replace** the derived allowlist rather than extend it, so the address
  the process bound stops being allowed, and nothing in front has to rewrite `Origin`. A value
  no browser would send — no scheme, a trailing slash, a path — is refused at start-up with the
  fix named.

Under `run`, which has no `--origin`:

* **Reach it at the address it bound — which requires that address to be loopback.**
  `ssh -L 8787:127.0.0.1:8787 host`, then open `http://127.0.0.1:8787`. This works only if the
  remote process bound `127.0.0.1`. Tunnelling to a process bound `0.0.0.0` still sends
  `Origin: http://127.0.0.1:8787` against an allowlist holding only `http://0.0.0.0:8787`, so
  it is still refused — a wildcard bind needs the proxy below, or `serve --origin` above.
* **Make the proxy allowlist the incoming `Origin`, then rewrite it** to the bound address.
  Rewriting unconditionally forwards every origin as the allowed one and hands back exactly
  the CSRF the check exists to stop, so both halves are needed:

  ```nginx
  map $http_origin $dagpane_origin_ok {
      default                       0;
      "https://panels.example.com"  1;
  }

  location /ws {
      if ($http_origin != "")   { set $dagpane_check "$dagpane_origin_ok"; }
      if ($dagpane_check = "0") { return 403; }

      proxy_pass http://127.0.0.1:8787;
      proxy_http_version 1.1;
      proxy_set_header Upgrade    $http_upgrade;
      proxy_set_header Connection "upgrade";
      proxy_set_header Origin     "http://127.0.0.1:8787";   # without this: 403
  }
  ```

  Authentication at the proxy is not a substitute for that allowlist — a CSRF request carries
  the viewer's own cookies, so an authenticating proxy lets it straight through. If nothing is
  authenticating either, fix that first: see §11.

**Prevent.** Make the upgrade probe, not `GET /`, the readiness check in whatever promotes a
deployment. A `200` on the page proves nothing about whether the app works.

---

## 2. It stopped at start-up, before printing a URL

**Confirm.** The process printed its banner and its settings, then an error naming an address,
and exited without printing a URL:

```
dagpane: Funnel explorer — 11 cells, 7 panes
dagpane: heartbeat 25s — keep the idle timeout in front of this above that
dagpane: writes    30s per frame beyond what a 128 kbit/s link needs — a client that stops reading, or falls that far behind, is dropped
dagpane: answers   30s per ping beyond what a 128 kbit/s link needs — a client that stops reading, or goes, is dropped
dagpane: 127.0.0.1:8805: Address already in use (os error 98)
```

**Cause.** The URL is printed only once its port is bound — `run`'s URL, `serve`'s app and
probe addresses, `host`'s list of apps — so a process that printed one is listening on that
port, and one that stopped before it could not bind. The last line names the address and why.
Common ones:

| message | meaning |
|---|---|
| `Address already in use (os error 98)` | something already holds that port |
| `Cannot assign requested address (os error 99)` | `--host` or `--bind` names an address this machine does not have |
| `Permission denied (os error 13)` | a port below 1024 without the capability to bind it |

`dagpane` 0.1.1 and earlier print the URL before the bind is attempted, so their logs can show a
URL and then this error, with nothing listening.

**Fix.** `ss -ltnp \| grep :PORT` to find the holder; change `--port`, or stop the other
process. `--port 0` takes any free port, and the URL says which. For a wrong address, use
`0.0.0.0` to bind everything or the address the interface actually has — and read the no-auth
warning that a non-loopback bind prints.

**Prevent.** The process exits non-zero on a bind failure (`1` on `Address already in use`), so
ordinary restart-on-failure handling is doing its job. A port held by *another* process still
leaves something answering there that is not this app, so make the readiness check ask the app
rather than the port: an HTTP probe against `/` in Kubernetes — `/readyz` on the probe port for
`serve` and `host` — and a `curl` gate elsewhere.

---

## 3. The process refuses to start

**Confirm.** `dagpane check <manifest>` — it is the same code path the server runs at start-up,
it exits non-zero on any problem, and it names the cause.

**Causes and what they look like.**

```
dagpane: app.toml: source `c`: gone.csv: gone.csv: No such file or directory (os error 2)
dagpane: app.toml: cell `s` reads `typo_here`, which is not an input or a cell
dagpane: app.toml: pane `nope` shows `nope`, which is not a cell
dagpane: app.toml: TOML parse error at line 3, column 9
```

| message | what happened | fix |
|---|---|---|
| `source ...: No such file or directory` | a CSV the manifest names was not delivered | fix the sync; the manifest's paths resolve against **its own directory**, not the working directory |
| `reads X, which is not an input or a cell` | a filter's `param` is misspelt, or names something declared *below* it | check spelling and ordering — an edge must already be declared at that point in the file |
| `pane X shows Y, which is not a cell` | a pane names a cell that was renamed or removed | rename the pane's `cell` |
| `TOML parse error at line N` | malformed manifest | the message points at the line |
| `two panes are named X` | two panes resolved to the same id | a pane's id defaults to its cell name — give the second an explicit `id` |
| a cycle, naming the loop | the graph is not a DAG | break the loop the message names |

**Prevent.** `dagpane check` as a systemd `ExecStartPre`, an init-container step, or a CI gate.
Non-zero means do not promote. This is the cheapest gate in the whole system.

---

## 4. One pane shows an error and the rest of the page is fine

**This is working as designed.** Errors are values (ADR-0004): a failing cell holds its error,
cells below it hold one naming the cell that actually failed, and the pass finishes. One broken
column takes out the panes that depend on it — not the page, and not the control that will let
you look at something else.

**Confirm and find the cause** with `explain`, which names the column and lists the ones that
are actually there:

```console
$ dagpane explain app.toml --set ch=search
    failed   scoped       no column `spend`; the table has `day`, `channel`, `campaign`,
                          `impressions`, `clicks`, `signups`
    failed   by_channel   upstream cell `scoped` failed: no column `spend`; the table has ...
```

Read the **first** `failed` line. Everything below it is collateral, and says so.

**Cause.** The data changed shape without the manifest changing: an upstream job dropped or
renamed a column, or changed a type. Note that `dagpane check` **passes** in this state — it
compiles the manifest and loads the sources, but it does not look inside a column. This is the
gap in the promotion gate, and it is deliberate rather than an oversight.

**Fix.** Restore the column upstream, or update the manifest to the new name and restart.

**Prevent.** Assert the schema in the job that *writes* the data, where the producer is. A
column contract is not something the consumer can check before the fact.

---

## 5. Every pane is empty or shows zero

**Confirm.** Check the row count actually loaded:

```console
$ wc -l /path/to/source.csv          # 1 means a header and no data
$ dagpane explain app.toml --set SOME_INPUT=VALUE
```

**Causes, most likely first.**

* **A CSV that arrived as a header with no rows.** A manifest over an empty table compiles,
  serves, and shows a page full of confident zeroes. Nothing in the app can distinguish this
  from "the filter matched nothing".
* **A filter whose default excludes everything** — a slider whose `default` is above the data's
  range, or a `select` default that is not one of the values present.
* **The wrong snapshot was promoted.** Check which one is live: `readlink .dagpane-bucket/current`.

**Fix.** For the empty CSV, fix the producing job and re-promote. For a filter default, correct
the `default` in the manifest — `dagpane explain` with no `--set` shows the first render, which
is what a viewer sees before touching anything.

**Prevent.** Set `MIN_ROWS` in `examples/bucket/serve.sh`. It refuses a snapshot whose smallest
CSV has fewer data rows than the floor, which is precisely the failure `check` cannot see:

```
REFUSING a518048d960f7604: under MIN_ROWS=100 — campaigns.csv:0; keeping 7db0d4483c87416b
```

---

## 6. Interactions are slow

**Confirm.** `dagpane explain` prints what an interaction costs in cells and panes; time it to
get the wall clock:

```console
$ time dagpane explain app.toml --set SOME_INPUT=VALUE
```

**Cause, in order of likelihood.**

1. **Too many rows.** The interactive ceiling is around 100 000. From `BENCHMARKS.md`: 10 000
   rows is ~67 ms per interaction, 100 000 is ~768 ms, 1 000 000 is ~10 s. A filter over the
   source cell costs the whole scan however few other cells run — the graph saves you the cells
   that did not change, not the scan itself.
2. **The graph is not saving anything.** If `explain` shows every cell evaluated and every pane
   sent, the page is structurally a full rerun and the manifest is buying nothing. Look for a
   control that everything hangs off, and fence off what does not need to move — a cell reading
   the *source* rather than the filtered cell is never recomputed by that control.
3. **An over-declared edge.** A cell that declares an input it only reads on a branch it did not
   take still re-runs. `explain` prints it as `same value`, and the cost is one recomputation
   rather than a cascade, but it is still a cost.

**Fix.** Aggregate upstream so the app reads the smaller table. That is the intended division of
labour: do the expensive, one-off work in the notebook, the warehouse, or the job that writes
the bucket, and let the manifest do the last mile.

**Prevent.** Assert on the counts in CI. `dagpane explain --json` gives a structure you can
assert against; `examples/tools/verify.py` is a worked version that fails when an app stops
skipping work.

---

## 7. Killed during start-up, or looping on restart

**Confirm.** Exit code 137, `OOMKilled` in a container status, or the kernel OOM killer in
`dmesg`. Compare the container's memory limit against the total size of the CSVs.

**Cause.** **Memory amplifies about seventeen times** over the CSV on disk — a 37.8 MB file
becomes roughly 637 MB resident. Sources load whole, at start-up, so the peak is during
start-up, which is why this reads as a crash loop rather than a slow leak.

**Fix.** Raise the limit to at least 20× the total CSV size, plus headroom. If that is not
available, reduce the data — aggregate upstream, or split one app into several over smaller
sources.

**Prevent.** Size against RSS rather than file size, and alert on RSS. The `frame-arrow`
backend cuts this by roughly 40%; `BENCHMARKS.md` has the per-representation figures and
`ARCHITECTURE.md` §6 names the seam.

---

## 8. The numbers are stale

**Confirm.** Compare the source file's modification time against the process start time:

```console
$ ps -o lstart= -p $(pgrep -f 'dagpane run')
$ stat -c '%y %n' /path/to/*.csv
```

If the file is newer than the process, the process is serving the older data — unless the
source is scheduled, which `dagpane check app.toml` says in one line per source
(`sales: re-read every 300 s …`).

**Cause.** One of two, and the console says which.

* **The source names no `refresh_secs`.** Not a bug: it was read when the app compiled and
  nothing re-reads it. Replacing the CSV under a running process changes nothing anybody sees.
* **It does, and the schedule is not applying what it reads.** Every refusal and failure is a
  line on stderr, repeated each interval: `refresh of … — …; keeping the rows it had`. The usual
  one is a change of columns — a refresh changes rows, never their shape — and the line names
  both shapes. A schedule also only runs while somebody is viewing the app; the first viewer
  after a quiet spell checks at once.

**Fix.** For new rows in place, name `refresh_secs` on the source. For a refused change of
columns, redeploy — the app was checked against the old shape. Otherwise restart the process;
on 0.1.1, with `SIGINT` — see §10.

**Prevent.** Run the refresh loop rather than a bare `aws s3 sync` at the live directory:
`examples/bucket/serve.sh` does `sync → check → swap → restart`, only restarts when the content
digest actually moved, and keeps the previous snapshots for rollback.

---

## 9. The refresh loop keeps refusing snapshots

**Confirm.** The loop is logging, and the previous snapshot is still serving — which is the
loop doing its job, not failing:

```
19:10:58Z  REFUSING 0b1ec55cf519a1ae: it does not compile (see above); keeping 7db0d4483c87416b
19:11:01Z  REFUSING a518048d960f7604: under MIN_ROWS=100 — campaigns.csv:0; keeping 7db0d4483c87416b
```

**Cause and fix.**

| log line | cause | fix |
|---|---|---|
| `it does not compile` | the staged manifest fails `check` — the reason is printed immediately above | fix it in the bucket; §3 decodes the message |
| `under MIN_ROWS=N` | a CSV arrived with too few rows | fix the producing job, or lower `MIN_ROWS` if the floor is wrong |
| `no <app>.toml in the synced prefix` | the prefix is wrong, or the manifest was not uploaded | the whole directory travels, not just the file |
| `sync failed` | credentials or connectivity to the bucket | the underlying CLI's own error is above |

**Do not "fix" this by removing the gate.** A refusal means the previous snapshot is still
serving and still correct. Promoting past it puts a broken page in front of people.

**Prevent.** Run `dagpane check` in the job that *publishes* to the bucket, so a bad manifest
never lands there in the first place.

---

## 10. The process vanished during a deploy

**Confirm.** The process exited on a stop or a rolling update, viewers saw the page drop rather
than settle, and the exit was "killed by signal 15" — or, in a container, the stop took the
whole grace period and the exit code was `137`.

**Cause.** From 0.2.0 `dagpane` handles both signals, in every mode. Verified by signalling a
running 0.2.0 under `run`, `serve` and `host`:

| signal | behaviour | exit |
|---|---|---|
| `SIGINT` | graceful — stops accepting, in-flight requests finish | `0` |
| `SIGTERM` | graceful, identically | `0` |

Under `serve` and `host` either signal runs the drain first. Every mode then closes each open
socket with `1001`, *going away*, and the page's banner reads "Reconnecting — the server this
page was on is stopping" while it goes back. If nothing answers — a single instance restarting
— it then falls to "Disconnected" and backs off until something does, which is expected. A
page that goes **straight** to "Disconnected" was cut off instead: by a `SIGKILL`, a binary
that predates the close, or a socket still open when the 2 s after the listener closed ran
out (the log says how many).

So the symptom means a binary that predates the fix: **0.1.1 handles `SIGINT` only**, and that
includes the `mancube/dagpane:0.1.1` image. **systemd and Kubernetes both send `SIGTERM` by
default**, so on 0.1.1 the default path is the abrupt one — and in that image the binary is
PID 1, where an unhandled `SIGTERM` is not a termination but *ignored*: the stop waits out its
grace period and ends in `SIGKILL`.

**Fix.** Nothing is lost — a session holds no work, only control positions, and the page
carries those to its next connection. Viewers reconnect to the view they left.

**Prevent.** Run 0.2.0 or later. CI requires `docker stop` to exit `0` on every image it
builds, and on the image it publishes, before it is pushed. On 0.1.1, ask for the graceful signal explicitly:

```ini
[Service]
KillSignal=SIGINT
TimeoutStopSec=15
```

```dockerfile
STOPSIGNAL SIGINT
```

For the `mancube/dagpane:0.1.1` image itself, `docker run --stop-signal SIGINT` does the same.
A `preStop` hook cannot change which signal is sent, so in Kubernetes on 0.1.1 this has to be
baked into an image of your own. Both are harmless, and unnecessary, on 0.2.0.

---

## 11. The wrong people can read it

Treat as an incident, not a bug report.

**Confirm.** From a machine that should *not* have access. `GET /` tells you the **port** is
reachable and nothing more: the page is served without a token by design — it is where signing
in starts — and it carries no values. The socket is what checks, so that is what to probe:

```console
$ curl -s -o /dev/null -w '%{http_code}\n' http://HOST:PORT/
200          # reachable. says nothing yet about the data.

$ curl -s -o /dev/null -w '%{http_code}\n' \
    -H 'Connection: Upgrade' -H 'Upgrade: websocket' \
    -H 'Sec-WebSocket-Version: 13' -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' \
    http://HOST:PORT/ws
101          # THIS is the incident: no token was asked for, so every pane is readable here.
401          # a door is on and refused an anonymous caller. Not this incident.
```

Send **no** `Origin` header, exactly as above. The upgrade's origin check deliberately waves
through non-browser clients, so with it absent what you are testing is the token check and
nothing else.

**Cause.** **Authentication is off unless it was turned on**, and it was not. With no
`--auth-jwks` there is no login and no authorisation: if the port is reachable, the data is
readable. The process warns about exactly this at start-up on any non-loopback bind, so the
warning is in your logs from the moment it went up:

```
dagpane: warning — binding 0.0.0.0:8787, which is reachable from outside this machine,
         with no authentication. Anyone who can reach that address can read every
         pane of this app. Pass --auth-jwks to put a front door on it, or put one in
         front. See SECURITY.md.
```

A process that *was* started with a door prints a line naming the key count, issuer, audience
and access rule instead. If you see neither, you are reading the logs of a different process.

**Immediate action.** Take the port off the network — stop the process, or restrict it at the
security group, firewall or NetworkPolicy. Do not rely on the Origin check for this: it stops a
*browser page* from opening the socket, not a script, and `GET /` is not origin-checked at all.

**Then.** Put a door on it before it goes back up — either `--auth-jwks` with a JWKS file
from your provider, or something in front that authenticates. And note the second half of the
problem, which **neither** answer fixes: **sources are shared across every session**, so there
is no per-viewer filtering to add and the built-in door grants access per *app*, never per
pane. If some viewers should not see some rows, those rows must not be in the file — split the
data into separate apps and let the apps claim decide.

**Prevent.** Default to loopback plus a tunnel, or bind wide with a door on. `SECURITY.md` has
the threat model and the full list of known weaknesses — including the three things the door
does not do — and `OPERATIONS.md` has the deployment shapes.

---

## 12. A number did not change, and I do not know if that is a bug

Worth its own entry, because "the page did not update" is the same observation as "the page is
working efficiently", and the runtime is the only thing that can tell you which.

**Confirm.** Ask it:

```console
$ dagpane explain app.toml --set device=mobile
    ran      scoped               changed
    ran      steps_present        same value — nothing below it ran
    reused   step_count           its inputs had not moved
  3 cell(s) never looked at: events, variant, all_sessions
  patch: 5 of 7 panes — visits, subscribers, funnel_revenue, by_step, step_table
```

There are three distinct reasons a pane does not repaint, and all three are correct:

| `explain` says | meaning |
|---|---|
| `same value — nothing below it ran` | the cell recomputed and landed on the value it already had |
| `reused — its inputs had not moved` | nothing upstream changed, so it was not recomputed |
| `never looked at` | the cell is not downstream of anything that changed |

**If the pane you expected to move is in `never looked at`,** that is your answer: it reads the
source rather than the filtered cell, so no control on that page can move it. That is often
deliberate — it is how a company total is fenced off from a regional filter — but if you meant
it to follow the filter, change the cell's `from` to read the filtered cell instead.

**If it says `same value` and you believe the value should have changed,** the arithmetic is the
thing to check, not the scheduler. `dagpane graph` prints the edges the runtime is actually
using.

**Prevent.** The engine's own guarantee here is checked by a differential oracle — 200 random
graphs, 2 400 interactions, each asserted both for correctness against a full recompute and for
economy against the structural closure (`crates/core/tests/oracle.rs`). A stale number on a page
that looks fine is the one failure mode this project is built not to have.

---

## 13. The log says a connection was dropped

```
dagpane: dropped a connection to sales: a frame waited 30.1s to be written, so its client had stopped reading or fallen behind
```

**What it means.** A frame — an answer, a refresh, a ping — could not be written within its
allowance, 30 s plus the time a 128 kbit/s link needs for the frame, so the client was not
reading, or had fallen too far behind what it was sent, and the server let the connection go
rather than hold its session (and, for a scheduled source, the rows it had) for as long as the
client liked. `OPERATIONS.md`, *A connection that stops reading*.

**Usually.** A tab whose process is wedged, a laptop that slept mid-frame, or a proxy that
stopped forwarding. A line now and then is the deadline doing its job; the bundled page
reconnects on its own if it is ever reading again.

**Many lines, for clients you know to be real.** Either they read more slowly than 128 kbit/s
— raise `--write-timeout-seconds` (on `run`, `serve` or `host`), or send fewer rows — or they
are sent more than their link carries, refreshes of a large table landing faster than they can
be read. A longer timeout only keeps those connected further behind; refresh less often, or
send fewer rows.

**Many lines, from clients you do not know.** Something is opening connections and not
reading them. The deadline bounds what each one holds; how many there are is the proxy's to
limit.

---

## 14. The log says a ping went unanswered

```
dagpane: dropped a connection to sales: a ping went 30.0s unanswered, so its client had stopped reading or gone
```

**What it means.** A ping was not answered within its allowance — 30 s, plus the time a
128 kbit/s link needs for everything the connection wrote since the client last answered one —
so the client had stopped reading or was no longer there, and the server let the connection go
rather than hold its session until TCP noticed: a quarter of an hour for a client that stopped
acknowledging, and never for one whose kernel still does. `OPERATIONS.md`, *A connection that
stops reading*.

**Usually.** A laptop shut or a phone off the network with the page still open: nothing told
the server, and nothing would have for a quarter of an hour. Or a phone that put its browser
in the background, which can suspend the page and its answers with it. A desktop browser
answers pings without the page's script, so a background tab or a busy page is not the cause,
and neither is a busy server: an answer that arrived while it was busy is read before anyone
is dropped. When the device wakes, or the page is opened again, the bundled page reconnects on
its own.

**Many lines, for clients you know to be real.** Something between them and this process is
not passing pings or their answers through — a proxy that ends the WebSocket itself and does
not forward its control frames — or their links stall for longer than the allowance. Raise
`--ping-timeout-seconds` (on `run`, `serve` or `host`), or set it to `0` and go back to the
cost described above.

**Many lines, from clients you do not know.** Something is opening connections and abandoning
them. The deadline bounds how long each one is held; how many there are is the proxy's to
limit.

---

## 15. dagpane host answers 404 for every app

**Confirm.** The page at the hostname users type is a `404`, and its body says the *name* is
the problem rather than the app:

```console
$ curl -s -H 'Host: sales.example.com' http://127.0.0.1:8787/
no app is served at sales.example.com: this server does not answer to that name
```

stderr names the domain it was under, once:

```
dagpane: refused Host "sales.example.com": no domain this host answers under contains it, so every name under "example.com" gets 404 (logged once per domain). If example.com is a domain you control and publish these apps under, pass --domain example.com
```

and the `names` line the process printed at start-up says exactly which names it does answer:

```
dagpane: names     <app>.localhost — e.g. sales.localhost; any other name, a bare <app> or an address gets 404
```

A body that stops at `no app is served at sales.example.com`, with nothing after it, is a
different problem: the name is one it answers, and there is no `sales.toml` — or the file's
name is not a DNS label, which start-up reported as skipped. And do not wait for the stderr
line: it is written once per domain, and at most 32 times per process for domains, addresses
and bare names together, so on a process that has been up a while, or been probed, it may
already have been spent. The body is the confirmation that does not run out.

**Cause.** `dagpane host` answers a name only if it is exactly one label — the app — in front
of a domain it was given: `<app>.localhost` always, and `<app>.<domain>` for each `--domain`.
Every other name is refused before anything is read, because whoever controls a name's DNS can
point it at this machine, and a page served under that name then sends its own name as both
`Host` and `Origin` (`OPERATIONS.md`, *The constraint that catches everyone*). Three ways to get
here:

| what you see | what happened |
|---|---|
| no `--domain` on the command line; `names` lists only `<app>.localhost` | the proxy forwards the public hostname, and its domain was never declared. A process bound beyond loopback warned about exactly this at start-up |
| `--domain sales.example.com`; `names` lists `<app>.sales.example.com` | the domain was set to one app's full hostname, so that app answers at `sales.sales.example.com` and nothing answers at `sales.example.com`. Start-up warned, and named the fix |
| the refused `Host` is an address, and stderr says `never by address` | the proxy sends its upstream's address as `Host` — nginx's `proxy_pass http://127.0.0.1:8787` with no `proxy_set_header Host`, or a balancer that rewrites it. A container's or Service's bare name (`Host: dagpane`) is the same mistake, logged as `never by a bare name`. The upgrade probe answers `403` here, not `404`: the `Origin` check runs first, and the `Host` is not the name in `Origin` — go by the page's `404` |

**Fix.** Pass the domain the apps sit **under** — the parent of their hostnames, not one app's
hostname — and restart:

```sh
dagpane host ./apps --domain example.com     # sales.toml answers at sales.example.com
```

If the proxy sends an address or its upstream's name, make it forward the hostname the user
typed: `proxy_set_header Host $host;` in nginx, or, behind eggrd, which sets `Host` from its
upstream URL, make that URL's hostname the public one (`docs/PUBLIC-HTTPS.md`). Then the upgrade
probe from **First five minutes**, with `Origin: https://sales.example.com` sent to
`https://sales.example.com/ws`, should answer `101`.

**Do not "fix" this by declaring whatever the log names.** The name in a refused `Host` came
from whoever sent the request, which is why the line says *if you control it*: declare a domain
whose DNS is yours. Never reach for a shared wildcard service (`localtest.me` and `lvh.me`,
which answer every name with loopback; `nip.io` and `sslip.io`, which answer with the address
the name spells) or a public suffix (`duckdns.org`, `github.io`, `co.uk`) to get
a name that resolves — declaring a domain trusts every name directly under it, and under those
it is somebody else's DNS that answers. The process refuses a single label such as `com` at
start-up; it cannot know which multi-label domains are public.

**Prevent.** Read the `names` line at every start-up — it is the one statement of what the
process answers to — and gate the deploy on the upgrade probe at the hostname users type, not on
`GET /` against the bound address.

---

## What to collect before escalating

```console
$ dagpane --version
$ dagpane check   /path/to/app.toml
$ dagpane graph   /path/to/app.toml
$ dagpane explain /path/to/app.toml --set <the control you touched>=<value> --json
$ head -3 /path/to/each-source.csv && wc -l /path/to/each-source.csv
```

Plus the process's start-up output, the bind address, whatever sits in front of it, and — if a
browser is involved — the exact hostname typed and the status of the `/ws` upgrade. Security
reports go the way `SECURITY.md` says, not into a public issue.
