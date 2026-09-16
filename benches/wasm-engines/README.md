# Which engine can go in a browser, and what it weighs

A probe, not a library. Each feature selects one candidate for the `Frame` seam
(`ARCHITECTURE.md` §6), links enough of it that the optimiser cannot strip it — build a
frame, filter it, sort it, take a slice, which is what the seven verbs reduce to — and gets
built for `wasm32-unknown-unknown` under this module's release profile.

```sh
rustup target add wasm32-unknown-unknown
./run.sh
```

Two things this exists to stop.

**A `cargo check` is not a size measurement**, and an empty `cdylib` that merely *depends*
on a crate links almost none of it. Hence the probe functions.

**A tier that fails to enable `getrandom`'s wasm backend looks exactly like a tier that
cannot build.** That produced a false negative here once already, so the backend is wired
into every tier and into `.cargo/config.toml`, not just the one being investigated. When a
candidate is reported as failing, check that first.

`src/bin/mem.rs` is the other half: the same 1M-row categorical column held three ways,
run natively, because the question there is bytes rather than portability.

Results and what they mean: `../../BENCHMARKS.md`.
