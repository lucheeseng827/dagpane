//! What a recompute pass actually did.
//!
//! This module is the product claim in data form. "Only the cells that depend on the input
//! recompute" is either a sentence in a README or a number a test can assert, and the
//! difference between those two is this file. Every pass returns a [`Trace`]; `dagpane
//! explain` prints one, the server ships a summary of one down the socket with every patch,
//! and the CI gate fails the build when the counts move.
//!
//! There are no timings here. [`crate::session`] is clockless on purpose — an engine that
//! needs a clock to be tested is an engine that behaves differently under a test harness —
//! so wall-clock time is measured by the caller around the pass and attached alongside.
//! `dagpane-serve` does exactly that.

use serde::{Deserialize, Serialize};

use crate::graph::CellId;

/// What happened to one cell that the pass visited.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum StepOutcome {
    /// A source cell whose value the pass changed. It is in the trace because the user
    /// touched it and a record of an interaction that omitted the thing the user touched
    /// would be a strange thing to read — and because `visited + untouched` has to add up
    /// to the whole app or the headline number is a lie.
    Set,
    /// The cell's compute ran.
    ///
    /// `changed` is the load-bearing half. `changed: false` is the **value-equality
    /// short-circuit**: the cell recomputed, produced a value identical to the one it
    /// already held, and therefore woke nothing downstream. A pass with many of these is
    /// telling you an edge is over-declared — the cell did not need to run — which is a cost
    /// to fix, not a bug.
    Evaluated {
        /// Whether the value it produced differs from the one it held. `false` means the
        /// pass stopped here: nothing downstream was woken.
        changed: bool,
    },
    /// Every one of the cell's inputs held the same value as when it was last computed, so
    /// the cached value stands and the compute did not run. This is where the saving is.
    Reused,
    /// The cell is in error: either its own compute failed, or an input was already in
    /// error and the compute was never called.
    Failed {
        /// The failure, rendered. An inherited one reads as ``upstream cell `x` failed: …``,
        /// so a step names the cell actually at fault without a second lookup.
        message: String,
    },
}

/// One visited cell.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// The cell's name. Carried in full rather than resolved by the reader, because a trace
    /// is read by `dagpane explain` and by a browser, neither of which holds the graph.
    pub cell: String,
    /// The cell's id, for a reader that does hold the graph and wants to index it.
    pub id: CellId,
    /// What the pass did with the cell. Flattened into the step on the wire, so a step is
    /// one JSON object rather than a wrapper around one.
    #[serde(flatten)]
    pub outcome: StepOutcome,
}

/// The record of one recompute pass.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trace {
    /// The pass number. Also the value written into every slot this pass changed, which is
    /// how the app layer asks "what moved since the last patch?".
    pub epoch: u64,
    /// How many cells the app has in total. The denominator of the claim.
    pub total_cells: usize,
    /// The inputs whose value actually changed. An input set to the value it already held
    /// does not appear here, and produces an empty pass.
    pub roots: Vec<String>,
    /// Every cell reached from `roots` over the dependency edges, in evaluation order.
    /// `steps.len()` is the size of the dirty closure: the cells the pass *touched*.
    pub steps: Vec<Step>,
}

impl Trace {
    /// Cells the pass touched at all.
    pub fn visited(&self) -> usize {
        self.steps.len()
    }

    /// Cells whose compute actually ran.
    pub fn evaluated(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| matches!(s.outcome, StepOutcome::Evaluated { .. }))
            .count()
    }

    /// Source cells the pass changed.
    pub fn set(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| matches!(s.outcome, StepOutcome::Set))
            .count()
    }

    /// Cells that served a cached value because none of their inputs had moved.
    pub fn reused(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| matches!(s.outcome, StepOutcome::Reused))
            .count()
    }

    /// Cells that ran and produced a value different from the one they held. These, and
    /// only these, are what a client has to be sent.
    pub fn changed(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| matches!(s.outcome, StepOutcome::Evaluated { changed: true }))
            .count()
    }

    /// Cells that ran and produced the value they already had — work the app did not need
    /// to do. See [`StepOutcome::Evaluated`].
    pub fn short_circuited(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| matches!(s.outcome, StepOutcome::Evaluated { changed: false }))
            .count()
    }

    /// Cells left in error by this pass, whether their own compute failed or an input's did.
    pub fn failed(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| matches!(s.outcome, StepOutcome::Failed { .. }))
            .count()
    }

    /// Cells the pass never looked at. The headline number, and the reason the project
    /// exists: in a rerun runtime this is always zero.
    pub fn untouched(&self) -> usize {
        self.total_cells.saturating_sub(self.visited())
    }

    /// The names of the cells whose compute ran, in evaluation order. This is what a test
    /// asserts against when the count alone would not distinguish "the right three cells ran"
    /// from "three cells ran".
    pub fn names_evaluated(&self) -> Vec<&str> {
        self.steps
            .iter()
            .filter(|s| matches!(s.outcome, StepOutcome::Evaluated { .. }))
            .map(|s| s.cell.as_str())
            .collect()
    }

    /// The one-line form, for a console and for a log.
    ///
    /// ```text
    /// epoch 8 — 37 cells, visited 5, evaluated 3, reused 2, changed 2, untouched 32
    /// ```
    pub fn summary(&self) -> String {
        format!(
            "epoch {} — {} cells, visited {}, evaluated {}, reused {}, changed {}, untouched {}",
            self.epoch,
            self.total_cells,
            self.visited(),
            self.evaluated(),
            self.reused(),
            self.changed(),
            self.untouched()
        )
    }
}
