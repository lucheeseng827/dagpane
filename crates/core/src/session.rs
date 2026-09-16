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

use crate::digest::{Digest, Digestible};
use crate::error::{CellError, SessionError};
use crate::graph::{CellId, Graph, Inputs, Kind};
use crate::trace::{Step, StepOutcome, Trace};
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

#[derive(Debug)]
struct Slot {
    outcome: Arc<Outcome>,
    digest: Digest,
    /// The digests of this cell's inputs the last time its compute ran. The comparison
    /// against the current ones is the whole of the reuse decision.
    input_digests: Vec<Digest>,
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
    staged: Vec<(CellId, Value)>,
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
                input_digests: Vec::new(),
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
                initial, digest, ..
            } = &session.graph.nodes[id.index()].kind
            {
                // `Arc::clone`, not a copy of the value. Every session over this graph points
                // at one allocation per untouched source, and the digest was taken once when
                // the graph was built.
                let (initial, digest) = (Arc::clone(initial), *digest);
                let slot = &mut session.slots[id.index()];
                slot.outcome = initial;
                slot.digest = digest;
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
        // Last write wins within a pass: a client that drags a slider sends a stream of
        // values and only the one it stopped on matters.
        if let Some(slot) = self.staged.iter_mut().find(|(c, _)| *c == id) {
            slot.1 = value;
        } else {
            self.staged.push((id, value));
        }
        Ok(())
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
        for (id, value) in staged {
            let outcome = Outcome::ok(value);
            let digest = outcome.digest();
            let slot = &mut self.slots[id.index()];
            if slot.valid && slot.digest == digest {
                continue;
            }
            slot.outcome = Arc::new(outcome);
            slot.digest = digest;
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
        for (id, value) in staged {
            let outcome = Outcome::ok(value);
            let digest = outcome.digest();
            let slot = &mut self.slots[id.index()];
            if slot.valid && slot.digest == digest {
                continue;
            }
            slot.outcome = Arc::new(outcome);
            slot.digest = digest;
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

            let current: Vec<Digest> = node
                .inputs
                .iter()
                .map(|i| self.slots[i.index()].digest)
                .collect();

            if self.slots[id.index()].valid && self.slots[id.index()].input_digests == current {
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

            let result = match upstream {
                Some(err) => Err(err),
                None => {
                    let refs: Vec<&Value> = held
                        .iter()
                        .map(|o| o.value().expect("errors were handled above"))
                        .collect();
                    compute.eval(Inputs::new(&node.name, &node.input_names, &refs))
                }
            };

            let outcome = match result {
                Ok(value) => Outcome::ok(value),
                Err(error) => Outcome::err(error),
            };
            let digest = outcome.digest();
            let step = match outcome.error() {
                Some(e) => StepOutcome::Failed {
                    message: e.to_string(),
                },
                None => StepOutcome::Evaluated {
                    changed: !self.slots[id.index()].valid
                        || self.slots[id.index()].digest != digest,
                },
            };

            let slot = &mut self.slots[id.index()];
            if !slot.valid || slot.digest != digest {
                slot.changed_at = epoch;
            }
            slot.outcome = Arc::new(outcome);
            slot.digest = digest;
            slot.input_digests = current;
            slot.valid = true;

            steps.push(Step {
                cell: node.name.clone(),
                id,
                outcome: step,
            });
        }

        steps
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
