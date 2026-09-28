//! The recompute pass: per-session state, and the loop that decides what runs.
//!
//! # The invariants
//!
//! Two properties are what a reactive runtime is *for*, and everything in this file exists
//! to hold them:
//!
//! **Glitch freedom.** No cell ever observes a mixture of old and new upstream values. The
//! guarantee is structural, not defensive: every cell carries a `height` — the longest path
//! from any source — computed once when the graph was built, and the pass evaluates in
//! ascending height. Every edge runs from a lower height to a strictly higher one, so by
//! the time a cell is evaluated every one of its inputs has already reached its final value
//! for this pass. A diamond (`a -> b`, `a -> c`, `{b,c} -> d`) evaluates `b` and `c` before
//! `d`, once each, and `d` sees both new values or neither. There is no re-entrancy, no
//! second pass, and no "recompute until stable" loop to converge.
//!
//! **A cell recomputes only when an input's *value* changed.** Dirty marks propagate over
//! the whole downstream closure — cheap, structural, one bit per cell — but a mark is only a
//! *candidate*. Before running a cell the pass compares the digests of its inputs against
//! the digests it recorded the last time it ran. Equal digests mean equal values, so the
//! cached output stands and the compute is skipped. That is what makes an over-declared
//! edge cost one recomputation instead of a cascade, and it is why a cell that recomputes to
//! the same value stops the pass dead at its own boundary.
//!
//! # What this costs, stated plainly
//!
//! The dirty closure is *structural*: a pass visits every cell downstream of the changed
//! input, even the ones that will turn out to reuse. Visiting is a digest comparison per
//! input, so it is cheap, but it is not free and it is not zero. `Trace::visited` reports it
//! and never hides it behind `Trace::evaluated`.

use std::sync::Arc;

use crate::digest::{Digest, Digestible, Hasher};
use crate::error::{CellError, SessionError};
use crate::frame::{column_digests, digest_frame_from_columns, shape_digest};
use crate::graph::{CellId, Graph, Inputs, Kind};
use crate::reads::{InputReads, ReadLog, RowRule};
use crate::trace::{Step, StepOutcome, Trace};
use crate::transform::{ordering, ordering_digest, selection, selection_digest};
use crate::value::Value;

/// What a cell holds: a value, or the error that stands in place of one.
///
/// An error is a value here, not a control-flow event. A data app is a place where one bad
/// column should take out one number and leave the page — and the slider that will fix it —
/// working.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Outcome {
    /// The cell computed successfully.
    Value {
        /// What it produced. [`Value::Null`] is a success like any other — a cell with
        /// nothing to show is not a cell that failed.
        value: Value,
    },
    /// The cell is in error, either its own or one inherited from upstream.
    Error {
        /// The failure. Digested along with the value it replaces, so a cell that keeps
        /// failing the same way keeps its dependents asleep and one that recovers wakes them.
        error: CellError,
    },
}

impl Outcome {
    /// A successful outcome.
    pub fn ok(value: Value) -> Outcome {
        Outcome::Value { value }
    }

    /// A failed one. The engine builds these while propagating; a compute closure returns
    /// `Err(CellError)` instead and never constructs an `Outcome` itself.
    pub fn err(error: CellError) -> Outcome {
        Outcome::Error { error }
    }

    /// The value, or `None` if the cell is in error.
    pub fn value(&self) -> Option<&Value> {
        match self {
            Outcome::Value { value } => Some(value),
            Outcome::Error { .. } => None,
        }
    }

    /// The error, or `None` if the cell holds a value.
    pub fn error(&self) -> Option<&CellError> {
        match self {
            Outcome::Error { error } => Some(error),
            Outcome::Value { .. } => None,
        }
    }

    /// Whether the cell is in error, for callers that only need to branch and not to read.
    pub fn is_err(&self) -> bool {
        matches!(self, Outcome::Error { .. })
    }
}

impl Digestible for Outcome {
    /// An error digests like any other value — message *and* cause, see the `Digestible`
    /// impl on [`CellError`] — so a cell that fails the same way twice does not re-wake
    /// everything below it, a cell that recovers does, and a cell whose failure moved to a
    /// different origin stops naming the old one.
    fn digest_into(&self, h: &mut crate::digest::Hasher) {
        match self {
            Outcome::Value { value } => {
                h.tag(0xa0);
                value.digest_into(h);
            }
            Outcome::Error { error } => {
                h.tag(0xa1);
                error.digest_into(h);
            }
        }
    }
}

/// The column-level digests of a frame value, taken once when the value was produced.
///
/// Only a frame has one. A slider has no columns to be granular about, and a cell holding a
/// scalar or an error is compared whole as it always was.
#[derive(Debug)]
pub(crate) struct Grain {
    /// Row count and whole schema — see [`crate::frame::shape_digest`]. Carried in every
    /// granular key so that a cell reading no columns still wakes when a row appears.
    shape: Digest,
    /// One digest per column, left to right.
    columns: Vec<Digest>,
}

/// What a cell recorded about one input the last time it ran, and what a later pass
/// compares against that input to decide whether the cached value still stands.
#[derive(Clone, Debug, PartialEq)]
enum InputKey {
    /// The input's whole-value digest. What every non-frame input gets, and what a compute
    /// that did not narrow gets — which is to say, exactly the behaviour this engine had
    /// before sub-node invalidation.
    Whole(Digest),
    /// The input's shape, the digests of the columns the cell actually read, and the
    /// *constraints* it placed on columns whose values it never used.
    ///
    /// A change to any column outside both lists leaves every one of these equal and the
    /// cell asleep.
    Narrowed {
        /// [`Grain::shape`] as it stood when the cell ran.
        shape: Digest,
        /// `(column index, that column's digest)`, in index order.
        cols: Vec<(u32, Digest)>,
        /// Rules whose *answer about the rows* the cell depended on while their column's
        /// values never reached its output.
        constraints: Vec<RowKey>,
    },
}

/// A row constraint, as the engine stores and checks it.
///
/// This is constrained memoization proper: the key is not a value but a *question and its
/// answer*. "Which rows of column 7 satisfy `>= 400`?" — and as long as the answer is the
/// same list of rows, a cell that filtered on it and totalled something else cannot have
/// moved, however much column 7 itself did. "In what order does column 7 rank the rows?"
/// is the same shape of question, and a uniform shift down a metric column answers it
/// identically.
#[derive(Clone, Debug, PartialEq)]
struct RowKey {
    /// The column the rule reads.
    column: u32,
    /// That column's digest when the cell last ran.
    ///
    /// Checked first and it is what keeps this cheap: an unmoved column cannot have moved
    /// its answer, so the overwhelmingly common case costs one digest compare and the rule
    /// is never re-run at all.
    column_digest: Digest,
    /// The question, re-runnable against the input frame.
    rule: RowRule,
    /// The answer: [`crate::transform::selection_digest`] of the rows a filter kept, or
    /// [`crate::transform::ordering_digest`] of the order a sort produced.
    rows: Digest,
}

#[derive(Debug)]
struct Slot {
    outcome: Arc<Outcome>,
    digest: Digest,
    /// This slot's per-column digests, when it holds a frame. `None` for everything else.
    grain: Option<Arc<Grain>>,
    /// What this cell recorded about each of its inputs the last time its compute ran. The
    /// comparison against the inputs as they stand now is the whole of the reuse decision.
    input_keys: Vec<InputKey>,
    /// False until the cell has been computed once. A source is valid from construction;
    /// a computed cell is not, which is why the first pass evaluates everything and says so.
    valid: bool,
    /// The epoch at which this slot's value last *changed*. The app layer diffs against it
    /// to build a patch, so a cell that recomputed to the same value is correctly absent
    /// from the wire.
    changed_at: u64,
}

/// One user's state: the values, and nothing else.
///
/// The graph is shared (`Arc<Graph>`) and immutable; a session is a vector of slots beside
/// it. That is the multi-tenancy story in one sentence — N sessions cost N × (values), never
/// N × (app), and never a process each.
///
/// **Exclusivity, stated accurately.** Every mutating entry point takes `&mut self`, so a
/// pass cannot interleave with another pass or with a read — the borrow checker enforces
/// that, and it is the whole guarantee. Streamlit's `session_state` not being thread-safe is
/// a named flaw of the incumbent; the answer here is not a lock the caller can forget but a
/// signature that will not compile without exclusive access.
///
/// This comment used to add "and deliberately not `Sync`". That was simply false — every
/// field is `Sync`, so the auto-impl applies — and a stated type-level invariant the compiler
/// does not enforce is worse than no statement. It is also not a property worth having:
/// sharing `&Session` across threads for reads (`get`, `share`, `changed_since`) is harmless,
/// and forbidding it with a `PhantomData` would buy nothing. `session_is_send_and_sync` in
/// `tests/reactive.rs` pins what is actually true.
pub struct Session {
    graph: Arc<Graph>,
    slots: Vec<Slot>,
    epoch: u64,
    staged: Vec<(CellId, Outcome)>,
    /// Visited marks for the dirty walk, stamped with the epoch rather than cleared. Epochs
    /// only ever increase, so a stale stamp can never be mistaken for a fresh one and the
    /// walk costs nothing per pass in clearing.
    mark: Vec<u64>,
}

impl std::fmt::Debug for Session {
    /// Deliberately not the values. A session holds a user's data, and a `{:?}` in a log
    /// line is the least deliberate way for it to leave the process.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("cells", &self.slots.len())
            .field("epoch", &self.epoch)
            .field("staged", &self.staged.len())
            .finish()
    }
}

impl Session {
    /// A fresh session: sources hold their declared initial values, computed cells hold
    /// nothing yet. Nothing has run — call [`Session::refresh`] for the first render.
    pub fn new(graph: Arc<Graph>) -> Session {
        let slots = (0..graph.len())
            .map(|_| Slot {
                outcome: Arc::new(Outcome::ok(Value::Null)),
                digest: Digest::EMPTY,
                grain: None,
                input_keys: Vec::new(),
                valid: false,
                changed_at: 0,
            })
            .collect::<Vec<_>>();

        let mut session = Session {
            mark: vec![0; slots.len()],
            slots,
            epoch: 0,
            staged: Vec::new(),
            graph,
        };

        for id in session.graph.order().to_vec() {
            if let Kind::Source {
                initial,
                digest,
                grain,
                ..
            } = &session.graph.nodes[id.index()].kind
            {
                // `Arc::clone`, not a copy of the value. Every session over this graph points
                // at one allocation per untouched source, and the digest was taken once when
                // the graph was built.
                // Every one of these is an `Arc` bump or a `Copy`. A source's per-column
                // digests were taken once when the graph was built, beside its whole-value
                // digest, so opening a session over a hundred-column table costs the same as
                // opening one over a slider.
                let (initial, digest, grain) = (Arc::clone(initial), *digest, grain.clone());
                let slot = &mut session.slots[id.index()];
                slot.outcome = initial;
                slot.digest = digest;
                slot.grain = grain;
                slot.valid = true;
            }
        }

        session
    }

    /// The graph this session runs. Shared: every session over one app holds the same
    /// `Arc`, and cloning it is how a caller keeps the app's structure — names, heights,
    /// closures — past the session's borrow.
    pub fn graph(&self) -> &Arc<Graph> {
        &self.graph
    }

    /// The number of passes this session has completed. Monotonic, and the version a caller
    /// remembers so it can ask [`Session::changed_since`] what to send next.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Stage a new value for a source cell. Nothing runs until [`Session::commit`].
    ///
    /// Staging rather than applying is what makes one interaction one pass: a client that
    /// moves two sliders in the same gesture sets both and commits once, and every cell
    /// downstream of both runs exactly once with both new values. Applying eagerly would
    /// run the shared subgraph twice and — for the moment between the two — with a mix of
    /// old and new, which is the glitch this engine exists to not have.
    pub fn set(&mut self, name: &str, value: Value) -> Result<(), SessionError> {
        let id = self
            .graph
            .id(name)
            .ok_or_else(|| SessionError::UnknownCell(name.to_string()))?;
        self.set_id(id, value)
    }

    /// Whether [`Session::set`] would accept this value, without staging it.
    ///
    /// Exists so a caller applying a batch can check every entry before applying any of
    /// them. Half-applying a batch and then reporting a rejection leaves the caller's idea of
    /// the state and the session's disagreeing, with nothing to say which is right.
    pub fn can_set(&self, name: &str, value: &Value) -> Result<(), SessionError> {
        let id = self
            .graph
            .id(name)
            .ok_or_else(|| SessionError::UnknownCell(name.to_string()))?;
        let node = &self.graph.nodes[id.index()];
        let declared = match &node.kind {
            Kind::Source { declared, .. } => *declared,
            Kind::Computed { .. } => {
                return Err(SessionError::NotAnInput {
                    name: node.name.clone(),
                })
            }
        };
        if !type_compatible(declared, value) {
            return Err(SessionError::TypeMismatch {
                name: node.name.clone(),
                expected: declared,
                found: value.type_name(),
            });
        }
        Ok(())
    }

    /// [`Session::set`] by id, skipping the name lookup — what the app layer uses once it
    /// has resolved a widget to a cell.
    ///
    /// # Errors
    ///
    /// [`SessionError::ForeignCell`] if the id is not this graph's,
    /// [`SessionError::NotAnInput`] if the cell is computed, or
    /// [`SessionError::TypeMismatch`] if the value's type does not match the declared one.
    pub fn set_id(&mut self, id: CellId, value: Value) -> Result<(), SessionError> {
        if !self.graph.contains(id) {
            return Err(SessionError::ForeignCell(id));
        }
        let node = &self.graph.nodes[id.index()];
        let declared = match &node.kind {
            Kind::Source { declared, .. } => *declared,
            Kind::Computed { .. } => {
                return Err(SessionError::NotAnInput {
                    name: node.name.clone(),
                })
            }
        };
        if !type_compatible(declared, &value) {
            return Err(SessionError::TypeMismatch {
                name: node.name.clone(),
                expected: declared,
                found: value.type_name(),
            });
        }
        self.stage(id, Outcome::ok(value));
        Ok(())
    }

    /// Stage an **outcome** — a value, or the error standing in place of one — for a source.
    ///
    /// [`Session::set`] is this with `Outcome::ok`, and is what a widget uses. This one
    /// exists for the frontier of a split graph: see [`crate::placement`]. A boundary cell
    /// is computed on the far side, so what has to cross is whatever that cell holds, and in
    /// this engine a cell holds an `Outcome` — a failure upstream of the cut has to arrive
    /// as a failure, not as a null that the client would draw as a legitimate empty answer.
    ///
    /// Errors are values here (`docs/adr/0004-errors-are-values.md`), so this widens the
    /// door by exactly the amount that sentence already promised.
    ///
    /// # Errors
    ///
    /// As [`Session::set_id`], except that an `Outcome::Error` skips the declared-type check
    /// — an error has no type to match, and refusing to deliver one would leave the client
    /// showing the last good value of a cell that is now broken.
    pub fn set_outcome(&mut self, name: &str, outcome: Outcome) -> Result<(), SessionError> {
        let id = self
            .graph
            .id(name)
            .ok_or_else(|| SessionError::UnknownCell(name.to_string()))?;
        self.set_outcome_id(id, outcome)
    }

    /// Whether [`Session::set_outcome`] would accept this, without staging it.
    ///
    /// The same reason [`Session::can_set`] exists, for the same caller shape: a batch has to
    /// be checkable before any of it is applied. `crates/core/src/placement.rs` is the one
    /// that needs it — a frontier refused halfway through would otherwise leave its earlier
    /// cells staged, and the caller's next commit would apply a fragment of an update the
    /// caller was told had failed.
    ///
    /// # Errors
    ///
    /// As [`Session::set_outcome_id`].
    pub fn can_set_outcome(&self, name: &str, outcome: &Outcome) -> Result<(), SessionError> {
        let id = self
            .graph
            .id(name)
            .ok_or_else(|| SessionError::UnknownCell(name.to_string()))?;
        match outcome.value() {
            Some(v) => self.can_set(name, v),
            // An error has no type to check against the declared one; it only has to be
            // going somewhere that can hold it.
            None => match self.graph.nodes[id.index()].kind {
                Kind::Source { .. } => Ok(()),
                Kind::Computed { .. } => Err(SessionError::NotAnInput {
                    name: name.to_string(),
                }),
            },
        }
    }

    /// [`Session::set_outcome`] by id.
    ///
    /// # Errors
    ///
    /// [`SessionError::ForeignCell`], [`SessionError::NotAnInput`] or
    /// [`SessionError::TypeMismatch`], as [`Session::set_id`].
    pub fn set_outcome_id(&mut self, id: CellId, outcome: Outcome) -> Result<(), SessionError> {
        match outcome.value() {
            Some(v) => {
                let v = v.clone();
                self.set_id(id, v)?;
                // `set_id` staged an equal outcome; nothing further to do.
                Ok(())
            }
            None => {
                if !self.graph.contains(id) {
                    return Err(SessionError::ForeignCell(id));
                }
                let node = &self.graph.nodes[id.index()];
                if matches!(node.kind, Kind::Computed { .. }) {
                    return Err(SessionError::NotAnInput {
                        name: node.name.clone(),
                    });
                }
                self.stage(id, outcome);
                Ok(())
            }
        }
    }

    /// Last write wins within a pass: a client that drags a slider sends a stream of values
    /// and only the one it stopped on matters.
    fn stage(&mut self, id: CellId, outcome: Outcome) {
        if let Some(slot) = self.staged.iter_mut().find(|(c, _)| *c == id) {
            slot.1 = outcome;
        } else {
            self.staged.push((id, outcome));
        }
    }

    /// Apply everything staged and recompute exactly what depends on whatever actually
    /// changed.
    ///
    /// Setting an input to the value it already holds is not a change: `commit` returns an
    /// empty trace and no cell runs. That is not an optimisation, it is the contract — a
    /// client that re-sends its state on reconnect must not cause a recomputation.
    pub fn commit(&mut self) -> Trace {
        self.epoch += 1;
        let epoch = self.epoch;

        let staged = std::mem::take(&mut self.staged);
        let mut roots: Vec<CellId> = Vec::with_capacity(staged.len());
        let mut root_names: Vec<String> = Vec::with_capacity(staged.len());
        for (id, outcome) in staged {
            let (digest, grain) = weigh(&outcome);
            let slot = &mut self.slots[id.index()];
            if slot.valid && slot.digest == digest {
                continue;
            }
            slot.outcome = Arc::new(outcome);
            slot.digest = digest;
            slot.grain = grain;
            slot.valid = true;
            slot.changed_at = epoch;
            roots.push(id);
            root_names.push(self.graph.name(id).to_string());
        }

        if roots.is_empty() {
            return Trace {
                epoch,
                total_cells: self.graph.len(),
                roots: root_names,
                steps: Vec::new(),
            };
        }

        let plan = self.dirty_closure(&roots);
        let closure_size = plan.len();
        let steps = self.evaluate(plan, epoch);
        let trace = Trace {
            epoch,
            total_cells: self.graph.len(),
            roots: root_names,
            steps,
        };
        debug_assert_trace(&trace, closure_size);
        trace
    }

    /// Evaluate every cell that needs it, over the whole graph.
    ///
    /// This is the first render, and the only pass that is allowed to be O(cells): computed
    /// slots start invalid, so the first `refresh` runs all of them and the trace says so.
    /// A later `refresh` is nearly free — every cell reuses — which makes it the right thing
    /// to call after a reconnect, when the client needs the current state and no work.
    pub fn refresh(&mut self) -> Trace {
        self.epoch += 1;
        let epoch = self.epoch;
        let staged = std::mem::take(&mut self.staged);
        let mut root_names = Vec::new();
        for (id, outcome) in staged {
            let (digest, grain) = weigh(&outcome);
            let slot = &mut self.slots[id.index()];
            if slot.valid && slot.digest == digest {
                continue;
            }
            slot.outcome = Arc::new(outcome);
            slot.digest = digest;
            slot.grain = grain;
            slot.valid = true;
            slot.changed_at = epoch;
            root_names.push(self.graph.name(id).to_string());
        }
        let plan: Vec<CellId> = self.graph.order().to_vec();
        let closure_size = plan.len();
        let steps = self.evaluate(plan, epoch);
        let trace = Trace {
            epoch,
            total_cells: self.graph.len(),
            roots: root_names,
            steps,
        };
        debug_assert_trace(&trace, closure_size);
        trace
    }

    /// The current value of a cell.
    pub fn get(&self, name: &str) -> Result<&Outcome, SessionError> {
        let id = self
            .graph
            .id(name)
            .ok_or_else(|| SessionError::UnknownCell(name.to_string()))?;
        Ok(self.get_id(id))
    }

    /// This cell's current outcome. A cell that has never been computed holds its source's
    /// initial value or, for a computed cell, [`Value::Null`] — so this never fails, and
    /// [`Session::is_computed`] is how a caller tells the two apart.
    ///
    /// # Panics
    ///
    /// If the id is from another graph.
    pub fn get_id(&self, id: CellId) -> &Outcome {
        &self.slots[id.index()].outcome
    }

    /// A cheap handle to a value, for a caller that wants to keep it past the borrow.
    pub fn share(&self, id: CellId) -> Arc<Outcome> {
        Arc::clone(&self.slots[id.index()].outcome)
    }

    /// The digest the cell's current outcome was recorded with. The engine's own comparison
    /// key, exposed because `dagpane explain` prints it: two runs that agree on these agree
    /// on every value, and a diff of them says exactly where they parted.
    ///
    /// # Panics
    ///
    /// If the id is from another graph.
    pub fn digest_of(&self, id: CellId) -> Digest {
        self.slots[id.index()].digest
    }

    /// Whether the cell has been computed since it was last invalidated. Tracked separately
    /// from the digest because [`Digest::EMPTY`] is a legitimate digest of a legitimate
    /// value, so "never computed" cannot be spelled as a reserved digest.
    ///
    /// # Panics
    ///
    /// If the id is from another graph.
    pub fn is_computed(&self, id: CellId) -> bool {
        self.slots[id.index()].valid
    }

    /// Every cell whose value changed at or after `epoch`, in evaluation order.
    ///
    /// This is how a patch is built. A cell that recomputed to the same value is not here,
    /// which is why an interaction that moves a filter but not the summary sends the table
    /// and not the summary.
    pub fn changed_since(&self, epoch: u64) -> Vec<CellId> {
        self.graph
            .order()
            .iter()
            .copied()
            .filter(|id| self.slots[id.index()].changed_at >= epoch && self.slots[id.index()].valid)
            .collect()
    }

    // ── the pass ────────────────────────────────────────────────────────────────────────

    /// Every cell reachable downstream of `roots`, in ascending height.
    ///
    /// Height order is the glitch-freedom guarantee; see the module docs. The sort is over
    /// the closure, not the graph, so a one-cell change on a thousand-cell app sorts a
    /// handful of ids.
    fn dirty_closure(&mut self, roots: &[CellId]) -> Vec<CellId> {
        let graph = Arc::clone(&self.graph);
        let stamp = self.epoch;
        let mut stack: Vec<CellId> = Vec::with_capacity(roots.len());
        for &r in roots {
            if self.mark[r.index()] != stamp {
                self.mark[r.index()] = stamp;
                stack.push(r);
            }
        }
        let mut out: Vec<CellId> = Vec::new();
        while let Some(id) = stack.pop() {
            out.push(id);
            for &dep in graph.dependents_of(id) {
                if self.mark[dep.index()] != stamp {
                    self.mark[dep.index()] = stamp;
                    stack.push(dep);
                }
            }
        }
        out.sort_by_key(|id| (graph.height(*id), id.0));
        out
    }

    /// The loop. `plan` must already be in ascending height order.
    fn evaluate(&mut self, plan: Vec<CellId>, epoch: u64) -> Vec<Step> {
        let graph = Arc::clone(&self.graph);
        let mut steps = Vec::with_capacity(plan.len());

        for id in plan {
            let node = &graph.nodes[id.index()];
            let compute = match &node.kind {
                // A source has no compute: `commit` already wrote it. It earns a step only
                // when this pass is the one that changed it, so a `refresh` — whose plan is
                // the whole graph — does not report every slider in the app as touched.
                Kind::Source { .. } => {
                    if self.slots[id.index()].changed_at == epoch {
                        steps.push(Step {
                            cell: node.name.clone(),
                            id,
                            outcome: StepOutcome::Set,
                        });
                    }
                    continue;
                }
                Kind::Computed { compute } => Arc::clone(compute),
            };

            if self.slots[id.index()].valid && self.inputs_unmoved(id, &node.inputs) {
                steps.push(Step {
                    cell: node.name.clone(),
                    id,
                    outcome: StepOutcome::Reused,
                });
                continue;
            }

            // Hold the inputs by `Arc` rather than by reference: the borrow of `self.slots`
            // has to end before the slot for `id` can be written, and cloning an `Arc` is a
            // refcount bump whatever the value is — a million-row table included.
            let held: Vec<Arc<Outcome>> = node
                .inputs
                .iter()
                .map(|i| Arc::clone(&self.slots[i.index()].outcome))
                .collect();

            // An input in error short-circuits the cell without calling its compute, and
            // names the cell where the failure started rather than the immediate input, so a
            // message eight cells downstream still points at the cause.
            let upstream = held.iter().enumerate().find_map(|(i, o)| {
                o.error().map(|e| {
                    let cause = e.cause(graph.name(node.inputs[i])).to_string();
                    CellError::Upstream {
                        message: e.message().to_string(),
                        cause,
                    }
                })
            });

            // Where the compute records which columns of which inputs its output actually
            // depended on. An inherited error never reaches a compute, so the log stays
            // whole and the cell is compared whole — which is right: the value it holds is
            // that input's error, not some columns of it.
            let log = ReadLog::new(node.inputs.len());
            let result = match upstream {
                Some(err) => Err(err),
                None => {
                    let refs: Vec<&Value> = held
                        .iter()
                        .map(|o| o.value().expect("errors were handled above"))
                        .collect();
                    compute.eval(Inputs::new(&node.name, &node.input_names, &refs, &log))
                }
            };

            let outcome = match result {
                Ok(value) => Outcome::ok(value),
                Err(error) => Outcome::err(error),
            };
            let (digest, grain) = weigh(&outcome);
            let step = match outcome.error() {
                Some(e) => StepOutcome::Failed {
                    message: e.to_string(),
                },
                None => StepOutcome::Evaluated {
                    changed: !self.slots[id.index()].valid
                        || self.slots[id.index()].digest != digest,
                },
            };

            // Built before the slot is borrowed mutably, and from the log the compute just
            // wrote rather than from anything declared ahead of time.
            let keys = self.keys_for(&node.inputs, log.take());

            let slot = &mut self.slots[id.index()];
            if !slot.valid || slot.digest != digest {
                slot.changed_at = epoch;
            }
            slot.outcome = Arc::new(outcome);
            slot.digest = digest;
            slot.grain = grain;
            slot.input_keys = keys;
            slot.valid = true;

            steps.push(Step {
                cell: node.name.clone(),
                id,
                outcome: step,
            });
        }

        steps
    }

    /// Whether every key this cell recorded still matches the input it was taken from.
    ///
    /// This is the reuse decision, and the only place granularity is actually spent. A
    /// [`InputKey::Whole`] key compares one digest, exactly as this engine always did. A
    /// [`InputKey::Columns`] key compares the input's shape and then only the columns the
    /// cell read — so a two-hundred-column frame with one column rewritten leaves a cell
    /// that reads three others with three equal digests and an unchanged shape.
    fn inputs_unmoved(&self, id: CellId, inputs: &[CellId]) -> bool {
        let keys = &self.slots[id.index()].input_keys;
        // A cell whose input count changed cannot have its old keys compared against its
        // new inputs. The graph is immutable so this cannot happen today; it is checked
        // rather than asserted because the cost is one integer compare and the failure it
        // would otherwise allow is silent staleness.
        if keys.len() != inputs.len() {
            return false;
        }
        keys.iter().zip(inputs).all(|(key, input)| {
            let slot = &self.slots[input.index()];
            match key {
                InputKey::Whole(d) => slot.digest == *d,
                // The input is no longer a frame — a cell that produced a table now
                // produces a scalar, or an error. There is nothing to compare the recorded
                // columns against, so the cell runs.
                InputKey::Narrowed { .. } if slot.grain.is_none() => false,
                InputKey::Narrowed {
                    shape,
                    cols,
                    constraints,
                } => {
                    let grain = slot.grain.as_ref().expect("checked on the arm above");
                    grain.shape == *shape
                        && cols
                            .iter()
                            .all(|(at, d)| grain.columns.get(*at as usize) == Some(d))
                        && constraints
                            .iter()
                            .all(|c| constraint_still_holds(c, slot, grain))
                }
            }
        })
    }

    /// Turn what a compute recorded into the keys its next pass will be judged against.
    ///
    /// Every path that cannot be made granular falls back to [`InputKey::Whole`], which is
    /// this engine's pre-existing behaviour and never wrong — only coarser. That includes an
    /// input that is not a frame, and a narrowing naming a column the frame does not have.
    fn keys_for(&self, inputs: &[CellId], reads: Vec<InputReads>) -> Vec<InputKey> {
        inputs
            .iter()
            .zip(reads)
            .map(|(input, read)| {
                let slot = &self.slots[input.index()];
                let InputReads::Narrowed {
                    columns,
                    constraints,
                } = read
                else {
                    return InputKey::Whole(slot.digest);
                };
                let Some(grain) = slot.grain.as_ref() else {
                    return InputKey::Whole(slot.digest);
                };
                let mut taken = Vec::with_capacity(columns.len());
                for c in &columns {
                    match grain.columns.get(*c as usize) {
                        Some(d) => taken.push((*c, *d)),
                        // A column index the frame does not have. The compute and the frame
                        // disagree about the shape of the thing it just read, and the safe
                        // reading of a disagreement is to keep the whole digest.
                        None => return InputKey::Whole(slot.digest),
                    }
                }
                let mut kept = Vec::with_capacity(constraints.len());
                for c in constraints {
                    match grain.columns.get(c.column as usize) {
                        Some(d) => kept.push(RowKey {
                            column: c.column,
                            column_digest: *d,
                            rule: c.rule,
                            rows: c.rows,
                        }),
                        None => return InputKey::Whole(slot.digest),
                    }
                }
                InputKey::Narrowed {
                    shape: grain.shape,
                    cols: taken,
                    constraints: kept,
                }
            })
            .collect()
    }
}

/// Whether a row constraint still holds against the input as it stands now.
///
/// The cheap check first, and it is the one that almost always answers: if the column the
/// rule reads has not moved, its answer cannot have moved either, and nothing is re-run.
/// Only a column that genuinely changed costs a re-evaluation — one pass over one column,
/// against a cell whose compute would otherwise touch every column of every surviving row.
///
/// The two rules re-run to different digest tags on purpose (see
/// [`crate::transform::selection_digest`] and [`crate::transform::ordering_digest`]): the
/// rows `[0, 2, 5]` kept by a filter and the order `[0, 2, 5]` produced by a sort are
/// different answers to different questions, and neither may ever validate the other.
fn constraint_still_holds(c: &RowKey, slot: &Slot, grain: &Grain) -> bool {
    if grain.columns.get(c.column as usize) == Some(&c.column_digest) {
        return true;
    }
    let Some(frame) = slot.outcome.value().and_then(|v| v.as_frame()) else {
        return false;
    };
    // A rule that no longer runs against this frame at all — its column was renamed or
    // retyped out from under it — costs a recomputation rather than an answer. The shape
    // digest should already have caught that; this is the belt to its braces.
    match &c.rule {
        RowRule::Filter(spec) => match selection(frame, spec) {
            Ok(keep) => selection_digest(&keep) == c.rows,
            Err(_) => false,
        },
        RowRule::Sort { column, descending } => match ordering(frame, column, *descending) {
            Ok(order) => ordering_digest(&order) == c.rows,
            Err(_) => false,
        },
    }
}

/// The digest of an outcome and, when it holds a frame, its per-column digests beside it.
///
/// One walk over the data, not two. [`crate::frame::digest_frame`] composes the frame digest
/// from the column digests, so the bytes absorbed here are exactly the bytes
/// `outcome.digest()` would absorb — `weigh_agrees_with_the_plain_digest` in `tests/reactive.rs`
/// is what holds that, because the two are written out separately and nothing but a test
/// stops them drifting.
fn weigh(outcome: &Outcome) -> (Digest, Option<Arc<Grain>>) {
    if let Outcome::Value {
        value: Value::Frame { v },
    } = outcome
    {
        let frame = v.as_frame();
        let columns = column_digests(frame);
        let mut h = Hasher::new();
        h.tag(0xa0);
        digest_frame_from_columns(frame.rows(), &columns, &mut h);
        let shape = shape_digest(frame);
        return (h.finish(), Some(Arc::new(Grain { shape, columns })));
    }
    (outcome.digest(), None)
}

/// [`weigh`]'s grain half, for a value whose digest the caller already holds.
///
/// `pub(crate)` because [`crate::graph::Graph`] takes a source's grain **once**, when the
/// graph is built, exactly as it already takes the source's digest. Doing it per session
/// instead would make opening a session O(the data) rather than an `Arc` bump, and N viewers
/// of one app would each hash the same table — which is precisely the cost the shared graph
/// exists to not pay.
pub(crate) fn grain_of(outcome: &Outcome) -> Option<Arc<Grain>> {
    match outcome {
        Outcome::Value {
            value: Value::Frame { v },
        } => {
            let frame = v.as_frame();
            Some(Arc::new(Grain {
                shape: shape_digest(frame),
                columns: column_digests(frame),
            }))
        }
        _ => None,
    }
}

/// The arithmetic every trace has to satisfy, checked on every pass in a debug build.
///
/// Here rather than in the tests so that all of them get it for free: a scheduler change
/// that starts double-counting a cell, or that visits something outside the closure it
/// computed, fails the next test anybody runs rather than the one somebody remembered to
/// write.
fn debug_assert_trace(trace: &Trace, closure_size: usize) {
    debug_assert_eq!(
        trace.visited(),
        trace.set() + trace.evaluated() + trace.reused() + trace.failed(),
        "every visited cell has exactly one outcome"
    );
    debug_assert!(
        trace.visited() <= closure_size,
        "the pass visited {} cells but planned {closure_size}",
        trace.visited()
    );
    debug_assert!(
        trace.visited() <= trace.total_cells,
        "the pass visited more cells than the app has"
    );
}

/// Whether a value may be written into a source of declared type `declared`.
///
/// Deliberately narrow, and deliberately not exact:
///
///   * a source declared `Null` accepts anything — that is how an app says "this is set by
///     the host and I am not going to describe it";
///   * a `Float` source accepts an `Int`, because a slider sitting on a whole number
///     arrives as an integer from a browser and a client must not have to know that;
///   * everything else must match.
///
/// The check exists because the alternative is a type error surfacing inside somebody's
/// compute function, three cells away from the socket that caused it.
fn type_compatible(declared: &'static str, incoming: &Value) -> bool {
    match (declared, incoming.type_name()) {
        ("null", _) => true,
        ("float", "int") => true,
        (expected, found) => expected == found,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{Column, Table};

    fn frame() -> Value {
        Value::table(
            Table::new(vec![
                Column::int("a", vec![Some(1), Some(2), None]),
                Column::text("b", vec![Some("x".into()), None, Some("z".into())]),
                Column::bool("c", vec![Some(true), Some(false), None]),
            ])
            .expect("one row count"),
        )
    }

    #[test]
    fn weigh_agrees_with_the_plain_digest() {
        // `weigh` re-implements what `Outcome::digest_into` then `Value::digest_into` absorb,
        // so that a frame's whole digest and its per-column digests come out of one walk over
        // the data. Two hand-written encodings of one thing drift, and the drift here would
        // be silent: every cached value in every live session compared against a digest taken
        // the other way. This is the test that does not let them.
        let outcome = Outcome::ok(frame());
        let (digest, grain) = weigh(&outcome);
        assert_eq!(digest, outcome.digest(), "weigh and Digestible disagree");
        assert_eq!(grain.expect("a frame has a grain").columns.len(), 3);
    }

    #[test]
    fn a_scalar_outcome_has_no_grain_and_still_digests() {
        let outcome = Outcome::ok(Value::int(7));
        let (digest, grain) = weigh(&outcome);
        assert_eq!(digest, outcome.digest());
        assert!(
            grain.is_none(),
            "an int has no columns to be granular about"
        );

        let failed = Outcome::err(CellError::failed("no"));
        let (digest, grain) = weigh(&failed);
        assert_eq!(digest, failed.digest());
        assert!(grain.is_none());
    }

    #[test]
    fn a_sources_column_digests_are_taken_by_the_graph_and_not_by_each_session() {
        // The property, pinned where a future edit would break it. Moving this work back into
        // `Session::new` would make opening a session O(the data) and would have N viewers of
        // one app each hash the same table — which is exactly the cost `Arc<Graph>` exists to
        // avoid, and which no test but this one would notice.
        let mut b = Graph::builder();
        b.source("wide", frame());
        b.source("knob", Value::int(1));
        let graph = b.build().expect("two sources are acyclic");

        let wide = graph.id("wide").expect("declared");
        let knob = graph.id("knob").expect("declared");
        match &graph.nodes[wide.index()].kind {
            Kind::Source { grain, .. } => {
                let grain = grain.as_ref().expect("a frame source carries its grain");
                assert_eq!(grain.columns.len(), 3);
            }
            Kind::Computed { .. } => panic!("`wide` is a source"),
        }
        match &graph.nodes[knob.index()].kind {
            Kind::Source { grain, .. } => {
                assert!(grain.is_none(), "an int source has no columns");
            }
            Kind::Computed { .. } => panic!("`knob` is a source"),
        }

        // And every session points at that one allocation rather than making its own.
        let a = Session::new(Arc::clone(&graph));
        let c = Session::new(Arc::clone(&graph));
        let ga = a.slots[wide.index()].grain.as_ref().expect("a frame");
        let gc = c.slots[wide.index()].grain.as_ref().expect("a frame");
        assert!(
            Arc::ptr_eq(ga, gc),
            "two sessions each computed their own column digests for one shared source"
        );
    }
}
