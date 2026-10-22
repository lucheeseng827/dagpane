#!/usr/bin/env bash
# What the process says its viewers hold, against what the kernel says they cost.
#
# `run.sh` priced a viewer from outside the process. The host now prices one from inside it —
# `/healthz` reports what each app's viewers hold beyond the sources they share — and this
# puts the two side by side over the same three apps:
#
#   1. the bundled 600-row example
#   2. the same pipeline over 200 000 rows             — cells that KEEP rows
#   3. the same 200 000 rows, aggregates only          — cells that keep none
#
# The ledger counts frames and the kernel counts everything, so they are not meant to agree.
# They are meant to differ by **the same amount for all three**: that gap is what a connection
# costs whatever it is looking at, and the ledger is right exactly when everything that varies
# with the pipeline is on its side of the gap.
#
#   ./benches/sessions/ledger.sh
#
# Bare Node, no npm dependency, Linux only — the same instrument as `run.sh`, from `lib.mjs`.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"

command -v node >/dev/null || { echo "node is required" >&2; exit 1; }
[ -r /proc/self/smaps_rollup ] || { echo "this rig needs Linux (smaps_rollup)" >&2; exit 1; }

TARGET="${CARGO_TARGET_DIR:-target}"
BIN="$TARGET/release/dagpane"
[ -x "$BIN" ] || cargo build --release -p dagpane-cli
export DAGPANE_BIN="$BIN"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# shellcheck source=benches/sessions/fixtures.sh
. "$HERE/fixtures.sh"
fixtures
# A host serves every manifest in its directory, so the bundled example gets one of its own
# rather than being served out of `examples/` beside whatever else is there.
mkdir -p "$WORK/small"
cp examples/sales.toml "$WORK/small/small.toml"
cp examples/sales.csv "$WORK/small/sales.csv"

echo "═══ 1. the bundled example — 600 rows, the pipeline keeps rows ═══"
DAGPANE_PORT=8820 DAGPANE_TAG=ledger-small node "$HERE/ledger.mjs" "$WORK/small" small

echo
echo "═══ 2. 200 000 rows, the SAME pipeline ═══"
DAGPANE_PORT=8821 DAGPANE_TAG=ledger-rows node "$HERE/ledger.mjs" "$WORK/rows" rows

echo
echo "═══ 3. 200 000 rows, aggregates only — nothing downstream keeps a row ═══"
DAGPANE_PORT=8822 DAGPANE_TAG=ledger-agg node "$HERE/ledger.mjs" "$WORK/agg" agg

echo
node - "$HERE/results" <<'JS'
import { readdir, readFile } from "node:fs/promises";
const dir = process.argv[2];
const files = (await readdir(dir)).filter((f) => f.endsWith(".json"));
const newest = async (tag) => {
  const mine = files.filter((f) => f.startsWith(tag + "-")).sort();
  if (!mine.length) return null;
  return JSON.parse(await readFile(`${dir}/${mine[mine.length - 1]}`, "utf8"));
};
const [small, rows, agg] = await Promise.all(
  ["ledger-small", "ledger-rows", "ledger-agg"].map(newest),
);
if (!small || !rows || !agg) { console.log("(not all three ran)"); process.exit(0); }
const kb = (b) => (b / 1024).toFixed(0).padStart(8);
console.log("═══ two instruments, one process each ═══\n");
console.log("  app                            kernel (RSS)   ledger   not in the ledger");
console.log("  ───────────────────────────    ────────────  ───────  ─────────────────");
const all = [["600 rows, keeps rows", small], ["200k rows, keeps rows", rows],
             ["200k rows, aggregates only", agg]];
for (const [name, r] of all) {
  console.log(`  ${name.padEnd(27)}  ${kb(r.marginal_rss_bytes_per_session)} kB  ` +
              `${kb(r.ledger_bytes_per_session)} kB  ${kb(r.unseen_rss_bytes_per_session)} kB`);
}
const gaps = all.map(([, r]) => r.unseen_rss_bytes_per_session);
const ledgers = all.map(([, r]) => r.ledger_bytes_per_session);
console.log(`\n  The ledgers span ${(Math.max(...ledgers) / Math.min(...ledgers)).toFixed(0)}×; ` +
            `the gaps span ${(Math.min(...gaps) / 1024).toFixed(0)}–` +
            `${(Math.max(...gaps) / 1024).toFixed(0)} kB.`);
console.log("  What the ledger leaves out is what a connection costs whatever it is looking at.");
JS
