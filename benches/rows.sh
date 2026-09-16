#!/usr/bin/env bash
# How dagpane's cost scales with row count.
#
# ROADMAP §2 asks one question before the value model can be reconsidered: "the row count
# and column width at which the current `Table` stops being the right answer". This is the
# harness that answers it, and BENCHMARKS.md is the answer.
#
# It drives the shipped binary the way a user does — `dagpane explain` on a manifest — so
# what it measures is the whole path: read the CSV, infer types, build the graph, compute
# every cell, then one interaction on top. No microbenchmark, no harness-only code path.
#
#   ./benches/rows.sh                          # the default sweep
#   ./benches/rows.sh 600 10000 100000         # your own sizes
#   DAGPANE=/path/to/dagpane ./benches/rows.sh # a binary you built elsewhere
#
# Peak RSS needs GNU time (`/usr/bin/time`). Without it the run still produces every
# timing and says the memory column is unavailable rather than guessing at it.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Only a directory this script created is a directory this script may delete. `WORK` is a
# documented knob (`WORK=~/bench ./benches/rows.sh`), and the cleanup at the bottom is an
# `rm -rf` — so the two together would silently destroy whatever the caller pointed at.
if [ -n "${WORK:-}" ]; then
  WORK_IS_OURS=""
else
  WORK="$(mktemp -d)"
  WORK_IS_OURS=1
fi
DAGPANE="${DAGPANE:-$HERE/target/release/dagpane}"
REPEATS="${REPEATS:-3}"
SIZES=("$@")
[ ${#SIZES[@]} -gt 0 ] || SIZES=(600 10000 100000 1000000)

[ -x "$DAGPANE" ] || {
  echo "no dagpane binary at $DAGPANE" >&2
  echo "build it first:  cargo build --release -p dagpane-cli" >&2
  exit 1
}

GNU_TIME=""
if command -v /usr/bin/time >/dev/null 2>&1 && /usr/bin/time -f "%M" true >/dev/null 2>&1; then
  GNU_TIME="/usr/bin/time"
fi

# Deterministic rows: a fixed seed, so two runs on one machine differ only by the
# machine, and the same sweep on another machine is comparing the same bytes.
gen() {
  local n="$1" out="$2"
  awk -v n="$n" 'BEGIN{
    srand(7);
    split("north south east west", R, " ");
    split("partner direct web retail", C, " ");
    print "order_id,day,region,channel,units,amount";
    for (i = 1; i <= n; i++)
      printf "%d,2026-08-%02d,%s,%s,%d,%.2f\n",
        i, int(rand()*28)+1, R[int(rand()*4)+1], C[int(rand()*4)+1],
        int(rand()*50)+1, rand()*994+5;
  }' > "$out"
}

# Median of REPEATS, in milliseconds. The median rather than the best: a dashboard user
# feels the typical interaction, not the luckiest one.
median_ms() {
  local i start end
  local -a runs=()
  for ((i = 0; i < REPEATS; i++)); do
    start=$(date +%s%N)
    "$@" > /dev/null
    end=$(date +%s%N)
    runs+=( $(( (end - start) / 1000000 )) )
  done
  printf '%s\n' "${runs[@]}" | sort -n | awk -v n="$REPEATS" 'NR == int((n+1)/2) { print }'
}

peak_rss_mb() {
  [ -n "$GNU_TIME" ] || { echo "-"; return; }
  local kb
  kb=$($GNU_TIME -f "%M" "$@" 2>&1 >/dev/null | tail -1)
  echo $(( kb / 1024 ))
}

echo "dagpane row-count sweep"
echo "binary   : $DAGPANE"
echo "repeats  : $REPEATS (median reported)"
[ -n "$GNU_TIME" ] || echo "note     : GNU time not found — peak RSS unavailable, timings unaffected"
echo
printf '%10s  %9s  %14s  %15s  %9s\n' ROWS CSV "FIRST RENDER" "+1 INTERACTION" "PEAK RSS"

for n in "${SIZES[@]}"; do
  csv="$WORK/sales_$n.csv"
  toml="$WORK/sales_$n.toml"
  gen "$n" "$csv"
  sed "s|sales\.csv|sales_$n.csv|" "$HERE/examples/sales.toml" > "$toml"

  mb=$(awk -v b="$(wc -c < "$csv")" 'BEGIN{ printf "%.1fM", b/1048576 }')
  first=$(median_ms "$DAGPANE" explain "$toml")
  inter=$(median_ms "$DAGPANE" explain "$toml" --set min_amount=400)
  rss=$(peak_rss_mb "$DAGPANE" explain "$toml")

  printf '%10s  %9s  %12sms  %13sms  %7sMB\n' "$n" "$mb" "$first" "$inter" "$rss"
done

echo
echo "The interaction column is the one to read: it is the whole cost of moving one"
echo "control, which is what a viewer waits for."
[ -n "${WORK_KEEP:-}" ] || [ -z "$WORK_IS_OURS" ] || rm -rf "$WORK"
