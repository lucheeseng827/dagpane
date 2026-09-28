#!/usr/bin/env python3
"""Regenerate every CSV under `examples/data/`.

Standard library only, and seeded: running this twice produces byte-identical files, so a
diff in `data/` is always a deliberate change to this script and never noise. There is no
pandas here on purpose — the generator has to run in the same container as `dagpane check`,
and that container has no package manager.

    python3 examples/tools/generate_data.py

The shape of the data is not incidental. Every dataset carries a low-cardinality column
whose *set of distinct values* survives the filters the app puts on top of it — `channel`,
`status`, `tier`, `pool`. That is what makes the examples teach something: a cell that
computes such a set recomputes when you move a control, produces the value it already had,
and stops the pass there. Widen the distinct values and the examples still run, but they
stop demonstrating the thing they exist to demonstrate.
"""

import csv
import datetime as dt
import pathlib
import random

DATA = pathlib.Path(__file__).resolve().parent.parent / "data"
SEED = 20260817
DAY_ZERO = dt.date(2026, 8, 3)


def day(offset):
    """An ISO date `offset` days after the window start, so every file shares a calendar."""
    return (DAY_ZERO + dt.timedelta(days=offset)).isoformat()


def write(name, header, rows):
    """Write one CSV under `data/` with a fixed line terminator, and report what it wrote."""
    DATA.mkdir(parents=True, exist_ok=True)
    path = DATA / name
    with path.open("w", newline="", encoding="utf-8") as fh:
        w = csv.writer(fh, lineterminator="\n")
        w.writerow(header)
        w.writerows(rows)
    print(f"  {path.relative_to(DATA.parent.parent)}  {len(rows)} rows")


def web_events(rng):
    """Funnel, experiment and cohort apps all read this one."""
    steps = [("visit", 1), ("signup", 2), ("activate", 3), ("subscribe", 4)]
    keep = {"visit": 1.0, "signup": 0.42, "activate": 0.23, "subscribe": 0.11}
    rows = []
    for d in range(28):
        for cohort in ("2026-06", "2026-07", "2026-08"):
            for variant in ("control", "treatment"):
                for device in ("desktop", "mobile", "tablet"):
                    base = rng.randint(120, 420)
                    if device == "tablet":
                        base //= 4
                    if cohort == "2026-08":
                        base = int(base * 1.25)
                    for step, order in steps:
                        lift = 1.0
                        if variant == "treatment" and step in ("activate", "subscribe"):
                            lift = 1.18
                        sessions = int(base * keep[step] * lift * rng.uniform(0.85, 1.15))
                        revenue = round(sessions * rng.uniform(11.0, 19.0), 2) if step == "subscribe" else 0.0
                        rows.append([day(d), cohort, variant, device, step, order, sessions, revenue])
    write(
        "web_events.csv",
        ["day", "cohort", "variant", "device", "step", "step_order", "sessions", "revenue"],
        rows,
    )


def campaigns(rng):
    """`04-campaigns.toml` — paid acquisition, one row per campaign-day."""
    channels = ("search", "social", "email", "partner")
    rows = []
    for d in range(0, 28, 1):
        for channel in channels:
            for n in range(1, 4):
                impressions = rng.randint(4_000, 90_000)
                ctr = {"search": 0.041, "social": 0.013, "email": 0.072, "partner": 0.026}[channel]
                clicks = int(impressions * ctr * rng.uniform(0.7, 1.3))
                spend = round(clicks * rng.uniform(0.35, 2.10), 2)
                signups = int(clicks * rng.uniform(0.03, 0.14))
                rows.append([day(d), channel, f"{channel}-{n:02d}", impressions, clicks, spend, signups])
    write(
        "campaigns.csv",
        ["day", "channel", "campaign", "impressions", "clicks", "spend", "signups"],
        rows,
    )


def pipeline_runs(rng):
    """`05-pipeline-freshness.toml` — scheduled runs, with per-run lateness."""
    pipelines = [
        ("orders_ingest", "platform", 0.6),
        ("orders_enrich", "platform", 1.4),
        ("clickstream_raw", "growth", 2.2),
        ("clickstream_sessions", "growth", 3.9),
        ("finance_ledger", "finance", 0.9),
        ("ml_features", "ml", 5.5),
        ("crm_sync", "growth", 1.1),
        ("inventory_snapshot", "platform", 0.4),
    ]
    rows = []
    run = 1000
    for d in range(21):
        for name, team, lateness in pipelines:
            for _ in range(rng.choice([1, 1, 2])):
                run += 1
                late = round(max(0.0, rng.gauss(lateness, lateness * 0.55)), 2)
                status = "ok"
                if rng.random() < 0.06:
                    status = "failed"
                elif late > lateness * 1.9:
                    status = "late"
                rows.append(
                    [
                        f"run-{run}",
                        day(d),
                        name,
                        team,
                        status,
                        round(rng.uniform(1.5, 48.0), 1),
                        0 if status == "failed" else rng.randint(5_000, 900_000),
                        late,
                    ]
                )
    write(
        "pipeline_runs.csv",
        ["run_id", "day", "pipeline", "owner_team", "status", "duration_min", "rows_out", "hours_late"],
        rows,
    )


def table_health(rng):
    """`06-data-quality.toml` — daily null rate and freshness per warehouse table.

    Carries `missed_sla` as a real boolean column: a checkbox can only gate a filter over one.
    """
    tables = [
        ("dim_customer", "core", 0.4),
        ("fct_order", "core", 0.1),
        ("fct_order_line", "core", 0.2),
        ("dim_product", "core", 1.9),
        ("stg_clickstream", "growth", 6.4),
        ("fct_session", "growth", 3.1),
        ("dim_campaign", "growth", 0.8),
        ("fct_ledger_entry", "finance", 0.3),
        ("dim_account", "finance", 0.2),
        ("feature_store_daily", "ml", 11.2),
    ]
    rows = []
    for d in range(21):
        for name, domain, nulls in tables:
            drift = rng.uniform(0.9, 1.1)
            freshness = round(max(0.1, rng.gauss(4.0, 3.0)), 1)
            rows.append(
                [
                    day(d),
                    name,
                    domain,
                    int(rng.randint(10_000, 4_000_000) * drift),
                    round(max(0.0, rng.gauss(nulls, nulls * 0.4 + 0.1)), 2),
                    freshness,
                    # A fact the loader records, not a threshold this file invents: whether the
                    # load landed inside the window its contract promises. A checkbox in the app
                    # can only toggle a filter cleanly when the column is already a boolean.
                    "true" if freshness > 6.0 else "false",
                    rng.choice([3, 3, 3, 4]),
                ]
            )
    write(
        "table_health.csv",
        ["day", "table_name", "domain", "rows", "null_rate_pct", "freshness_hours", "missed_sla",
         "schema_version"],
        rows,
    )


def warehouse_queries(rng):
    """`07-warehouse-spend.toml` — per-query cost, by team and warehouse size."""
    teams = ("growth", "finance", "ml", "platform", "support")
    warehouses = ("xsmall", "small", "large")
    rows = []
    qid = 0
    for d in range(14):
        for _ in range(140):
            qid += 1
            team = rng.choice(teams)
            wh = rng.choices(warehouses, weights=[6, 3, 1])[0]
            scanned = round(rng.expovariate(1 / 18.0) + 0.05, 3)
            rate = {"xsmall": 0.9, "small": 2.4, "large": 9.1}[wh]
            seconds = round(scanned * rng.uniform(1.4, 6.0) + 0.6, 2)
            rows.append(
                [
                    f"q-{qid:05d}",
                    day(d),
                    team,
                    wh,
                    round(seconds / 3600 * rate, 4),
                    scanned,
                    seconds,
                    f"{team}_svc" if rng.random() < 0.4 else f"analyst_{rng.randint(1, 9)}",
                ]
            )
    write(
        "warehouse_queries.csv",
        ["query_id", "day", "team", "warehouse", "cost_usd", "scanned_gb", "seconds", "run_by"],
        rows,
    )


def column_census(rng):
    """`08-schema-drift.toml` — four weekly column snapshots, with a column that
    appears partway through the window and one that goes away."""
    tables = {
        "fct_order": ["order_id", "account_key", "ordered_at", "amount", "currency", "channel"],
        "dim_account": ["account_key", "account_name", "region", "plan", "created_at"],
        "stg_clickstream": ["event_id", "session_key", "event_at", "path", "referrer", "device"],
        "fct_ledger_entry": ["entry_id", "account_key", "posted_at", "amount", "memo"],
    }
    types = {
        "order_id": "int", "account_key": "int", "entry_id": "int", "event_id": "int",
        "session_key": "int", "amount": "float", "currency": "text", "channel": "text",
        "account_name": "text", "region": "text", "plan": "text", "path": "text",
        "referrer": "text", "device": "text", "memo": "text",
    }
    rows = []
    for d in (0, 7, 14, 21):
        for table, columns in tables.items():
            cols = list(columns)
            # a column that shows up partway through the window, and one that goes away
            if d >= 14 and table == "fct_order":
                cols.append("discount_code")
            if d >= 7 and table == "stg_clickstream":
                cols.append("consent_state")
            if d >= 21 and table == "dim_account":
                cols.remove("plan")
            for column in cols:
                ctype = types.get(column, "text" if column.endswith(("_at", "_code", "_state")) else "text")
                if column.endswith("_at"):
                    ctype = "timestamp"
                rows.append(
                    [
                        day(d),
                        table,
                        column,
                        ctype,
                        "true" if column not in ("order_id", "account_key", "entry_id", "event_id") else "false",
                        rng.randint(50_000, 9_000_000),
                    ]
                )
    write(
        "column_census.csv",
        ["snapshot_day", "table_name", "column_name", "column_type", "nullable", "seen_count"],
        rows,
    )


def deploys(rng):
    """`09-deploys.toml` — deploys per service, with lead time and rollbacks."""
    services = [
        ("checkout", "payments", 0.04),
        ("ledger", "payments", 0.02),
        ("catalog", "retail", 0.07),
        ("search", "retail", 0.09),
        ("identity", "platform", 0.03),
        ("gateway", "platform", 0.05),
        ("notifier", "growth", 0.11),
    ]
    rows = []
    did = 0
    for d in range(28):
        for service, team, fail in services:
            for _ in range(rng.choices([0, 1, 2, 3], weights=[3, 5, 3, 1])[0]):
                did += 1
                rolled_back = rng.random() < fail
                rows.append(
                    [
                        f"dep-{did:04d}",
                        day(d),
                        d,
                        service,
                        team,
                        rng.choice(["production", "production", "production", "staging"]),
                        round(max(0.4, rng.gauss(26.0, 20.0)), 1),
                        round(max(0.5, rng.gauss(7.5, 4.0)), 1),
                        "true" if rolled_back else "false",
                    ]
                )
    write(
        "deploys.csv",
        ["deploy_id", "day", "day_index", "service", "team", "environment", "lead_time_hours",
         "duration_min", "rolled_back"],
        rows,
    )


def service_slo(rng):
    """`10-slo-burn.toml` — daily error-budget burn per service and tier."""
    services = [
        ("checkout", "tier1", 0.0008),
        ("ledger", "tier1", 0.0004),
        ("gateway", "tier1", 0.0011),
        ("catalog", "tier2", 0.0035),
        ("search", "tier2", 0.0061),
        ("identity", "tier1", 0.0006),
        ("notifier", "tier3", 0.0142),
        ("reporting", "tier3", 0.0098),
    ]
    rows = []
    for d in range(28):
        for service, tier, rate in services:
            requests = rng.randint(200_000, 3_400_000)
            spike = 6.0 if rng.random() < 0.05 else 1.0
            errors = int(requests * rate * rng.uniform(0.6, 1.5) * spike)
            objective = {"tier1": 0.999, "tier2": 0.995, "tier3": 0.99}[tier]
            budget = requests * (1 - objective)
            rows.append(
                [
                    day(d),
                    d,
                    service,
                    tier,
                    requests,
                    errors,
                    round(errors / budget * 100, 2),
                    int(max(40, rng.gauss(310, 180))),
                    "true" if (errors / budget > 1.5) else "false",
                ]
            )
    write(
        "service_slo.csv",
        ["day", "day_index", "service", "tier", "requests", "errors", "budget_burn_pct",
         "latency_p99_ms", "paged"],
        rows,
    )


def ci_builds(rng):
    """`11-ci-builds.toml` — build durations and retries, with flakier e2e jobs."""
    jobs = [
        ("build", "core-api", 240), ("unit", "core-api", 420), ("integration", "core-api", 980),
        ("build", "web", 160), ("unit", "web", 210), ("e2e", "web", 1450),
        ("build", "infra", 95), ("plan", "infra", 320),
        ("unit", "ml-svc", 540), ("train-smoke", "ml-svc", 1720),
    ]
    rows = []
    bid = 0
    for d in range(14):
        for job, repo, base in jobs:
            for _ in range(rng.randint(3, 9)):
                bid += 1
                flaky = rng.random() < (0.13 if job in ("e2e", "integration") else 0.03)
                status = "failed" if flaky and rng.random() < 0.6 else "passed"
                retries = rng.randint(1, 3) if flaky else 0
                rows.append(
                    [
                        f"b-{bid:05d}",
                        day(d),
                        d,
                        repo,
                        job,
                        rng.choice(["main", "main", "pr", "pr", "pr"]),
                        int(max(20, rng.gauss(base, base * 0.3))),
                        status,
                        retries,
                    ]
                )
    write(
        "ci_builds.csv",
        ["build_id", "day", "day_index", "repo", "job", "branch", "duration_sec", "status", "retries"],
        rows,
    )


def nodes(rng):
    """`12-capacity.toml` — a node fleet across four pools and three zones."""
    pools = [("general", 0.055), ("memory", 0.121), ("compute", 0.094), ("spot", 0.019)]
    zones = ("a", "b", "c")
    rows = []
    n = 0
    for pool, rate in pools:
        for zone in zones:
            for _ in range(rng.randint(6, 14)):
                n += 1
                capacity = {"general": 58, "memory": 30, "compute": 44, "spot": 58}[pool]
                pods = rng.randint(2, capacity)
                rows.append(
                    [
                        f"node-{pool}-{zone}-{n:03d}",
                        pool,
                        f"eu-west-1{zone}",
                        round(min(99.0, max(3.0, rng.gauss(52, 24))), 1),
                        round(min(99.0, max(8.0, rng.gauss(61, 20))), 1),
                        pods,
                        capacity,
                        rate,
                        "true" if pool == "spot" else "false",
                    ]
                )
    write(
        "nodes.csv",
        ["node", "pool", "zone", "cpu_pct", "mem_pct", "pods", "pod_capacity", "cost_per_hour", "preemptible"],
        rows,
    )


def weekly_metrics(rng):
    """`13-weekly-review.toml` — 26 weeks of revenue and accounts by region and segment."""
    regions = ("north", "south", "east", "west")
    segments = ("smb", "mid-market", "strategic")
    rows = []
    for week in range(1, 27):
        for region in regions:
            for segment in segments:
                base = {"smb": 41_000, "mid-market": 128_000, "strategic": 402_000}[segment]
                growth = 1 + (week / 260.0)
                revenue = round(base * growth * rng.uniform(0.86, 1.14), 2)
                accounts = int({"smb": 420, "mid-market": 96, "strategic": 18}[segment] * rng.uniform(0.9, 1.1))
                rows.append(
                    [
                        f"2026-W{week:02d}",
                        week,
                        region,
                        segment,
                        revenue,
                        accounts,
                        max(0, int(accounts * rng.uniform(0.01, 0.06))),
                        max(0, int(accounts * rng.uniform(0.002, 0.021))),
                        rng.randint(4, 180),
                    ]
                )
    write(
        "weekly_metrics.csv",
        ["week", "week_index", "region", "segment", "revenue", "accounts", "new_accounts",
         "churned_accounts", "support_tickets"],
        rows,
    )


def delivery_items(rng):
    """`14-team-throughput.toml` — delivered work items, with a `completed` boolean
    beside the status text so the checkbox has a column it can filter."""
    teams = ("payments", "retail", "platform", "growth", "ml")
    kinds = ("feature", "bug", "chore", "incident")
    rows = []
    iid = 0
    for week in range(1, 27):
        for team in teams:
            for _ in range(rng.randint(4, 15)):
                iid += 1
                kind = rng.choices(kinds, weights=[5, 4, 3, 1])[0]
                cycle = {"feature": 9.0, "bug": 3.2, "chore": 2.1, "incident": 0.8}[kind]
                status = rng.choices(["done", "in-progress"], weights=[18, 1])[0]
                rows.append(
                    [
                        f"item-{iid:04d}",
                        f"2026-W{week:02d}",
                        team,
                        kind,
                        rng.choice([1, 2, 3, 5, 8]),
                        round(max(0.2, rng.gauss(cycle, cycle * 0.6)), 1),
                        status,
                        "true" if status == "done" else "false",
                    ]
                )
    write(
        "delivery_items.csv",
        ["item_id", "week", "team", "kind", "points", "cycle_time_days", "status", "completed"],
        rows,
    )


def accounts(rng):
    """`15-unit-economics.toml` — 360 accounts with revenue and cost to serve."""
    regions = ("north", "south", "east", "west")
    plans = ("starter", "growth", "scale")
    segments = ("smb", "mid-market", "strategic")
    rows = []
    for n in range(1, 361):
        plan = rng.choices(plans, weights=[5, 3, 2])[0]
        segment = {"starter": "smb", "growth": "mid-market", "scale": "strategic"}[plan]
        monthly = round({"starter": 290, "growth": 1_850, "scale": 9_400}[plan] * rng.uniform(0.7, 1.4), 2)
        users = rng.randint(3, {"starter": 30, "growth": 220, "scale": 1800}[plan])
        rows.append(
            [
                f"acct-{n:04d}",
                rng.choice(regions),
                plan,
                segment,
                monthly,
                round(monthly * rng.uniform(0.04, 0.31), 2),
                round(monthly * rng.uniform(0.09, 0.44), 2),
                users,
                rng.randint(0, 24),
            ]
        )
    write(
        "accounts.csv",
        ["account", "region", "plan", "segment", "monthly_revenue", "support_cost", "infra_cost",
         "active_users", "open_tickets"],
        rows,
    )



def host_metrics(rng):
    """A node agent's scrape, flattened one column per metric — the wide case.

    Every other dataset here is eight or nine columns, which is what a table somebody
    designed looks like. This is what a table nobody designed looks like: an exporter emits
    a metric per core, per device, per interface, per mount, and the warehouse job pivots the
    lot into one row per host per scrape. A hundred and twenty-two columns is not an
    exaggeration of that shape, it is a small example of it.

    It exists so `19-wide-telemetry.toml` has something real to sit on. A dashboard over this
    reads four or five columns; the other hundred and fifteen are carried because dropping
    them would mean somebody deciding, in advance, which metric nobody will ever ask about.
    That is the situation sub-node invalidation is for, and it is not a contrived one.
    """
    header = ["host", "minute"]
    for core in range(8):
        for mode in ("user", "system", "iowait", "idle"):
            header.append(f"cpu{core}_{mode}")
    for m in (
        "mem_total_bytes", "mem_available_bytes", "mem_free_bytes", "mem_cached_bytes",
        "mem_buffers_bytes", "mem_dirty_bytes", "mem_mapped_bytes", "mem_slab_bytes",
        "mem_swap_total_bytes", "mem_swap_free_bytes", "mem_page_faults", "mem_oom_kills",
    ):
        header.append(m)
    for dev in ("sda", "sdb", "sdc", "nvme0", "nvme1", "nvme2"):
        for m in ("read_bytes", "write_bytes", "read_ops", "write_ops"):
            header.append(f"disk_{dev}_{m}")
    for nic in ("eth0", "eth1", "eth2", "lo", "veth0", "veth1"):
        for m in ("rx_bytes", "tx_bytes", "rx_errors", "tx_errors"):
            header.append(f"net_{nic}_{m}")
    for mount in ("root", "var", "tmp", "home", "data", "logs", "cache", "boot"):
        for m in ("used_bytes", "inodes_used"):
            header.append(f"fs_{mount}_{m}")
    for m in (
        "load1", "load5", "load15", "procs_running", "procs_blocked", "ctx_switches",
        "interrupts", "uptime_secs", "fd_open", "fd_max", "tcp_established", "tcp_time_wait",
    ):
        header.append(m)

    hosts = [f"web-{i:02d}" for i in range(1, 13)] + [f"db-{i:02d}" for i in range(1, 6)]
    rows = []
    for host in hosts:
        for minute in range(12):
            row = [host, minute]
            for name in header[2:]:
                if name.startswith("cpu"):
                    row.append(rng.randint(0, 100))
                elif name.startswith("mem_") or name.startswith("fs_"):
                    row.append(rng.randint(1_000_000, 64_000_000_000))
                elif name.startswith("disk_") or name.startswith("net_"):
                    row.append(rng.randint(0, 5_000_000_000))
                elif name.startswith("load"):
                    row.append(round(rng.uniform(0.0, 16.0), 2))
                else:
                    row.append(rng.randint(0, 100_000))
            rows.append(row)
    write("host_metrics.csv", header, rows)


def main():
    """Regenerate every dataset from one seeded RNG, in a fixed order.

    The order matters: the functions share the generator, so reordering them changes every
    file below the change.
    """
    rng = random.Random(SEED)
    print("dagpane: regenerating example data")
    for fn in (
        web_events, campaigns, pipeline_runs, table_health, warehouse_queries, column_census,
        deploys, service_slo, ci_builds, nodes, weekly_metrics, delivery_items, accounts,
        # Appended last on purpose: these functions share one generator, so inserting
        # anywhere above would change every file below the insertion point.
        host_metrics,
    ):
        fn(rng)
    print("done")


if __name__ == "__main__":
    main()
