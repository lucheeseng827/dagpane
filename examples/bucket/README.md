# A panel whose data lives in a bucket

```
sync.sh     pull an app directory out of object storage. One job, no serving.
serve.sh    sync -> compile -> if it changed AND it compiles, swap the snapshot and restart.
```

## The constraint this is built around

dagpane loads its sources **once, at start-up**, and shares them immutably across every
session. There is no file watcher and no hot reload. That is not an oversight — it is what
makes a hundred viewers a hundred vectors of values over one allocation per source — the
source; a session's own computed frames are not shared and `BENCHMARKS.md` prices them — and what
makes a pass a pure function of the inputs.

So "the dashboard follows the bucket" means exactly one thing:

```
sync  →  compile  →  if it changed AND it compiles  →  swap the snapshot  →  restart
```

`serve.sh` is that loop. Doing it in that order, rather than pointing a cron line at the live
directory, buys three things:

**A snapshot that will not compile never reaches a viewer.** `dagpane check` runs on the
staged copy before anything is swapped. A half-finished upload, a CSV the sync did not
deliver, a manifest a colleague broke in the bucket — the previous snapshot keeps serving, and
the log says why.

**An unchanged bucket costs nothing.** The content digest gate means a restart only happens
when the bytes actually moved. A five-minute poll over a bucket that updates daily restarts
once a day, not 288 times.

**Rollback is a symlink.** Snapshots are kept by digest under `snapshots/`, and `current`
points at the live one. Going back is `ln -sfn` and a restart, with no bucket round trip.

## Try it without a bucket

`sync.sh` takes a local directory anywhere it takes a bucket URI, so the loop is testable on
a laptop with nothing configured:

```sh
mkdir -p /tmp/fakebucket
cp examples/apps/04-campaigns.toml /tmp/fakebucket/app.toml
sed -i 's|../data/campaigns.csv|campaigns.csv|' /tmp/fakebucket/app.toml
cp examples/data/campaigns.csv /tmp/fakebucket/

INTERVAL=10 MIN_ROWS=100 examples/bucket/serve.sh /tmp/fakebucket app.toml
```

Now edit files in `/tmp/fakebucket` and watch the log. This is a real run of that loop, with
a broken manifest published at 19:10:58 and a truncated CSV at 19:11:01:

```
19:10:52Z  promoted 7db0d4483c87416b
19:10:52Z  serving snapshot 7db0d4483c87416b on http://127.0.0.1:8795 (pid 3957)
19:10:55Z  bucket unchanged (7db0d4483c87416b) — no restart
19:10:58Z  REFUSING 0b1ec55cf519a1ae: it does not compile (see above); keeping 7db0d4483c87416b
19:11:01Z  REFUSING a518048d960f7604: under MIN_ROWS=100 — campaigns.csv:0; keeping 7db0d4483c87416b
```

The page stayed up and stayed correct through both.

## Against a real bucket

```sh
INTERVAL=300 PORT=8787 ./serve.sh s3://my-bucket/panels/sales            app.toml
INTERVAL=300           ./serve.sh gs://my-bucket/panels/sales            app.toml
INTERVAL=300           ./serve.sh az://my-container/panels/sales         app.toml
INTERVAL=300           ./serve.sh rclone:myremote:my-bucket/panels/sales app.toml
```

| variable | default | what it does |
|---|---|---|
| `INTERVAL` | `300` | seconds between polls. Set it against how often the data genuinely moves |
| `PORT` / `HOST` | `8787` / `127.0.0.1` | bind address. Read the warning below before changing `HOST` |
| `MIN_ROWS` | `0` (off) | refuse a snapshot whose smallest CSV has fewer data rows than this |
| `KEEP` | `5` | snapshots kept for rollback |
| `WORK` | `./.dagpane-bucket` | where stage, snapshots and `current` live |
| `DAGPANE` | `dagpane` | path to the binary |

**Credentials are whatever the underlying CLI already reads** — an instance role, a workload
identity, a profile, the ambient environment. Nothing in these scripts takes a key, and
nothing in them should be given one.

## The bucket layout

A manifest's paths resolve against **its own directory**, so the directory — not the file — is
the unit that travels. Upload it whole:

```
s3://my-bucket/panels/sales/
  app.toml
  orders.csv
  regions.csv
```

One prefix per panel. **`sync.sh` mirrors on every backend**, so a file removed upstream is
removed locally too — a stale CSV nobody meant to keep serving is the failure this prevents.
S3, GCS and rclone get that from their own delete flag; `az` and a local directory have no such
flag, so those two download into a fresh directory and replace the destination only once the
transfer succeeds. A failed transfer therefore leaves the previous snapshot exactly as it was.

Only the local-directory branch is exercised by this repository's own walkthrough. The four
cloud branches are each vendor's documented mirror command and are not run by any test here.

The publishing side is whatever already writes to the bucket: an unload, a dbt post-hook, a CI
job, or `Panel.write()` from a notebook followed by `aws s3 sync`. See `../notebook/`, which
produces exactly this directory shape.

## What the gate does and does not catch

The gate is only worth what it actually rules out, so:

| failure | caught by | outcome |
|---|---|---|
| malformed manifest TOML | `check` | refused, previous snapshot keeps serving |
| a CSV the sync did not deliver | `check` | refused |
| a filter naming an input that no longer exists | `check` | refused |
| a pane naming a cell that no longer exists | `check` | refused |
| a cycle, a duplicate cell or pane id | `check` | refused |
| a CSV that arrived as a header with no rows | `MIN_ROWS` | refused, if you set it |
| **a column the upstream job stopped producing** | **neither** | promoted, then fails at run time — see below |

That last row is the one to understand. `check` compiles the manifest and loads the sources;
it does not look inside a column. A CSV that still parses but has lost a column the app
filters on compiles cleanly and fails when the cell runs:

```
failed   scoped   no column `spend`; the table has `day`, `channel`, `campaign`, `impressions`, `clicks`, `signups`
failed   by_channel   upstream cell `scoped` failed: no column `spend`; the table has ...
```

That is errors-as-values working as designed (ADR-0004): the failing cell holds its error, the
cells below it name the cell that actually failed, and **the rest of the page keeps working**.
One dropped column takes out the panes that depend on it, not the dashboard and not the
control that would let you look at something else. It is a good failure — but it is a run-time
one, so if you need it caught before promotion, assert the schema in the job that writes to
the bucket, where the producer is.

## Running it for real

**systemd** — the loop is already a supervisor, so this is a plain long-running unit:

```ini
[Unit]
Description=dagpane panel from object storage
After=network-online.target

[Service]
ExecStart=/opt/dagpane/serve.sh s3://my-bucket/panels/sales app.toml
Environment=INTERVAL=300 PORT=8787 HOST=127.0.0.1 MIN_ROWS=1 DAGPANE=/usr/local/bin/dagpane
WorkingDirectory=/var/lib/dagpane
Restart=always
DynamicUser=yes

[Install]
WantedBy=multi-user.target
```

**Containers.** The published image is `scratch` plus one static musl binary — no shell, no
`aws`, no `cp`. So `serve.sh` cannot run *inside* it, and that is the right shape anyway: run
the sync as a sidecar or init container that writes into a shared volume, and let the dagpane
container do nothing but serve what it finds there. The tradeoff is that the dagpane container
must be restarted to pick up a new snapshot — which is a deployment's normal job, and in
Kubernetes is what a `CronJob` that syncs and then rolls the Deployment does:

```sh
# the sidecar / init container, on any image that has your cloud CLI
aws s3 sync s3://my-bucket/panels/sales /shared --delete

# the dagpane container — no shell needed, and no auth, so keep it off the public network
docker run --rm -p 8787:8787 -v /shared:/app:ro \
  mancube/dagpane:0.1.1 run /app/app.toml --host 0.0.0.0
```

Run `dagpane check /app/app.toml` as the init container's last step to keep the gate: a
non-zero exit there stops the rollout, and the previously running pods carry on serving.

## Authentication, which is off unless you turn it on

`HOST` defaults to loopback, and binding anything else prints a warning that says so.
`--host 0.0.0.0` in a container makes the page — and therefore the bucket's contents —
readable by anyone who can route to the port. Either pass `--auth-jwks` with a mounted JWKS
file, or put something that authenticates in front. `SECURITY.md` covers both in full.

Two things that follow and are easy to miss. A restart drops every open page, because a
session *is* a connection — viewers reconnect, and the client replays the control values held
in its own URL, so they come back to the view they were on rather than to the manifest
defaults. And everyone who gets through sees the same snapshot, because sources are shared
across sessions and the front door does not change that: access is per app, so do not put rows
in the bucket that some viewers should not see.
