// What the process says its viewers hold, against what the kernel says they cost.
//
// `--budget-mb` counts source bytes, and `session-memory.mjs` showed that a viewer costs what
// their pipeline materialises on top of those — 7 MB for one that keeps rows, 145 kB for one
// that aggregates. The host now prices that itself: each viewer's connection holds a lease the
// host can see, and `/healthz` reports the bytes their materialised frames hold beyond the
// sources they share. **A figure the process computes about itself is a claim, and this is the
// measurement that could falsify it.** Two instruments on one process, read at the same rungs:
//
//   * the **ledger** — the per-app `session_bytes` the admin `/healthz` reports, which walks
//     every frame a session holds and counts each allocation once, sources excluded;
//   * the **kernel** — RSS from `/proc/PID/smaps_rollup`, ramped and reduced to a slope by
//     `lib.mjs`, exactly as the held rig does it.
//
// They are not meant to agree. The ledger counts frames; a viewer also costs a socket's
// buffers, a task, a slot vector and the pane views a frame is rendered into, and none of
// those is a frame. What they should do is **differ by the same amount whatever the app** —
// the gap is the per-connection floor, and the ledger is right exactly when everything that
// changes with the pipeline is on its side of it. Three apps whose ledgers are three orders
// of magnitude apart are what makes that a test rather than a coincidence.
//
// Method, beyond the held rig's:
//
//   * **`dagpane host`, not `run`.** The ledger lives in the host — `run` serves one app and
//     prices nothing — so this is the one session rig that has to route by `Host`.
//   * **The app is opened once and closed before rung 0.** The host compiles an app on its
//     first request, so without that the first rung would carry the sources as well as the
//     first viewer, and the slope would be weighing the load.
//   * **One process per app**, as in the held rig, so one app's allocator cannot hand another
//     app's freed pages to it.
//
//   node benches/sessions/ledger.mjs <dir> <app>

import { spawn } from "node:child_process";
import { request } from "node:http";
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { createHash, randomBytes } from "node:crypto";
import { sleep, weigh, median, slope, machine, provenance } from "./lib.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, "../..");
const DAGPANE = process.env.DAGPANE_BIN ?? join(ROOT, "target/release/dagpane");
const DIR = process.argv[2];
const APP = process.argv[3];
if (!DIR || !APP) {
  console.error("usage: node benches/sessions/ledger.mjs <dir> <app>");
  process.exit(2);
}
const PORT = Number(process.env.DAGPANE_PORT ?? 8820);
const ADMIN = Number(process.env.DAGPANE_ADMIN_PORT ?? PORT + 1000);

// The held rig's ladder, settle and samples, so the two rigs' RSS slopes are the same
// estimator over the same rungs and can be put side by side.
const LADDER = [0, 1, 2, 4, 8, 16, 32, 64, 128, 256, 512];
const SETTLE_MS = 700;
const SAMPLES = 5;
const OPEN_TIMEOUT_MS = 60_000;

/// One viewer of one app of a host: a socket upgraded by hand, and resolved on `init`.
///
/// **By hand because the built-in `WebSocket` cannot do this.** A host routes on the `Host`
/// header, and Node's `WebSocket` sends the URL's own authority whatever it is told — so
/// `ws://127.0.0.1` reaches no app and `ws://rows.localhost` depends on the resolver knowing
/// that name, which glibc's does not. `node:http` sends the header it is given.
///
/// On `init` and not on the upgrade, for the held rig's reason and one more: the connection
/// records its ledger entry after its first pass and before it sends `init`, so once `init`
/// has arrived the figure this rig is checking includes this viewer.
function openHostSession(port, app) {
  return new Promise((resolve, reject) => {
    let settled = false;
    const done = (fn, arg) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      fn(arg);
    };
    const timer = setTimeout(
      () => done(reject, new Error(`session sent no init within ${OPEN_TIMEOUT_MS} ms`)),
      OPEN_TIMEOUT_MS,
    );
    const req = request({
      host: "127.0.0.1",
      port,
      path: "/ws",
      headers: {
        Host: `${app}.localhost:${port}`,
        Connection: "Upgrade",
        Upgrade: "websocket",
        "Sec-WebSocket-Key": randomBytes(16).toString("base64"),
        "Sec-WebSocket-Version": "13",
      },
    });
    req.on("error", (e) => done(reject, new Error("session failed to connect: " + e.message)));
    req.on("response", (res) =>
      done(reject, new Error(`no upgrade: HTTP ${res.statusCode} for app ${app}`)),
    );
    req.on("upgrade", (_res, socket, head) => {
      // Server frames are unmasked and, from this server, never fragmented: a header, a
      // length in one of three widths, and the payload.
      let buf = head;
      const parse = () => {
        while (buf.length >= 2) {
          const op = buf[0] & 0x0f;
          let len = buf[1] & 0x7f;
          let at = 2;
          if (len === 126) {
            if (buf.length < 4) return;
            len = buf.readUInt16BE(2);
            at = 4;
          } else if (len === 127) {
            if (buf.length < 10) return;
            len = Number(buf.readBigUInt64BE(2));
            at = 10;
          }
          if (buf.length < at + len) return;
          const payload = buf.subarray(at, at + len);
          buf = buf.subarray(at + len);
          if (op === 8) return done(reject, new Error("session closed before init"));
          if (op === 1 && JSON.parse(payload.toString("utf8")).type === "init") {
            // Keep reading and discarding, so a viewer that is only holding never leaves the
            // server blocked on a write it stopped reading.
            socket.removeListener("data", onData);
            socket.on("data", () => {});
            return done(resolve, socket);
          }
        }
      };
      const onData = (chunk) => {
        buf = Buffer.concat([buf, chunk]);
        parse();
      };
      socket.on("data", onData);
      socket.on("error", (e) => done(reject, new Error("session socket: " + e.message)));
      socket.on("close", () => done(reject, new Error("session closed before init")));
      parse();
    });
    req.end();
  });
}

async function health() {
  const res = await fetch(`http://127.0.0.1:${ADMIN}/healthz`);
  if (!res.ok) throw new Error(`/healthz answered ${res.status}`);
  return res.json();
}

/// The app's entry in `/healthz`, once its session count is the one expected.
///
/// The count is checked rather than trusted: a ledger read at a rung that had not yet counted
/// its last viewer, or still counted the warm-up one, would put an off-by-one session into a
/// figure whose whole point is that it is exact.
async function entryWith(sessions) {
  for (let i = 0; i < 500; i++) {
    const h = await health();
    const entry = h.apps.find((a) => a.app === APP);
    if (entry && entry.sessions === sessions) return { h, entry };
    await sleep(10);
  }
  throw new Error(`/healthz never reported ${sessions} session(s) on ${APP}`);
}

async function waitReady(server) {
  for (let i = 0; i < 200; i++) {
    if (server.exitCode !== null) {
      throw new Error(`server exited with ${server.exitCode} before it listened`);
    }
    try {
      if ((await fetch(`http://127.0.0.1:${ADMIN}/readyz`)).ok) return;
    } catch {
      /* not listening yet */
    }
    await sleep(50);
  }
  throw new Error(`server never answered on ${ADMIN}`);
}

async function main() {
  // Read before the server does, for the held rig's reason: the digests identify what the
  // measured process loaded, not whatever was on disk when the ramp ended.
  const manifestPath = join(DIR, `${APP}.toml`);
  const manifestBytes = await readFile(manifestPath);
  const csvName = manifestBytes.toString("utf8").match(/^csv\s*=\s*"([^"]+)"/m)?.[1];
  if (!csvName) throw new Error(`${manifestPath} names no csv source`);
  const csvBytes = await readFile(join(DIR, csvName));

  // Heartbeat off, because these viewers answer no ping and a heartbeat is not what is being
  // weighed. The sweep off, because an app it evicted mid-ramp would leave the ledger in
  // `held_after_eviction` and this rig reading an entry that is no longer there.
  const argv = [
    "host", DIR,
    "--port", String(PORT),
    "--admin-port", String(ADMIN),
    "--admin-bind", "127.0.0.1",
    "--heartbeat-seconds", "0",
    "--idle-minutes", "0",
  ];
  const server = spawn(DAGPANE, argv, { stdio: "ignore" });
  const sessions = [];
  const rungs = [];
  try {
    await waitReady(server);
    const pid = server.pid;
    if ((await weigh(pid)).rss === null) throw new Error("no smaps_rollup — this rig needs Linux");

    // Compile and load the app, then let its only viewer go, so rung 0 is the app resident
    // with nobody looking at it.
    (await openHostSession(PORT, APP)).destroy();
    const { entry: loaded } = await entryWith(0);

    console.log(`\none app, one host process, ${SAMPLES} samples a rung, median reported\n`);
    console.log("  sessions        RSS     ledger   RSS/session   ledger/session");
    console.log("  ────────  ─────────  ─────────  ────────────  ───────────────");

    for (const want of LADDER) {
      while (sessions.length < want) sessions.push(await openHostSession(PORT, APP));
      await sleep(SETTLE_MS);

      const taken = [];
      for (let i = 0; i < SAMPLES; i++) {
        taken.push(await weigh(pid));
        await sleep(80);
      }
      const rss = median(taken.map((t) => t.rss));
      const pss = median(taken.map((t) => t.pss));
      // The ledger is exact, so one reading is the reading — but it is taken after the RSS
      // samples, so a viewer still being counted is waited for rather than missed.
      const { h, entry } = await entryWith(want);
      rungs.push({
        sessions: want,
        rss,
        pss,
        samples: taken,
        ledger_bytes: entry.session_bytes,
        source_bytes: entry.bytes,
        held_after_eviction: h.held_after_eviction.length,
      });

      const per = want === 0 ? null : (rss - rungs[0].rss) / want;
      const ledgerPer = want === 0 ? null : entry.session_bytes / want;
      console.log(
        `  ${String(want).padStart(8)}  ${(rss / 1048576).toFixed(1).padStart(7)} MB  ` +
          `${(entry.session_bytes / 1048576).toFixed(1).padStart(7)} MB  ` +
          (per === null ? "           —" : `${(per / 1024).toFixed(1).padStart(9)} kB`) +
          "  " +
          (ledgerPer === null ? "              —" : `${(ledgerPer / 1024).toFixed(1).padStart(12)} kB`),
      );
    }

    const held = rungs.filter((r) => r.sessions > 0);
    const xs = held.map((r) => r.sessions);
    const rssPer = slope(xs, held.map((r) => r.rss));
    const pssPer = slope(xs, held.map((r) => r.pss));
    const ledgerPer = slope(xs, held.map((r) => r.ledger_bytes));
    const top = held[held.length - 1];
    // Every viewer here is the same viewer — same app, same defaults, nothing moved — so the
    // ledger should be a straight line through the origin. Checked, not assumed: a lease that
    // leaked or a viewer counted twice would bend it.
    const exact = top.ledger_bytes / top.sessions;
    const straight = held.every((r) => r.ledger_bytes === exact * r.sessions);
    const unseen = rssPer - ledgerPer;

    console.log(`\n  marginal RSS per session      ${(rssPer / 1024).toFixed(2)} kB   (least squares)`);
    console.log(`  ledger per session            ${(ledgerPer / 1024).toFixed(2)} kB   ` +
      (straight ? "(the same for every viewer)" : "(NOT the same for every viewer)"));
    console.log(`  RSS the ledger does not see   ${(unseen / 1024).toFixed(2)} kB per session`);
    console.log(`  source bytes, the budget's    ${(loaded.bytes / 1048576).toFixed(2)} MB`);

    const out = {
      machine: await machine(),
      provenance: await provenance(DAGPANE, [DAGPANE, ...argv]),
      when: new Date().toISOString(),
      app: `fixture:${APP}.toml`,
      manifest_sha256: createHash("sha256").update(manifestBytes).digest("hex"),
      csv_bytes: csvBytes.length,
      csv_sha256: createHash("sha256").update(csvBytes).digest("hex"),
      settle_ms: SETTLE_MS,
      samples_per_rung: SAMPLES,
      rungs,
      source_bytes: loaded.bytes,
      marginal_rss_bytes_per_session: rssPer,
      marginal_pss_bytes_per_session: pssPer,
      ledger_bytes_per_session: ledgerPer,
      ledger_same_for_every_viewer: straight,
      unseen_rss_bytes_per_session: unseen,
      fixed_rss_bytes: rungs[0].rss,
    };
    await mkdir(join(HERE, "results"), { recursive: true });
    const tag = process.env.DAGPANE_TAG ?? `ledger-${APP}`;
    const path = join(HERE, "results", `${tag}-${new Date().toISOString().replace(/[:.]/g, "-")}.json`);
    await writeFile(path, JSON.stringify(out, null, 2));
    console.log(`\n  written to ${path.replace(ROOT + "/", "")}`);
  } finally {
    for (const socket of sessions) socket.destroy();
    server.kill("SIGKILL");
  }
}

await main();
