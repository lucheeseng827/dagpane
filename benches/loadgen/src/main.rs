//! Drives dagpane's WebSocket directly. No browser per viewer.
//!
//! The browser rig beside this one (`benches/fleet/`) is the only honest way to compare three
//! runtimes — one driver, one definition of "the interaction is done", no protocol
//! reimplementation to argue about. It is also why that rig cannot answer the question the
//! roadmap asks: a Chromium page per viewer, on the same machine as the fleet, runs out of
//! machine long before dagpane does, and every point past that measures Playwright.
//!
//! So this exists, and its scope is exactly the gap: **dagpane only, protocol level, enough
//! concurrency to find the server's ceiling instead of the driver's.** It makes no
//! cross-runtime claim and cannot — Streamlit speaks protobuf and marimo a dialect of its
//! own, and three bespoke drivers would each be fair to one runtime. The two rigs are
//! published separately and never averaged.
//!
//! # What one viewer does
//!
//! ```text
//!   connect ──> init ──> [ wait for its turn ──> Set ──> Patch ──> check ] * rounds
//! ```
//!
//! and **check** is the half that makes this a benchmark rather than a packet blaster. An
//! oracle computed from the CSV says what `revenue` must read at each control setting, and
//! every reply is held to it. Two ways to fail:
//!
//! * the patch carries the pane and the number is wrong;
//! * the patch **omits** the pane and the number should have moved. That one is the product
//!   claim in reverse — a patch carries only panes whose rendered view changed — so a
//!   generator that ignored absences would be unable to tell "nothing needed to change" from
//!   "the engine missed an invalidation", which is the one bug this runtime must not have.
//!
//! # Two floors, both measured, neither subtracted
//!
//! `--self-test` drives an in-process echo over the same tokio, the same tungstenite and the
//! same serde, and reports what the generator costs when the server is a `clone()`. Every run
//! also measures a `Refresh` round trip: the same socket, the same codec, every pane
//! re-rendered, and **no pass** — so the gap between that and a `Set` is the pass, on the
//! same machine, at the same load. Neither is subtracted from anything. They are printed so
//! a reader can see how much room is left between the measurement and the tool.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// The control settings a viewer cycles through.
///
/// Never two the same in a row, and none of them is the app's default — so every interaction
/// genuinely moves the graph and a missing pane in a reply is a finding rather than an
/// expected no-op. `check` below still handles the unchanged case correctly; this list is
/// what makes the *common* path exercise the engine.
const FLOORS: &[f64] = &[100.0, 400.0, 250.0, 600.0, 50.0];

#[derive(Parser, Debug)]
#[command(
    name = "dagpane-loadgen",
    about = "Protocol-level load against a running dagpane server."
)]
struct Args {
    /// Where the server is listening.
    #[arg(long, default_value = "127.0.0.1:8787")]
    addr: String,

    /// How many apps to drive.
    #[arg(long, default_value_t = 1)]
    apps: usize,

    /// The `Host` header to send, with `{i}` replaced by the app's 1-based number.
    ///
    /// Required to reach a `dagpane host` fleet, which routes on the header's first label —
    /// `a{i}.localhost`. Omitted means talk to `--addr` directly, which is what a single
    /// `dagpane run` wants.
    ///
    /// **Explicit rather than inferred from `--apps`.** An earlier version switched on
    /// `apps > 1`, which is a rule about a count and not about the server: it silently sent
    /// `Host: 127.0.0.1:PORT` at a one-app host fleet, whose first label is `127`, and got a
    /// 404 that looks like the server being broken.
    #[arg(long)]
    host_pattern: Option<String>,

    /// Sessions per app.
    #[arg(long, default_value_t = 1)]
    viewers: usize,

    /// Interactions per viewer.
    #[arg(long, default_value_t = 50)]
    rounds: usize,

    /// Milliseconds between one viewer's interactions. A think time, not a rate limit: the
    /// schedule does not drift, so arriving late is recorded rather than absorbed.
    #[arg(long, default_value_t = 300)]
    think_ms: u64,

    /// The CSV the app reads, for the oracle.
    #[arg(long, default_value = "benches/fleet/apps/sales.csv")]
    csv: PathBuf,

    /// Ramp the app count through this list and report the last one inside `--budget-ms`.
    /// The server must already hold at least the largest of them.
    #[arg(long, value_delimiter = ',')]
    ramp: Option<Vec<usize>>,

    /// The p99 a run has to stay inside to count as passing, in milliseconds.
    #[arg(long, default_value_t = 250.0)]
    budget_ms: f64,

    /// Measure the generator against an in-process echo and exit.
    #[arg(long)]
    self_test: bool,

    /// Write the report here as JSON.
    #[arg(long)]
    json: Option<PathBuf>,

    /// How many cores the SERVER was given, for the apps-per-core arithmetic. This tool
    /// cannot see how the server was pinned, so it is told rather than guessed.
    #[arg(long, default_value_t = 1.0)]
    server_cores: f64,
}

// ── the oracle ─────────────────────────────────────────────────────────────────────────────

/// What `revenue` must read at this control setting.
///
/// Read from the CSV, with no dependency on the runtime under test. Formatted exactly as the
/// metric pane formats it — two decimals, no separator — so the comparison is a string
/// comparison against the bytes that actually go on the wire.
fn oracle(rows: &[f64], floor: f64) -> String {
    let total: f64 = rows.iter().copied().filter(|a| *a >= floor).sum();
    format!("{total:.2}")
}

fn load_amounts(path: &std::path::Path) -> Result<Vec<f64>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut lines = text.lines();
    let header = lines.next().ok_or("the CSV has no header row")?;
    let column = header
        .split(',')
        .position(|c| c.trim() == "amount")
        .ok_or("the CSV has no `amount` column")?;
    lines
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            line.split(',')
                .nth(column)
                .ok_or_else(|| format!("short row: {line}"))?
                .trim()
                .parse::<f64>()
                .map_err(|e| format!("{line}: {e}"))
        })
        .collect()
}

// ── one viewer ─────────────────────────────────────────────────────────────────────────────

#[derive(Default, Debug)]
struct Samples {
    set_ms: Vec<f64>,
    refresh_ms: Vec<f64>,
    slip_ms: Vec<f64>,
    /// Panes carried by each `Patch`. The product's own claim, counted rather than asserted:
    /// the app has four panes and a control move should move three of them.
    panes_per_patch: Vec<usize>,
    wrong: Vec<String>,
}

impl Samples {
    fn merge(&mut self, other: Samples) {
        self.set_ms.extend(other.set_ms);
        self.refresh_ms.extend(other.refresh_ms);
        self.slip_ms.extend(other.slip_ms);
        self.panes_per_patch.extend(other.panes_per_patch);
        self.wrong.extend(other.wrong);
    }
}

/// Connect, and return the socket plus the pane id showing `revenue` and its first value.
async fn open(addr: &str, host: Option<&str>) -> Result<(WebSocket, String, String), String> {
    let url = match host {
        Some(name) => format!("ws://{name}/ws"),
        None => format!("ws://{addr}/ws"),
    };
    let request = url.into_client_request().map_err(|e| e.to_string())?;
    let stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| format!("connect {addr}: {e}"))?;
    // Nagle off. A load generator that batches its own sends measures Nagle.
    stream.set_nodelay(true).map_err(|e| e.to_string())?;
    let (mut socket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .map_err(|e| match host {
            // `dagpane host` answers 404 for a name it routes nothing at, so the status alone
            // reads as a broken server when it is really a wrong Host header.
            None => format!(
                "upgrade: {e}\n  If this server is a `dagpane host` fleet it routes on the \
                 Host header's first label, and none was sent. Pass \
                 --host-pattern 'a{{i}}.localhost'."
            ),
            Some(name) => format!("upgrade to {name}: {e}"),
        })?;

    let init = next_json(&mut socket).await?;
    if init["type"] != "init" {
        return Err(format!("expected init, got {}", init["type"]));
    }
    let pane_id = init["panes"]
        .as_array()
        .ok_or("init carried no panes")?
        .iter()
        .find(|p| p["cell"] == "revenue")
        .and_then(|p| p["id"].as_str())
        .ok_or("no pane shows `revenue`; is this the benchmark app?")?
        .to_string();
    let first = init["views"]
        .as_array()
        .ok_or("init carried no views")?
        .iter()
        .find(|v| v["id"] == pane_id.as_str())
        .and_then(|v| v["view"]["value"].as_str())
        .ok_or("the revenue pane has no value in the opening frame")?
        .to_string();
    Ok((socket, pane_id, first))
}

type WebSocket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

async fn next_json(socket: &mut WebSocket) -> Result<serde_json::Value, String> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))
            }
            Some(Ok(Message::Close(_))) | None => return Err("the server closed the socket".into()),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(format!("socket: {e}")),
        }
    }
}

async fn viewer(
    addr: String,
    host: Option<String>,
    rounds: usize,
    think: Duration,
    // How far into the think period this viewer's schedule starts. See `run_point`.
    offset: Duration,
    amounts: Arc<Vec<f64>>,
    sent: Arc<AtomicU64>,
) -> Result<Samples, String> {
    let (mut socket, pane_id, first) = open(&addr, host.as_deref()).await?;
    let mut samples = Samples::default();

    // The opening frame is itself an assertion: a server that renders the wrong number on
    // first paint fails here rather than after fifty timed interactions.
    let want_initial = oracle(&amounts, 0.0);
    if first != want_initial {
        return Err(format!(
            "first render is {first}, oracle says {want_initial}"
        ));
    }
    let mut on_screen = first;

    // Spread, so the fleet does not beat as one drum. Every viewer sharing a period and a
    // start time synchronises into waves: the server sees N requests at once and then
    // nothing, and the percentiles that come back describe the interference pattern rather
    // than the server. Real viewers are not in step. The offset is derived from the viewer's
    // index rather than drawn at random, so two runs of the same ladder are comparable.
    //
    // It comes BEFORE the `Refresh` below, and that ordering is the whole of a third rig bug.
    // With the sleep after it, every viewer sent its `Refresh` the instant it connected — so
    // the refresh figure was measured under a synchronised burst while the `Set` figures were
    // measured under a spread schedule, and the gap between them carried the difference in
    // arrival pattern rather than the cost of a pass. Two measurements are only comparable
    // under the same load, and "the same load" is a property of the schedule, not of the
    // socket.
    tokio::time::sleep(offset).await;

    // One `Refresh` before the timed loop: same socket, same codec, every pane re-rendered,
    // no pass. Not a floor to subtract — a bound on what the protocol costs when the engine
    // does nothing.
    let seq = sent.fetch_add(1, Ordering::Relaxed);
    let started = Instant::now();
    send(
        &mut socket,
        &serde_json::json!({"type": "refresh", "seq": seq}),
    )
    .await?;
    let reply = await_seq(&mut socket, seq, "refreshed").await?;
    samples
        .refresh_ms
        .push(started.elapsed().as_secs_f64() * 1000.0);
    drop(reply);

    let mut schedule = Instant::now();
    for round in 0..rounds {
        schedule += think;
        let now = Instant::now();
        if schedule > now {
            tokio::time::sleep(schedule - now).await;
        }
        samples
            .slip_ms
            .push((Instant::now().saturating_duration_since(schedule)).as_secs_f64() * 1000.0);

        let floor = FLOORS[round % FLOORS.len()];
        let want = oracle(&amounts, floor);
        let seq = sent.fetch_add(1, Ordering::Relaxed);

        let started = Instant::now();
        send(
            &mut socket,
            &serde_json::json!({
                "type": "set",
                "seq": seq,
                "values": { "min_amount": { "kind": "float", "v": floor } }
            }),
        )
        .await?;
        let patch = await_seq(&mut socket, seq, "patch").await?;
        samples
            .set_ms
            .push(started.elapsed().as_secs_f64() * 1000.0);

        let panes = patch["panes"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        samples.panes_per_patch.push(panes.len());

        match panes
            .iter()
            .find(|p| p["id"] == pane_id.as_str())
            .and_then(|p| p["view"]["value"].as_str())
        {
            Some(got) => {
                if got != want {
                    samples.wrong.push(format!(
                        "floor {floor}: pane says {got}, oracle says {want}"
                    ));
                }
                on_screen = got.to_string();
            }
            None => {
                // The pane is absent, which the protocol says means its view did not change.
                // Then the value already on screen must be the right one — and if it is not,
                // the engine skipped a recompute it owed, which is the failure this runtime
                // exists to not have.
                if on_screen != want {
                    samples.wrong.push(format!(
                        "floor {floor}: no patch for `revenue`, but the screen says \
                         {on_screen} and the oracle says {want} — a missed invalidation"
                    ));
                }
            }
        }
    }

    let _ = socket.close(None).await;
    Ok(samples)
}

async fn send(socket: &mut WebSocket, value: &serde_json::Value) -> Result<(), String> {
    socket
        .send(Message::text(value.to_string()))
        .await
        .map_err(|e| format!("send: {e}"))
}

/// The next frame, which must be the reply to this `seq` and of this kind.
///
/// **Not a loop, deliberately.** A session is one connection and this generator keeps exactly
/// one request outstanding on it, so the next frame IS the answer. Skipping frames until a
/// matching `seq` turned up would quietly tolerate a server that answered out of order or
/// mixed two sessions' replies onto one socket — the single thing `crates/host` promises it
/// does not do, and the thing a load generator is best placed to catch. Anything unexpected
/// is an error carrying the frame.
async fn await_seq(
    socket: &mut WebSocket,
    seq: u64,
    kind: &str,
) -> Result<serde_json::Value, String> {
    let message = next_json(socket).await?;
    if message["type"] == "rejected" {
        return Err(format!(
            "server rejected seq {}: {}",
            message["seq"], message["message"]
        ));
    }
    if message["seq"].as_u64() != Some(seq) {
        return Err(format!(
            "seq {} answered out of band by a {} (expected {seq}) — one socket carried \
             another session's reply",
            message["seq"], message["type"]
        ));
    }
    if message["type"] != kind {
        return Err(format!(
            "expected {kind} for seq {seq}, got {}",
            message["type"]
        ));
    }
    Ok(message)
}

// ── statistics ─────────────────────────────────────────────────────────────────────────────

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    // Nearest-rank, matching `benches/fleet/fleet.py`, so two rigs' percentiles mean the
    // same thing.
    let index = ((p / 100.0 * sorted.len() as f64 + 0.5).round() as usize).saturating_sub(1);
    sorted[index.min(sorted.len() - 1)]
}

fn own_cpu_seconds() -> f64 {
    let ticks = 100.0; // _SC_CLK_TCK, fixed at 100 on every Linux this builds for.
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|stat| {
            let after: Vec<&str> = stat[stat.rfind(')')? + 2..].split_whitespace().collect();
            let utime: f64 = after.get(11)?.parse().ok()?;
            let stime: f64 = after.get(12)?.parse().ok()?;
            Some((utime + stime) / ticks)
        })
        .unwrap_or(0.0)
}

#[derive(Debug, serde::Serialize)]
struct Run {
    apps: usize,
    viewers_per_app: usize,
    sessions: usize,
    interactions: usize,
    wall_s: f64,
    throughput_per_s: f64,
    set_p50_ms: f64,
    set_p99_ms: f64,
    set_max_ms: f64,
    /// The same socket and codec with no pass behind it. A bound on protocol cost, never
    /// subtracted from the numbers above.
    refresh_p50_ms: f64,
    panes_per_patch_median: f64,
    generator_cpu_share: f64,
    generator_saturated: bool,
    /// How far behind its own schedule the fleet fell, at p99.
    ///
    /// A **server** signal, not a tool one — see `run_point`. A closed-loop viewer cannot
    /// start its next interaction until the last one answered, so slip is the server's reply
    /// latency spilling past the think period: the offered rate falls below the intended one
    /// because the server cannot absorb it. Large slip beside a small `generator_cpu_share`
    /// is the ceiling, stated twice.
    schedule_slip_p99_ms: f64,
    within_budget: bool,
    oracle_failures: Vec<String>,
}

async fn run_point(args: &Args, apps: usize, amounts: Arc<Vec<f64>>) -> Result<Run, String> {
    let sessions = apps * args.viewers;
    let sent = Arc::new(AtomicU64::new(1));
    let think = Duration::from_millis(args.think_ms);

    let mut tasks = Vec::with_capacity(sessions);
    let cpu_before = own_cpu_seconds();
    let wall_before = Instant::now();

    let mut index = 0usize;
    for app in 0..apps {
        let host = args.host_pattern.as_ref().map(|pattern| {
            let port = args.addr.rsplit(':').next().unwrap_or("80");
            format!("{}:{port}", pattern.replace("{i}", &(app + 1).to_string()))
        });
        for _ in 0..args.viewers {
            let offset = think.mul_f64(index as f64 / sessions as f64);
            index += 1;
            tasks.push(tokio::spawn(viewer(
                args.addr.clone(),
                host.clone(),
                args.rounds,
                think,
                offset,
                amounts.clone(),
                sent.clone(),
            )));
        }
    }

    let mut all = Samples::default();
    for task in tasks {
        all.merge(task.await.map_err(|e| format!("viewer panicked: {e}"))??);
    }

    let wall = wall_before.elapsed().as_secs_f64();
    let cpu = own_cpu_seconds() - cpu_before;

    let mut set = all.set_ms.clone();
    set.sort_by(f64::total_cmp);
    let mut refresh = all.refresh_ms.clone();
    refresh.sort_by(f64::total_cmp);
    let mut slip = all.slip_ms.clone();
    slip.sort_by(f64::total_cmp);
    let mut panes: Vec<f64> = all.panes_per_patch.iter().map(|n| *n as f64).collect();
    panes.sort_by(f64::total_cmp);

    // The generator's share of the cores it was given — `available_parallelism` honours the
    // affinity mask, so a run pinned with `taskset -c 1-3` divides by three.
    //
    // **This is the only generator-saturation signal, and the schedule slip below is
    // deliberately not one.** The first version used slip and was wrong: a viewer here is
    // closed-loop — it sends, awaits its reply, then sleeps to its next slot — so a slow
    // reply pushes the slot back and shows up as slip whether or not the generator has any
    // work to do. The measured runs make that unmistakable: at 224 apps the slip p99 was
    // 107 ms while the generator used 8% of three cores, and at 320 apps it was 2.9 SECONDS
    // at the same 7%. A tool using seven per cent of its cores is not saturated; the server
    // was. Flagging that as the generator giving out would have thrown away the very point
    // at which the ceiling was found.
    let cores = std::thread::available_parallelism().map_or(1.0, |n| n.get() as f64);
    let cpu_share = if wall > 0.0 {
        cpu / (wall * cores)
    } else {
        0.0
    };
    let slip_p99 = percentile(&slip, 99.0);
    let p99 = percentile(&set, 99.0);

    Ok(Run {
        apps,
        viewers_per_app: args.viewers,
        sessions,
        interactions: set.len(),
        wall_s: (wall * 100.0).round() / 100.0,
        throughput_per_s: ((set.len() as f64 / wall) * 100.0).round() / 100.0,
        set_p50_ms: (percentile(&set, 50.0) * 10.0).round() / 10.0,
        set_p99_ms: (p99 * 10.0).round() / 10.0,
        set_max_ms: (set.last().copied().unwrap_or(f64::NAN) * 10.0).round() / 10.0,
        refresh_p50_ms: (percentile(&refresh, 50.0) * 10.0).round() / 10.0,
        panes_per_patch_median: percentile(&panes, 50.0),
        generator_cpu_share: (cpu_share * 1000.0).round() / 1000.0,
        // The browser rig's CPU threshold, kept identical so "saturated" means one thing in
        // this project. Its lateness threshold is deliberately NOT carried over: that rig is
        // open-loop enough for lateness to mean the driver, and this one is not.
        generator_saturated: cpu_share > 0.80,
        schedule_slip_p99_ms: (slip_p99 * 10.0).round() / 10.0,
        within_budget: p99 <= args.budget_ms,
        oracle_failures: all.wrong,
    })
}

// ── the generator's own floor ──────────────────────────────────────────────────────────────

/// Drive an in-process echo over the same tokio, tungstenite and serde.
///
/// What this establishes: the round-trip cost that belongs to the *tool*. Everything a real
/// run measures sits on top of it, and if a run's latency approaches this number the run is
/// measuring the generator.
async fn self_test() -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _ = stream.set_nodelay(true);
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                while let Some(Ok(message)) = ws.next().await {
                    if message.is_text() && ws.send(message).await.is_err() {
                        return;
                    }
                }
            });
        }
    });

    let request = format!("ws://{addr}/ws")
        .into_client_request()
        .map_err(|e| e.to_string())?;
    let stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| e.to_string())?;
    stream.set_nodelay(true).map_err(|e| e.to_string())?;
    let (mut socket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .map_err(|e| e.to_string())?;

    let payload = serde_json::json!({"type": "set", "seq": 1, "values": {"min_amount": {"kind": "float", "v": 400.0}}});
    let mut samples = Vec::new();
    for _ in 0..2000 {
        let started = Instant::now();
        send(&mut socket, &payload).await?;
        let _ = next_json(&mut socket).await?;
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "generator floor over an in-process echo, {} round trips:\n  \
         p50 {:.3} ms   p99 {:.3} ms   max {:.3} ms",
        samples.len(),
        percentile(&samples, 50.0),
        percentile(&samples, 99.0),
        samples.last().copied().unwrap_or(f64::NAN),
    );
    println!(
        "  Everything this tool reports sits on top of that. A run whose p50 approaches it \
         is measuring the tool."
    );
    Ok(())
}

// ── main ───────────────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args = Args::parse();
    match run(args).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("dagpane-loadgen: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), String> {
    if args.self_test {
        return self_test().await;
    }

    let amounts = Arc::new(load_amounts(&args.csv)?);
    let ladder = args.ramp.clone().unwrap_or_else(|| vec![args.apps]);
    let mut runs = Vec::new();
    let mut failures = 0usize;

    println!(
        "{:>6}  {:>8}  {:>9}  {:>9}  {:>9}  {:>9}  {:>7}  {:>8}  {:>7}",
        "apps", "sessions", "p50 ms", "p99 ms", "max ms", "refresh", "ok/s", "slip p99", "gen cpu"
    );

    for apps in ladder {
        let run = run_point(&args, apps, amounts.clone()).await?;
        println!(
            "{:>6}  {:>8}  {:>9.1}  {:>9.1}  {:>9.1}  {:>9.1}  {:>7.0}  {:>8.1}  {:>7.3}{}{}{}",
            run.apps,
            run.sessions,
            run.set_p50_ms,
            run.set_p99_ms,
            run.set_max_ms,
            run.refresh_p50_ms,
            run.throughput_per_s,
            run.schedule_slip_p99_ms,
            run.generator_cpu_share,
            if run.generator_saturated {
                "  GEN-SATURATED"
            } else {
                ""
            },
            if run.within_budget {
                ""
            } else {
                "  OVER BUDGET"
            },
            if run.oracle_failures.is_empty() {
                ""
            } else {
                "  WRONG ANSWERS"
            },
        );
        for failure in &run.oracle_failures {
            println!("        ! {failure}");
        }
        failures += run.oracle_failures.len();
        runs.push(run);
    }

    // The number the roadmap asks for, stated only when the run is entitled to state it.
    let passing = runs
        .iter()
        .filter(|r| r.within_budget && !r.generator_saturated && r.oracle_failures.is_empty())
        .map(|r| r.apps)
        .max();
    println!();
    match passing {
        Some(apps) if Some(apps) == runs.last().map(|r| r.apps) => println!(
            "Every rung passed. {apps} apps is a FLOOR, not the ceiling: the ladder ran out \
             before the server did. Extend --ramp."
        ),
        Some(apps) => println!(
            "{apps} apps stayed inside {:.0} ms at p99 on {:.0} core(s) \
             => {:.1} apps/core, at {} viewer(s) each.",
            args.budget_ms,
            args.server_cores,
            apps as f64 / args.server_cores,
            args.viewers,
        ),
        None => println!(
            "No rung passed: nothing here is an apps-per-core figure. Check the reasons \
             printed above — over budget, generator saturated, or wrong answers."
        ),
    }
    if failures > 0 {
        println!(
            "\n{failures} interaction(s) disagreed with the oracle. These are NOT performance \
             results; a runtime that renders a wrong number has not earned a latency figure."
        );
    }

    if let Some(path) = &args.json {
        let report = serde_json::json!({
            "schema": 1,
            "tool": "dagpane-loadgen",
            "measured_at": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            "settings": {
                "addr": args.addr,
                "viewers_per_app": args.viewers,
                "rounds_per_viewer": args.rounds,
                "think_ms": args.think_ms,
                "budget_ms": args.budget_ms,
                "server_cores": args.server_cores,
            },
            "apps_per_core": passing.map(|a| a as f64 / args.server_cores),
            "runs": runs,
        });
        std::fs::write(path, serde_json::to_string_pretty(&report).unwrap() + "\n")
            .map_err(|e| format!("{}: {e}", path.display()))?;
        println!("\nwrote {}", path.display());
    }

    if failures > 0 {
        return Err("the oracle rejected at least one reply".into());
    }
    Ok(())
}
