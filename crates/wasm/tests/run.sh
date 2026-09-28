#!/usr/bin/env bash
# Build the module and drive it in a real WebAssembly engine.
#
# `cargo test -p dagpane-wasm` tests `engine.rs` on the host, which is where the logic is. It
# does NOT test the four `extern "C"` exports, the hand-written length prefix or the
# JavaScript that has to agree with both — and a hand-rolled ABI goes wrong exactly there.
# `benches/wasm-engines/README.md` learnt the same lesson from the other side: a `cargo check`
# is not a measurement.
#
# So this builds the artefact and runs it. Node is the engine because it is a V8 with no
# browser attached; nothing in the test is Node-specific beyond reading a file.
#
#   ./crates/wasm/tests/run.sh
#
# Also exports the bundled example and drives THAT, which is what asserts the published
# interaction costs the same in a browser as it does on a server.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$HERE"

command -v node >/dev/null || { echo "node is required to run the wasm module" >&2; exit 1; }

# Honour CARGO_TARGET_DIR. The mirror guard sets it outside the staged tree — the publish is
# an `rsync --delete` of that directory and build output has no business in it — so a script
# that hardcoded `target/` would read nothing on a clean runner, or, worse, read a stale
# artefact on a machine that had one and pass for the wrong reason. This one did exactly that
# before it was caught.
TARGET="${CARGO_TARGET_DIR:-target}"
WASM="$TARGET/wasm32-unknown-unknown/release/dagpane_wasm.wasm"
DAGPANE="$TARGET/release/dagpane"
DIST="${DIST:-$TARGET/export-smoke}"

echo "building the module"
cargo build -p dagpane-wasm --target wasm32-unknown-unknown --release --locked

raw=$(stat -c%s "$WASM" 2>/dev/null || stat -f%z "$WASM")
gz=$(gzip -9 -c "$WASM" | wc -c | tr -d ' ')
printf "  %s raw, %s gzipped\n" \
  "$(numfmt --to=iec-i --suffix=B "$raw" 2>/dev/null || echo "$raw B")" \
  "$(numfmt --to=iec-i --suffix=B "$gz" 2>/dev/null || echo "$gz B")"

echo "exporting the bundled example"
cargo build --release --locked -p dagpane-cli
rm -rf "$DIST"
"$DAGPANE" export examples/sales.toml --out "$DIST" --wasm "$WASM" >/dev/null

echo "driving both"
node crates/wasm/tests/smoke.mjs "$WASM" "$DIST"
