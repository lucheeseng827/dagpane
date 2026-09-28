#!/usr/bin/env bash
# Apps-per-core, on a core.
#
# Starts `dagpane host` PINNED to a named CPU, deploys N apps into it, and ramps the
# protocol-level generator until p99 leaves the budget. The pinning is the point: "per core"
# is not a figure of speech, and a server free to use every core on the box produces a number
# that divides by a denominator nobody measured.
#
#   ./benches/loadgen/apps-per-core.sh
#   SERVER_CPU=0 GEN_CPUS=1-3 APPS=256 RAMP=1,4,16,64,256 ./benches/loadgen/apps-per-core.sh
#
# The generator is pinned to the OTHER cores. A load generator sharing a core with the server
# is measuring the two of them fighting.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
MODULE="$(cd "$HERE/../.." && pwd)"

SERVER_CPU="${SERVER_CPU:-0}"
GEN_CPUS="${GEN_CPUS:-1-$(( $(nproc) - 1 ))}"

# The defaults are disjoint — CPU 0 for the server, 1..n-1 for the generator — and that is
# the whole point of this script: the generator must not fight the thing it is measuring.
# Overriding one of them alone breaks it silently. `SERVER_CPU=2` with the default `GEN_CPUS`
# pins both to CPU 2, the run completes, prints a ladder, and the number is a fiction. So it
# is checked rather than trusted.
cpu_list() {
    local spec="$1" part lo hi i out=""
    local -a parts
    IFS=',' read -ra parts <<< "$spec"
    for part in "${parts[@]}"; do
        if [[ "$part" == *-* ]]; then
            lo="${part%%-*}"
            hi="${part##*-}"
            for (( i = lo; i <= hi; i++ )); do out+="$i "; done
        else
            out+="$part "
        fi
    done
    printf '%s' "$out"
}

overlap=""
for c in $(cpu_list "$SERVER_CPU"); do
    for g in $(cpu_list "$GEN_CPUS"); do
        [ "$c" = "$g" ] && overlap="$overlap $c"
    done
done
if [ -n "$overlap" ]; then
    echo "SERVER_CPU=$SERVER_CPU and GEN_CPUS=$GEN_CPUS share CPU(s):$overlap" >&2
    echo "The generator would compete with the server it is measuring, so \"per core\" would" >&2
    echo "mean nothing and the ladder would still print a number. Set both, disjointly —" >&2
    echo "for example SERVER_CPU=2 GEN_CPUS=0,1,3" >&2
    exit 2
fi
APPS="${APPS:-128}"
RAMP="${RAMP:-1,2,4,8,16,32,64,128}"
VIEWERS="${VIEWERS:-2}"
ROUNDS="${ROUNDS:-40}"
THINK_MS="${THINK_MS:-200}"
BUDGET_MS="${BUDGET_MS:-250}"
PORT="${PORT:-19200}"
OUT="${OUT:-$HERE/results/apps-per-core-$(hostname)-$(date -u +%Y%m%d-%H%M%S).json}"

server="$MODULE/target/release/dagpane"
gen="$HERE/target/release/dagpane-loadgen"
[ -x "$server" ] || { echo "build it: cargo build --release -p dagpane-cli"; exit 1; }
[ -x "$gen" ] || { echo "build it: cargo build --release --manifest-path benches/loadgen/Cargo.toml"; exit 1; }

fleet="$(mktemp -d)"
trap 'kill "${server_pid:-}" 2>/dev/null || true; rm -rf "$fleet"' EXIT

cp "$MODULE/benches/fleet/apps/sales.csv" "$fleet/"
for i in $(seq 1 "$APPS"); do
  # A different title per app, so these are N genuinely different apps. `AppKey` is
  # (app id, manifest digest), and N copies of identical bytes under one id would be one
  # compiled graph — which would measure the map and not the apps.
  sed "s/title = \"Fleet benchmark\"/title = \"Fleet benchmark $i\"/" \
    "$MODULE/benches/fleet/apps/sales.toml" > "$fleet/a$i.toml"
done

mkdir -p "$(dirname "$OUT")"
echo "server: cpu $SERVER_CPU, $APPS apps, port $PORT"
echo "generator: cpus $GEN_CPUS, $VIEWERS viewer(s) per app, ${THINK_MS}ms think, p99 budget ${BUDGET_MS}ms"
echo

taskset -c "$SERVER_CPU" "$server" host "$fleet" --port "$PORT" --budget-mb 2048 >/dev/null 2>&1 &
server_pid=$!

for _ in $(seq 1 100); do
  if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then exec 3<&- 3>&-; break; fi
  sleep 0.2
done

server_cores="$(taskset -c "$SERVER_CPU" nproc)"
taskset -c "$GEN_CPUS" "$gen" \
  --addr "127.0.0.1:$PORT" \
  --host-pattern 'a{i}.localhost' \
  --ramp "$RAMP" \
  --viewers "$VIEWERS" \
  --rounds "$ROUNDS" \
  --think-ms "$THINK_MS" \
  --budget-ms "$BUDGET_MS" \
  --server-cores "$server_cores" \
  --csv "$MODULE/benches/fleet/apps/sales.csv" \
  --json "$OUT"

echo
echo "server resident at the end:"
grep -E '^(VmRSS|VmHWM):' "/proc/$server_pid/status" | sed 's/^/  /'
