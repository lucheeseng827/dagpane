// What does a viewer who is *interacting* cost, over one who is only holding a socket?
//
// `ROADMAP.md` §7's remaining first item. `session-memory.mjs` answered the other half and
// said, in as many words, what it was leaving out:
//
//   > **Idle sessions.** The question is what holding a viewer costs, not what serving one
//   > costs. An interaction allocates transiently and would be measured as noise.
//
// That exclusion was right for that rig and it left a number nobody had. "Not zero" is not
// something an operator can size against, and the shape of the answer was not obvious either:
// a pass could cost a frame, or a whole pipeline, or nothing if the engine computed in place.
//
// # The instrument, which is why this is measurable at all
//
// A pass over the bundled example takes about a third of a millisecond. **A sampler cannot
// see it.** Read RSS every 50 ms and you will observe an interaction costing nothing, roughly
// a hundred and fifty times in a hundred and fifty-one, and publish it. So the peak here is
// not sampled: `VmHWM` is maintained by the kernel on every page fault and `clear_refs` resets
// it, so each window reports its true high-water however briefly it was touched. See `lib.mjs`.
//
// # The experiment
//
// One process, one app, **a fixed number of viewers held open** — so the held cost is in the
// baseline and out of the answer — and a ramp in how many of them are *dragging a slider as
// fast as the server will answer*. Closed loop deliberately: an arrival rate is a second
// parameter whose value would have to be defended, and the ceiling is what an operator needs
// room for.
//
// One viewer is held back from the ramp and never drags anything. It asks the server for the
// current state every 50 ms and times the answer, which is **the half of this cost that is not
// paid by the viewer who is busy** — see `probe`.
//
// **Each rung is a fresh process, each rung fires the same burst twice, and each rung is run
// three times over.** All three are corrections to earlier versions of this rig, and the first
// of them is the first thing the rig found: *after a burst, resident memory does not come back.*
// A single process walked up the ladder therefore started every rung from the previous rung's
// peak, measured how much **more** than that it needed, and printed a jumping sequence of
// numbers as if it were the cost of a rising amount of work. Each change answers a question
// that confusion was hiding:
//
//   * a fresh process per rung makes the rungs **independent** — the K in a rung's answer is
//     that rung's K and not the largest K seen so far;
//   * the same burst **twice** separates the two explanations for memory that does not come
//     back. A leak grows every time the work is done. A plateau does not. If the second
//     identical burst is free, the process reached a working set rather than losing memory,
//     and those have nothing in common but the shape of one reading;
//   * **three processes a rung, medianed**, because `peak − idle` is how much *new* memory the
//     burst has to fault in, and that depends on what the allocator was already holding free.
//     On an app whose sessions materialise little, two runs of one rung came back 12 MB and
//     0 kB — both true of their own process. The spread is printed beside the median.
//
// The rung at **K = 0** is the control: nothing is driven, so a peak above the idle reading
// there would be the instrument measuring something other than the experiment. So is the idle
// column itself — every rung holds the same viewers on a fresh process, so an idle reading
// that drifts across rungs is a rung that was not independent after all.
//
// `DAGPANE_CORES` runs the server under `taskset`, which is a second experiment rather than a
// convenience: tokio sizes its worker pool from the affinity mask and a pass runs on a worker,
// so the same ramp on one core and on four asks whether the transient is bounded by the
// viewers or by the threads.
//
//   node benches/sessions/busy-session.mjs [manifest]

import { spawn } from "node:child_process";
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, join, resolve } from "node:path";
import { createHash } from "node:crypto";
import {
  sleep, weigh, peakRss, resetPeak, median, quantile, openSession, waitReady, machine, provenance,
} from "./lib.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, "../..");
const DAGPANE = process.env.DAGPANE_BIN ?? join(ROOT, "target/release/dagpane");
const MANIFEST = process.argv[2] ?? join(ROOT, "examples/sales.toml");
/// How a result names its app. A manifest inside the repository is named by its path relative
/// to the module root; a generated fixture is named by its file, because its directory is a
/// `mktemp -d` that will not exist when anyone reads the result. `manifest_sha256` below is
/// what actually identifies it.
///
/// **Resolved first.** The callers pass `examples/sales.toml` relative to the module root and
/// the fixtures as absolute paths, so comparing the raw argument against `ROOT` labelled a
/// bundled example as a fixture.
const APP_PATH = resolve(MANIFEST);
const APP_ID = APP_PATH.startsWith(ROOT + "/")
  ? APP_PATH.slice(ROOT.length + 1)
  : `fixture:${APP_PATH.split("/").pop()}`;
const PORT = Number(process.env.DAGPANE_PORT ?? 8797);
const CORES = process.env.DAGPANE_CORES ?? null;       // e.g. "0" or "0-3"; unset = every core

/// Viewers held open in every rung. The held cost is then the same in every rung and cancels
/// out of the transient — which is the whole point, since the held cost is the other rig's
/// answer and repeating it here would be measuring it twice and calling the sum new.
const HELD = Number(process.env.DAGPANE_HELD ?? 64);
/// How many of them are dragging. Doubling, because the interesting question is where it stops
/// growing and a doubling ladder spends its samples finding a bend. `DAGPANE_LADDER` overrides
/// it, which is how a single rung is re-run against a longer window to ask whether a rung's
/// answer was about its viewers or about how long it was given.
/// Validated rather than trusted, because three of this knob's plausible typos fail as a
/// measurement rather than as an error. A **negative** rung reaches `sessions.slice(0, busy)`,
/// which drives every session *but* the last few and then labels the row with the number it
/// did not drive. A **non-numeric** one becomes `NaN`, which compares false against `HELD` and
/// is dropped without a word. And a ladder with **no rung above zero** leaves the summary with
/// an empty list to take a ceiling over, which throws several minutes after the typo.
function parseLadder(spec) {
  const words = spec.split(",").map((k) => k.trim()).filter((k) => k.length);
  const bad = words.filter((k) => !/^\d+$/.test(k));
  if (bad.length) throw new Error(`DAGPANE_LADDER: not whole numbers: ${bad.join(", ")}`);
  const ks = [...new Set(words.map(Number))].sort((a, b) => a - b);
  if (!ks.length) throw new Error("DAGPANE_LADDER: no rungs");
  // An explicitly named rung above HELD is an error where the default ladder's is a trim:
  // asking for 64 dragging viewers out of 8 held is a mistake worth stopping for, whereas
  // the built-in ladder is a shape that is meant to be cut to whatever HELD allows.
  const over = ks.filter((k) => k > HELD);
  if (over.length)
    throw new Error(
      `DAGPANE_LADDER: ${over.join(", ")} exceeds DAGPANE_HELD=${HELD} — a rung cannot drag ` +
        "more viewers than are held open. Raise DAGPANE_HELD or drop the rung."
    );
  if (!ks.some((k) => k > 0))
    throw new Error(
      "DAGPANE_LADDER: needs a rung above 0 — a ladder of nothing but the control measures " +
        "the control."
    );
  return ks;
}
const LADDER = process.env.DAGPANE_LADDER
  ? parseLadder(process.env.DAGPANE_LADDER)
  : [0, 1, 2, 4, 8, 16, 32, 64].filter((k) => k <= HELD);
const WINDOW_MS = Number(process.env.DAGPANE_WINDOW_MS ?? 3000);
const SETTLE_MS = 700;
const SAMPLES = 5;
/// Whole rungs, repeated, with the median taken — and this is the third correction the rig
/// needed rather than a habit. `peak − idle` is how much *new* resident memory a burst has to
/// fault in, so it depends on how much the allocator was already holding free when the burst
/// started, and that varies between one process and the next. On an app whose sessions hold
/// almost nothing it varies by more than the answer: two runs of the same rung came back 12 MB
/// and 0 kB, and both were true readings of two processes that differed only in what the
/// allocator had happened to keep. A median over fresh processes is the honest summary, and
/// the spread is printed beside it rather than hidden by it.
const REPS = Number(process.env.DAGPANE_REPS ?? 3);
/// How often the neighbour asks the server for the time of day. Paced rather than closed loop
/// on purpose: a probe that hammered would be part of the load it is trying to observe.
const PROBE_EVERY_MS = 50;
/// Above this many passes in a window the raw timings are summarised rather than stored. The
/// bundled example does twenty thousand in three seconds and nobody is going to read them.
const RAW_MICROS_CAP = 2000;

/// The control every app in this directory has, and the values to cycle it through.
///
/// **Distinct, and never twice in a row.** Send the same value again and the source cell's
/// digest is unchanged, the engine reuses, and no pass runs — the rig would be weighing an
/// empty window and reporting it as the cost of an interaction. The cursor that guarantees
/// this belongs to the socket rather than to the burst, because the first message of a burst
/// is the one that would otherwise repeat the last message of the previous one.
const INPUT = "min_amount";
const VALUES = [25, 75, 125, 175, 225, 275, 325, 375];

/// One busy viewer, closed loop: set, wait for the patch that answers it, set again.
///
/// Every reply is checked for three things, and the third is the one that matters: a patch
/// carries only panes whose rendered view moved, so a patch with **no** panes means this
/// viewer's slider did nothing and the window measured an idle process. A rig that ignored
/// that could publish "an interaction is free" while reporting that it never caused one.
function drive(ws, stop, tally, cursor) {
  return new Promise((resolve, reject) => {
    let seq = cursor.seq;
    const cleanup = () => ws.removeEventListener("message", onMessage);
    const onMessage = (e) => {
      const msg = JSON.parse(e.data);
      if (msg.type === "error") {
        cleanup();
        reject(new Error(`server rejected seq ${msg.seq}: ${msg.message}`));
        return;
      }
      if (msg.type !== "patch") return;
      if (msg.seq !== seq) {
        cleanup();
        reject(new Error(`seq ${msg.seq} answered out of band (expected ${seq})`));
        return;
      }
      if (!msg.panes?.length) {
        cleanup();
        reject(new Error(`patch for seq ${seq} carried no panes — this window measured nothing`));
        return;
      }
      tally.passes += 1;
      if (typeof msg.stats?.micros === "number") tally.micros.push(msg.stats.micros);
      send();
    };
    const send = () => {
      if (stop.done) {
        cleanup();
        cursor.seq = seq;
        resolve();
        return;
      }
      seq += 1;
      cursor.step += 1;
      const v = VALUES[cursor.step % VALUES.length];
      ws.send(JSON.stringify({ type: "set", seq, values: { [INPUT]: { kind: "float", v } } }));
    };
    ws.addEventListener("message", onMessage);
    send();
  });
}

/// The viewer who is **not** dragging anything, timed while everybody else is.
///
/// This is the other half of what a busy session costs, and it is not paid by the person who
/// is busy. A pass runs inline on the async worker that owns the connection — `crates/serve`
/// calls `session.commit()` in the `async fn` and not on a blocking pool — so a viewer who is
/// sitting still shares a worker with viewers who are not, and waits behind them. Reading that
/// out of the source is easy and has been done more than once; putting a number on it is what
/// this is for.
///
/// `Refresh` is the probe because it is the cheapest thing the protocol has that still goes
/// all the way in and back: a graph walk where every cell reuses, no recomputation, every pane
/// re-rendered. Whatever it waits for is therefore the queue and not the work.
///
/// **`benches/loadgen/` is this repository's latency rig and stays so.** This measures one
/// thing that one cannot: what a viewer who is doing nothing experiences while their
/// neighbours are busy.
function probe(ws, stop, samples, cursor) {
  return new Promise((resolve, reject) => {
    // The cursor belongs to the socket and outlives the burst, exactly as a driver's does — and
    // here it is not a nicety. A burst can end with an answer still owed (that is the finding
    // at the top of the ladder), and a probe that restarted its counter would accept that late
    // reply as the *next* burst's first sample and time it from the wrong instant.
    let seq = cursor.seq;
    let sentAt = 0;
    let outstanding = false;
    const cleanup = () => {
      clearInterval(tick);
      ws.removeEventListener("message", onMessage);
    };
    const onMessage = (e) => {
      const msg = JSON.parse(e.data);
      if (msg.type === "error") {
        cleanup();
        reject(new Error(`server rejected probe ${msg.seq}: ${msg.message}`));
        return;
      }
      if (msg.type !== "refreshed" || msg.seq !== seq) return;
      outstanding = false;
      // `performance.now()` rather than `Date.now()`: on the bundled example the whole round
      // trip is under a millisecond, and a millisecond clock would report it as zero.
      samples.push({ ms: performance.now() - sentAt, censored: false });
      if (stop.done) {
        cursor.seq = seq;
        cleanup();
        resolve();
        return;
      }
      setTimeout(send, PROBE_EVERY_MS);
    };
    const send = () => {
      if (stop.done) {
        cursor.seq = seq;
        cleanup();
        resolve();
        return;
      }
      seq += 1;
      sentAt = performance.now();
      outstanding = true;
      ws.send(JSON.stringify({ type: "refresh", seq }));
    };
    // The probe can be waiting on an answer that has not come and will not come before the
    // window closes — which is itself the finding at the top of the ladder, where one request
    // went out and nothing came back for three seconds. Such a wait is **censored**: the rig
    // knows it was at least this long and cannot know how much longer. Recording it as a
    // number and saying so is right; waiting for it would hang the rig, and dropping it would
    // throw away the only reading the worst rung produced.
    const tick = setInterval(() => {
      if (!stop.done) return;
      if (outstanding) samples.push({ ms: performance.now() - sentAt, censored: true });
      cursor.seq = seq;
      cleanup();
      resolve();
    }, 25);
    ws.addEventListener("message", onMessage);
    send();
  });
}

/// Drive `busy` of the sessions for one window, and report what they got through.
async function burst(sessions, cursors, neighbour, neighbourCursor, busy) {
  const stop = { done: false };
  const tally = { passes: 0, micros: [] };
  const waits = [];
  const drivers = sessions.slice(0, busy).map((ws, i) => drive(ws, stop, tally, cursors[i]));
  drivers.push(probe(neighbour, stop, waits, neighbourCursor));
  const started = Date.now();
  await sleep(WINDOW_MS);
  stop.done = true;
  // Each driver has exactly one request outstanding, so every one resolves within a pass of
  // being told to stop. A timeout rather than a bare await: a rig that hangs here is reporting
  // a server that stopped answering, and should say so rather than wait.
  await Promise.race([
    Promise.all(drivers),
    sleep(60_000).then(() => {
      throw new Error(`drivers did not finish after the ${busy}-viewer window`);
    }),
  ]);
  // **Completed and censored are aggregated apart, and that is a correction.** A censored wait
  // is recorded at the moment the window shuts, so its number is the truncation time and not
  // the wait — known too small, by an unknown amount. Folding those into a median treats a
  // lower bound as an observation, and it does it in the direction that flatters: at the worst
  // rung of the worst app the mixed median read 431.6 ms where the completed probes alone say
  // 2 248.5 ms, and it produced an apparent *fall* in the bystander's wait between thirty-two
  // dragging viewers and sixty-four. Nothing in a server does that. So `neighbour_p50_ms` and
  // `neighbour_worst_ms` are over the probes that came back; the mixed figures are kept beside
  // them under `_with_censored`, because the count of what was dropped is the reader's warning
  // that the rung is thin, and `neighbour_waits_ms` carries every sample with its flag either
  // way.
  const done = waits.filter((w) => !w.censored).map((w) => w.ms);
  const ms = waits.map((w) => w.ms);
  return {
    passes: tally.passes,
    per_second: tally.passes / ((Date.now() - started) / 1000),
    p50: quantile(tally.micros, 0.5),
    p90: quantile(tally.micros, 0.9),
    p99: quantile(tally.micros, 0.99),
    micros_min: tally.micros.length ? Math.min(...tally.micros) : null,
    micros_max: tally.micros.length ? Math.max(...tally.micros) : null,
    // The raw pass timings when there are few enough to be worth carrying. A fast app does
    // twenty thousand passes in a window and the file would be megabytes of them; the summary
    // above is what survives that, and `passes` says how many it was taken over.
    micros: tally.micros.length <= RAW_MICROS_CAP ? tally.micros : null,
    micros_truncated: tally.micros.length > RAW_MICROS_CAP,
    // **The neighbour's samples are kept whole, always.** They are what the conclusions rest
    // on, there are never many — two, at the top rung — and an aggregate over two samples that
    // does not say it is over two samples is the thing this field exists to prevent.
    neighbour_probes: waits.length,
    neighbour_censored: waits.filter((w) => w.censored).length,
    neighbour_waits_ms: waits,
    neighbour_p50_ms: quantile(done, 0.5),
    neighbour_worst_ms: done.length ? Math.max(...done) : null,
    neighbour_p50_with_censored_ms: quantile(ms, 0.5),
    neighbour_worst_with_censored_ms: ms.length ? Math.max(...ms) : null,
  };
}

/// RSS, settled and medianed — the same reading `session-memory.mjs` takes, for the same
/// reason: one sample catches whatever the allocator happened to be doing at that instant.
async function settled(pid) {
  await sleep(SETTLE_MS);
  const taken = [];
  for (let i = 0; i < SAMPLES; i++) {
    taken.push(await weigh(pid));
    await sleep(80);
  }
  return {
    rss: median(taken.map((t) => t.rss)),
    pss: median(taken.map((t) => t.pss)),
    samples: taken,
  };
}

async function threadsOf(pid) {
  const m = (await readFile(`/proc/${pid}/status`, "utf8")).match(/^Threads:\s+(\d+)/m);
  return m ? Number(m[1]) : null;
}

/// A server and its viewers, from nothing. One per repetition of every rung — see the header.
/// Exactly what gets spawned, in one place, so `bringUp` and the recorded provenance cannot
/// drift apart — they did, and the affinity wrapper went unrecorded on the two runs whose whole
/// subject is the affinity mask.
function serverArgv(port) {
  const argv = [DAGPANE, "run", MANIFEST, "--port", String(port)];
  return CORES ? ["taskset", "-c", CORES, ...argv] : argv;
}

/// The port a given repetition of a given rung gets, in one place because the recorded
/// provenance has to agree with it. A port per rung and per repetition: the previous process
/// is killed rather than asked to leave, and a rig that raced a dying listener for its port
/// would fail in the one way that looks exactly like the thing it is measuring.
const portFor = (rung, rep) => PORT + rung * REPS + rep;

async function bringUp(port) {
  const [cmd, ...args] = serverArgv(port);
  const server = spawn(cmd, args, { stdio: "ignore" });
  const sessions = [];
  let neighbour = null;
  let neighbourCursor = null;
  try {
    await waitReady(server, port);
    if ((await peakRss(server.pid)) === null) throw new Error("no VmHWM — this rig needs Linux");
    for (let i = 0; i < HELD; i++) sessions.push(await openSession(port));
    // One more socket than the ladder can ever drive, held back deliberately: at the top rung
    // every one of the HELD viewers is dragging, and a neighbour taken from among them would
    // be one of the busy ones. It is in every rung's baseline, so it cancels out of the
    // transient exactly as the held viewers do.
    neighbour = await openSession(port);
    // Its own `seq` range, so a stray reply is obviously not a pass, and its own cursor.
    neighbourCursor = { seq: 1_000_000 };
  } catch (e) {
    server.kill("SIGKILL");
    throw e;
  }
  return {
    server,
    sessions,
    neighbour,
    neighbourCursor,
    // One cursor per socket, living as long as the socket — see `drive`.
    cursors: sessions.map((_, i) => ({ seq: 0, step: i })),
    threads: await threadsOf(server.pid),
  };
}

const mb = (b) => (b / 1048576).toFixed(1);
const kb = (b) => (b / 1024).toFixed(0);

/// The app's sources, weighed and digested.
///
/// The manifest's digest is not enough on its own to say which app a result is of, and this
/// directory is exactly where that bites: the 600-row ramp and the 200 000-row ramp run the
/// **same manifest** over different CSVs — that is the experiment's premise — so two results
/// with identical `manifest_sha256` can be of apps two orders of magnitude apart. The data is
/// the other half of the identity.
async function sourcesOf(manifestPath, manifestText) {
  const dir = dirname(manifestPath);
  const names = [...manifestText.matchAll(/^\s*csv\s*=\s*"([^"]+)"/gm)].map((m) => m[1]);
  const out = [];
  for (const name of names) {
    try {
      const bytes = await readFile(join(dir, name));
      out.push({
        csv: name,
        bytes: bytes.length,
        sha256: createHash("sha256").update(bytes).digest("hex"),
      });
    } catch {
      out.push({ csv: name, bytes: null, sha256: null });
    }
  }
  return out;
}

async function main() {
  const manifestText = await readFile(MANIFEST);
  const MANIFEST_SHA256 = createHash("sha256").update(manifestText).digest("hex");
  const MANIFEST_BYTES = manifestText.length;
  const SOURCES = await sourcesOf(APP_PATH, manifestText.toString("utf8"));
  const rungs = [];
  const ports = [];
  let threads = null;

  console.log(
    `\n${HELD} viewers held open on a fresh process per rung` +
    `${CORES ? ` · server pinned to cores ${CORES}` : ""} · ` +
    `${WINDOW_MS} ms a burst, twice a rung, ${REPS} rungs medianed\n`
  );
  console.log("                                                                                          the neighbour waits");
  console.log("  dragging    idle RSS   transient      spread    retained      repeat    passes/s  pass p50        p50    worst  answered");
  console.log("  ────────  ──────────  ──────────  ──────────  ──────────  ──────────  ──────────  ────────  ─────────  ───────  ────────");

  for (const [i, busy] of LADDER.entries()) {
    const reps = [];
    for (let rep = 0; rep < REPS; rep++) {
      const port = portFor(i, rep);
      ports.push(port);
      const up = await bringUp(port);
      threads = up.threads;
      const pid = up.server.pid;
      try {
        const before = await settled(pid);
        await resetPeak(pid);
        const one = await burst(up.sessions, up.cursors, up.neighbour, up.neighbourCursor, busy);
        const peak = await peakRss(pid);

        const between = await settled(pid);
        await resetPeak(pid);
        const two = await burst(up.sessions, up.cursors, up.neighbour, up.neighbourCursor, busy);
        const repeatPeak = await peakRss(pid);
        const after = await settled(pid);

        reps.push({
          rss_idle: before.rss,
          pss_idle: before.pss,
          // Every reading behind those medians, so the medians can be recomputed rather than
          // taken on trust.
          rss_samples: { before: before.samples, between: between.samples, after: after.samples },
          peak_rss: peak,
          rss_between: between.rss,
          rss_after: after.rss,
          transient_bytes: peak - before.rss,
          retained_bytes: between.rss - before.rss,
          repeat_transient_bytes: repeatPeak - between.rss,
          first: one,
          repeat: two,
        });
      } finally {
        for (const ws of [...up.sessions, up.neighbour]) {
          try { ws?.close(); } catch { /* going away anyway */ }
        }
        up.server.kill("SIGKILL");
      }
    }

    const over = (f) => median(reps.map(f));
    const sum = (xs) => xs.reduce((a, b) => a + b, 0);
    const transient = over((r) => r.transient_bytes);
    const spread = Math.max(...reps.map((r) => r.transient_bytes)) -
                   Math.min(...reps.map((r) => r.transient_bytes));
    const rung = {
      busy,
      reps,
      rss_idle: over((r) => r.rss_idle),
      transient_bytes: transient,
      transient_spread_bytes: spread,
      retained_bytes: over((r) => r.retained_bytes),
      repeat_transient_bytes: over((r) => r.repeat_transient_bytes),
      passes_per_second: over((r) => r.first.per_second),
      pass_micros_p50: over((r) => r.first.p50 ?? 0),
      pass_micros_p99: over((r) => r.first.p99 ?? 0),
      neighbour_p50_ms: over((r) => r.first.neighbour_p50_ms ?? 0),
      neighbour_worst_ms: over((r) => r.first.neighbour_worst_ms ?? 0),
      // SUMMED across the repetitions, not medianed, and the difference is not cosmetic. A rung
      // whose three processes censored 0, 1 and 0 probes has a median of 0 — so the one reading
      // that says "this wait was longer than the rig could see" disappears into the aggregate
      // that is supposed to carry it. Everything else here is a typical value and a median is
      // right; this is a count of doubts and it has to add up.
      neighbour_probes: sum(reps.map((r) => r.first.neighbour_probes)),
      neighbour_censored: sum(reps.map((r) => r.first.neighbour_censored)),
      // How wide the aperture was. A wait longer than the window is unobservable by
      // construction, so any statistic here is bounded by this number and says nothing about
      // what lies beyond it.
      window_ms: WINDOW_MS,
    };
    rungs.push(rung);

    console.log(
      `  ${String(busy).padStart(8)}  ${mb(rung.rss_idle).padStart(7)} MB  ` +
      `${kb(transient).padStart(7)} kB  ` +
      `${kb(spread).padStart(7)} kB  ` +
      `${kb(rung.retained_bytes).padStart(7)} kB  ` +
      `${kb(rung.repeat_transient_bytes).padStart(7)} kB  ` +
      (busy === 0
        ? "         —         —"
        : `${rung.passes_per_second.toFixed(1).padStart(10)}  ${(rung.pass_micros_p50 / 1000).toFixed(1).padStart(6)}ms`) +
      `  ${rung.neighbour_p50_ms.toFixed(1).padStart(7)}ms  ` +
      `${rung.neighbour_worst_ms.toFixed(1).padStart(7)}ms   ` +
      `${String(rung.neighbour_probes - rung.neighbour_censored).padStart(5)}` +
      // The censored count is the rung's health warning rather than a footnote: both figures
      // to its left are over the probes that came back, so a rung that lost half of them is
      // reporting the half that was quick enough to be seen.
      `${rung.neighbour_censored > 0 ? ` (+${rung.neighbour_censored} never came back)` : ""}`
    );
  }

  // ── what the ramp says ───────────────────────────────────────────────────────────────
  //
  // A ceiling rather than a slope, and that is a finding rather than a presentation choice:
  // a slope is the right summary of a line, and fitting one to a ramp that bends averages the
  // rising part against the flat part and describes neither.
  const driven = rungs.filter((r) => r.busy > 0);
  const one = driven.find((r) => r.busy === 1);
  const ceiling = driven.reduce((a, b) => (b.transient_bytes > a.transient_bytes ? b : a));
  const control = rungs.find((r) => r.busy === 0);
  const idles = rungs.map((r) => r.rss_idle);

  if (one) {
    console.log(`\n  one pass in flight              ${kb(one.transient_bytes).padStart(9)} kB`);
  }
  console.log(`  the ceiling                     ${kb(ceiling.transient_bytes).padStart(9)} kB   (at ${ceiling.busy} dragging)`);
  if (one) {
    console.log(`  ceiling / one pass              ${(ceiling.transient_bytes / one.transient_bytes).toFixed(2).padStart(9)}`);
  }
  console.log(`  server threads                  ${String(threads).padStart(9)}`);
  if (control) {
    console.log(`\n  control, nobody dragging        ${kb(control.transient_bytes).padStart(9)} kB   (should be ~0)`);
  }
  console.log(`  idle spread across rungs        ${kb(Math.max(...idles) - Math.min(...idles)).padStart(9)} kB   (rungs are independent iff small)`);
  console.log(
    `\n  of the ceiling rung's ${kb(ceiling.transient_bytes)} kB transient,` +
    `\n    still resident once quiet     ${kb(ceiling.retained_bytes).padStart(9)} kB` +
    `\n    an identical second burst     ${kb(ceiling.repeat_transient_bytes).padStart(9)} kB   (a leak repeats; a plateau does not)`
  );

  const quiet = rungs.find((r) => r.busy === 0);
  const loudest = rungs[rungs.length - 1];
  if (quiet && loudest.busy > 0) {
    console.log(
      `\n  a viewer who is doing nothing, while ${loudest.busy} neighbours drag:` +
      `\n    p50 of its round trip  ${quiet.neighbour_p50_ms.toFixed(1).padStart(7)} ms  →  ${loudest.neighbour_p50_ms.toFixed(1).padStart(7)} ms` +
      `\n    worst                  ${quiet.neighbour_worst_ms.toFixed(1).padStart(7)} ms  →  ${loudest.neighbour_worst_ms.toFixed(1).padStart(7)} ms` +
      (loudest.neighbour_censored > 0
        ? `\n    — over the ${loudest.neighbour_probes - loudest.neighbour_censored} probes that came back. ` +
          `${loudest.neighbour_censored} did not, and those are lower bounds this rung cannot` +
          `\n      place: with the window closed the rig knows only that they exceeded their truncation.` +
          `\n      Widen DAGPANE_WINDOW_MS until that count is small before trusting the pair above.`
        : "")
    );
  }

  const out = {
    machine: await machine(),
    // What built and ran this. `libc` above all: this directory's own spot check found that
    // whether a burst's memory comes back is a property of the C library, so two result files
    // can disagree completely, both be right, and look identical without it.
    // The argv template, with `<port>` for the one field that varies — writing a concrete port
    // here would name a process that ran once out of the dozens this file summarises, and read
    // as though the whole run were one server. `ports` below lists every port actually
    // spawned. `taskset` is in it: `cores` records the mask too, but a command line a reader
    // has to reassemble from two fields is not a command line.
    provenance: await provenance(DAGPANE, serverArgv("<port>")),
    when: new Date().toISOString(),
    // A STABLE identity, because the interesting fixtures are generated into a `mktemp -d`
    // that is gone by the time anybody reads this file. The digest is the real answer — two
    // results are of the same app iff these match — and `app` is only the human label.
    app: APP_ID,
    manifest_sha256: MANIFEST_SHA256,
    manifest_bytes: MANIFEST_BYTES,
    sources: SOURCES,
    cores: CORES,
    ports,
    server_threads: threads,
    held_sessions: HELD,
    neighbour_probe_every_ms: PROBE_EVERY_MS,
    window_ms: WINDOW_MS,
    repetitions_per_rung: REPS,
    fresh_process_per_rung: true,
    input: INPUT,
    values: VALUES,
    rungs,
    transient_bytes_one_pass: one ? one.transient_bytes : null,
    transient_bytes_ceiling: ceiling.transient_bytes,
    ceiling_at_busy: ceiling.busy,
    idle_spread_bytes: Math.max(...idles) - Math.min(...idles),
  };
  await mkdir(join(HERE, "results"), { recursive: true });
  const tag = process.env.DAGPANE_TAG ?? "busy";
  const path = join(HERE, "results", `${tag}-${new Date().toISOString().replace(/[:.]/g, "-")}.json`);
  await writeFile(path, JSON.stringify(out, null, 2));
  console.log(`\n  written to ${path.replace(ROOT + "/", "")}`);
}

await main();
