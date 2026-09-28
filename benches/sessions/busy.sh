#!/usr/bin/env bash
# What a viewer who is *interacting* costs, over one who is only holding a socket.
#
# `run.sh` weighs a held viewer. This weighs a busy one, over the same two apps, so the pair
# of answers can be read together:
#
#   1. the bundled 600-row example
#   2. the same pipeline over 200 000 rows             — cells that KEEP rows
#   3. the same 200 000 rows, aggregates only          — cells that keep none
#
# 2 against 3 is the experiment again, and it does not come out the way the held numbers did.
# A held session costs what its pipeline materialises and those two are 50x apart; a busy one
# costs what its pipeline **touches**, and the aggregating app touches every row it later
# throws away.
#
#   ./benches/sessions/busy.sh
#
# It also runs one app twice under `taskset`, on one core and on four. That is the second
# experiment rather than a tidiness measure: tokio sizes its worker pool from the affinity
# mask and a pass runs on a worker, so the pair asks whether the transient is bounded by the
# viewers dragging or by the threads available to serve them.
#
# Bare Node, no npm dependency. Linux only — `/proc/PID/status` and `clear_refs` are the
# instrument, and `benches/sessions/lib.mjs` says why nothing weaker would do.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"

command -v node >/dev/null || { echo "node is required" >&2; exit 1; }
command -v taskset >/dev/null || { echo "taskset is required (util-linux)" >&2; exit 1; }
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
DAGPANE_PORT=8900 DAGPANE_TAG=busy-small node "$HERE/busy-session.mjs" examples/sales.toml

echo
echo "═══ 2. 200 000 rows, the SAME pipeline — cells that keep rows ═══"
DAGPANE_PORT=8940 DAGPANE_TAG=busy-rows node "$HERE/busy-session.mjs" "$WORK/rows/rows.toml"

echo
echo "═══ 3. 200 000 rows, aggregates only — nothing downstream keeps a row ═══"
DAGPANE_PORT=8980 DAGPANE_TAG=busy-agg node "$HERE/busy-session.mjs" "$WORK/agg/agg.toml"

# ── the second experiment ─────────────────────────────────────────────────────────────────
#
# Same app, same ramp, one core against four. The aggregating app, because its held cost is
# almost nothing — so whatever moves here is the pass and not the viewer.
#
# A shorter ladder than section 3's, and deliberately: section 3 has already walked this app
# rung by rung, so what these two are for is the comparison BETWEEN them. The repetition count
# is left at the rig's default of three, because the method is three processes a rung medianed
# and a pair of runs that quietly used two would be a different method wearing the same words.
CORE_LADDER=0,1,4,16,64

echo
echo "═══ 4. the same aggregating app on ONE core ═══"
DAGPANE_PORT=9020 DAGPANE_TAG=busy-agg-1core DAGPANE_CORES=0 DAGPANE_LADDER="$CORE_LADDER" \
  node "$HERE/busy-session.mjs" "$WORK/agg/agg.toml"

echo
echo "═══ 5. the same aggregating app on FOUR cores ═══"
DAGPANE_PORT=9060 DAGPANE_TAG=busy-agg-4core DAGPANE_CORES=0-3 DAGPANE_LADDER="$CORE_LADDER" \
  node "$HERE/busy-session.mjs" "$WORK/agg/agg.toml"

# ── the tail the other sections cannot see ────────────────────────────────────────────────
#
# Every window above is three seconds, and a bystander's wait longer than its own window cannot
# be observed: the rig records it as censored and reports a lower bound. At the top rung the
# bystander gets about TWO answers in three seconds, so "2.7 s" is two samples taken through a
# three-second aperture — which is no basis at all for saying anything about the 25 s keepalive
# a hosted deployment relies on.
#
# This rung opens the aperture past that. One app, the worst one, the top rung, and thirty
# seconds: long enough that a wait which would starve a keepalive has somewhere to show up.
echo
echo "═══ 6. the worst app at the top rung, through a window that can see past 25 s ═══"
DAGPANE_PORT=9100 DAGPANE_TAG=busy-rows-longwindow DAGPANE_LADDER=0,64 DAGPANE_WINDOW_MS=30000 \
  node "$HERE/busy-session.mjs" "$WORK/rows/rows.toml"

# ── the allocator the container ships ─────────────────────────────────────────────────────
#
# Opt-in, because building it needs a target this repository does not install by default:
#
#   rustup target add x86_64-unknown-linux-musl && apt-get install -y musl-tools
#   CC_x86_64_unknown_linux_musl=musl-gcc \
#     cargo build --release --target x86_64-unknown-linux-musl -p dagpane-cli
#   DAGPANE_MUSL_BIN=target/x86_64-unknown-linux-musl/release/dagpane ./benches/sessions/busy.sh
#
# It is worth the trouble: the `Dockerfile` ships that binary and it does not behave like this
# one. Every result file records its own `provenance.libc`, so the two never get mixed up.
if [ -n "${DAGPANE_MUSL_BIN:-}" ]; then
  echo
  echo "═══ 7. the MUSL build — what the container actually runs ═══"
  # The ladder starts at ONE dragging viewer and that rung is the point of it. Without it the
  # section can say musl is slower and cannot say why: a build that is uniformly 1.7x slower and
  # a build that serialises under concurrency look identical at four dragging viewers and want
  # different fixes. Holding the session count at sixty-five and moving only how many are working
  # is what separates them.
  DAGPANE_BIN="$DAGPANE_MUSL_BIN" DAGPANE_PORT=9140 DAGPANE_TAG=busy-rows-musl \
    DAGPANE_LADDER=0,1,4,64 DAGPANE_REPS=2 \
    node "$HERE/busy-session.mjs" "$WORK/rows/rows.toml"
fi

echo
node - "$HERE/results" <<'JS'
import { readdir, readFile } from "node:fs/promises";
const dir = process.argv[2];
const files = (await readdir(dir)).filter((f) => f.endsWith(".json"));
const newest = async (tag) => {
  const mine = files.filter((f) => f.startsWith(tag + "-2")).sort();
  if (!mine.length) return null;
  return JSON.parse(await readFile(`${dir}/${mine[mine.length - 1]}`, "utf8"));
};
const kb = (b) => (b / 1024).toFixed(0).padStart(9);
const held = { "busy-small": "small", "busy-rows": "rows", "busy-agg": "agg" };

console.log("═══ what a pass costs, and whether it comes back ═══\n");
console.log("  app                          held/session   one pass    ceiling   comes back");
console.log("  ───────────────────────────  ────────────  ─────────  ─────────  ───────────");
for (const [tag, name] of [["busy-small", "600 rows, keeps rows"],
                           ["busy-rows", "200k rows, keeps rows"],
                           ["busy-agg", "200k rows, aggregates only"]]) {
  const b = await newest(tag);
  if (!b) { console.log(`  ${name.padEnd(27)}  (not run)`); continue; }
  const h = await newest(held[tag]);
  const top = b.rungs.reduce((a, c) => (c.transient_bytes > a.transient_bytes ? c : a));
  const back = top.transient_bytes - top.retained_bytes;
  console.log(
    `  ${name.padEnd(27)}  ` +
    `${h ? kb(h.marginal_rss_bytes_per_session) : "        ?"} kB  ` +
    `${kb(b.transient_bytes_one_pass)}kB  ${kb(b.transient_bytes_ceiling)}kB  ` +
    `${kb(back)} kB`
  );
}

const one = await newest("busy-agg-1core");
const four = await newest("busy-agg-4core");
if (one && four) {
  console.log("\n═══ bounded by the viewers, or by the threads? ═══\n");
  console.log("  cores    threads    ceiling   at K   passes/s");
  console.log("  ─────  ─────────  ─────────  ─────  ─────────");
  for (const r of [one, four]) {
    const top = r.rungs.reduce((a, c) => (c.transient_bytes > a.transient_bytes ? c : a));
    const busiest = r.rungs[r.rungs.length - 1];
    console.log(
      `  ${String(r.cores).padStart(5)}  ${String(r.server_threads).padStart(9)}  ` +
      `${kb(r.transient_bytes_ceiling)}kB  ${String(top.busy).padStart(5)}  ` +
      `${busiest.passes_per_second.toFixed(1).padStart(9)}`
    );
  }
}
JS
