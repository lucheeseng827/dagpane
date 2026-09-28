// Does the page come back when the server does?
//
// `cargo test` can drive the socket, and `tests/heartbeat.rs` does. It cannot drive the part
// that matters here, which is a **browser** deciding what to do about a closed one — that
// logic lives in `client.html` and nothing in this repository had ever executed it.
//
// The property, stated so it can fail: a viewer whose replica goes away gets their page back
// **without touching it**. Not a banner telling them to reload; not a page that renders and
// answers nothing. The panes repaint and the controls still work.
//
// That is exactly what a rolling update does to every open connection on purpose, so this is
// the test that decides whether `dagpane serve`'s drain buys a viewer anything at all.
//
// Real Chromium over the DevTools Protocol, driven from bare Node with **no npm dependency**
// — Node 22 has a WebSocket, and the browser is the one already on the machine. The same rule
// the rest of this project follows: no package manager in the way of running the tests.
//
//   node crates/serve/tests/reconnect.mjs <path-to-dagpane> [chrome]

import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const HERE = dirname(fileURLToPath(import.meta.url));
const MANIFEST = join(HERE, "../../../examples/sales.toml");
const DAGPANE = process.argv[2];
const CHROME =
  process.argv[3] ??
  process.env.DAGPANE_CHROME ??
  "/opt/pw-browsers/chromium-1194/chrome-linux/chrome";

if (!DAGPANE) {
  console.error("usage: node reconnect.mjs <path-to-dagpane> [chrome]");
  process.exit(2);
}

// A port of our own. The server is killed and restarted on it, which is the whole point, so
// it cannot be an ephemeral one the OS picks twice.
const APP_PORT = 8791;
const CDP_PORT = 9334;
const AUTH_PORT = 8792;
const AUTH_OK_PORT = 8793;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failures = 0;
function ok(what) { console.log(`  ok   ${what}`); }
function bad(what, detail) { console.log(`  FAIL ${what}\n       ${detail}`); failures += 1; }

// ── the server, which this test starts and stops on purpose ─────────────────────────────

let server = null;
async function startServer() {
  // The port has to be FREE first. This waits on a port rather than on a process, so anything
  // else already listening there would be mistaken for our server — the test would then kill a
  // child that never bound, watch a stranger's server keep serving, and hang waiting for a
  // disconnection that is never coming. A stray from an interrupted run is exactly how that
  // happens, and it did.
  try {
    await fetch(`http://127.0.0.1:${APP_PORT}/`);
    throw new Error(
      `something is already listening on ${APP_PORT}. This test owns that port — it stops and ` +
      `restarts the server on it — so it cannot share. Kill the stray and re-run.`
    );
  } catch (e) {
    if (e instanceof Error && e.message.startsWith("something is already")) throw e;
    // Anything else is the connection being refused, which is what we want.
  }

  server = spawn(DAGPANE, ["run", MANIFEST, "--port", String(APP_PORT)], { stdio: "ignore" });
  for (let i = 0; i < 200; i++) {
    if (server.exitCode !== null) {
      throw new Error(`the server exited with ${server.exitCode} instead of serving`);
    }
    try {
      const r = await fetch(`http://127.0.0.1:${APP_PORT}/`);
      if (r.ok) return;
    } catch { /* not up yet */ }
    await sleep(50);
  }
  throw new Error("the server never began serving");
}

async function stopServer() {
  if (!server) return;
  const dead = new Promise((r) => server.once("exit", r));
  server.kill("SIGKILL");     // abrupt on purpose: a reclaimed spot instance is not graceful
  await dead;
  server = null;
  // And make sure the port is actually free, or the "restart" would race the old listener.
  for (let i = 0; i < 100; i++) {
    try { await fetch(`http://127.0.0.1:${APP_PORT}/`); await sleep(50); }
    catch { return; }
  }
}

// ── the browser ──────────────────────────────────────────────────────────────────────────

const browser = spawn(CHROME, [
  "--headless=new", `--remote-debugging-port=${CDP_PORT}`, "--no-sandbox",
  "--disable-gpu", "--disable-dev-shm-usage", "about:blank",
], { stdio: "ignore" });

async function debuggerUrl() {
  for (let i = 0; i < 200; i++) {
    try { return (await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/version`)).json()).webSocketDebuggerUrl; }
    catch { await sleep(100); }
  }
  throw new Error("chromium never opened its debug port");
}

let ws, nextId = 0, sessionId;
const pending = new Map();
function call(method, params = {}) {
  return new Promise((res, rej) => {
    const id = ++nextId;
    pending.set(id, (m) => (m.error ? rej(new Error(`${method}: ${m.error.message}`)) : res(m.result)));
    ws.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
  });
}

async function evaluate(expression) {
  const r = await call("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
  if (r.exceptionDetails) throw new Error(r.exceptionDetails.text + " — " + expression);
  return r.result.value;
}

/// Poll a boolean expression in the page until it holds, or give up.
async function until(expression, ms, what) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    if (await evaluate(expression)) return true;
    await sleep(100);
  }
  bad(what, `never became true within ${ms}ms: ${expression}`);
  return false;
}

// What the page shows, as a person would read it.
const PANE_COUNT = "document.querySelectorAll('.pane').length";
const BANNER = "(() => { const b = document.getElementById('offline'); " +
  "return b.style.display === 'none' ? '' : b.textContent; })()";

async function main() {
  await startServer();
  const url = await debuggerUrl();
  ws = new WebSocket(url);
  await new Promise((r) => ws.addEventListener("open", r, { once: true }));
  ws.addEventListener("message", (e) => {
    const m = JSON.parse(e.data);
    if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); }
  });
  const { targetId } = await call("Target.createTarget", { url: "about:blank" });
  ({ sessionId } = await call("Target.attachToTarget", { targetId, flatten: true }));

  console.log("\na page whose server goes away and comes back");

  await call("Page.navigate", { url: `http://127.0.0.1:${APP_PORT}/` });
  if (!(await until(`${PANE_COUNT} >= 7`, 20000, "the app renders"))) return;
  ok("the app renders");

  // Move a control, so the page has state worth keeping and `saved` is non-empty. That is
  // what rides the reconnect's query string.
  await evaluate(`
    (() => {
      const s = document.querySelector('input[type=range]');
      s.value = 400; s.dispatchEvent(new Event('input', { bubbles: true }));
    })()
  `);
  if (!(await until("location.hash.includes('s=')", 5000, "the control moved"))) return;
  ok("the control moved, and the viewer's state is in the URL");
  const before = await evaluate(PANE_COUNT);

  // ── the replica goes away, mid-pass ───────────────────────────────────────────────────
  //
  // `inFlight` is set while a `Set` is waiting for its answer, and it is cleared only BY that
  // answer. A socket that dies in that window leaves it set forever, and `flush` refuses to
  // send while it is — so the page reconnects, repaints, and then silently ignores every
  // control for the rest of its life. That is worse than the banner it replaced, because it
  // looks like success.
  //
  // Set the flag rather than racing for it. The race is real and this is exactly the state it
  // produces; reproducing it by timing would make this test flaky about the one thing it is
  // most worth being sure of.
  await evaluate("inFlight = true");
  await stopServer();
  if (!(await until(`${BANNER}.includes('Reconnecting')`, 10000, "the page says it is reconnecting")))
    return;
  ok("the page says it is reconnecting, not merely disconnected");

  // The page is still readable while it is gone — the last state the server sent.
  if ((await evaluate(PANE_COUNT)) === before) ok("the panes stay on screen while it is away");
  else bad("the panes stay on screen while it is away", "they disappeared");

  // ── and comes back ────────────────────────────────────────────────────────────────────
  await startServer();
  if (!(await until(`${BANNER} === ''`, 40000, "the page reconnects on its own")))
    return;
  ok("the page reconnects on its own — no reload, no click");

  if (!(await until(`${PANE_COUNT} >= 7`, 10000, "the panes come back"))) return;
  ok("the panes come back");

  // THE assertion. `inFlight` is only cleared by a reply, so a socket dropped mid-pass
  // leaves it set — and `flush` refuses to send while it is. A page that reconnected but
  // could not send would render perfectly and answer nothing, which is a worse failure than
  // the banner it replaced, because it looks like success.
  const seqBefore = await evaluate("seq");
  await evaluate(`
    (() => {
      const s = document.querySelector('input[type=range]');
      s.value = 600; s.dispatchEvent(new Event('input', { bubbles: true }));
    })()
  `);
  if (!(await until(`seq > ${seqBefore}`, 10000, "a control still works after reconnecting")))
    return;
  if (!(await until("inFlight === false", 10000, "and the server answered it"))) return;
  ok("a control still works after reconnecting, and the server answered it");
}

// ── the one close that must not retry ───────────────────────────────────────────────────
//
// Everything above is about coming back. This is about the case that must not: a page holding
// a token the server will not accept. Retrying that on a timer hammers a door that is never
// going to open, and buries the one message that tells the viewer what to do about it.
//
// The refusal is real rather than simulated — a front door with a generated key set, and a
// token that is not signed by it.

import { generateKeyPairSync, createSign } from "node:crypto";
import { writeFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";

/// A JWT the server will actually accept, signed with the key set it was given.
///
/// Minted here rather than mocked because the case below needs the door to OPEN — a page that
/// gets in, loses its server, and has to get back in. A bogus token only exercises the refusal.
function mintToken(privateKey, { iss, aud }) {
  const b64 = (o) =>
    Buffer.from(typeof o === "string" ? o : JSON.stringify(o))
      .toString("base64url");
  const now = Math.floor(Date.now() / 1000);
  const head = b64({ alg: "ES256", kid: "test", typ: "JWT" });
  const body = b64({ iss, aud, sub: "viewer@example", iat: now - 5, exp: now + 3600 });
  // ES256 wants the raw r||s pair; Node's default for EC is DER, so this is not optional.
  const sig = createSign("sha256")
    .update(`${head}.${body}`)
    .sign({ key: privateKey, dsaEncoding: "ieee-p1363" })
    .toString("base64url");
  return `${head}.${body}.${sig}`;
}

/// A front door, and a page that gets through it.
async function authenticatedPageReconnects() {
  console.log("\nan authenticated page whose server restarts");

  const dir = mkdtempSync(join(tmpdir(), "dagpane-jwks-ok-"));
  const jwksPath = join(dir, "jwks.json");
  const { publicKey, privateKey } = generateKeyPairSync("ec", { namedCurve: "P-256" });
  writeFileSync(jwksPath, JSON.stringify({
    keys: [{ ...publicKey.export({ format: "jwk" }), kid: "test", alg: "ES256", use: "sig" }],
  }));
  const ISS = "https://issuer.example";
  const AUD = "dagpane";
  const token = mintToken(privateKey, { iss: ISS, aud: AUD });

  const args = [
    "run", MANIFEST, "--port", String(AUTH_OK_PORT),
    "--auth-jwks", jwksPath, "--auth-issuer", ISS, "--auth-audience", AUD, "--auth-any-app",
  ];
  let guarded = null;
  const up = async () => {
    for (let i = 0; i < 200; i++) {
      try { if ((await fetch(`http://127.0.0.1:${AUTH_OK_PORT}/`)).ok) return true; }
      catch { await sleep(50); }
    }
    return false;
  };

  try {
    guarded = spawn(DAGPANE, args, { stdio: "ignore" });
    if (!(await up())) { bad("the guarded server starts", "it never began serving"); return; }

    await call("Page.navigate", { url: `http://127.0.0.1:${AUTH_OK_PORT}/` });
    await until("document.readyState === 'complete'", 10000, "the guarded page loads");
    await evaluate(`sessionStorage.setItem('dagpane.token', ${JSON.stringify(token)}); true`);
    await call("Page.reload");

    if (!(await until(`${PANE_COUNT} >= 7`, 20000, "the authenticated page renders"))) return;
    ok("a real token opens the door and the app renders");

    // The server goes. This is the ONLY difference from the unauthenticated case, and it used
    // to be the difference between reconnecting and not: `dropped` saw a socket that never
    // opened on a page holding a token and called that a refusal, so an authenticated viewer
    // stopped retrying the moment their server restarted — the exact case this whole feature
    // exists for, broken for exactly the deployments that have a front door.
    const dead = new Promise((r) => guarded.once("exit", r));
    guarded.kill("SIGKILL");
    await dead;
    guarded = null;

    if (!(await until(`${BANNER}.includes('Reconnecting')`, 10000,
      "an authenticated page retries rather than declaring itself refused"))) return;

    // **Stay down.** The first backoff is a quarter of a second, so a server that comes back
    // immediately is reconnected to by the retry that was already scheduled — and the attempt
    // that runs while nothing is listening never happens. That attempt is the whole bug: it
    // opens no socket, and a page holding a token used to read that as a refusal and stop.
    // Three seconds is several backoffs.
    await sleep(3000);
    const midOutage = await evaluate(BANNER);
    if (!midOutage.includes("Reconnecting")) {
      bad("it keeps retrying while the server is away",
          `after 3s of outage the banner read: ${midOutage}`);
      return;
    }
    ok("it keeps retrying while the server is away, across several attempts");

    guarded = spawn(DAGPANE, args, { stdio: "ignore" });
    if (!(await up())) { bad("the guarded server restarts", "it never came back"); return; }
    if (!(await until(`${BANNER} === ''`, 40000, "the authenticated page reconnects"))) return;
    if (!(await until(`${PANE_COUNT} >= 7`, 10000, "its panes come back"))) return;
    ok("it reconnects on its own, with the token it still holds");
  } finally {
    if (guarded) {
      const dead = new Promise((r) => guarded.once("exit", r));
      guarded.kill("SIGKILL");
      await dead;
    }
  }
}

async function refusalDoesNotRetry() {
  console.log("\na page the server refuses");

  const dir = mkdtempSync(join(tmpdir(), "dagpane-jwks-"));
  const jwksPath = join(dir, "jwks.json");
  // A real, well-formed key set. Nothing is ever signed with it: the point is that the token
  // the page presents is NOT from it, so the door refuses and the upgrade never opens.
  const { publicKey } = generateKeyPairSync("ec", { namedCurve: "P-256" });
  const jwk = publicKey.export({ format: "jwk" });
  writeFileSync(jwksPath, JSON.stringify({
    keys: [{ ...jwk, kid: "test", alg: "ES256", use: "sig" }],
  }));

  const guarded = spawn(DAGPANE, [
    "run", MANIFEST, "--port", String(AUTH_PORT),
    "--auth-jwks", jwksPath,
    "--auth-issuer", "https://issuer.example",
    "--auth-audience", "dagpane",
    "--auth-any-app",
  ], { stdio: "ignore" });

  try {
    let up = false;
    for (let i = 0; i < 200 && !up; i++) {
      try { up = (await fetch(`http://127.0.0.1:${AUTH_PORT}/`)).ok; } catch { await sleep(50); }
    }
    if (!up) { bad("the guarded server starts", "it never began serving"); return; }

    // Plant a token the door will not take, on the origin the page will run at, before the
    // page runs. Same storage the sign-in flow writes to.
    await call("Page.navigate", { url: `http://127.0.0.1:${AUTH_PORT}/` });
    await until("document.readyState === 'complete'", 10000, "the guarded page loads");
    await evaluate("sessionStorage.setItem('dagpane.token', 'not.a.real.token'); true");
    await call("Page.reload");

    if (!(await until(`${BANNER}.includes('refused')`, 15000, "the page says it was refused")))
      return;
    ok("the page says it was refused, and offers a sign-in");

    // And it stays that way. A retry would replace this banner with "Reconnecting in Ns"
    // within the first backoff, which is under a second.
    await sleep(4000);
    const banner = await evaluate(BANNER);
    if (banner.includes("Reconnecting")) {
      bad("a refusal is not retried", `the page began retrying: ${banner}`);
    } else if (banner.includes("refused")) {
      ok("a refusal is not retried on a timer");
    } else {
      bad("a refusal is not retried", `unexpected banner: ${banner}`);
    }
  } finally {
    guarded.kill("SIGKILL");
    await new Promise((r) => guarded.once("exit", r));
  }
}

try {
  await main();
  await authenticatedPageReconnects();
  await refusalDoesNotRetry();
} catch (e) {
  bad("the test itself", e.message);
} finally {
  try { ws?.close(); } catch { /* already gone */ }
  browser.kill();
  await stopServer();
}

console.log(failures ? `\n${failures} check(s) failed` : "\nall checks passed");
process.exit(failures ? 1 : 0);
