//! `dagpane-wasm` — the engine in a browser, speaking the protocol it already speaks.
//!
//! # What this is
//!
//! `dagpane-serve` holds an [`AppSession`](dagpane_app::AppSession), takes a
//! [`ClientMessage`](dagpane_app::ClientMessage) off a socket, and answers with a
//! [`ServerMessage`](dagpane_app::ServerMessage). This crate holds an `AppSession`, takes a
//! `ClientMessage` from a function call, and answers with a `ServerMessage`.
//!
//! That is the whole idea, and the sameness is the point. **The client does not change.** Its
//! transport becomes a choice — a WebSocket or a call into linear memory — and everything
//! above the transport is byte-identical, so an app moved from a server into a browser is the
//! same app and not a port of one.
//!
//! ```text
//!                   ┌── WebSocket ──→ dagpane-serve ─┐
//!   the client ──→  │                                ├──→ AppSession ──→ dagpane-core
//!                   └── dp_send() ──→ dagpane-wasm ──┘
//! ```
//!
//! # What it deliberately is not
//!
//! **Not a SQL engine in a browser.** `ROADMAP.md` §4 is blunt about the payload argument:
//! it is false for anything that ships a query engine — DuckDB-Wasm is 34 MB, Pyodide 13 MB —
//! and true only if the client stays a thin evaluator. This crate is that thin evaluator. It
//! links `dagpane-app` with the Arrow backend **off**, and `BENCHMARKS.md` carries what the
//! artefact actually weighs beside the engines it is not.
//!
//! **Not per-cell placement.** §4's defensible version is "the placement of each cell is a
//! deployment decision rather than a rewrite". This crate moves the *whole graph*, which is
//! the smaller half. What it adds is the seam — [`dagpane_app::Sources`] — that per-cell
//! placement would be built on; it is not a down payment being described as the thing.
//!
//! **Not a reason to stop measuring.** §4's trigger is an interaction whose latency is
//! dominated by the round trip, measured on a real app. `benches/roundtrip.sh` is that
//! measurement and `BENCHMARKS.md` reports it. If a pass costs more than the network does,
//! this crate makes a page slower and the honest thing is to say so.
//!
//! # Using it
//!
//! From JavaScript, through `dagpane.js`:
//!
//! ```js
//! const app = await dagpane.open("dagpane.wasm", {
//!   manifest: await (await fetch("app.toml")).text(),
//!   sources: { sales: await (await fetch("sales.csv")).text() },
//! });
//! const patch = app.send({ type: "set", seq: 1, values: { floor: { kind: "float", v: 20 } } });
//! ```
//!
//! From Rust — a test, a host embedding the engine, anything with no browser in it — through
//! [`Engine`], which is the same code path and is what this crate's own tests use.

#![deny(missing_debug_implementations)]
#![deny(missing_docs)]

pub mod engine;

// The ABI is wasm32-only. Building it for the host would export four `extern "C"` symbols
// from every test binary that links this crate, for a calling convention nothing on the host
// uses — and it is the one module here that needs `unsafe`, so keeping it off every other
// target is worth the `cfg`.
#[cfg(target_arch = "wasm32")]
mod abi;

pub use engine::{open_json, Config, Engine, Failure};

/// A failure envelope as JSON. The one reply shape that never depends on an app existing.
///
/// Used by the ABI, which is wasm32-only, so on any other target nothing calls it — hence the
/// `cfg` rather than an `allow(dead_code)`: the function is genuinely not part of the host
/// build, and saying so is more honest than silencing the warning.
#[cfg(target_arch = "wasm32")]
pub(crate) fn failed(message: &str) -> String {
    serde_json::to_string(&Failure::Failed {
        message: message.to_string(),
    })
    .unwrap_or_else(|_| r#"{"type":"failed","message":"unreportable"}"#.to_string())
}

/// The JavaScript that drives the module: `open`, `send`, and the length-prefix convention.
///
/// Carried in the binary rather than as a file on disk for the same reason `dagpane-serve`
/// carries its client that way — a page that needs a package manager to load is not the
/// product this repository is describing. `dagpane export` writes it out beside the
/// `.wasm`; a host doing its own bundling can read it from here.
pub const GLUE_JS: &str = include_str!("dagpane.js");
