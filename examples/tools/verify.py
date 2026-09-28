#!/usr/bin/env python3
"""Compile every example app and assert that one interaction on it is actually cheap.

    python3 examples/tools/verify.py                  # uses ./target/release/dagpane
    DAGPANE=/usr/local/bin/dagpane python3 examples/tools/verify.py

This is a gate, not a demo. `dagpane check` alone proves an app compiles; it does not prove
the app is worth building this way. So for each app this runs one representative interaction
through `dagpane explain --json` and asserts both halves of the claim the examples are here
to make:

  * something was SKIPPED — at least one cell downstream of nothing that moved was never
    looked at, and fewer panes went on the wire than the app has;
  * something was SHOWN  — at least one pane did go on the wire, because an interaction that
    changes nothing visible is a broken example, not an efficient one.

An app that fails the first assertion is one where every cell hangs off every control, which
is a page that would have been honest as a full rerun. An app that fails the second has a
control wired to nothing. Both are example bugs, and both are silent without this script.

Exits non-zero on the first failure, so it can be a CI step.
"""

import json
import os
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
APPS = ROOT / "apps"
DAGPANE = os.environ.get("DAGPANE") or str(ROOT.parent / "target" / "release" / "dagpane")

# One representative interaction per app: the control a reader of the guide would reach for
# first. Keep these in step with the `[[input]]` names — a typo here is a hard error from the
# binary rather than a silently skipped check.
INTERACTIONS = {
    "01-funnel.toml": ["device=mobile"],
    "02-experiment.toml": ["cohort=2026-07"],
    "03-cohort.toml": ["funnel_step=subscribe"],
    "04-campaigns.toml": ["min_spend=400"],
    "05-pipeline-freshness.toml": ["late_after_hours=4"],
    "06-data-quality.toml": ["max_null_pct=2"],
    "07-warehouse-spend.toml": ["min_scanned_gb=10"],
    "08-schema-drift.toml": ["name_like=account"],
    "09-deploys.toml": ["environment=staging"],
    "10-slo-burn.toml": ["min_burn_pct=50"],
    "11-ci-builds.toml": ["min_seconds=300"],
    "12-capacity.toml": ["cpu_ceiling=60"],
    "13-weekly-review.toml": ["region=north"],
    "14-team-throughput.toml": ["kind=incident"],
    "15-unit-economics.toml": ["plan=scale"],
    # The margin floor rather than the plan, deliberately: it is the interaction that shows
    # the per-row arithmetic staying put while the judgement over it moves.
    "16-margins.toml": ["target_margin=70"],
    # The deploy side rather than the SLO side: it is the interaction that shows one control
    # reaching one half of a join and provably not the other.
    "17-service-risk.toml": ["environment=staging"],
    # The deploy side again, and for the same reason: it is the control that reaches one half
    # of a join written in SQL and provably not the other.
    "18-team-load.toml": ["environment=staging"],
    "19-wide-telemetry.toml": ["busy_cpu=40"],
    # Placed, and checked here for the same reason as every other app: a cut changes WHERE
    # cells run and must not change WHAT they compute. `dagpane explain` runs the whole graph
    # in one process whatever the placement says, so these counts are the undivided ones —
    # which is the point. If a `place` line ever started changing them, this row would move.
    "20-placed.toml": ["min_amount=400"],
    # The app every number in the top-level README is measured on. Included so a change to
    # the engine that breaks the headline example fails here too.
    "../sales.toml": ["min_amount=400"],
}


def run(args):
    """Run `dagpane` with `args`; return stdout, or None having printed why it failed."""
    proc = subprocess.run([DAGPANE, *args], capture_output=True, text=True)
    if proc.returncode != 0:
        print(f"    ! dagpane {' '.join(args)} exited {proc.returncode}")
        print("     ", (proc.stderr or proc.stdout).strip().replace("\n", "\n      "))
        return None
    return proc.stdout


def main():
    """Check and probe every app, print the table, and return a non-zero exit on any failure."""
    if not pathlib.Path(DAGPANE).exists():
        sys.exit(
            f"no dagpane binary at {DAGPANE}\n"
            "build one with `cargo build --release -p dagpane-cli`, or point DAGPANE at yours"
        )

    print(f"dagpane: verifying {len(INTERACTIONS)} example apps with {DAGPANE}\n")
    print(f"  {'app':<28} {'cells':>5} {'ran':>4} {'skipped':>8} {'panes':>6} {'sent':>5}")
    print(f"  {'-' * 28} {'-' * 5} {'-' * 4} {'-' * 8} {'-' * 6} {'-' * 5}")

    failures = []
    for name, sets in INTERACTIONS.items():
        app = APPS / name
        label = pathlib.Path(name).name

        if run(["check", str(app)]) is None:
            failures.append(f"{label}: check failed")
            continue

        args = ["explain", str(app), "--json"]
        for s in sets:
            args += ["--set", s]
        out = run(args)
        if out is None:
            failures.append(f"{label}: explain failed")
            continue

        trace = json.loads(out)
        interaction = trace["interaction"]
        cells = interaction["trace"]["total_cells"]
        ran = sum(1 for s in interaction["trace"]["steps"] if s["outcome"] == "evaluated")
        untouched = len(interaction["untouched"])
        panes_total = interaction["panes_total"]
        panes_sent = len(interaction["panes_sent"])

        print(f"  {label:<28} {cells:>5} {ran:>4} {untouched:>8} {panes_total:>6} {panes_sent:>5}")

        if untouched == 0:
            failures.append(f"{label}: every cell was visited — nothing is fenced off from the controls")
        if panes_sent >= panes_total:
            failures.append(f"{label}: all {panes_total} panes repainted — the graph bought nothing")
        if panes_sent == 0:
            failures.append(f"{label}: no pane changed — `{sets[0]}` is wired to nothing")

    print()
    if failures:
        print("FAILED")
        for f in failures:
            print(f"  - {f}")
        return 1
    print(f"ok — {len(INTERACTIONS)} apps compile, and every one of them skipped work.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
