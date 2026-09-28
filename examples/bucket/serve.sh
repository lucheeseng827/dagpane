#!/bin/sh
# Serve a dagpane app whose data lives in a cloud bucket, and keep it current.
#
#   ./serve.sh s3://my-bucket/panels/sales app.toml
#   INTERVAL=300 PORT=8787 ./serve.sh gs://my-bucket/panels/sales app.toml
#
# ── What this is working around, stated plainly ────────────────────────────────────────
#
# dagpane loads its sources ONCE, at start-up, and shares them immutably across every
# session. That is not an oversight: it is what makes a hundred viewers a hundred vectors of
# values over one allocation per source, and what makes a pass a pure function of the inputs.
# There is no file watcher and no hot reload.
#
# So "the dashboard follows the bucket" means exactly one thing, and this script is it:
#
#     sync -> compile -> if it changed AND it compiles, swap the snapshot and restart
#
# Three properties fall out of doing it in that order, and they are the reason this is a
# script rather than a cron line calling `aws s3 sync` at the live directory:
#
#   * A SNAPSHOT THAT WILL NOT COMPILE NEVER REACHES A VIEWER. `dagpane check` runs on the
#     staged copy before anything is swapped, so a half-finished upload, a CSV the sync did
#     not deliver, a manifest a colleague broke in the bucket, or a filter naming an input
#     that no longer exists leaves the previous snapshot serving, and says why. Syncing
#     straight into the live directory gets you a dashboard that is already broken by the
#     time anyone notices.
#
#     Know what `check` does NOT cover, because the gate is only worth what it actually
#     rules out. It compiles the manifest and loads the sources; it does not look inside a
#     column. A CSV that still parses but has lost a column the app filters on compiles
#     cleanly here and fails at RUN time — one cell holds the error, the cells below it name
#     that cell, and the rest of the page keeps working (see ADR-0004, errors are values).
#     That is a good failure, but it is not one this gate catches. A CSV that arrives with
#     its header and no rows is not caught either, which is what MIN_ROWS below is for.
#   * AN UNCHANGED BUCKET COSTS NOTHING. The digest gate means a restart only happens when the
#     bytes actually moved. A five-minute poll over a bucket that updates daily restarts once
#     a day, not 288 times.
#   * ROLLBACK IS A SYMLINK. Snapshots are kept by digest; `current` points at the live one.
#     Going back is `ln -sfn` and a restart, with no bucket round trip.
#
# ── What a restart costs ───────────────────────────────────────────────────────────────
#
# A dagpane session IS a connection. Restarting drops every open page; each one reconnects and
# gets a fresh first render. Nobody loses work — there is no work to lose, only control
# positions, which reset to their manifest defaults. Set INTERVAL against how often the data
# genuinely moves, not as fast as the bucket will answer.
#
# ── No authentication ──────────────────────────────────────────────────────────────────
#
# There is none in this version. HOST defaults to loopback. If you set it to 0.0.0.0 to put
# this in a container, the thing in front of it has to authenticate, or the bucket's contents
# are readable by anyone who can route to the port. See SECURITY.md.
set -eu

SRC="${1:?usage: serve.sh <bucket-uri> [manifest-name]}"
APP="${2:-app.toml}"

WORK="${WORK:-./.dagpane-bucket}"
INTERVAL="${INTERVAL:-300}"
PORT="${PORT:-8787}"
HOST="${HOST:-127.0.0.1}"
KEEP="${KEEP:-5}"
DAGPANE="${DAGPANE:-dagpane}"
# Refuse a snapshot whose smallest CSV has fewer than this many data rows. 0 disables it.
# This is the guard for the failure `check` cannot see: an upstream job that half-ran and
# published a header with nothing under it. A manifest over an empty table compiles, serves,
# and shows a page full of confident zeroes.
MIN_ROWS="${MIN_ROWS:-0}"

STAGE="$WORK/stage"
SNAPS="$WORK/snapshots"
CURRENT="$WORK/current"
HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

mkdir -p "$STAGE" "$SNAPS"

log() { printf '%s  %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"; }

# A digest of the staged tree's CONTENT — names, sizes and bytes. Cheap, and it does not
# depend on mtimes, which a sync tool is free to rewrite on an identical file.
digest_of() {
  find "$1" -type f ! -name '.*' -exec sha256sum {} + 2>/dev/null \
    | sed "s| $1/| |" | LC_ALL=C sort | sha256sum | cut -c1-16
}

server_pid=""
stop_server() {
  [ -n "$server_pid" ] || return 0
  kill "$server_pid" 2>/dev/null || true
  wait "$server_pid" 2>/dev/null || true
  server_pid=""
}
start_server() {
  "$DAGPANE" run "$CURRENT/$APP" --port "$PORT" --host "$HOST" &
  server_pid=$!
  log "serving snapshot $(basename "$(readlink "$CURRENT")") on http://$HOST:$PORT (pid $server_pid)"
}
trap 'log "stopping"; stop_server; exit 0' INT TERM

live=""
while :; do
  if ! "$HERE/sync.sh" "$SRC" "$STAGE"; then
    log "sync failed; leaving the running snapshot in place"
  else
    staged=$(digest_of "$STAGE")

    if [ "$staged" = "$live" ]; then
      log "bucket unchanged ($staged) — no restart"
    elif [ ! -f "$STAGE/$APP" ]; then
      log "REFUSING $staged: no $APP in the synced prefix; keeping ${live:-nothing}"
    elif [ "$MIN_ROWS" -gt 0 ] && thin=$(find "$STAGE" -name '*.csv' -exec sh -c \
           'n=$(($(wc -l < "$1") - 1)); [ "$n" -lt "$2" ] && echo "$(basename "$1"):$n"' _ {} "$MIN_ROWS" \; \
         ) && [ -n "$thin" ]; then
      log "REFUSING $staged: under MIN_ROWS=$MIN_ROWS — $thin; keeping ${live:-nothing}"
    elif ! "$DAGPANE" check "$STAGE/$APP"; then
      # The whole point of the order. The bad snapshot stops here.
      log "REFUSING $staged: it does not compile (see above); keeping ${live:-nothing}"
    else
      dest="$SNAPS/$staged"
      rm -rf "$dest"
      cp -R "$STAGE" "$dest"
      ln -sfn "$(CDPATH= cd -- "$dest" && pwd)" "$CURRENT"
      log "promoted $staged"

      stop_server
      start_server
      live="$staged"

      # Keep the last few for rollback: `ln -sfn snapshots/<digest> current` and restart.
      #
      # Both sides are resolved with `cd && pwd` before comparing. They are not the same
      # string otherwise: the symlink stores an absolute path while `ls` prints whatever
      # $SNAPS is, which is relative under the default $WORK — so a naive comparison never
      # matches and the loop deletes the snapshot it is serving. Only reachable at KEEP=0,
      # because the live snapshot is the newest and `tail` otherwise never reaches it.
      # shellcheck disable=SC2012
      # `pwd -P`, not `pwd`: plain `pwd` prints the LOGICAL path, so cd-ing into the
      # `current` symlink and asking where you are answers ".../current" rather than the
      # snapshot it points at, and the comparison misses again.
      live_abs=$(CDPATH= cd -- "$CURRENT" 2>/dev/null && pwd -P) || live_abs=""
      ls -1dt "$SNAPS"/*/ 2>/dev/null | tail -n "+$((KEEP + 1))" | while read -r old; do
        old_abs=$(CDPATH= cd -- "$old" 2>/dev/null && pwd -P) || continue
        [ "$old_abs" = "$live_abs" ] && continue
        rm -rf "$old"
      done
    fi
  fi

  # If the server died on its own, bring it back rather than leaving a quiet hole.
  if [ -n "$server_pid" ] && ! kill -0 "$server_pid" 2>/dev/null; then
    log "server exited unexpectedly; restarting it"
    server_pid=""
    start_server
  fi

  sleep "$INTERVAL" &
  wait $! || true
done
