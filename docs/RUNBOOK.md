# Runbook

Symptom first. `OPERATIONS.md` is the companion for deciding how to run this; reach for this
file when it is already running and something is wrong.

Every command here is one you can paste, and every quoted output was produced by
`dagpane 0.1.1` on this tree rather than written from memory.

---

## First five minutes

```console
$ dagpane check /path/to/app.toml            # does the app still compile?     exit 0 = yes
$ curl -s -o /dev/null -w '%{http_code}\n' http://HOST:PORT/          # is the page served?
$ curl -s -o /dev/null -w '%{http_code}\n' \
    -H 'Connection: Upgrade' -H 'Upgrade: websocket' \
    -H 'Sec-WebSocket-Version: 13' -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' \
    -H "Origin: https://THE-HOSTNAME-USERS-TYPE" \
    https://THE-HOSTNAME-USERS-TYPE/ws       # 101 = healthy.  403 = read §1.  Anything else = §2
$ dagpane explain /path/to/app.toml --set SOME_INPUT=VALUE    # does an interaction still work?
```

Those four answers separate almost everything below. The third is the one people skip, and it
is the one that catches the most common production failure in this version.

| what you see | go to |
|---|---|
| page loads, controls do nothing | [§1](#1-the-page-loads-and-the-controls-do-nothing) |
| nothing is listening | [§2](#2-nothing-is-listening-even-though-it-printed-a-url) |
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

**Cause.** The `/ws` upgrade checks the browser's `Origin` against an allowlist derived from
**the address the process bound**, and nothing else. A proxy, ingress or load balancer on a
real hostname means the browser sends that hostname, which never matches the bound address. It
is an anti-CSRF measure, it is working as designed, and there is no flag to change it.

**If the process was started with `--host 0.0.0.0`, this fires unconditionally.** The allowlist
is derived from the bound address literally, so `0.0.0.0:8787` allows exactly
`http://0.0.0.0:8787` — which no browser ever sends. Every containerised deployment hits this,
proxy or not, because `--host 0.0.0.0` is what makes a container reachable at all. Verified:

```console
$ # process bound 0.0.0.0:8813, reached over plain localhost, no proxy anywhere
$ curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8813/         # the page
200
$ curl ... -H 'Origin: http://127.0.0.1:8813' http://127.0.0.1:8813/ws     # the socket
403
```

There will be **nothing in the server log** about this. Nothing is logged per connection.

**Fix — pick one.**

* **Reach it at the address it bound — which requires that address to be loopback.**
  `ssh -L 8787:127.0.0.1:8787 host`, then open `http://127.0.0.1:8787`. This works only if the
  remote process bound `127.0.0.1`. Tunnelling to a process bound `0.0.0.0` still sends
  `Origin: http://127.0.0.1:8787` against an allowlist holding only `http://0.0.0.0:8787`, so
  it is still refused — a wildcard bind needs the proxy below.
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

## 2. Nothing is listening, even though it printed a URL

**Confirm.** The process printed its banner and a URL, then exited or sat there while the port
refused connections:

```
dagpane: Funnel explorer — 11 cells, 7 panes
dagpane: http://127.0.0.1:8805
dagpane: Address already in use (os error 98)
```

**Cause.** The banner and the URL are printed **before the bind is attempted**, so that URL
line is not evidence that anything is listening. The real answer is the line after it. Common
ones:

| message | meaning |
|---|---|
| `Address already in use (os error 98)` | something already holds that port |
| `Cannot assign requested address (os error 99)` | `--host` names an address this machine does not have |
| `Permission denied (os error 13)` | a port below 1024 without the capability to bind it |

**Fix.** `ss -ltnp \| grep :PORT` to find the holder; change `--port`, or stop the other
process. For a wrong `--host`, use `0.0.0.0` to bind everything or the address the interface
actually has — and read the no-auth warning that a non-loopback bind prints.

**Prevent.** Check both. The process does exit non-zero on a bind failure (verified: `1` on
`Address already in use`), so ordinary restart-on-failure handling is doing its job — but the
printed URL is not evidence of anything, and a port held by *another* process leaves you with
something answering on it that is not this app. So keep the exit-status handling and add a
readiness check on the port itself: an HTTP probe against `/` in Kubernetes,
`ExecStartPost`/`systemd-notify` or a simple `curl` gate elsewhere.

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

If the file is newer than the process, the process is serving the older data.

**Cause.** **Not a bug.** Sources are loaded once, at start-up, and shared immutably across
every session. There is no file watcher and no hot reload. Replacing the CSV under a running
process changes nothing that anybody sees.

**Fix.** Restart the process. Prefer `SIGINT` — see §10.

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
than settle, and the exit was "killed by signal 15".

**Cause.** Signal handling, verified:

| signal | behaviour | exit |
|---|---|---|
| `SIGINT` | graceful — stops accepting, in-flight requests finish | `0` |
| `SIGTERM` | **not handled** — immediate termination | killed by signal 15 |

**systemd and Kubernetes both send `SIGTERM` by default**, and the published image sets no
`STOPSIGNAL`, so the default path is the abrupt one.

**Fix.** Nothing is lost — a session holds no work, only control positions, which reset to the
manifest defaults on reconnect. Viewers reconnect and get a fresh first render.

**Prevent.** Ask for the graceful signal explicitly:

```ini
[Service]
KillSignal=SIGINT
TimeoutStopSec=15
```

```dockerfile
STOPSIGNAL SIGINT
```

A `preStop` hook cannot change which signal is sent, so in Kubernetes this has to be baked into
an image of your own.

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
