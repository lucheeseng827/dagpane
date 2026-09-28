# The fleet benchmark

What `ROADMAP.md` §7 asks for: *apps-per-core and p99 under N concurrent viewers* — the
number it calls "a number no incumbent publishes and this project could win", against a
marimo and a Streamlit baseline running the same app on the same hardware. `BENCHMARKS.md`
publishes the result. This file is how to reproduce it and what to distrust.

```sh
cargo build --release -p dagpane-cli
python3 -m venv .venv && .venv/bin/pip install streamlit marimo playwright pandas
DAGPANE_BENCH_VENV=$PWD/.venv python3 benches/fleet/fleet.py --all --apps 1,2,4,8,16
```

Two environment variables, both optional:

| | |
|---|---|
| `DAGPANE_BENCH_VENV` | where to find `streamlit` and `marimo`, if not on `PATH` |
| `DAGPANE_BENCH_CHROMIUM` | an explicit Chromium, for images that ship one Playwright did not install |

## The four arrangements

| runtime | shape | why it is here |
|---|---|---|
| `dagpane` | one process per app | the apples-to-apples comparison: the same arrangement the two Python baselines use |
| `dagpane-host` | **N apps in one process** | what `crates/host` exists for, and what a hosting claim is actually about |
| `streamlit` | one process per app | the incumbent |
| `marimo` | one process per app | the closer comparison — it also builds a dependency graph |

Running dagpane both ways is the point. Comparing one process holding sixteen apps against
sixteen Python processes holding one each would be comparing two decisions at once, and the
`dagpane` row is what separates them: it is this runtime arranged the way the baselines are,
so the difference between the two dagpane rows is multiplexing alone and the difference
between `dagpane` and the baselines is the runtime alone.

## The app

`apps/sales.toml`, `apps/streamlit_app.py`, `apps/marimo_app.py` — the same app, written
three times, each idiomatically in its own runtime. Five derived values over 600 rows: a
filter driven by one control, two aggregates over it, a group-by, and one aggregate over the
**source** that no setting of the control can invalidate.

Each file's own header argues its case for being idiomatic. The Streamlit one is worth
reading before quoting any number from this benchmark: it caches the CSV read (what every
real Streamlit app does) and does not cache the derived frames (what Streamlit's execution
model does by default). Caching the derived frames too is a real third design that would
land somewhere between the two runtimes; it is not what is measured here, and that is a
choice rather than an oversight.

## What is measured, and how

**Resident cost per app.** N apps, each with one live browser session — a session, not an
idle process, because a Streamlit process nobody has connected to has not run the script yet
and its memory says so. Sampled from `/proc/*/smaps_rollup` over the whole server process
tree, two seconds after the fleet settles.

**Both RSS and PSS, because they disagree and the disagreement is the finding.** Summing RSS
across sixteen Python processes counts every shared page of libpython, pandas and numpy
sixteen times, so it overstates what the fleet costs the machine — and overstates it more as
the fleet grows. PSS divides each shared page by the number of processes mapping it. P2's
criterion says RSS, so RSS is reported; **where the two differ, PSS is the one an
apps-per-core figure should use.** Both are in the result file so a reader can recompute
either.

**Interaction latency.** Set the control, wait until the number on screen is the number the
oracle says it should be. Identical semantics in all four. The oracle is computed from the
CSV by the harness with the standard library — no pandas, so it shares no dependency with
two of the runtimes it judges — which means **a runtime that renders a wrong number fails
rather than scoring well.**

## What to distrust

**The driver floor.** A browser round trip through Playwright costs ~17–19 ms on the machine
these numbers came from, measured each run against a local page whose "server" is a
`textContent` assignment and reported as `driver_floor_ms`. It is inside every latency here
and it is **not** subtracted, because subtracting a median from a p99 is not arithmetic. For
the Python baselines it is a rounding error. For dagpane it is most of the number: read the
dagpane latencies as *"at or below the driver floor"* rather than as a measurement of the
server.

**Saturation.** A browser per viewer is expensive and this rig runs the driver on the same
machine as the fleet. Each run reports the driver's CPU share and how late it dispatched
against its own schedule; past 80% or 50 ms the run is marked `saturated` and **every latency
in it is a lower bound on what the server could have done** — including the baselines', so a
saturated row is not evidence about anything except that the rig ran out of machine.

**One machine, and a shared one.** The committed result carries its `machine` block. A
figure that feeds a pricing model needs a dedicated host with a known neighbour, and the
absolute apps-per-core number from a shared container is not that. The *ratios* survive
better than the absolutes: all four runtimes take the same noise.

**Six hundred rows.** This measures the cost of holding and serving an app, not the cost of a
large source — `BENCHMARKS.md`'s row sweep already measured that, and found the wall at
around a hundred thousand rows. An app-per-core figure over 600-row apps is a figure about
the runtime's overhead, which is what it is for.
