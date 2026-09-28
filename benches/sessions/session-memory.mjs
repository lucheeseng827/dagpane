// What does one more viewer of the SAME app cost?
//
// `ROADMAP.md` §7's first item, and the only one of its four that `BENCHMARKS.md` did not
// already answer. The fleet rig measures the marginal cost of one more **app**; the loadgen
// rig measures how many apps one core can serve. Neither weighs a **session**, and §7 names
// that gap precisely:
//
//   > `Arc::ptr_eq` on an untouched source across two sessions is asserted by a test rather
//   > than inferred from an RSS reading.
//
// So the sharing this project's whole memory story rests on — *a hundred viewers of a 600-row
// app are a hundred slot vectors over one table, not a hundred copies of it* — was proven
// structurally and never weighed. This weighs it.
//
// Method, and what makes it narrow enough to believe:
//
//   * **One app, one process, one `Arc<App>`.** `dagpane run`, not `host`: a fleet would mix
//     the per-app term into the per-session one, and the per-app term is already measured.
//   * **Idle sessions.** The question is what holding a viewer costs, not what serving one
//     costs. An interaction allocates transiently and would be measured as noise.
//   * **Ramped, and the answer is the slope rather than any single reading.** A process's RSS
//     includes an allocator that grows in steps and does not shrink; one subtraction at one N
//     would measure the step and call it the session.
//   * **PSS beside RSS**, read from `smaps_rollup`, for the same reason the fleet rig reports
//     both: shared pages should not be counted twice. For one process they are close, and a
//     gap between them is itself worth seeing.
//
// The instrument and the estimator are in `lib.mjs`, shared with `busy-session.mjs` — which
// weighs the same viewer while it is *interacting*, and is the other half of this answer.
//
//   node benches/sessions/session-memory.mjs [path-to-dagpane]

import { spawn } from "node:child_process";
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, join, resolve } from "node:path";
import { createHash } from "node:crypto";
import { sleep, weigh, median, slope, openSession, waitReady, machine, provenance } from "./lib.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, "../..");
const DAGPANE = process.env.DAGPANE_BIN ?? join(ROOT, "target/release/dagpane");
// The app under test. Given, because the decisive experiment is running the SAME ramp against
// two apps whose data differs by two orders of magnitude — see the header.
const MANIFEST = process.argv[2] ?? join(ROOT, "examples/sales.toml");
const PORT = Number(process.env.DAGPANE_PORT ?? 8794);
/// A stable name for the app, because two of the three ramps are generated into a `mktemp -d`
/// that is gone by the time anybody reads the result. The digest beside it is the real identity.
const APP_PATH = resolve(MANIFEST);
const APP_ID = APP_PATH.startsWith(ROOT + "/")
  ? APP_PATH.slice(ROOT.length + 1)
  : `fixture:${APP_PATH.split("/").pop()}`;

// The ladder. Doubling, because the slope is what is wanted and a doubling ladder spends its
// samples where the curve could bend rather than spreading them evenly over a straight line.
const LADDER = [0, 1, 2, 4, 8, 16, 32, 64, 128, 256, 512];
const SETTLE_MS = 700;      // let the accept loop and the allocator finish before weighing
const SAMPLES = 5;          // per rung, and the median is taken

async function main() {
  // Read the inputs BEFORE the server does. These were hashed after the ramps finished, which
  // meant the digest identified whatever was on disk at the end rather than what the measured
  // process loaded at the start. Nothing here rewrites them, but a result that claims to
  // identify its inputs should not depend on that being true of everything else on the machine.
  const manifestBytes = await readFile(MANIFEST);
  const csvBytes = await readFile(join(dirname(MANIFEST), "sales.csv"));

  const server = spawn(DAGPANE, ["run", MANIFEST, "--port", String(PORT)], { stdio: "ignore" });
  const sessions = [];
  const rungs = [];
  try {
    await waitReady(server, PORT);
    const pid = server.pid;
    const first = await weigh(pid);
    if (first.rss === null) throw new Error("no smaps_rollup — this rig needs Linux");

    console.log(`\none app, one process, ${SAMPLES} samples a rung, median reported\n`);
    console.log("  sessions        RSS        PSS   RSS/session");
    console.log("  ────────  ─────────  ─────────  ────────────");

    for (const want of LADDER) {
      while (sessions.length < want) sessions.push(await openSession(PORT));
      await sleep(SETTLE_MS);

      const taken = [];
      for (let i = 0; i < SAMPLES; i++) {
        taken.push(await weigh(pid));
        await sleep(80);
      }
      const rss = median(taken.map((t) => t.rss));
      const pss = median(taken.map((t) => t.pss));
      // The readings behind the medians, so a reader can recompute them rather than trust them.
      rungs.push({ sessions: want, rss, pss, samples: taken });

      const per = want === 0 ? null : (rss - rungs[0].rss) / want;
      console.log(
        `  ${String(want).padStart(8)}  ${(rss / 1048576).toFixed(1).padStart(7)} MB  ` +
        `${(pss / 1048576).toFixed(1).padStart(7)} MB  ` +
        (per === null ? "           —" : `${(per / 1024).toFixed(1).padStart(9)} kB`)
      );
    }

    const held = rungs.filter((r) => r.sessions > 0);
    const rssPer = slope(held.map((r) => r.sessions), held.map((r) => r.rss));
    const pssPer = slope(held.map((r) => r.sessions), held.map((r) => r.pss));
    const fixed = rungs[0].rss;

    console.log(`\n  marginal RSS per session   ${(rssPer / 1024).toFixed(2)} kB   (least squares)`);
    console.log(`  marginal PSS per session   ${(pssPer / 1024).toFixed(2)} kB`);
    console.log(`  fixed cost, no sessions    ${(fixed / 1048576).toFixed(1)} MB`);

    // The comparison the claim is actually about. A session that COPIED the app's sources
    // would cost at least this much; the bundled example's CSV is the thing being shared.
    const csv = csvBytes.length;
    console.log(
      `\n  the CSV behind the app is ${(csv / 1024).toFixed(1)} kB. A session that copied it ` +
      `would cost\n  at least that; measured is ${(rssPer / csv).toFixed(3)}× of it.`
    );

    const out = {
      machine: await machine(),
      provenance: await provenance(DAGPANE, [DAGPANE, "run", MANIFEST, "--port", String(PORT)]),
      when: new Date().toISOString(),
      app: APP_ID,
      manifest_sha256: createHash("sha256").update(manifestBytes).digest("hex"),
      // The manifest digest alone does not identify the app here: two of the three ramps run
      // the SAME manifest over CSVs two orders of magnitude apart, which is the experiment.
      csv_bytes: csv,
      csv_sha256: createHash("sha256").update(csvBytes).digest("hex"),
      settle_ms: SETTLE_MS,
      samples_per_rung: SAMPLES,
      rungs,
      marginal_rss_bytes_per_session: rssPer,
      marginal_pss_bytes_per_session: pssPer,
      fixed_rss_bytes: fixed,
    };
    await mkdir(join(HERE, "results"), { recursive: true });
    const tag = process.env.DAGPANE_TAG ?? "sessions";
    const path = join(HERE, "results", `${tag}-${new Date().toISOString().replace(/[:.]/g, "-")}.json`);
    await writeFile(path, JSON.stringify(out, null, 2));
    console.log(`\n  written to ${path.replace(ROOT + "/", "")}`);
  } finally {
    for (const ws of sessions) { try { ws.close(); } catch { /* going away anyway */ } }
    server.kill("SIGKILL");
  }
}

await main();
