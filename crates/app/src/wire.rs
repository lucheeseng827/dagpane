//! The two messages that cross the socket.
//!
//! The shape of this module *is* the product claim. A rerun runtime sends a page; this one
//! sends [`ServerMessage::Patch`], whose `panes` array holds only the panes whose cell
//! produced a different value — and a [`PassStats`] block saying how much of the app ran to
//! produce them. Anyone can open a browser's network tab and check.
//!
//! # An interaction, end to end
//!
//! ```json
//! → {"type":"set","seq":7,"values":{"min_amount":{"kind":"float","v":250.0}}}
//! ← {"type":"patch","seq":7,"epoch":8,
//!    "panes":[{"id":"by_region","view":{"view":"table","head":[…],"rows":[…],"total_rows":2}}],
//!    "stats":{"epoch":8,"total_cells":9,"visited":4,"evaluated":3,"reused":0,
//!             "changed":2,"untouched":5,"micros":412}}
//! ```
//!
//! Five of the app's nine cells were never looked at, and the pane whose value did not move
//! is not in the array.
//!
//! `seq` is the client's own counter, echoed back. It is not used for ordering — a WebSocket
//! delivers in order and the server answers one message at a time — it exists so a client
//! can tell which of its own interactions a patch answers, which is what makes a "still
//! computing" indicator possible without guessing.

use dagpane_core::{Outcome, Trace};
use serde::{Deserialize, Serialize};

use crate::view::{Pane, View};
use crate::widget::Widget;

/// What a client sends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Set one or more inputs and recompute once.
    ///
    /// Several values in one message is the normal case, not an optimisation: a client that
    /// sent them one at a time would cause one pass each, and the shared subgraph would run
    /// once per input with a mixture of old and new values in between.
    Set {
        /// The client's own counter, echoed on the reply so it can tell which interaction
        /// was answered. See this module's docs for why it is not an ordering mechanism.
        seq: u64,
        /// Cell name to new value. A `BTreeMap` so the encoding is stable and a recorded
        /// message compares byte for byte against a replay of it. Every entry is validated
        /// before any is applied — a partly-applied batch would leave the client's idea of
        /// the state and the session's disagreeing.
        values: std::collections::BTreeMap<String, dagpane_core::Value>,
    },
    /// Send me the current state of everything. Costs a graph walk and no recomputation,
    /// because every cell reuses.
    ///
    /// **This is not what a reconnect does, and the bundled client never sends it.** That
    /// sentence used to say it was, and it was wrong for a structural reason: a session *is*
    /// a connection here, so a reconnect is a new session, and a new session's opening frame
    /// is an [`ServerMessage::Init`] carrying the viewer's own values back off the query
    /// string. There is nothing left over to refresh.
    ///
    /// What it is for is a client that keeps a socket across losing its own state — not a
    /// page, which loses its socket when it loses its state. The server answers it, the wasm
    /// engine answers it, and nothing in this repository asks.
    Refresh {
        /// The client's counter, echoed on the [`ServerMessage::Refreshed`] that answers.
        seq: u64,
    },
}

/// What the server sends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// The app itself, once per connection: enough to build the page.
    Init {
        /// The app's title.
        title: String,
        /// The subtitle, omitted from the encoding when there is none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subtitle: Option<String>,
        /// Every control, with its bounds. Sent once: they are static for the life of the
        /// process, so no later message repeats them.
        widgets: Vec<Widget>,
        /// Every pane's definition, in display order. Also sent once, for the same reason —
        /// which is what lets a [`ServerMessage::Patch`] carry views alone.
        panes: Vec<Pane>,
        /// Every pane's current view, so the page is complete on arrival.
        views: Vec<PaneUpdate>,
        /// Scripts the client loads before its first paint, each registering renderers a
        /// `custom` pane can name. Paths relative to wherever the client is being served
        /// from; see [`crate::PaneKind::Custom`] for why this is a list of names and not of
        /// code.
        ///
        /// Omitted from the encoding when empty, so an app with no custom panes — which is
        /// every app that existed before they did — sends the byte-identical opening frame it
        /// sent before.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        renderers: Vec<String>,
        /// What the page needs to compile and run its own half — absent unless this app
        /// declared a placement, so an app that named none sends what it always sent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        //
        // **Boxed**, and that is about `Patch` rather than about this. `ServerMessage` is one
        // enum, so every variant is as large as the largest — and `Init` goes once per
        // connection while `Patch` goes on every interaction. Carrying the boot block inline
        // would widen the message the hot path moves, to hold a field the hot path never has.
        // Serde sees straight through a `Box`, so the wire format is the same bytes either way.
        client_half: Option<Box<ClientHalf>>,
        /// Every boundary cell of a split app, so the half running in the page can compute
        /// from real values on its first pass rather than from the nulls its sources start at.
        ///
        /// The **full** frontier and not a delta — see
        /// `dagpane_core::placement::Split::full_frontier` for the bug that distinction
        /// exists to prevent. Omitted from the encoding for an app that declared no
        /// placement, which is every app that existed before `place` did, so their opening
        /// frame is byte-identical to the one they sent before.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        frontier: Vec<BoundaryValue>,
        /// What the session's first pass cost.
        stats: PassStats,
        /// Inputs a resume asked for and did not get — see [`crate::resume`].
        ///
        /// Omitted from the encoding when empty, which is every connection that did not
        /// carry a saved state, so the ordinary opening frame is byte-identical to the one
        /// before this field existed.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        dropped: Vec<crate::resume::Dropped>,
    },
    /// The panes whose value changed, and what it cost.
    Patch {
        /// The `seq` of the [`ClientMessage::Set`] this answers.
        seq: u64,
        /// Only the panes whose rendered view changed. A cell that recomputed to the same
        /// value, and a pane whose view is unchanged despite a changed cell, are both absent
        /// — this array is the product claim, in bytes a network tab can count.
        panes: Vec<PaneUpdate>,
        /// The boundary cells that moved, for the half running in the page.
        ///
        /// Empty on an unsplit app, and empty on a split one whenever the pass moved nothing
        /// on the frontier — which is the common case and the whole economy of a cut. A
        /// client applies the whole of this and then commits **once**; see
        /// [`crate::AppSession::deliver`].
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        frontier: Vec<BoundaryValue>,
        /// What the pass cost.
        stats: PassStats,
    },
    /// Every pane's current view, in answer to a `Refresh`.
    ///
    /// Distinct from `Patch` because a refresh **runs no pass**: it re-renders state the
    /// session already holds. Sending it as a `Patch` meant the stats block reported zero
    /// visited and zero evaluated beside a full set of panes, which reads as though the
    /// engine produced seven panes from no work. The counts are genuinely zero; the message
    /// kind is what says why.
    Refreshed {
        /// The `seq` of the [`ClientMessage::Refresh`] this answers.
        seq: u64,
        /// Every pane, not just changed ones — the client is assumed to have lost its state.
        panes: Vec<PaneUpdate>,
        /// Zero visited, evaluated and changed, because no pass ran — only `epoch` and
        /// `total_cells` are filled in. The message kind is what explains that; see above.
        stats: PassStats,
    },
    /// The client asked for something the app will not do — an unknown input, a value of the
    /// wrong type. Distinct from a cell error, which is data and belongs in a pane.
    Rejected {
        /// The `seq` of the message being refused.
        seq: u64,
        /// Why, in words meant for the person at the browser. Nothing was applied.
        message: String,
    },
}

/// One boundary cell crossing the cut, on the wire.
///
/// `dagpane_core::placement::Frontier` is the in-process shape and holds `Arc<Outcome>`, so
/// moving a frontier inside one process costs a refcount bump. This is the shape that has to
/// be serialised, which is why it is a separate type rather than serde on that one: the
/// distinction between "shared" and "copied" is exactly what a transport pays for, and a
/// single type with a `Serialize` impl would have hidden it.
///
/// An **[`Outcome`]** and not a `Value`. A boundary cell is computed on the far side, and a
/// cell in this engine holds either a value or the error standing in place of one — so a
/// failure upstream of the cut arrives as a failure. Sending null instead would draw a broken
/// pane as a legitimately empty one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoundaryValue {
    /// The cell's name, the same on both sides.
    pub cell: String,
    /// What it holds: `{"state":"value","value":…}` or `{"state":"error","error":…}`.
    pub outcome: Outcome,
}

impl BoundaryValue {
    /// The wire form of an in-process frontier.
    ///
    /// This is where a split stops being free: every boundary value is cloned out of its
    /// `Arc` because it is about to become bytes. `dagpane check` prints the frontier width
    /// so an author can see what they signed up for before a viewer pays for it.
    pub fn of(frontier: &dagpane_core::placement::Frontier) -> Vec<BoundaryValue> {
        frontier
            .cells
            .iter()
            .map(|(cell, outcome)| BoundaryValue {
                cell: cell.clone(),
                outcome: (**outcome).clone(),
            })
            .collect()
    }

    /// Back to the in-process form, for a receiver that is about to deliver it.
    pub fn into_frontier(cells: &[BoundaryValue], epoch: u64) -> dagpane_core::placement::Frontier {
        dagpane_core::placement::Frontier {
            epoch,
            cells: cells
                .iter()
                .map(|b| (b.cell.clone(), std::sync::Arc::new(b.outcome.clone())))
                .collect(),
        }
    }
}

/// Everything the page needs to compile and run its own half of a split app.
///
/// Sent once, in the opening frame, and only for an app that declared a placement.
///
/// # Why a manifest and not a graph
///
/// A graph holds closures and cannot be serialised; shipping one would mean a second
/// representation of an app to keep in step with the first, which is how two descriptions of
/// a dashboard drift apart. So the page compiles the same manifest the server compiled,
/// through the same `compile_with`, and derives the same cut from it — ADR-0007 made that
/// choice for the whole-graph case and this is the same choice for half of one.
///
/// # Why schemas and not rows
///
/// `compile_with` loads every `[[source]]` because a CSV's column types are decided by
/// reading it — and the point of cutting below the data is that the page does not get the
/// data. The compiler wants **types**, which are a name and a `ColumnType` each, so that is
/// what crosses. The rows follow as the frontier.
/// `a_half_compiled_from_shapes_is_the_same_half` is the property this rests on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClientHalf {
    /// The manifest, as text.
    ///
    /// **Re-emitted from what the server parsed**, not read from disk again. What the page
    /// compiles is then exactly what the server compiled, rather than a file that may have
    /// been edited since the process started — and a manifest's bytes are its identity here,
    /// so two halves compiled from different bytes are two different apps.
    pub manifest: String,
    /// One entry per `[[source]]`: its columns, in order, with no rows behind them.
    ///
    /// Taken from the frames the server already loaded, so producing this costs no re-read.
    pub sources: std::collections::BTreeMap<String, Vec<ColumnSpec>>,
    /// Each `[app] renderers` script, by the path the manifest declared.
    ///
    /// **The page cannot compile its half without these.** `compile_with` demands a declared
    /// script's bytes by name — a missing one is the same `ManifestError::Renderer` a missing
    /// file is, because a `custom` pane with no renderer is a blank card. The manifest here is
    /// re-emitted whole, so it declares every renderer the app declares, and the page's
    /// compile would fail on the first of them with "was not supplied to this host".
    ///
    /// Every one of them, not only the ones the page's own panes draw: the page compiles the
    /// whole re-emitted manifest, and `[app] renderers` is one list rather than a list per
    /// side. Filtering it would mean re-emitting a *different* manifest, which is exactly the
    /// thing [`ClientHalf::manifest`] exists not to do.
    ///
    /// A served page ends up holding these bytes twice — once here, to compile with, and once
    /// as the module it imports from `GET /<renderer>`, because a browser's module loader
    /// takes a URL and not a string. That is the honest cost of the boot block being
    /// self-contained, and renderer scripts are small next to the frontier beside them.
    ///
    /// Omitted from the encoding when empty, which is every app that declares no renderer.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub renderers: std::collections::BTreeMap<String, String>,
}

/// One column of a source's shape.
///
/// A struct rather than a tuple because it goes on a wire and is read by a person debugging
/// a page: `{"name":"amount","type":"float"}` says what it is and `["amount","float"]` does
/// not.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColumnSpec {
    /// The column's name, as the source reports it.
    pub name: String,
    /// Its element type.
    #[serde(rename = "type")]
    pub ty: dagpane_core::ColumnType,
}

/// One pane's new view, addressed by the pane's stable id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaneUpdate {
    /// The pane's id, as sent in [`ServerMessage::Init`]. Not the cell name: two panes can
    /// show one cell and each needs its own update.
    pub id: String,
    /// What the pane now shows.
    pub view: View,
}

/// What one pass cost. Derived from a [`Trace`], plus the one number the engine deliberately
/// cannot measure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassStats {
    /// The pass number, monotonic per session.
    pub epoch: u64,
    /// Cells in the app. The denominator of every ratio below.
    pub total_cells: usize,
    /// Cells the pass touched at all — the size of the dirty closure.
    pub visited: usize,
    /// Cells whose compute actually ran.
    pub evaluated: usize,
    /// Cells that served a cached value because no input of theirs had moved.
    pub reused: usize,
    /// Cells that ran and produced a different value. These, and only these, can put a pane
    /// on the wire.
    pub changed: usize,
    /// Cells the pass never looked at: `total_cells - visited`. The headline number, and zero
    /// in every rerun-the-script runtime.
    pub untouched: usize,
    /// Wall-clock microseconds for the pass. `None` from anywhere that has no clock —
    /// `dagpane-core` is clockless by design, so this is filled in by whoever ran the pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub micros: Option<u64>,
}

impl PassStats {
    /// The counts from a trace, with `micros` left unset — `dagpane-core` has no clock, so
    /// only a caller that timed the pass can fill it in with [`PassStats::with_micros`].
    pub fn from_trace(trace: &Trace) -> PassStats {
        PassStats {
            epoch: trace.epoch,
            total_cells: trace.total_cells,
            visited: trace.visited(),
            evaluated: trace.evaluated(),
            reused: trace.reused(),
            changed: trace.changed(),
            untouched: trace.untouched(),
            micros: None,
        }
    }

    /// Attaches the wall-clock time the caller measured around the pass.
    pub fn with_micros(mut self, micros: u64) -> PassStats {
        self.micros = Some(micros);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dagpane_core::Value;

    #[test]
    fn a_set_message_round_trips() {
        let mut values = std::collections::BTreeMap::new();
        values.insert("min_amount".to_string(), Value::float(250.0));
        let m = ClientMessage::Set { seq: 7, values };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"type\":\"set\""), "{json}");
        assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), m);
    }

    #[test]
    fn stats_are_absent_rather_than_null_when_there_is_no_clock() {
        let s = PassStats::default();
        assert!(!serde_json::to_string(&s).unwrap().contains("micros"));
        assert!(serde_json::to_string(&s.with_micros(5))
            .unwrap()
            .contains("\"micros\":5"));
    }

    #[test]
    fn an_unknown_message_type_is_rejected_rather_than_guessed() {
        assert!(serde_json::from_str::<ClientMessage>(r#"{"type":"nudge"}"#).is_err());
    }
}
