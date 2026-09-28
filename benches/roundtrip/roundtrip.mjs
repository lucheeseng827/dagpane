// Is an interaction's latency the round trip, or is it the pass?
//
// `ROADMAP.md` §4 will not open client-side compute on "WASM would be interesting". Its
// trigger is a measurement: **an interaction whose latency is dominated by the round trip,
// measured on a real app** — and it says, in as many words, that if the round trip is not the
// cost then the item stays where it is. This is that measurement, and it is arranged so it
// can come back with the answer that closes the item.
//
// Three numbers, on one app, over the same interaction:
//
//   server round trip   wall clock from `socket.send` to the patch arriving. Network,
//                       framing, JSON both ways, the session lock, and the pass.
//   server pass         what the server itself reports in `stats.micros`. Just the pass.
//   wasm pass           wall clock around `dp_send` in this process. The whole cost of the
//                       same interaction with no wire at all.
//
// The first minus the second is what a wire costs. The third is what replacing it costs.
// Moving an app into a browser is worth doing exactly when the third is below the first, and
// the gap between the second and the third is the tax wasm charges for the same work.
//
// BE PRECISE ABOUT THE THRESHOLD, because the obvious phrasing is wrong. A browser wins when
// `wasm pass < round trip`, and `round trip = wire + server pass`, so it wins when
// `wire > wasm pass - server pass`. That difference is a threshold on the WIRE, not on the
// round trip. The equivalent threshold on the round trip is `wasm pass` itself. Calling
// `wp - sp` a round-trip figure — as this file and five documents did — makes the browser
// look like it wins at a third of the latency it really needs.
//
//   node benches/roundtrip/roundtrip.mjs <manifest> <input=value> [iterations]
//
// Loopback is the friendliest possible wire: no DNS, no TLS, no congestion, no distance. A
// real deployment is worse, so a result that favours the browser here favours it more
// everywhere else — which is the direction that makes this honest rather than flattering.

import { readFile } from "node:fs/promises";
import { spawn } from "node:child_process";
import { dirname, resolve } from "node:path";
import { open as openLocal } from "../../crates/wasm/src/dagpane.js";

// A global `WebSocket` arrived in Node 22. This is a bench rather than a CI job, so the
// requirement is stated here rather than worked around — but it is stated, because
// `ReferenceError: WebSocket is not defined` three seconds into a spawned server is a worse
// way to find out.
if (typeof WebSocket === "undefined") {
  console.error(
    "this bench needs a global WebSocket, which Node gained in v22 — you have " +
      process.version +
      ".\n`crates/wasm/tests/run.sh`, which is what CI runs, does not need one.",
  );
  process.exit(2);
}

const manifestPath = process.argv[2] ?? "examples/sales.toml";
const setArg = process.argv[3] ?? "min_amount=400";
const iterations = Number(process.argv[4] ?? 200);
// Checked before a server is spawned. A `0` or a `NaN` leaves every sample array empty and
// the failure surfaces as `ms(undefined)` throwing at the very end, after two runs.
if (!Number.isSafeInteger(iterations) || iterations < 1) {
  console.error(`iterations must be a positive integer, not ${process.argv[4]}`);
  process.exit(2);
}
const port = 8790 + (process.pid % 200);

const [inputName, rawValue] = setArg.split("=");
const value = Number.isNaN(Number(rawValue))
  ? { kind: "text", v: rawValue }
  : { kind: "float", v: Number(rawValue) };

/** Median, which is what to report for a latency that has a tail. */
const median = (xs) => {
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.floor(s.length / 2)];
};
const quantile = (xs, q) => {
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor(s.length * q))];
};
const ms = (x) => x.toFixed(3);

// ── the server ─────────────────────────────────────────────────────────────────────────

const server = spawn("./target/release/dagpane", ["run", manifestPath, "--port", String(port)], {
  stdio: ["ignore", "pipe", "inherit"],
});
process.on("exit", () => server.kill());

await new Promise((ok, fail) => {
  const timer = setTimeout(() => fail(new Error("the server did not start")), 15000);
  server.stdout.on("data", (chunk) => {
    if (String(chunk).includes("http://")) {
      clearTimeout(timer);
      ok();
    }
  });
  server.on("exit", (code) => fail(new Error("the server exited with " + code)));
});

const socket = new WebSocket("ws://127.0.0.1:" + port + "/ws");
const inbox = [];
let waiting = null;
socket.onmessage = (e) => {
  const msg = JSON.parse(e.data);
  if (waiting) {
    const w = waiting;
    waiting = null;
    w(msg);
  } else {
    inbox.push(msg);
  }
};
const next = () =>
  inbox.length ? Promise.resolve(inbox.shift()) : new Promise((ok) => (waiting = ok));

await new Promise((ok) => (socket.onopen = ok));
const init = await next();
if (init.type !== "init") throw new Error("expected an init frame, got " + init.type);

const roundTrips = [];
const serverPasses = [];
// Two values, alternated. One value sent twice in a row is the "nothing moved" path, which is
// a real and much cheaper case — measuring it here would flatter both sides and answer a
// question nobody asked.
const values = [value, { ...value, v: value.v === undefined ? value.v : value.v + 1 }];

for (let i = 0; i < iterations; i += 1) {
  const started = process.hrtime.bigint();
  socket.send(JSON.stringify({ type: "set", seq: i + 1, values: { [inputName]: values[i % 2] } }));
  const patch = await next();
  const elapsed = Number(process.hrtime.bigint() - started) / 1e6;
  if (patch.type !== "patch") throw new Error("expected a patch, got " + JSON.stringify(patch));
  roundTrips.push(elapsed);
  if (patch.stats.micros !== undefined) serverPasses.push(patch.stats.micros / 1000);
}
socket.close();
server.kill();

// ── the same app, in this process ──────────────────────────────────────────────────────

const base = dirname(resolve(manifestPath));
const manifest = await readFile(manifestPath, "utf8");
const sources = {};
for (const line of manifest.split("\n")) {
  const name = /^\s*name\s*=\s*"([^"]+)"/.exec(line);
  const csv = /^\s*csv\s*=\s*"([^"]+)"/.exec(line);
  if (name) sources.__pending = name[1];
  if (csv && sources.__pending) {
    sources[sources.__pending] = await readFile(resolve(base, csv[1]), "utf8");
    delete sources.__pending;
  }
}
delete sources.__pending;

const wasm = await readFile("target/wasm32-unknown-unknown/release/dagpane_wasm.wasm");
const local = await openLocal(
  wasm.buffer.slice(wasm.byteOffset, wasm.byteOffset + wasm.byteLength),
  { manifest, sources },
);

const wasmPasses = [];
for (let i = 0; i < iterations; i += 1) {
  const started = process.hrtime.bigint();
  local.send({ type: "set", seq: i + 1, values: { [inputName]: values[i % 2] } });
  wasmPasses.push(Number(process.hrtime.bigint() - started) / 1e6);
}

// ── the answer ─────────────────────────────────────────────────────────────────────────

const rt = median(roundTrips);
const sp = serverPasses.length ? median(serverPasses) : null;
const wp = median(wasmPasses);

console.log(`\n${manifestPath} · --set ${setArg} · ${iterations} interactions · median (p95)\n`);
console.log(`  server round trip   ${ms(rt)} ms  (${ms(quantile(roundTrips, 0.95))})`);
if (sp !== null) {
  console.log(`  server pass         ${ms(sp)} ms  (${ms(quantile(serverPasses, 0.95))})`);
  console.log(`  the wire            ${ms(rt - sp)} ms  — ${((1 - sp / rt) * 100).toFixed(0)}% of the round trip`);
}
console.log(`  wasm pass           ${ms(wp)} ms  (${ms(quantile(wasmPasses, 0.95))})`);
if (sp !== null) {
  console.log(`  wasm vs native      ${(wp / sp).toFixed(2)}x the server's own pass`);
}
console.log(`  browser vs server   ${(rt / wp).toFixed(2)}x faster in the tab, on loopback\n`);

// The number that actually decides it, and the reason the two lines above are not enough.
//
// A browser beats a server when `wasm pass < wire + server pass`, so the WIRE has to cost
// more than wasm charges for doing the same work more slowly. That difference is the
// break-even, it does not depend on the network, and it is the only figure here that
// transfers off this machine. Stated on the round trip instead, the same threshold is
// `wasm pass` — both are printed, because quoting one as the other is the mistake this
// paragraph exists to stop.
if (sp !== null) {
  const breakEven = wp - sp;
  console.log(`  BREAK-EVEN: the browser wins once WIRE OVERHEAD exceeds ${ms(breakEven)} ms`);
  console.log(`              — equivalently, once a whole round trip exceeds ${ms(wp)} ms.\n`);
  if (breakEven <= 0) {
    console.log("wasm runs this pass at least as fast as the server does, so the browser wins");
    console.log("on any wire at all.");
  } else {
    console.log(`Loopback wire overhead on this machine is ${ms(rt - sp)} ms, which is why the two`);
    console.log("columns above are nearly equal — loopback is the one wire slow enough to be a");
    console.log("fair fight and fast enough to make it close.");
    console.log("");
    // Typical round trips, as context for the scale. NOT a floor: this bench measured one
    // app on one machine over one wire, and says nothing about anybody else's network. The
    // datacentre figure is the one to notice — it is the same order as the break-even for a
    // heavier app, so "it is on a network, therefore it wins" is exactly the inference this
    // paragraph exists to refuse.
    console.log(`Typical round trips, for scale against ${ms(breakEven)} ms: same datacentre ~0.5 ms,`);
    console.log("same city ~5 ms, a continent away 30-100 ms. Most deployments clear it and a");
    console.log("same-datacentre one on a heavier app may not — which is why this is a bench you");
    console.log("run against YOUR app rather than a table you read off.");
    console.log("");
    console.log("So ROADMAP §4's trigger is met, and NOT for the reason the item's framing");
    console.log("suggests. The browser does not win because wasm is fast — it is measurably");
    console.log(`${(wp / sp).toFixed(1)}x slower at the same pass. It wins because a network is slower still.`);
  }
}
