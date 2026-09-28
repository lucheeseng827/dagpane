#!/usr/bin/env bash
# What one more viewer of the SAME app costs, and what decides it.
#
# Three ramps, and the comparison between them is the result rather than any one of them:
#
#   1. the bundled 600-row example                     — the app every other number here uses
#   2. the same pipeline over 200 000 rows             — a pipeline whose cells KEEP rows
#   3. the same 200 000 rows, aggregates only          — a pipeline whose cells keep none
#
# 2 against 3 is the experiment. Same data, same process, same sockets; the only difference is
# whether anything downstream of the source holds onto rows. If a session's cost tracked the
# app's *sources*, those two would cost the same.
#
#   ./benches/sessions/run.sh
#
# Bare Node, no npm dependency — Node 22 has a WebSocket, and `/proc/PID/smaps_rollup` is the
# kernel's own accounting rather than `ps`'s. Linux only, for that reason.
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

echo "═══ 1. the bundled example — 600 rows, the pipeline keeps rows ═══"
DAGPANE_PORT=8794 DAGPANE_TAG=small node "$HERE/session-memory.mjs" examples/sales.toml

echo
echo "═══ 2. 200 000 rows, the SAME pipeline ═══"
DAGPANE_PORT=8795 DAGPANE_TAG=rows node "$HERE/session-memory.mjs" "$WORK/rows/rows.toml"

echo
echo "═══ 3. 200 000 rows, aggregates only — nothing downstream keeps a row ═══"
DAGPANE_PORT=8796 DAGPANE_TAG=agg node "$HERE/session-memory.mjs" "$WORK/agg/agg.toml"

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
const [small, rows, agg] = await Promise.all([newest("small"), newest("rows"), newest("agg")]);
if (!small || !rows || !agg) { console.log("(not all three ran)"); process.exit(0); }
const kb = (b) => (b / 1024).toFixed(0).padStart(8);
const mb = (b) => (b / 1048576).toFixed(1).padStart(7);
console.log("═══ the comparison ═══\n");
console.log("  app                              source   per session   ratio");
console.log("  ───────────────────────────  ──────────  ────────────  ──────");
for (const [name, r] of [["600 rows, keeps rows", small], ["200k rows, keeps rows", rows],
                         ["200k rows, aggregates only", agg]]) {
  console.log(`  ${name.padEnd(27)}  ${mb(r.csv_bytes)} MB  ${kb(r.marginal_rss_bytes_per_session)} kB  ` +
              `${(r.marginal_rss_bytes_per_session / r.csv_bytes).toFixed(3).padStart(6)}`);
}
const factor = rows.marginal_rss_bytes_per_session / agg.marginal_rss_bytes_per_session;
console.log(`\n  Rows 2 and 3 are the SAME DATA in the SAME PROCESS. ${factor.toFixed(0)}× apart.`);
console.log("  So a session costs what its pipeline MATERIALISES, not what the app SOURCES.");
JS
