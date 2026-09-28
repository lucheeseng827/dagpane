// Does the artefact actually run?
//
// `cargo build --target wasm32-unknown-unknown` proves the code compiles. It does not prove
// that four `extern "C"` exports, a hand-written length prefix and a hand-written allocator
// agree with the JavaScript that calls them — and that seam is exactly where a hand-rolled
// ABI goes wrong. `benches/wasm-engines/README.md` already learnt this once in the other
// direction: a `cargo check` is not a measurement.
//
// So this instantiates the real module in a real WebAssembly engine, drives a real app
// through it, and asserts the counts the rest of the project asserts. Node is the runtime
// because it is a V8 with no browser attached; nothing here is Node-specific but `readFile`
// and the exit code.
//
//   node crates/wasm/tests/smoke.mjs target/wasm32-unknown-unknown/release/dagpane_wasm.wasm

import { readFile } from "node:fs/promises";
import { open } from "../src/dagpane.js";

const wasmPath = process.argv[2] ?? "target/wasm32-unknown-unknown/release/dagpane_wasm.wasm";

const MANIFEST = `
[app]
title = "Sales"
[[source]]
name = "sales"
csv = "sales.csv"
[[input]]
name = "floor"
slider = { min = 0.0, max = 100.0, step = 1.0, default = 0.0 }
[[cell]]
name = "scoped"
from = "sales"
[[cell.step]]
filter = { column = "amount", op = "ge", param = "floor" }
[[cell]]
name = "total"
from = "scoped"
[[cell.step]]
group_by = { agg = [{ column = "amount", agg = "sum", as = "t" }] }
[[cell.step]]
scalar = { column = "t" }
[[cell]]
name = "by_region"
from = "scoped"
[[cell.step]]
group_by = { by = ["region"], agg = [{ column = "amount", agg = "sum", as = "revenue" }] }
[[pane]]
cell = "total"
metric = { label = "Revenue" }
[[pane]]
cell = "by_region"
custom = { renderer = "treemap", columns = ["region", "revenue"], options = { palette = "warm" } }
`;

const SALES = "region,amount\nnorth,10\nnorth,30\nsouth,5\n";

let failures = 0;
function check(name, condition, detail) {
  if (condition) {
    console.log("  ok   " + name);
  } else {
    failures += 1;
    console.log("  FAIL " + name + (detail === undefined ? "" : " — " + JSON.stringify(detail)));
  }
}

const bytes = await readFile(wasmPath);
const app = await open(bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength), {
  manifest: MANIFEST,
  sources: { sales: SALES },
});

console.log("the module instantiates and opens an app");
check("the init frame carries the title", app.init.title === "Sales", app.init.title);
check("every pane arrives drawn", app.init.views.length === 2, app.init.views);
check(
  // Five cells: the source, the control, and three computed. The first render runs the
  // three; the source and the control already hold values and are never looked at, which is
  // the same arithmetic `dagpane explain` prints for this app on the server.
  "the first render computed the three computed cells and looked at nothing else",
  app.init.stats.total_cells === 5 &&
    app.init.stats.evaluated === 3 &&
    app.init.stats.untouched === 2,
  app.init.stats,
);
check(
  "a scalar pane is formatted by the engine, not by the page",
  app.init.views.find((v) => v.id === "total").view.value === "45",
);

const custom = app.init.views.find((v) => v.id === "by_region").view;
check("a custom pane names its renderer", custom.renderer === "treemap", custom);
check(
  "a custom pane sends the projection the manifest asked for",
  custom.data === "table" && custom.head.map((h) => h.name).join(",") === "region,revenue",
  custom,
);
check(
  "a renderer's options ride the pane, never the view",
  JSON.stringify(custom).indexOf("warm") === -1 &&
    JSON.stringify(app.init.panes).indexOf("warm") !== -1,
);

console.log("an interaction");
const patch = app.send({ type: "set", seq: 1, values: { floor: { kind: "float", v: 20 } } });
check("it answers the seq it was given", patch.type === "patch" && patch.seq === 1, patch);
check(
  "only the cells downstream of the control ran",
  patch.stats.evaluated === 3 && patch.stats.untouched === 1,
  patch.stats,
);
check("the metric moved", patch.panes.find((p) => p.id === "total").view.value === "30");

console.log("an interaction that changes nothing");
const quiet = app.send({ type: "set", seq: 2, values: { floor: { kind: "float", v: 21 } } });
check(
  "no pane goes on the wire when no view moved",
  quiet.panes.length === 0,
  quiet.panes,
);

console.log("a refusal");
const no = app.send({ type: "set", seq: 3, values: { nope: { kind: "float", v: 1 } } });
check("an unknown input is rejected by name", no.type === "rejected" && no.message.includes("nope"), no);

console.log("memory does not grow without bound");
const before = app.exports.memory.buffer.byteLength;
for (let i = 0; i < 2000; i += 1) {
  app.send({ type: "set", seq: 100 + i, values: { floor: { kind: "float", v: i % 50 } } });
}
const after = app.exports.memory.buffer.byteLength;
// Every call allocates a request buffer and a reply buffer and frees both. A leak in the
// hand-written prefix arithmetic shows up here and nowhere else in this repository.
check(
  "2000 interactions do not grow the heap",
  after <= before * 2,
  { before, after },
);


// ── phase two: the exported bundle ─────────────────────────────────────────────────────
//
// `dagpane export` writes a page with `window.DAGPANE_LOCAL` in it and the module beside it.
// Reading that config back and opening the module with it is exactly what the page does when
// a browser evaluates it, minus the DOM — so this asserts that an exported app is a working
// app rather than a directory of plausible files.

const exportDir = process.argv[3];
if (exportDir) {
  console.log("\nthe exported bundle at " + exportDir);
  const html = await readFile(exportDir + "/index.html", "utf8");

  const assignment = "window.DAGPANE_LOCAL = ";
  const from = html.indexOf(assignment);
  check("the page carries a config block", from !== -1);
  const payload = html.slice(from + assignment.length, html.indexOf("</script>", from));
  const config = JSON.parse(payload.replace(/;\s*$/, ""));

  check("it names the module beside it", config.wasm === "./dagpane.wasm", config.wasm);
  check("it carries the manifest", config.manifest.includes("[app]"));
  check(
    "it carries the rows, not a path to them",
    Object.values(config.sources).every((t) => t.includes("\n")),
    Object.keys(config.sources),
  );

  const exported = await readFile(exportDir + "/dagpane.wasm");
  const app2 = await open(
    exported.buffer.slice(exported.byteOffset, exported.byteOffset + exported.byteLength),
    config,
  );
  check("the exported app opens", typeof app2.init.title === "string", app2.init.title);
  check("every pane is drawn", app2.init.views.length === app2.init.panes.length);

  // The published claim, from the exported bundle: the same interaction `.github/workflows/ci.yml` asserts
  // on the server. If the browser build recomputed a different number of cells it would be a
  // different product with the same name.
  const before = app2.init.stats;
  const p2 = app2.send({ type: "set", seq: 1, values: { min_amount: { kind: "float", v: 400 } } });
  check(
    "the same interaction costs the same as it does on a server",
    p2.stats.total_cells === 11 &&
      p2.stats.visited === 8 &&
      p2.stats.evaluated === 6 &&
      p2.stats.reused === 1 &&
      p2.stats.changed === 4 &&
      p2.stats.untouched === 3,
    p2.stats,
  );
  check("and sends three of seven panes", p2.panes.length === 3 && before.total_cells === 11, {
    sent: p2.panes.map((p) => p.id),
  });
}

console.log(failures === 0 ? "\nall checks passed" : `\n${failures} check(s) FAILED`);
process.exit(failures === 0 ? 0 : 1);
