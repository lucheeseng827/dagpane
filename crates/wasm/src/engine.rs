//! The part of this crate that is not about pointers.
//!
//! One type, two methods, and both of them are `&str -> String`. Everything here is ordinary
//! safe Rust and is tested on the host: `cargo test -p dagpane-wasm` needs no browser, no
//! `wasm32` toolchain and no JavaScript, which is the property that makes the wasm build a
//! packaging question rather than a correctness one.

use std::collections::BTreeMap;
use std::sync::Arc;

use dagpane_app::{manifest, AppSession, ClientMessage, PassStats, ServerMessage, Sources};
use dagpane_core::{Placement, Value};
use serde::{Deserialize, Serialize};

/// What a host hands over to start an app.
///
/// Deliberately *the manifest and the bytes*, not a compiled graph. A graph holds closures
/// and cannot be serialised, and shipping one would mean a second representation of an app
/// that has to be kept in step with the first — two ways to describe a dashboard is how the
/// two of them drift. The browser compiles the same TOML the server compiles, through the
/// same [`dagpane_app::compile_with`], so a manifest that is wrong is wrong in both places
/// and says so the same way.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    /// The manifest, as text.
    pub manifest: String,
    /// One entry per `[[source]]`, keyed by the source's **name** — `"sales"`, not
    /// `"sales.csv"`. A source the map does not carry is an error naming it, because a host
    /// with no filesystem has no second place to look.
    #[serde(default)]
    pub sources: BTreeMap<String, String>,
    /// One entry per `[app] renderers` script, keyed by the path the manifest declared —
    /// `"charts.js"`. A declared script missing here is a compile error, for the same reason
    /// a missing file is on a server: a `custom` pane with no renderer is a blank card.
    #[serde(default)]
    pub renderers: BTreeMap<String, String>,
    /// Inputs to apply before the first render, the way a resumed socket session does.
    ///
    /// Same door, same predicates, same report of what could not be applied — see
    /// [`dagpane_app::resume`]. A bookmarked URL restores identically whichever side the
    /// engine is on, which is the property that makes the placement a deployment choice
    /// rather than a different product.
    #[serde(default)]
    pub resume: BTreeMap<String, Value>,
    /// `"client"` when a server is feeding this page a frontier; absent otherwise.
    ///
    /// Absent is the default and means **run the whole app here**, which is what a
    /// `dagpane export` bundle does: it has no server, so there is nobody to send a frontier
    /// and nothing to wait for. That is sound rather than a fallback — a cut says where cells
    /// *may* run, and running all of them in one session is the undivided app whose values a
    /// split is required to reproduce.
    ///
    /// `"client"` is the other case: `dagpane run` on a placed app, where this module owns
    /// the page's half and the socket carries the rest. Opening that half without a server
    /// would show nulls, so a host that sets this must also deliver the `init` frontier.
    #[serde(default)]
    pub side: Option<dagpane_core::Placement>,
}

/// What this crate answers with when a request never reached an app at all.
///
/// A cell failing is data and belongs in a pane; a manifest that will not compile is not.
/// [`ServerMessage::Rejected`] is the socket's word for the second kind, and it carries a
/// `seq` — which an `open` does not have — so this is its own small envelope rather than a
/// borrowed one with a lie in the sequence number.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Failure {
    /// The app could not be built, or the request could not be understood.
    Failed {
        /// What went wrong, in words meant for whoever is looking at the page.
        message: String,
    },
}

impl Failure {
    fn json(message: impl Into<String>) -> String {
        let f = Failure::Failed {
            message: message.into(),
        };
        // A failure whose own encoding fails has nowhere left to go; the literal below is
        // valid JSON of the same shape and keeps the host's parser from seeing garbage.
        serde_json::to_string(&f)
            .unwrap_or_else(|_| r#"{"type":"failed","message":"unreportable"}"#.to_string())
    }
}

/// One viewer's app, in whatever process this crate was linked into.
#[derive(Debug)]
pub struct Engine {
    session: AppSession,
}

impl Engine {
    /// Compile an app from a manifest and the bytes of its sources.
    ///
    /// # Errors
    ///
    /// The manifest's own error text, unchanged — it is the same compiler the server runs, so
    /// a browser reports a misspelt column in the same sentence `dagpane check` does.
    pub fn open(config: &Config) -> Result<(Engine, ServerMessage), String> {
        let parsed = manifest::parse(&config.manifest).map_err(|e| e.to_string())?;

        let mut bound: BTreeMap<String, Arc<dyn dagpane_app::Source>> = BTreeMap::new();
        for spec in &parsed.source {
            let Some(text) = config.sources.get(&spec.name) else {
                continue; // `compile_with` reports this, with the message it already has.
            };
            // Named for the path the manifest gave it, so an unparseable CSV in a browser
            // names the same file a server would have named.
            let named = spec
                .csv
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| format!("{}.csv", spec.name));
            bound.insert(
                spec.name.clone(),
                Arc::new(dagpane_app::BytesSource::new(
                    named,
                    dagpane_app::FileFormat::Csv,
                    text.as_str(),
                )),
            );
        }

        let app = Arc::new(
            manifest::compile_with(
                &parsed,
                Sources::Bound {
                    sources: &bound,
                    renderers: &config.renderers,
                },
            )
            .map_err(|e| e.to_string())?,
        );
        let (mut session, trace, dropped) = match config.side {
            // An export, or an app that named no placement: the whole graph, here.
            None => AppSession::resume(app, &config.resume),
            // The page's half of a split, waiting on a frontier. A `resume` value naming a
            // control the server half owns is reported rather than refused — a bookmarked
            // link carries every control the app has, and half of them legitimately belong
            // to the other side.
            // Checked rather than assumed: `open_side` panics on an app with no client half,
            // which is a fair contract for a Rust caller and a terrible one here. This config
            // arrives as JSON from a host, and a panic in wasm is an abort that takes the
            // instance with it — so a manifest that named no placement is an error with a
            // sentence in it, not a dead module.
            Some(Placement::Client) if !app.cut.is_split() => {
                return Err(
                    "`side` is \"client\" but this manifest declares no `place`, so it \
                            has no client half: remove `side`, or place a cell"
                        .to_string(),
                )
            }
            Some(Placement::Client) => {
                AppSession::resume_side(app, Some(Placement::Client), &config.resume)
            }
            // The type can say it and the contract cannot mean it. This module IS the page,
            // so opening the server half here would quietly serve the wrong half of the app
            // — every server pane and none of the page's — while looking like it worked.
            // Refused by name rather than coerced to `None`, because a host that asked for
            // this has a bug and silently giving it something else hides the bug.
            Some(Placement::Server) => {
                return Err(
                    "`side` may be \"client\" or absent: this module runs in the page, \
                            so it is never the server half of a cut"
                        .to_string(),
                )
            }
        };
        // `micros` is `None` and stays `None`. This crate has no clock for the same reason
        // `dagpane-core` has none, and a browser measuring itself with `performance.now()`
        // would be reporting its own scheduler. The host may time the call it just made; the
        // engine will not pretend to have.
        let init = session.init_message_with(&trace, None, dropped);
        Ok((Engine { session }, init))
    }

    /// Answer one [`ClientMessage`], exactly as the socket loop in `dagpane-serve` does.
    ///
    /// The same two messages in, the same four out. That sameness is the entire point of this
    /// crate: a client does not learn which side of the wire it is talking to, so moving an
    /// app from a server into a browser is a change of transport and not a change of product.
    pub fn handle(&mut self, request: &str) -> String {
        let message: ClientMessage = match serde_json::from_str(request) {
            Ok(m) => m,
            Err(e) => return Failure::json(format!("unreadable message: {e}")),
        };
        let reply = match message {
            ClientMessage::Refresh { seq } => {
                let panes = self.session.full_views();
                ServerMessage::Refreshed {
                    seq,
                    panes,
                    stats: PassStats {
                        epoch: self.session.session().epoch(),
                        total_cells: self.session.app().graph.len(),
                        ..PassStats::default()
                    },
                }
            }
            ClientMessage::Set { seq, values } => match self.session.set(&values) {
                Err(message) => ServerMessage::Rejected { seq, message },
                Ok(()) => {
                    let (trace, panes) = self.session.commit();
                    ServerMessage::Patch {
                        seq,
                        panes,
                        // The page is the far end of a cut, never the near one: nothing
                        // flows back across it, which is the rule the whole design rests on.
                        // `AppSession::frontier` returns empty for a client side, so this is
                        // the rule showing up as a fact rather than as a check.
                        frontier: Vec::new(),
                        stats: PassStats::from_trace(&trace),
                    }
                }
            },
        };
        serde_json::to_string(&reply)
            .unwrap_or_else(|e| Failure::json(format!("unencodable reply: {e}")))
    }

    /// Take a `ServerMessage` from the socket, apply whatever of it belongs here, and answer
    /// with this half's own patch.
    ///
    /// The page forwards the server's message and does nothing else with it. Putting the
    /// extraction here rather than in JavaScript is deliberate: the one obligation the
    /// transport carries is that a pass's boundary values are applied **together**, and that
    /// is a property of the code that stages them. A page that unpacked a frontier itself
    /// could deliver half of one, and no test in this repository would see it.
    ///
    /// Both kinds of message are accepted, because both carry a frontier and they differ in
    /// which one: `init` carries every boundary cell, `patch` only those that moved. The
    /// reply is a `patch` holding the panes **this** half repainted, which is empty whenever
    /// the frontier moved nothing the page draws.
    ///
    /// A message with no frontier in it is not an error — it is an unsplit app, or a pass
    /// that moved nothing on the frontier — and answers with an empty patch.
    pub fn deliver(&mut self, request: &str) -> String {
        let message: ServerMessage = match serde_json::from_str(request) {
            Ok(m) => m,
            Err(e) => return Failure::json(format!("unreadable server message: {e}")),
        };
        // Only the page's half of a split has anything to deliver TO. An undivided session —
        // an export, or an app that named no placement — has no boundary sources, and its
        // `set_outcome` would land on an ordinary source and overwrite the app's own data.
        // The README says this call has nothing to do there; this is the line that makes
        // that true rather than merely written down.
        if self.session.side() != Some(Placement::Client) {
            return self.nothing_moved(0);
        }
        let (seq, cells) = match &message {
            ServerMessage::Init {
                frontier, stats, ..
            } => (stats.epoch, frontier.clone()),
            ServerMessage::Patch { seq, frontier, .. } => (*seq, frontier.clone()),
            // A refusal or a refresh carries no frontier and moves nothing here.
            _ => (0, Vec::new()),
        };

        if cells.is_empty() {
            return self.nothing_moved(seq);
        }

        let frontier = dagpane_app::BoundaryValue::into_frontier(&cells, seq);
        if let Err(message) = self.session.deliver(&frontier) {
            return Failure::json(message);
        }
        // ONE commit, after the whole frontier is staged. See the module docs on
        // `dagpane_core::placement`: committing inside the loop is the glitch this design
        // exists to make unavailable, and `deliver` staging rather than applying is what
        // keeps it out of reach from here.
        let (trace, panes) = self.session.commit();
        serde_json::to_string(&ServerMessage::Patch {
            seq,
            panes,
            frontier: Vec::new(),
            stats: PassStats::from_trace(&trace),
        })
        .unwrap_or_else(|e| Failure::json(format!("unencodable reply: {e}")))
    }

    /// An empty patch: nothing crossed, so nothing here moved.
    ///
    /// The counts are genuinely zero and the epoch is the one this half already had, which is
    /// the honest report — a pass that did not run must not appear as a pass that did nothing.
    fn nothing_moved(&self, seq: u64) -> String {
        serde_json::to_string(&ServerMessage::Patch {
            seq,
            panes: Vec::new(),
            frontier: Vec::new(),
            stats: PassStats {
                epoch: self.session.session().epoch(),
                total_cells: self.session.session().graph().len(),
                ..PassStats::default()
            },
        })
        .unwrap_or_else(|e| Failure::json(format!("unencodable reply: {e}")))
    }

    /// The session, for a host that wants to read a cell directly rather than through a pane.
    pub fn session(&self) -> &AppSession {
        &self.session
    }
}

/// Compile an app and return its opening frame, or the failure, as JSON.
///
/// The string the ABI hands back, and what a host that links this crate as an `rlib` should
/// call too — so that the browser path and the native path cannot answer differently.
pub fn open_json(config_json: &str) -> (Option<Engine>, String) {
    let config: Config = match serde_json::from_str(config_json) {
        Ok(c) => c,
        Err(e) => return (None, Failure::json(format!("unreadable config: {e}"))),
    };
    match Engine::open(&config) {
        Err(message) => (None, Failure::json(message)),
        Ok((engine, init)) => match serde_json::to_string(&init) {
            Ok(json) => (Some(engine), json),
            Err(e) => (None, Failure::json(format!("unencodable init: {e}"))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dagpane_app::View;

    const SALES: &str = "region,amount\nnorth,10\nnorth,30\nsouth,5\n";

    const APP: &str = r#"
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
name = "n"
from = "scoped"
[[cell.step]]
count = true
[[pane]]
cell = "total"
metric = { label = "Revenue" }
[[pane]]
cell = "n"
metric = { label = "Orders" }
"#;

    fn config() -> Config {
        Config {
            side: None,
            manifest: APP.to_string(),
            sources: [("sales".to_string(), SALES.to_string())]
                .into_iter()
                .collect(),
            renderers: BTreeMap::new(),
            resume: BTreeMap::new(),
        }
    }

    fn open() -> Engine {
        Engine::open(&config()).expect("the fixture compiles").0
    }

    fn metric(panes: &[dagpane_app::PaneUpdate], id: &str) -> String {
        let p = panes
            .iter()
            .find(|p| p.id == id)
            .expect("a pane by that id");
        match &p.view {
            View::Metric { value, .. } => value.clone(),
            other => panic!("expected a metric, got {other:?}"),
        }
    }

    #[test]
    fn an_app_compiles_from_a_manifest_and_bytes_with_no_filesystem_anywhere() {
        let (_, init) = Engine::open(&config()).unwrap();
        let ServerMessage::Init {
            title,
            panes,
            views,
            stats,
            ..
        } = init
        else {
            panic!("expected an init frame")
        };
        assert_eq!(title, "Sales");
        assert_eq!(panes.len(), 2);
        assert_eq!(metric(&views, "total"), "45");
        assert_eq!(metric(&views, "n"), "3");
        assert_eq!(stats.total_cells, 5, "1 source + 1 input + 3 cells");
        assert_eq!(
            stats.micros, None,
            "this crate has no clock and does not pretend to"
        );
    }

    #[test]
    fn an_interaction_patches_only_what_moved_exactly_as_the_socket_does() {
        // The claim this whole project is about, made on the side of the wire that has no
        // wire. If moving a control in a browser sent every pane, the browser build would be
        // a different product wearing the same name.
        let mut e = open();
        let reply =
            e.handle(r#"{"type":"set","seq":1,"values":{"floor":{"kind":"float","v":20.0}}}"#);
        let ServerMessage::Patch {
            seq, panes, stats, ..
        } = serde_json::from_str(&reply).unwrap()
        else {
            panic!("expected a patch, got {reply}")
        };
        assert_eq!(seq, 1);
        assert_eq!(metric(&panes, "total"), "30");
        assert_eq!(metric(&panes, "n"), "1");
        assert_eq!(
            stats.evaluated, 3,
            "scoped, total and n; the source did not run"
        );
        assert_eq!(stats.untouched, 1, "`sales` was never looked at");
    }

    #[test]
    fn a_control_that_moves_to_the_same_answer_sends_nothing() {
        let mut e = open();
        e.handle(r#"{"type":"set","seq":1,"values":{"floor":{"kind":"float","v":1.0}}}"#);
        let reply =
            e.handle(r#"{"type":"set","seq":2,"values":{"floor":{"kind":"float","v":2.0}}}"#);
        let ServerMessage::Patch { panes, stats, .. } = serde_json::from_str(&reply).unwrap()
        else {
            panic!("expected a patch")
        };
        assert!(
            panes.is_empty(),
            "no row sits between 1 and 2, so nothing on the page can have moved: {panes:?}"
        );
        assert!(stats.evaluated > 0, "the cells did run — they just agreed");
    }

    #[test]
    fn a_source_nobody_supplied_is_named_rather_than_reached_for() {
        // The failure mode this rules out is a browser build that quietly tries to open a
        // file, gets a confusing error from a wasm shim, and blames the manifest.
        let mut c = config();
        c.sources.clear();
        let e = Engine::open(&c).unwrap_err();
        assert!(e.contains("sales"), "{e}");
        assert!(e.contains("no rows were supplied"), "{e}");
    }

    #[test]
    fn a_manifest_that_will_not_compile_says_so_in_the_servers_own_words() {
        let mut c = config();
        c.manifest = c
            .manifest
            .replace("column = \"amount\", op", "column = \"amont\", op");
        let e = Engine::open(&c).unwrap_err();
        assert!(e.contains("amont"), "{e}");
        assert!(
            e.contains("did you mean"),
            "the same hint `dagpane check` gives: {e}"
        );
    }

    #[test]
    fn a_resumed_session_costs_one_pass_and_reports_what_it_dropped() {
        let mut c = config();
        c.resume.insert("floor".to_string(), Value::float(20.0));
        c.resume.insert("gone".to_string(), Value::float(1.0));
        let (_, init) = Engine::open(&c).unwrap();
        let ServerMessage::Init { views, dropped, .. } = init else {
            panic!("expected an init frame")
        };
        assert_eq!(
            metric(&views, "total"),
            "30",
            "restored before the first render"
        );
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].input, "gone");
    }

    #[test]
    fn an_input_the_app_does_not_have_is_rejected_and_nothing_is_applied() {
        let mut e = open();
        let reply =
            e.handle(r#"{"type":"set","seq":9,"values":{"nope":{"kind":"float","v":1.0}}}"#);
        let ServerMessage::Rejected { seq, message } = serde_json::from_str(&reply).unwrap() else {
            panic!("expected a rejection, got {reply}")
        };
        assert_eq!(seq, 9);
        assert!(message.contains("nope"), "{message}");
    }

    #[test]
    fn a_refresh_re_sends_every_pane_and_says_no_pass_ran() {
        let mut e = open();
        let reply = e.handle(r#"{"type":"refresh","seq":4}"#);
        let ServerMessage::Refreshed { seq, panes, stats } = serde_json::from_str(&reply).unwrap()
        else {
            panic!("expected a refreshed frame, got {reply}")
        };
        assert_eq!(seq, 4);
        assert_eq!(panes.len(), 2);
        assert_eq!(
            stats.evaluated, 0,
            "a refresh re-renders; it does not recompute"
        );
        assert_eq!(stats.total_cells, 5);
    }

    #[test]
    fn a_message_that_is_not_one_is_refused_rather_than_guessed_at() {
        let mut e = open();
        for junk in ["", "{", r#"{"type":"nudge"}"#, "[]"] {
            let reply = e.handle(junk);
            let f: Failure = serde_json::from_str(&reply)
                .unwrap_or_else(|_| panic!("a failure must itself be readable JSON: {reply}"));
            let Failure::Failed { message } = f;
            assert!(!message.is_empty(), "{junk:?}");
        }
    }

    #[test]
    fn open_json_reports_a_bad_config_without_panicking() {
        let (engine, json) = open_json("not json at all");
        assert!(engine.is_none());
        assert!(serde_json::from_str::<Failure>(&json).is_ok(), "{json}");
    }

    #[test]
    fn the_server_half_is_not_something_this_module_can_be() {
        // `Config::side` is an `Option<Placement>` and the type can therefore say "server",
        // which the contract cannot mean: this module IS the page. Opening that half would
        // serve every server pane and none of the page's while looking like it worked.
        let mut config = config();
        config.side = Some(dagpane_core::Placement::Server);
        let e = Engine::open(&config).expect_err("the page is never the server half");
        assert!(e.contains("client"), "{e}");
        assert!(e.contains("runs in the page"), "{e}");

        // And `client` on an app that named no placement is an error with a sentence in it
        // rather than a panic. `AppSession::open_side` panics there — a fair contract for a
        // Rust caller and a terrible one for a config that arrives as JSON, since a panic in
        // wasm aborts the instance.
        config.side = Some(dagpane_core::Placement::Client);
        let e = Engine::open(&config).expect_err("sales.toml declares no placement");
        assert!(e.contains("no `place`"), "{e}");

        // The ordinary case: absent, and the whole app runs here.
        config.side = None;
        assert!(Engine::open(&config).is_ok());
    }

    #[test]
    fn delivering_to_an_undivided_session_changes_nothing() {
        // An exported bundle has no server, so nothing should ever call this — but a host
        // that does must not be able to overwrite the app's own data through it. Without the
        // side check, `set_outcome` would land on an ordinary source and replace the rows.
        let (mut engine, _) = Engine::open(&config()).expect("the bundled example");
        let before = engine.session().session().epoch();

        let forged = serde_json::json!({
            "type": "patch",
            "seq": 1,
            "panes": [],
            "frontier": [{
                "cell": "sales",
                "outcome": {"state": "value", "value": {"kind": "int", "v": 0}}
            }],
            "stats": {"epoch": 1, "total_cells": 1, "visited": 0, "evaluated": 0,
                      "reused": 0, "changed": 0, "untouched": 0}
        })
        .to_string();

        let reply = engine.deliver(&forged);
        assert!(!reply.contains("\"type\":\"failed\""), "{reply}");
        assert_eq!(
            engine.session().session().epoch(),
            before,
            "a delivery to an undivided session ran a pass"
        );
        assert!(
            engine
                .session()
                .session()
                .get("sales")
                .expect("the source")
                .value()
                .and_then(|v| v.as_frame())
                .is_some(),
            "the forged frontier replaced the app's own rows"
        );
    }
}
