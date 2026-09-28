# The protocol-level load generator

`benches/fleet/` drives three runtimes through one browser, which is the only honest way to
compare them — one driver, one definition of "the interaction is done", no protocol
reimplementation to argue about. It is also why that rig cannot find dagpane's ceiling: a
Chromium page per viewer, on the same machine as the fleet, runs out of machine first, and
its round trip costs ~24 ms against a dagpane interaction that costs about one.

This tool is exactly that gap and nothing more. **dagpane only, protocol level.**

```sh
cargo build --release -p dagpane-cli
cargo build --release --manifest-path benches/loadgen/Cargo.toml

# What the tool itself costs, before it measures anything
./benches/loadgen/target/release/dagpane-loadgen --self-test

# Apps-per-core, on a core
./benches/loadgen/apps-per-core.sh
APPS=512 RAMP=128,256,384,512 VIEWERS=4 ./benches/loadgen/apps-per-core.sh
```

It makes **no cross-runtime claim and cannot**. Streamlit speaks protobuf and marimo a
dialect of its own; three bespoke drivers would each be fair to one runtime and the numbers
could not be put in one table. The two rigs are published separately and never averaged.

## What one viewer does

```
connect ──> init ──> [ wait for its slot ──> Set ──> Patch ──> check ] * rounds
```

**check** is what makes this a benchmark and not a packet blaster. An oracle computed from
the CSV says what `revenue` must read at each control setting, and every reply is held to it.
Two ways to fail, and the second is the interesting one:

- the patch carries the pane and the number is wrong;
- the patch **omits** the pane and the number should have moved. A patch carries only panes
  whose rendered view changed, so a generator that ignored absences could not tell "nothing
  needed to change" from "the engine missed an invalidation" — the one bug this runtime must
  not have. Any mismatch fails the run and the tool exits non-zero: *a runtime that renders a
  wrong number has not earned a latency figure.*

## Why the server is pinned

`apps-per-core.sh` runs the server under `taskset` on one named CPU and the generator on the
others. "Per core" is then not a figure of speech, and the generator is not fighting the
thing it is measuring. A server free to use every core produces a number that divides by a
denominator nobody measured.

## The two floors, and the one signal that is not what it looks like

`--self-test` drives an in-process echo over the same tokio, the same tungstenite and the same
serde: **p50 67 µs, p99 124 µs** on the machine in `BENCHMARKS.md`. Everything else this tool
reports sits on top of that, and a run whose p50 approaches it is measuring the tool.

Every run also times a `Refresh` — same socket, same codec, every pane re-rendered, **no
pass**. The gap between that and a `Set` is the pass *only when both are measured under the
same load*, and "the same load" is a property of the arrival schedule rather than of the
socket: each viewer now sleeps to its own offset **before** sending its `Refresh`, so the two
figures see the same traffic shape. Neither floor is subtracted from anything.

**The committed runs under `results/` predate that fix**, so their `refresh_*` percentiles
were taken under a synchronised burst while their `set_*` percentiles were not. Read those two
numbers separately; the gap between them in those files is not the pass. The figure the
apps-per-core claim rests on is `set_p99` against the latency budget, which is unaffected.

**`schedule_slip_p99_ms` is a server signal, not a tool one, and an earlier version had that
backwards.** A viewer here is closed-loop: it sends, awaits its reply, then sleeps to its next
slot. A slow reply pushes the slot back, so slip appears whether or not the generator has any
work to do. The measured runs make it unmistakable — at 224 apps the slip p99 was 107 ms while
the generator used 8% of three cores, and at 320 apps it was 2.9 **seconds** at the same 7%. A
tool using seven per cent of its cores is not saturated; the server was. Treating that as the
generator giving out would have discarded the exact point where the ceiling was found.
`generator_cpu_share` is the only tool-saturation signal, and it is reported on every rung.

## Two more things the rig did to itself

**Viewers start spread evenly across one think period.** Without that, every viewer shares a
period and a start time, the fleet synchronises into waves, and the percentiles describe the
interference pattern rather than the server — which is what the first run of this ladder
produced: a p50 of 1.0 ms at 160 apps sitting beside a p99 of 102 ms. The offset is derived
from the viewer's index rather than drawn at random, so two runs of the same ladder are
comparable.

**The spread has to happen before the `Refresh`, not after it.** It did not, at first. Every
viewer sent its `Refresh` the moment it connected, so that one measurement was taken under a
synchronised burst and every `Set` after it was taken under the schedule — and the gap between
them, which the section above reads as the cost of a pass, was carrying the difference between
two traffic shapes instead. It is the same mistake as the one above wearing different clothes:
a number that describes the arrival pattern being read as a number about the server. Found by
review rather than by the rig, which is worth saying, because the two below it were found by
running the thing and looking at what did not add up.

## What it still does not tell you

- **Nothing about Streamlit or marimo.** See above.
- **Nothing about a large source.** These are 600-row apps; the row sweep in `BENCHMARKS.md`
  is where the data wall was measured.
- **Nothing about a real workload's shape.** Every viewer moves one control on a fixed period.
  A real dashboard is mostly idle and occasionally bursty, and the apps-per-core figure is
  conditional on the rate stated beside it.
