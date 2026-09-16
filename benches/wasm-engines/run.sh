#!/usr/bin/env bash
# Which dataframe engines build for wasm32-unknown-unknown, and how big each one is.
#
# ROADMAP §2 has to choose a representation behind `Value::Frame`, and §4 cares what that
# choice costs a browser. This answers both, for every candidate, under the profile that
# would actually ship.
#
#   ./run.sh                 # every tier
#   ./run.sh arrow-min       # just one
#
# Requires the wasm32 target:  rustup target add wasm32-unknown-unknown
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

TIERS=("$@")
[ ${#TIERS[@]} -gt 0 ] || TIERS=(arrow-min arrow-parquet polars polars-parquet datafusion)
W=target/wasm32-unknown-unknown/release/wasm_engines.wasm

printf '%-16s %-8s %10s %10s\n' TIER BUILDS RAW GZIPPED
for tier in "${TIERS[@]}"; do
  rm -f "$W"
  if cargo build --release --target wasm32-unknown-unknown --features "$tier" >/dev/null 2>&1 && [ -f "$W" ]; then
    printf '%-16s %-8s %10s %10s\n' "$tier" ok \
      "$(du -b "$W" | cut -f1 | numfmt --to=iec-i --suffix=B)" \
      "$(gzip -9 -c "$W" | wc -c | numfmt --to=iec-i --suffix=B)"
  else
    printf '%-16s %-8s %10s %10s\n' "$tier" FAILED - -
  fi
done

echo
echo "and what each representation costs in memory for one categorical column:"
cargo run --release --features arrow-min --bin mem 2>/dev/null | tail -4
