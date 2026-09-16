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

use dagpane_core::Trace;
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
    /// Send me the current state of everything. What a client does after a reconnect; costs
    /// a graph walk and no recomputation, because every cell reuses.
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
        /// What the session's first pass cost.
        stats: PassStats,
    },
    /// The panes whose value changed, and what it cost.
    Patch {
        /// The `seq` of the [`ClientMessage::Set`] this answers.
        seq: u64,
        /// Only the panes whose rendered view changed. A cell that recomputed to the same
        /// value, and a pane whose view is unchanged despite a changed cell, are both absent
        /// — this array is the product claim, in bytes a network tab can count.
        panes: Vec<PaneUpdate>,
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
