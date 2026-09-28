// What the two rigs in this directory share: the instrument, and the estimator.
//
// Extracted rather than copied, and not for brevity. `session-memory.mjs` weighs a viewer
// that is **holding** a socket; `busy-session.mjs` weighs one that is **passing**. The whole
// value of publishing both is that their two numbers can be put beside each other — and two
// copies of a least-squares slope are two numbers that agree right up until one is edited.
//
// Linux only, and deliberately: every reading here comes from `/proc`, which is the kernel's
// own accounting rather than a userspace guess.

import { readFile, writeFile, stat } from "node:fs/promises";
import { createHash } from "node:crypto";
import { spawn } from "node:child_process";
import { hostname } from "node:os";

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/// RSS and PSS in bytes, from the kernel rather than from `ps`.
///
/// PSS beside RSS because shared pages should not be counted twice. For one process the two
/// are close, and a gap between them is itself worth seeing.
export async function weigh(pid) {
  const text = await readFile(`/proc/${pid}/smaps_rollup`, "utf8");
  const field = (name) => {
    const m = text.match(new RegExp(`^${name}:\\s+(\\d+) kB`, "m"));
    return m ? Number(m[1]) * 1024 : null;
  };
  return { rss: field("Rss"), pss: field("Pss") };
}

/// The kernel's own high-water mark of this process's RSS, in bytes.
///
/// **This is the instrument that makes a transient measurable at all.** A sampler that reads
/// RSS every 50 ms cannot see an allocation that lives for 400 µs, and a pass over a frame is
/// nearer the second than the first — so a rig built on sampling would report that an
/// interaction costs nothing and would be wrong by however much it missed. `VmHWM` is
/// maintained by the kernel on every page fault, so the peak is observed whether or not
/// anybody was looking when it happened.
export async function peakRss(pid) {
  const text = await readFile(`/proc/${pid}/status`, "utf8");
  const m = text.match(/^VmHWM:\s+(\d+) kB/m);
  return m ? Number(m[1]) * 1024 : null;
}

/// Reset that high-water mark to the process's current RSS, so the next reading is the peak
/// of a named window rather than of the process's whole life.
///
/// `5` is `CLEAR_REFS_MM_HIWATER_RSS`. Without it `VmHWM` would still be carrying the first
/// rung's peak at the last rung, and every transient after the largest would read as zero.
export async function resetPeak(pid) {
  await writeFile(`/proc/${pid}/clear_refs`, "5");
}

export const median = (xs) => {
  const s = [...xs].sort((a, b) => a - b);
  return s.length % 2 ? s[(s.length - 1) / 2] : (s[s.length / 2 - 1] + s[s.length / 2]) / 2;
};

export const quantile = (xs, q) => {
  if (!xs.length) return null;
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor(q * s.length))];
};

/// Least-squares slope of y on x: the marginal cost over the whole measured range rather than
/// between two chosen points. A process's RSS includes an allocator that grows in steps and
/// does not shrink, so one subtraction at one N measures the step and calls it the session.
///
/// **It assumes the relationship is a line, and one of this directory's two rigs measures
/// something that is not one** — see `busy-session.mjs`, which reports a ceiling instead and
/// says why a slope would have been the wrong summary of it.
export function slope(xs, ys) {
  const n = xs.length;
  const mx = xs.reduce((a, b) => a + b, 0) / n;
  const my = ys.reduce((a, b) => a + b, 0) / n;
  let num = 0, den = 0;
  for (let i = 0; i < n; i++) {
    num += (xs[i] - mx) * (ys[i] - my);
    den += (xs[i] - mx) ** 2;
  }
  return den === 0 ? 0 : num / den;
}

/// How long a session may take to send its opening frame before the rig gives up on it.
///
/// Generous on purpose: opening the sixty-fifth session of a 200 000-row app means the server
/// is running that pipeline for the sixty-fifth time, and on a slow allocator that is seconds.
/// It is a deadlock bound, not a latency budget.
const OPEN_TIMEOUT_MS = 60_000;

/// One viewer: a socket that opens and waits for its opening frame.
///
/// Resolving on `init` rather than on the upgrade matters — a socket that has connected but
/// not been answered has not yet cost the server a session, and counting it would put the
/// rung's memory reading ahead of the sessions it claims to be weighing.
///
/// **Three ways this can end and all three are handled**, which they were not at first. A
/// socket that fails to connect raises `error`. A socket the server closes cleanly raises
/// `close` and **no** `error` — so without that listener the promise stayed pending and the
/// whole rig hung on a server that had decided to go away. And a socket that opens and simply
/// never says `init` raises neither, which is what the timeout is for. Every path clears the
/// others, so a late event cannot settle an already-settled promise.
export function openSession(port) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(`ws://127.0.0.1:${port}/ws`);
    let settled = false;
    const done = (fn, arg) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      ws.removeEventListener("error", onError);
      ws.removeEventListener("close", onClose);
      ws.removeEventListener("message", onMessage);
      fn(arg);
    };
    const onError = (e) =>
      done(reject, new Error("session failed to connect: " + (e?.message ?? "error")));
    const onClose = (e) =>
      done(reject, new Error(`session closed before init (code ${e?.code ?? "?"})`));
    const onMessage = (e) => {
      const msg = JSON.parse(e.data);
      if (msg.type === "init") done(resolve, ws);
    };
    const timer = setTimeout(
      () => done(reject, new Error(`session sent no init within ${OPEN_TIMEOUT_MS} ms`)),
      OPEN_TIMEOUT_MS,
    );
    ws.addEventListener("error", onError);
    ws.addEventListener("close", onClose);
    ws.addEventListener("message", onMessage);
  });
}

/// Wait for the server to answer, and fail loudly rather than hang if it never does.
///
/// It checks the child is still alive between polls: a server that died on startup — a port
/// already taken, a manifest that does not compile — otherwise leaves the rig polling a
/// closed port for its whole timeout and then blaming the timeout.
export async function waitReady(server, port, tries = 200) {
  for (let i = 0; i < tries; i++) {
    if (server.exitCode !== null) {
      throw new Error(`server exited with ${server.exitCode} before it listened`);
    }
    try {
      if ((await fetch(`http://127.0.0.1:${port}/`)).ok) return;
    } catch {
      await sleep(50);
    }
  }
  throw new Error(`server never answered on ${port}`);
}

/// What machine this was, recorded beside every result because none of these absolutes
/// travel between machines and the ratios are what does.
export async function machine() {
  const cpuinfo = await readFile("/proc/cpuinfo", "utf8");
  const model = cpuinfo.match(/^model name\s*:\s*(.+)$/m);
  const mem = (await readFile("/proc/meminfo", "utf8")).match(/^MemTotal:\s+(\d+) kB/m);
  return {
    host: hostname(),
    cpus: cpuinfo.split(/^processor\s*:/m).length - 1,
    cpu_model: model ? model[1].trim() : null,
    mem_total_bytes: mem ? Number(mem[1]) * 1024 : null,
    kernel: (await readFile("/proc/version", "utf8")).trim(),
  };
}

/// What produced a result, recorded beside it because a number without its build is a number
/// nobody can check.
///
/// **The allocator field is not housekeeping.** This directory's own musl spot check found that
/// whether a burst's memory comes back is a property of the C library and not of this runtime —
/// so two result files can disagree completely, be equally correct, and look identical unless
/// something wrote down which binary produced which. `ldd` on the binary is the cheapest
/// honest answer: a static musl build says so, a glibc build names its loader.
export async function provenance(binary, argv) {
  // `ok` matters as much as `out`. A probe that could not start and a probe that ran and printed
  // nothing are the same empty string, and collapsing them let a failed `git status` be recorded
  // as a clean worktree — a provenance field asserting something it had not checked.
  const run = (cmd, args) =>
    new Promise((resolve) => {
      const p = spawn(cmd, args, { stdio: ["ignore", "pipe", "ignore"] });
      let out = "";
      p.stdout.on("data", (d) => { out += d; });
      p.on("close", (code) => resolve({ ok: code === 0, out: out.trim() }));
      p.on("error", () => resolve({ ok: false, out: null }));
    });
  let size = null;
  try {
    size = (await stat(binary)).size;
  } catch { /* the caller's own spawn will report a missing binary far more clearly */ }
  const linkage = (await run("ldd", [binary])).out;
  // The digest is the only field here that actually identifies the executable. A path, a size and
  // a revision can all be equal across different bytes — a rebuild with a changed dependency, a
  // dirty tree, a stale target directory — so without this a committed result cannot be checked
  // against the build that produced it. Taken from the same file the rig is about to spawn.
  let sha256 = null;
  try {
    sha256 = createHash("sha256").update(await readFile(binary)).digest("hex");
  } catch { /* `binary_bytes` already records whether the file was readable */ }
  // **Detecting the libc is harder than it looks, and the first version of this got it exactly
  // backwards.** It searched the binary for "musl" and fell back to "glibc"; the real static
  // musl build this project ships contains no such string — it contains no libc markers at all
  // — so it was reported as glibc, which is worse than reporting nothing. glibc's own `ldd`
  // calls it merely "statically linked" and names no libc either.
  //
  // What is actually reliable is the other direction: a glibc build leaves `GLIBC`, `ld-linux`
  // and `__libc_start_main` in the file, and a fully static one leaves none of them. So glibc is
  // detected positively, "static" is detected positively, and musl is an INFERENCE from the
  // target triple in the path — labelled as one, with everything it rests on recorded beside it.
  let markers = null;
  try {
    const bytes = await readFile(binary, "latin1");
    markers = {
      glibc: /GLIBC|ld-linux|__libc_start_main/.test(bytes),
      musl: /musl|ld-musl/i.test(bytes),
    };
  } catch { /* `binary_bytes` above already records whether the file was readable */ }
  const revision = await run("git", ["rev-parse", "HEAD"]);
  // TRACKED files only, and this is not a nicety either. A plain `git status --porcelain`
  // counts untracked files, and this rig writes its results into the working tree — so the
  // second ramp of a run saw the first ramp's own output and recorded `dirty: true`, which
  // reads as "the sources were modified" and was not true of anything. The question the field
  // is asking is whether the code that produced the number matches the revision beside it, and
  // that is a question about tracked files. What was untracked is reported separately rather
  // than dropped, because "the rig wrote its results" and "somebody left a file here" look the
  // same from inside and only one of them is uninteresting.
  const status = await run("git", ["status", "--porcelain", "--untracked-files=no"]);
  const untracked = await run("git", ["ls-files", "--others", "--exclude-standard"]);
  const isStatic = !!linkage && /statically linked|not a dynamic executable/i.test(linkage);
  // Any target triple, not just this host's: an `aarch64-unknown-linux-musl` or
  // `armv7-unknown-linux-musleabihf` build is the same inference, and a rig that reported
  // "glibc" for one of them would be wrong in the direction that matters most here.
  const tripleSaysMusl = /(^|\/)[a-z0-9_]+-[a-z0-9_]+-linux-musl[a-z0-9]*(\/|$)/i.test(binary);
  const libc =
    markers?.glibc || (linkage && /ld-linux|libc\.so\.6/.test(linkage))
      ? "glibc"
      : markers?.musl || (linkage && /musl/i.test(linkage))
        ? "musl"
        : isStatic && tripleSaysMusl
          ? "musl (inferred from the target triple; the binary carries no libc marker)"
          : isStatic
            ? "static, libc not identified"
            : "unknown";
  return {
    command: argv.join(" "),
    binary,
    binary_bytes: size,
    binary_sha256: sha256,
    libc,
    linkage,
    libc_evidence: {
      ldd: linkage ? linkage.split("\n")[0] : null,
      static: isStatic,
      glibc_markers_in_binary: markers ? markers.glibc : null,
      musl_markers_in_binary: markers ? markers.musl : null,
      path_names_musl_triple: tripleSaysMusl,
    },
    revision: revision.ok ? revision.out || null : null,
    // `null` rather than `false` when the probe itself failed: "the tree was clean" and "nobody
    // could tell" are different facts and only one of them was established.
    dirty: status.ok ? status.out.length > 0 : null,
    untracked_files: untracked.ok ? untracked.out.split("\n").filter(Boolean).length : null,
    node: process.version,
    // The glibc knobs that change what this directory measures. Null is the default and is
    // worth recording as null rather than omitting: a reader cannot tell "unset" from
    // "nobody thought to look".
    malloc_env: Object.fromEntries(
      ["MALLOC_ARENA_MAX", "MALLOC_TRIM_THRESHOLD_", "MALLOC_MMAP_THRESHOLD_"]
        .map((k) => [k, process.env[k] ?? null]),
    ),
  };
}
