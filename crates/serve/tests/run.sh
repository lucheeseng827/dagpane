#!/usr/bin/env bash
# Drive the bundled client in a real browser.
#
# `cargo test -p dagpane-serve` tests the socket, and `tests/heartbeat.rs` tests the keepalive
# on it. Neither can test the half of this that lives in `client.html` — what a *browser* does
# when a socket closes — and until this script existed nothing in the repository had ever
# executed that code. The static check in `lib.rs` greps the file; it does not run it.
#
# What this asserts is the property a rolling update turns on: a viewer whose replica goes
# away gets their page back without touching it. See the header of `reconnect.mjs`.
#
#   ./crates/serve/tests/run.sh
#
# Chromium over the DevTools Protocol, driven from bare Node. **No npm dependency**: Node 22
# has a WebSocket and the browser is whichever one the machine already has. That is the same
# rule the wasm smoke test follows, and the reason both can run on a clean checkout.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$HERE"

command -v node >/dev/null || { echo "node is required to drive the browser" >&2; exit 1; }

# Find a Chromium. `DAGPANE_CHROME` wins; then the Playwright layout a CI image usually has;
# then whatever is on PATH. Skipping rather than failing when there is none is deliberate —
# this is the one test in the tree that needs a browser, and a contributor without one should
# still be able to run everything else and see that they did.
CHROME="${DAGPANE_CHROME:-}"
# Checked, not trusted: an explicit path that is not there should say so here rather than as a
# spawn failure inside the test, where it reads like the browser crashed.
if [ -n "$CHROME" ] && [ ! -x "$CHROME" ]; then
  echo "DAGPANE_CHROME=$CHROME is not an executable" >&2
  exit 1
fi
if [ -z "$CHROME" ]; then
  for candidate in \
    /opt/pw-browsers/chromium-*/chrome-linux/chrome \
    "$(command -v chromium || true)" \
    "$(command -v chromium-browser || true)" \
    "$(command -v google-chrome || true)"
  do
    if [ -n "$candidate" ] && [ -x "$candidate" ]; then CHROME="$candidate"; break; fi
  done
fi
if [ -z "$CHROME" ]; then
  # A skip is the right answer for a contributor who has no browser and wants to run
  # everything else. It is the WRONG answer for CI, where a step that skips itself is a test
  # that is not running behind a green tick. So CI sets this and gets a failure instead.
  if [ -n "${DAGPANE_REQUIRE_BROWSER:-}" ]; then
    echo "no chromium found, and DAGPANE_REQUIRE_BROWSER is set — failing rather than" >&2
    echo "pretending this ran. Install one, or point DAGPANE_CHROME at it." >&2
    exit 1
  fi
  echo "no chromium found — set DAGPANE_CHROME to one. Skipping the browser test." >&2
  exit 0
fi
echo "browser: $CHROME"

# Honour CARGO_TARGET_DIR, for the same reason the wasm runner does: the mirror guard sets it
# outside the staged tree, and a script that hardcoded `target/` would read nothing on a clean
# runner or a stale artefact on a machine that had one.
TARGET="${CARGO_TARGET_DIR:-target}"
BIN="$TARGET/release/dagpane"

# ALWAYS build, never "build if missing". `client.html` is `include_str!`d into the binary, so
# an existing one carries whatever the page looked like when it was compiled — and a test that
# drives a stale page passes or fails for the wrong reason. This cost an hour: a fix to the
# reconnect logic was tested against a binary compiled before it. Cargo is incremental; a
# no-op build is a second.
cargo build --release -p dagpane-cli

exec node crates/serve/tests/reconnect.mjs "$BIN" "$CHROME"
