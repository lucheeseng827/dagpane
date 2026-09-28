#!/usr/bin/env python3
"""The fleet benchmark: apps-per-core, and what an interaction costs under load.

This is the rig `ROADMAP.md` §7 asks for. It measures **the same app, in three runtimes, on
the same machine, driven by the same browser**, and it is arranged so that the parts most
likely to flatter one of them are the parts it refuses to do.

    python3 benches/fleet/fleet.py --runtime dagpane --apps 8 --viewers 2
    python3 benches/fleet/fleet.py --all --apps 1,2,4,8 --viewers 2

What it measures
----------------

**Resident cost per app.** Deploy N apps, open one live session on each — a session, not an
idle process, because a Streamlit process that nobody has connected to has not run the
script yet and its memory says so. Sample the whole server process tree. The slope of bytes
against N is the marginal cost of one more app; the intercept is what the runtime costs
before it holds anything.

**Interaction latency.** Set the control to a value, wait until the number on screen is the
number the oracle says it should be, report the wall time. Identical semantics in all three:
*move the control, wait for the page to tell the truth.*

The oracle is computed from the CSV by this script, independently of all three runtimes, so
a runtime that renders a wrong number **fails the benchmark** rather than scoring well on it.
That has caught nothing so far. It is here because a performance harness that cannot tell a
fast wrong answer from a fast right one is a harness that eventually publishes one.

What it refuses to do
---------------------

**No instrumentation in the apps.** The driver reads the real rendered metric through each
runtime's own selectors, pinned below. A probe element would be stable across versions and
would also mean measuring something the user never sees. When a runtime's markup changes the
harness fails loudly at start-up -- see `assert_selectors` -- rather than silently measuring
the wrong node.

**No timing inside the page.** `performance.now()` around the interaction would shave the
driver's round trip off the number. It would also shave a different amount off each runtime.
The measurement is wall time in the driver, the driver's own floor is measured and reported
beside it (`driver_floor_ms`), and that floor is the same for all three.

**No claim when the driver is the bottleneck.** A browser per viewer is expensive, and a load
generator that saturates before the server does produces numbers about itself. Every run
reports the driver's own CPU share and how late it dispatched against its own schedule; past
the thresholds below the result is marked `saturated` and every latency in it is a lower
bound on what the server could have done.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import platform
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

HERE = Path(__file__).resolve().parent
APPS = HERE / "apps"
MODULE = HERE.parent.parent

# The driver is a browser, and the browser in an environment may not be the one the installed
# Playwright expects. Honoured if set; otherwise Playwright's own default.
CHROMIUM = os.environ.get("DAGPANE_BENCH_CHROMIUM")

# Past either of these the driver is a meaningful part of what is being timed, and the run
# says so rather than publishing a number about itself. Both are judgement calls and both are
# recorded in the result file so a reader can apply their own.
SATURATION_CPU_SHARE = 0.80
SATURATION_LATENESS_MS = 50.0


# -- the app, and the truth about it -------------------------------------------------------


def oracle(floor: float) -> tuple[str, str]:
    """What `revenue` and `orders` must read at this control setting.

    Computed from the CSV with the standard library -- no pandas, so the oracle does not
    share a dependency with two of the three runtimes it judges.
    """
    import csv as _csv

    total = 0.0
    count = 0
    with (APPS / "sales.csv").open() as fh:
        for row in _csv.DictReader(fh):
            amount = float(row["amount"])
            if amount >= floor:
                total += amount
                count += 1
    return f"{total:.2f}", str(count)


def normalise(text: str) -> str:
    """Strip presentation so three runtimes' numbers can be compared as numbers.

    dagpane renders `131255.70` and the two Python runtimes render `131,255.70`. That is a
    formatting difference and not a correctness one, and a harness that failed on it would be
    asserting that three teams chose the same thousands separator.
    """
    return text.replace(",", "").replace(" ", "").strip()


# The same normalisation, in the page. Kept as one string used by both waits below, because
# two copies of a comparison is two chances for the wait and the assertion to disagree.
MATCH_JS = """([sel, read, want]) => {
     const els = [...document.querySelectorAll(sel)];
     if (els.length < 2) return false;
     const text = e => (read === 'text' ? e.innerText : (e.getAttribute(read) || ''))
                         .replace(/[",]/g, '').trim();
     return text(els[0]) === want;
   }"""


# -- the three runtimes --------------------------------------------------------------------


@dataclass
class Runtime:
    """How to start one app, and where its control and its numbers are on screen.

    The selectors are pinned rather than discovered, and `assert_selectors` checks every one
    of them against a live app before a run starts. A silent selector change is the failure
    mode that turns this whole file into a random number generator.
    """

    name: str
    #: argv for one app on one port.
    command: Callable[[int], list[str]]
    #: CSS for the control the driver sets.
    control: str
    #: CSS for the elements holding the rendered numbers, in page order.
    metrics: str
    #: How to read one metric element's text. marimo puts it in an attribute.
    read: str = "text"
    #: Seconds to wait for a first render before giving up on an app.
    boot_timeout: float = 180.0
    #: One process per app (Streamlit, marimo, `dagpane run`), or one process for the whole
    #: fleet (`dagpane host`)? This is the axis the whole benchmark exists to measure, so it
    #: is a property of the runtime rather than a flag on the run.
    one_process: bool = False
    #: Everything the fleet needs on disk before it starts, given a scratch directory and an
    #: app count. Returns extra argv for the single process, when there is one.
    prepare: Callable[[Path, int], list[str]] | None = None


def venv_bin(name: str) -> str:
    """`streamlit`/`marimo` from a virtualenv if one is pointed at, else from PATH."""
    for candidate in (
        Path(sys.prefix) / "bin" / name,
        Path(os.environ.get("DAGPANE_BENCH_VENV", "/nonexistent")) / "bin" / name,
    ):
        if candidate.exists():
            return str(candidate)
    found = shutil.which(name)
    if not found:
        sys.exit(f"{name} is not installed; see benches/fleet/README.md")
    return found


def write_host_fleet(scratch: Path, apps: int) -> list[str]:
    """N manifests in one directory, for `dagpane host` to serve from one process.

    Each carries a different title, so they are N genuinely different apps rather than one
    app N times — `AppKey` is (app id, manifest digest), and N copies of identical bytes
    under one id would be one compiled graph and would measure nothing.
    """
    shutil.copy(APPS / "sales.csv", scratch / "sales.csv")
    template = (APPS / "sales.toml").read_text()
    for i in range(1, apps + 1):
        (scratch / f"a{i}.toml").write_text(
            template.replace('title = "Fleet benchmark"', f'title = "Fleet benchmark {i}"')
        )
    return []


RUNTIMES: dict[str, Runtime] = {
    "dagpane": Runtime(
        name="dagpane",
        command=lambda port: [
            str(MODULE / "target/release/dagpane"),
            "run",
            str(APPS / "sales.toml"),
            "--port",
            str(port),
        ],
        control='input[type="number"]',
        metrics=".metric-value",
    ),
    # The same binary, the other way round: N apps in ONE process, which is what
    # `crates/host` exists for and what the product's hosting claim is actually about.
    # Routed by the first label of the Host header, so the driver visits `a3.localhost:PORT`
    # and the browser resolves every `*.localhost` to loopback without a hosts file.
    "dagpane-host": Runtime(
        name="dagpane-host",
        command=lambda port: [
            str(MODULE / "target/release/dagpane"),
            "host",
            "__SCRATCH__",
            "--port",
            str(port),
        ],
        control='input[type="number"]',
        metrics=".metric-value",
        one_process=True,
        prepare=lambda scratch, apps: write_host_fleet(scratch, apps),
    ),
    "streamlit": Runtime(
        name="streamlit",
        command=lambda port: [
            venv_bin("streamlit"),
            "run",
            str(APPS / "streamlit_app.py"),
            "--server.port",
            str(port),
            "--server.headless",
            "true",
            "--browser.gatherUsageStats",
            "false",
        ],
        control='[data-testid="stNumberInput"] input',
        metrics='[data-testid="stMetricValue"]',
    ),
    "marimo": Runtime(
        name="marimo",
        command=lambda port: [
            venv_bin("marimo"),
            "run",
            str(APPS / "marimo_app.py"),
            "--port",
            str(port),
            "--headless",
            "--no-token",
        ],
        control="marimo-number input",
        metrics="marimo-stat",
        read="data-value",
    ),
}


# -- memory, as the kernel accounts for it -------------------------------------------------


def tree_pids(root: int) -> list[int]:
    """`root` and every descendant of it, now.

    Walked from /proc rather than remembered from `Popen`, because Streamlit and marimo both
    fork workers and a benchmark that counted only the process it launched would report a
    fraction of what the fleet costs.
    """
    children: dict[int, list[int]] = {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            stat = (entry / "stat").read_text()
        except OSError:
            continue
        # The command field can contain spaces and parentheses, so ppid is found relative to
        # the LAST `)`. Splitting the whole line mis-parses a process called `(Web Content)`.
        after = stat[stat.rfind(")") + 2 :].split()
        children.setdefault(int(after[1]), []).append(int(entry.name))

    out, stack = [], [root]
    while stack:
        pid = stack.pop()
        out.append(pid)
        stack.extend(children.get(pid, []))
    return out


def memory_of(pids: list[int]) -> tuple[int, int]:
    """(sum of RSS, sum of PSS) in bytes, over these processes.

    **Both, because they disagree and the disagreement is the finding.** Summing RSS across a
    fleet of Python processes counts every shared page of libpython, pandas and numpy once
    per process -- so it overstates what the fleet actually costs the machine, and overstates
    it more the more apps there are. PSS divides each shared page by the number of processes
    mapping it, and is the number that answers "how much of this machine is gone".

    P2's criterion says RSS, so RSS is reported. PSS is reported beside it, and where the two
    differ PSS is the one an apps-per-core figure should use. Publishing both is cheaper than
    arguing about which, and it lets a reader recompute either.
    """
    rss = pss = 0
    for pid in pids:
        try:
            for line in (Path("/proc") / str(pid) / "smaps_rollup").read_text().splitlines():
                if line.startswith("Rss:"):
                    rss += int(line.split()[1]) * 1024
                elif line.startswith("Pss:"):
                    pss += int(line.split()[1]) * 1024
        except OSError:
            # The process exited between the walk and the read. Skipped rather than retried:
            # a sample is a moment, and a fleet that is losing processes is a finding the
            # boot check will already have reported.
            continue
    return rss, pss


def cpu_of(pids: list[int]) -> float:
    """Total CPU seconds these processes have used."""
    ticks = os.sysconf("SC_CLK_TCK")
    total = 0.0
    for pid in pids:
        try:
            stat = (Path("/proc") / str(pid) / "stat").read_text()
        except OSError:
            continue
        after = stat[stat.rfind(")") + 2 :].split()
        total += (int(after[11]) + int(after[12])) / ticks  # utime + stime
    return total


# -- the fleet -----------------------------------------------------------------------------


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@dataclass
class Fleet:
    """N apps, however this runtime prefers to arrange them.

    The two arrangements are the measurement, not an implementation detail: Streamlit,
    marimo and `dagpane run` put one app in one process, and `dagpane host` puts N apps in
    one. Everything downstream — URLs, memory sampling, teardown — works the same either way,
    which is what makes the comparison a comparison.
    """

    runtime: Runtime
    procs: list[subprocess.Popen] = field(default_factory=list)
    ports: list[int] = field(default_factory=list)
    urls: list[str] = field(default_factory=list)
    scratch: Path | None = None

    def start(self, apps: int) -> None:
        if self.runtime.one_process:
            self.scratch = Path(tempfile.mkdtemp(prefix="dagpane-fleet-"))
            if self.runtime.prepare:
                self.runtime.prepare(self.scratch, apps)
            port = free_port()
            argv = [
                str(self.scratch) if a == "__SCRATCH__" else a
                for a in self.runtime.command(port)
            ]
            self.ports.append(port)
            self.procs.append(self._spawn(argv))
            # `*.localhost` resolves to loopback in every browser this drives, so N apps on
            # one port need no hosts file and no proxy.
            self.urls = [f"http://a{i}.localhost:{port}/" for i in range(1, apps + 1)]
            return

        for _ in range(apps):
            port = free_port()
            self.ports.append(port)
            self.procs.append(self._spawn(self.runtime.command(port)))
        self.urls = [f"http://127.0.0.1:{port}/" for port in self.ports]

    def _spawn(self, argv: list[str]) -> subprocess.Popen:
        return subprocess.Popen(
            argv,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            # Its own group, so stopping the fleet stops the workers it forked.
            start_new_session=True,
        )

    def wait_listening(self) -> None:
        deadline = time.monotonic() + self.runtime.boot_timeout
        for port in self.ports:
            while True:
                if time.monotonic() > deadline:
                    raise SystemExit(f"{self.runtime.name}: port {port} never opened")
                try:
                    with socket.create_connection(("127.0.0.1", port), timeout=1):
                        break
                except OSError:
                    time.sleep(0.2)

    def pids(self) -> list[int]:
        out: list[int] = []
        for proc in self.procs:
            out.extend(tree_pids(proc.pid))
        return out

    def stop(self) -> None:
        for proc in self.procs:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
            except OSError:
                pass
        deadline = time.monotonic() + 10
        for proc in self.procs:
            try:
                proc.wait(timeout=max(0.1, deadline - time.monotonic()))
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
                except OSError:
                    pass
        self.procs.clear()
        self.ports.clear()
        self.urls.clear()
        if self.scratch:
            shutil.rmtree(self.scratch, ignore_errors=True)
            self.scratch = None


# -- driving -------------------------------------------------------------------------------


async def read_metrics(page, runtime: Runtime) -> list[str]:
    if runtime.read == "text":
        return [normalise(t) for t in await page.locator(runtime.metrics).all_inner_texts()]
    values = await page.eval_on_selector_all(
        runtime.metrics, f"els => els.map(e => e.getAttribute({runtime.read!r}))"
    )
    # marimo's attribute holds a JSON string, quotes included.
    return [normalise(json.loads(v) if v and v.startswith('"') else (v or "")) for v in values]


async def open_session(browser, runtime: Runtime, url: str):
    """A page with the app loaded and its first render on screen."""
    page = await browser.new_page()
    await page.goto(url, wait_until="domcontentloaded")
    want_revenue, _ = oracle(0.0)
    await page.wait_for_function(
        MATCH_JS,
        arg=[runtime.metrics, runtime.read, want_revenue],
        timeout=runtime.boot_timeout * 1000,
    )
    return page


async def interact(page, runtime: Runtime, floor: float) -> float:
    """Set the control and wait until the page tells the truth. Returns milliseconds."""
    want_revenue, _ = oracle(floor)
    started = time.perf_counter()
    await page.fill(runtime.control, str(floor))
    await page.press(runtime.control, "Enter")
    await page.wait_for_function(
        MATCH_JS, arg=[runtime.metrics, runtime.read, want_revenue], timeout=120_000
    )
    return (time.perf_counter() - started) * 1000


async def viewer(page, runtime: Runtime, floors: list[float], think: float, out: dict) -> None:
    """One viewer, moving the control on a schedule and recording what it cost.

    The schedule does not drift. Each interaction is due at a fixed offset from the last, and
    when the driver arrives late that lateness is recorded rather than absorbed -- an absorbed
    schedule is how a saturated load generator reports a healthy one.
    """
    schedule = time.perf_counter()
    for floor in floors:
        schedule += think
        delay = schedule - time.perf_counter()
        if delay > 0:
            await asyncio.sleep(delay)
        out["lateness_ms"].append(max(0.0, (time.perf_counter() - schedule) * 1000))
        out["latency_ms"].append(await interact(page, runtime, floor))


async def measure_driver_floor(browser) -> float:
    """What the driver costs even when the server is instant.

    A local page with an input and a span wired together synchronously: the same `fill`,
    `press` and `wait_for_function` round trip, over a "server" that cannot be slow. Every
    latency below carries this, and carries it equally for all three runtimes.
    """
    page = await browser.new_page()
    await page.set_content(
        """<input type="number" id="c" value="0">
           <span class="metric-value" id="m">0</span>
           <script>c.onchange = () => { m.textContent = c.value; };</script>"""
    )
    samples = []
    for i in range(1, 11):
        started = time.perf_counter()
        await page.fill("#c", str(i))
        await page.press("#c", "Enter")
        await page.wait_for_function(
            "v => document.getElementById('m').textContent === v", arg=str(i)
        )
        samples.append((time.perf_counter() - started) * 1000)
    await page.close()
    return statistics.median(samples)


def percentile(values: list[float], p: float) -> float:
    if not values:
        return float("nan")
    ordered = sorted(values)
    # Nearest-rank. With a few hundred samples an interpolating definition would imply a
    # precision the sample size does not support.
    index = min(len(ordered) - 1, max(0, round(p / 100 * len(ordered) + 0.5) - 1))
    return ordered[index]


async def assert_selectors(browser, runtime: Runtime, url: str) -> None:
    """Every pinned selector resolves on a live app, or the run stops here.

    The whole harness rests on these. A Streamlit or marimo release that renames a test id
    would otherwise leave the driver waiting on a node that no longer exists, time out, and
    report the runtime as slow.
    """
    page = await browser.new_page()
    await page.goto(url, wait_until="domcontentloaded")
    try:
        await page.wait_for_selector(runtime.control, timeout=runtime.boot_timeout * 1000)
        await page.wait_for_selector(runtime.metrics, timeout=runtime.boot_timeout * 1000)
    except Exception as exc:  # noqa: BLE001 -- the message matters more than the type
        raise SystemExit(
            f"{runtime.name}: the pinned selectors do not resolve ({exc}).\n"
            f"  control: {runtime.control}\n  metrics: {runtime.metrics}\n"
            "This runtime's markup has changed. Update RUNTIMES in benches/fleet/fleet.py "
            "and re-run; do NOT publish a result from a harness that could not find the "
            "numbers it is timing."
        ) from exc
    count = len(await read_metrics(page, runtime))
    if count != 3:
        raise SystemExit(f"{runtime.name}: expected 3 metric elements, found {count}")
    await page.close()


async def run_one(
    browser, runtime: Runtime, apps: int, viewers: int, rounds: int, think: float
) -> dict:
    """One (runtime, apps, viewers) point."""
    fleet = Fleet(runtime)
    result: dict = {
        "runtime": runtime.name,
        "apps": apps,
        "viewers_per_app": viewers,
        "fleet_shape": "one-process" if runtime.one_process else "process-per-app",
    }
    try:
        boot_started = time.perf_counter()
        fleet.start(apps)
        fleet.wait_listening()
        await assert_selectors(browser, runtime, fleet.urls[0])

        pages = []
        for url in fleet.urls:
            for _ in range(viewers):
                pages.append(await open_session(browser, runtime, url))
        result["boot_and_first_render_s"] = round(time.perf_counter() - boot_started, 2)

        # Resident, with every app holding a live session. Sampled after the fleet has
        # settled rather than at its peak: the question is what it costs to KEEP an app, and
        # a first render's transient allocation is not that.
        await asyncio.sleep(2.0)
        pids = fleet.pids()
        rss, pss = memory_of(pids)
        result["processes"] = len(pids)
        result["resident_rss_bytes"] = rss
        result["resident_pss_bytes"] = pss

        # Never the value already on screen, and never twice in a row. `open_session` waits
        # until the page shows `oracle(0.0)`, so a plan starting at 0.0 made round 0 a no-op:
        # `wait_for_function` found its condition already true and that sample timed the
        # driver rather than a round trip. Same rule, and the same reason, as the FLOORS list
        # in `benches/loadgen/src/main.rs`.
        floors = [100.0, 400.0, 250.0, 600.0, 50.0]
        plan = [floors[i % len(floors)] for i in range(rounds)]
        out: dict[str, list[float]] = {"latency_ms": [], "lateness_ms": []}

        cpu_before, wall_before = cpu_of(tree_pids(os.getpid())), time.perf_counter()
        server_cpu_before = cpu_of(pids)

        await asyncio.gather(*(viewer(p, runtime, plan, think, out) for p in pages))

        wall = time.perf_counter() - wall_before
        driver_cpu = cpu_of(tree_pids(os.getpid())) - cpu_before
        result["server_cpu_s"] = round(cpu_of(fleet.pids()) - server_cpu_before, 3)
        result["wall_s"] = round(wall, 2)
        result["interactions"] = len(out["latency_ms"])
        result["throughput_per_s"] = round(len(out["latency_ms"]) / wall, 2) if wall else None
        result["p50_ms"] = round(percentile(out["latency_ms"], 50), 1)
        result["p99_ms"] = round(percentile(out["latency_ms"], 99), 1)
        result["max_ms"] = round(max(out["latency_ms"]), 1) if out["latency_ms"] else None

        # The driver's own saturation, reported whether or not it is flattering.
        cpu_share = driver_cpu / (wall * (os.cpu_count() or 1)) if wall else 0.0
        lateness_p99 = percentile(out["lateness_ms"], 99)
        result["driver_cpu_share"] = round(cpu_share, 3)
        result["driver_dispatch_lateness_p99_ms"] = round(lateness_p99, 1)
        result["saturated"] = bool(
            cpu_share > SATURATION_CPU_SHARE or lateness_p99 > SATURATION_LATENESS_MS
        )

        # **Lateness only means something against a schedule.** With `--think 0` every viewer
        # is due immediately and for ever, so lateness is zero by construction and the flag
        # above cannot fire however hard the driver is struggling — a 128-viewer run at
        # think=0 measured seconds per interaction and reported itself healthy, which is how
        # this check came to exist. A run with no think time is a thundering herd: the
        # memory sample it takes before the load phase is valid, and its latencies are
        # queueing in the driver and are not a measurement of anything.
        result["latency_reportable"] = think > 0
        if not result["latency_reportable"]:
            result["latency_note"] = (
                "think=0: every viewer dispatches at once, so these latencies are driver "
                "queueing. The memory figures in this run are unaffected."
            )

        for page in pages:
            await page.close()
    finally:
        fleet.stop()
    return result


async def main_async(args) -> None:
    from playwright.async_api import async_playwright

    names = list(RUNTIMES) if args.all else [args.runtime]
    app_counts = [int(n) for n in args.apps.split(",")]

    async with async_playwright() as p:
        launch: dict = {"args": ["--no-sandbox"]}
        if CHROMIUM:
            launch["executable_path"] = CHROMIUM
        browser = await p.chromium.launch(**launch)

        floor = await measure_driver_floor(browser)
        report = {
            "schema": 1,
            "measured_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "machine": {
                "hostname": socket.gethostname(),
                "platform": platform.platform(),
                "cpus": os.cpu_count(),
                "memory_bytes": os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES"),
            },
            "driver_floor_ms": round(floor, 1),
            "settings": {
                "rounds_per_viewer": args.rounds,
                "think_seconds": args.think,
                "saturation_cpu_share": SATURATION_CPU_SHARE,
                "saturation_lateness_ms": SATURATION_LATENESS_MS,
            },
            "runs": [],
        }
        print(f"driver floor: {floor:.1f} ms (subtract nothing; it is in every number below)")

        for name in names:
            for apps in app_counts:
                print(f"  {name:10} apps={apps:<3} viewers={args.viewers} ...", end="", flush=True)
                run = await run_one(
                    browser, RUNTIMES[name], apps, args.viewers, args.rounds, args.think
                )
                report["runs"].append(run)
                latency = (
                    f" p50 {run['p50_ms']:>7.1f} ms  p99 {run['p99_ms']:>7.1f} ms"
                    if run["latency_reportable"]
                    else f" {'latency not reportable (think=0)':>30}"
                )
                print(
                    latency
                    + f"  rss {run['resident_rss_bytes'] / 1e6:>7.1f} MB"
                    + f"  pss {run['resident_pss_bytes'] / 1e6:>7.1f} MB"
                    + ("  SATURATED" if run["saturated"] else "")
                )

        await browser.close()

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    path = out / f"{report['machine']['hostname']}-{time.strftime('%Y%m%d-%H%M%S')}.json"
    path.write_text(json.dumps(report, indent=2) + "\n")
    print(f"\nwrote {path}")


def main() -> None:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--runtime", choices=list(RUNTIMES), default="dagpane")
    parser.add_argument("--all", action="store_true", help="every runtime, in order")
    parser.add_argument("--apps", default="1", help="comma-separated app counts, e.g. 1,2,4,8")
    parser.add_argument("--viewers", type=int, default=1, help="browser sessions per app")
    parser.add_argument("--rounds", type=int, default=20, help="interactions per viewer")
    parser.add_argument(
        "--think", type=float, default=0.5, help="seconds between a viewer's interactions"
    )
    parser.add_argument("--out", default=str(HERE / "results"))
    args = parser.parse_args()

    if not (MODULE / "target/release/dagpane").exists():
        sys.exit("build the binary first: cargo build --release -p dagpane-cli")
    asyncio.run(main_async(args))


if __name__ == "__main__":
    main()
