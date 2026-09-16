//! The graph: cells, declared edges, and everything that can be computed about them once.
//!
//! A [`Graph`] is immutable and shared. Every session holds an `Arc<Graph>` and its own
//! slice of values, which is the whole multi-tenancy story: the expensive artefact — the
//! topological order, the reverse edges, the compute closures — is built once at start-up
//! and never copied, and a session is a vector of slots.
//!
//! # Why the edges are declared
//!
//! The two live options were declared edges (Dash's callback graph, this) and traced edges
//! (Observable, marimo, and every Rust signal library, which discover a dependency by
//! recording reads through a thread-local observer while a closure runs). Tracing is
//! friendlier to write and it is the wrong trade here, for three reasons that all point the
//! same way:
//!
//!   * **A cycle becomes a build error rather than a run-time surprise.** A traced graph
//!     cannot know its edges until it has run the closure, so it finds a cycle by running
//!     into it, in front of a user, in one particular session.
//!   * **The graph can be built once and shared immutably.** Traced edges are discovered
//!     per evaluation, so they live in per-session mutable state and every session pays to
//!     rediscover the same structure.
//!   * **`dagpane graph` can print the app before it runs.** The claim this project makes
//!     is about *which cells run*; a structure that only exists mid-evaluation cannot be
//!     inspected, diffed in review, or asserted on in CI.
//!
//! The cost is real and is not hidden: a cell that reads an input only on some branch still
//! declares it, so it recomputes when that input changes even though its output will not
//! move. The digest short-circuit in [`crate::session`] contains the damage — the cell
//! recomputes, produces the same value, and nothing downstream of it runs — which turns an
//! over-declared edge from a correctness problem into a cost. See docs/adr/0002.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{BuildError, CellError};
use crate::value::Value;

/// A cell's identity: an index into the graph's node vector.
///
/// Indices rather than names because the hot loop indexes slots several times per cell and
/// a string hash per access would be the dominant cost of a small pass. Ids are only valid
/// for the graph that issued them; [`crate::session::Session`] checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CellId(pub(crate) u32);

impl CellId {
    /// The raw index, for callers keeping their own parallel array of per-cell state. Valid
    /// as a subscript only against the graph that issued the id — [`Graph::contains`] is the
    /// check, and using an id from another graph is [`crate::error::SessionError::ForeignCell`].
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for CellId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// The values a cell's compute function sees: its declared inputs, in declaration order.
///
/// Errors never reach here. If any input is in error the engine marks this cell
/// [`CellError::Upstream`] and does not call the compute at all, so a cell author writes
/// the happy path and nothing else.
#[derive(Debug)]
pub struct Inputs<'a> {
    cell: &'a str,
    names: &'a [String],
    values: &'a [&'a Value],
}

impl<'a> Inputs<'a> {
    pub(crate) fn new(cell: &'a str, names: &'a [String], values: &'a [&'a Value]) -> Inputs<'a> {
        Inputs {
            cell,
            names,
            values,
        }
    }

    /// How many inputs this cell declared. Fixed at build time, so a compute may index up to
    /// it without checking.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the cell declared no inputs. True of a computed cell that is a constant — the
    /// engine evaluates it once and never again, since nothing can make it dirty.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The `i`th input, untyped.
    ///
    /// # Panics
    ///
    /// If `i` is past [`Inputs::len`]. Deliberate: the index is against a list the cell
    /// itself declared, so an out-of-range one is a bug in the cell, not bad data, and it
    /// should not be reportable as a [`CellError`] the user is asked to make sense of.
    pub fn get(&self, i: usize) -> &'a Value {
        self.values[i]
    }

    /// By declared input name. A cell with six inputs is unreadable by position.
    pub fn by_name(&self, name: &str) -> Option<&'a Value> {
        self.names
            .iter()
            .position(|n| n == name)
            .map(|i| self.values[i])
    }

    fn mismatch(&self, i: usize, expected: &str) -> CellError {
        CellError::failed(format!(
            "cell `{}`: input `{}` should be {expected}, got {}",
            self.cell,
            self.names.get(i).map(String::as_str).unwrap_or("?"),
            self.values[i].type_name()
        ))
    }

    /// The `i`th input as an integer.
    ///
    /// The five typed accessors below are the normal way to read an input: each returns a
    /// [`CellError`] naming the cell, the input and both types, so a mismatch reaches the
    /// user as a sentence instead of as a panic. A float is not accepted here — see
    /// [`crate::value::Value::as_int`].
    ///
    /// # Errors
    ///
    /// [`CellError::Failed`] if the input is not an `Int`.
    pub fn int(&self, i: usize) -> Result<i64, CellError> {
        self.values[i]
            .as_int()
            .ok_or_else(|| self.mismatch(i, "an int"))
    }

    /// The `i`th input as a float. An `Int` widens, so a slider on a whole number still
    /// reads.
    ///
    /// # Errors
    ///
    /// [`CellError::Failed`] if the input is neither `Float` nor `Int`.
    pub fn float(&self, i: usize) -> Result<f64, CellError> {
        self.values[i]
            .as_float()
            .ok_or_else(|| self.mismatch(i, "a number"))
    }

    /// The `i`th input as a boolean.
    ///
    /// # Errors
    ///
    /// [`CellError::Failed`] if the input is not a `Bool`.
    pub fn bool(&self, i: usize) -> Result<bool, CellError> {
        self.values[i]
            .as_bool()
            .ok_or_else(|| self.mismatch(i, "a bool"))
    }

    /// The `i`th input as text. Borrowed for the session's lifetime, not the call's, so a
    /// compute can build its result out of slices of its inputs.
    ///
    /// # Errors
    ///
    /// [`CellError::Failed`] if the input is not `Text`.
    pub fn text(&self, i: usize) -> Result<&'a str, CellError> {
        self.values[i]
            .as_text()
            .ok_or_else(|| self.mismatch(i, "text"))
    }

    /// The `i`th input as a frame, borrowed — the reason a transform over a large frame
    /// costs one pass and not a clone.
    ///
    /// Was `table`, returning a `&Table`. A cell body reads rows and columns through the
    /// trait now, which is what lets the same body run over either backend without
    /// knowing which one it got.
    ///
    /// # Errors
    ///
    /// [`CellError::Failed`] if the input is not a frame.
    pub fn frame(&self, i: usize) -> Result<&'a dyn crate::frame::Frame, CellError> {
        self.values[i]
            .as_frame()
            .ok_or_else(|| self.mismatch(i, "a table"))
    }

    /// The input's frame as a shared handle, for a caller that needs to keep it.
    ///
    /// An `Arc` bump rather than a copy — a pipeline that applies no step returns the very
    /// frame it was given, which is what the old `Cow::Borrowed` fast path bought.
    pub fn frame_arc(
        &self,
        i: usize,
    ) -> Result<std::sync::Arc<dyn crate::frame::Frame>, CellError> {
        match &self.values[i] {
            crate::value::Value::Frame { v } => Ok(v.arc()),
            _ => Err(self.mismatch(i, "a table")),
        }
    }
}

/// What a computed cell does.
///
/// `Send + Sync` because the graph is shared across sessions and sessions across threads;
/// `'static` because the graph outlives every session that borrows it. A closure that
/// captures a connection pool satisfies all three; one that captures a `&str` from `main`
/// does not, and that is the intended pressure — a compute must be as long-lived as the app.
pub trait Compute: Send + Sync + 'static {
    /// Produce this cell's value from its declared inputs.
    ///
    /// Takes `&self`: a compute is called concurrently by sessions on different threads and
    /// must not accumulate state between passes. It must also be *pure* in the sense the
    /// engine relies on — the same inputs give the same output — because a cell whose result
    /// depends on anything else will be skipped by the digest comparison exactly when it
    /// mattered.
    ///
    /// # Errors
    ///
    /// [`CellError`] for a failure the user should see. The pass continues either way; the
    /// error becomes this cell's value and every cell below it is marked
    /// [`CellError::Upstream`].
    fn eval(&self, inputs: Inputs<'_>) -> Result<Value, CellError>;
}

impl<F> Compute for F
where
    F: for<'a> Fn(Inputs<'a>) -> Result<Value, CellError> + Send + Sync + 'static,
{
    fn eval(&self, inputs: Inputs<'_>) -> Result<Value, CellError> {
        self(inputs)
    }
}

/// What kind of cell this is. The distinction is not cosmetic: only a source can be set
/// from outside, and only a source can start a recompute pass.
pub(crate) enum Kind {
    /// A value set from outside — a widget, a loaded file, a clock tick pushed in by the
    /// host.
    ///
    /// The starting value is held behind an `Arc` **in the graph**, and its digest is taken
    /// once, here, rather than per session. That is not a micro-optimisation: the starting
    /// value of a source is usually the app's data, a session is created per viewer, and
    /// cloning a loaded CSV — and re-hashing it — for every viewer would make the shared
    /// graph a lie. Two sessions over one graph hold the *same allocation* for every source
    /// neither has set, and a test asserts it with `Arc::ptr_eq`.
    Source {
        initial: Arc<crate::session::Outcome>,
        digest: crate::digest::Digest,
        /// The declared type NAME, for the check in `Session::set`.
        ///
        /// This used to be a whole second `Value`, which meant every loaded table was held
        /// twice in the graph — a duplicate of the exact thing this module makes a point of
        /// sharing by `Arc`. The check only ever needed the type.
        declared: &'static str,
    },
    Computed {
        compute: Arc<dyn Compute>,
    },
}

pub(crate) struct Node {
    pub(crate) name: String,
    pub(crate) kind: Kind,
    pub(crate) inputs: Vec<CellId>,
    /// The input names, kept beside the ids so `Inputs::by_name` costs no allocation and
    /// an error message can say which input was wrong.
    pub(crate) input_names: Vec<String>,
    /// Longest path from any source. Ascending height is the evaluation order, and that
    /// single fact is what makes the engine glitch-free — see [`crate::session`].
    pub(crate) height: u32,
    /// Reverse edges. Dirty marks travel along these.
    pub(crate) dependents: Vec<CellId>,
}

/// An immutable, checked app graph.
pub struct Graph {
    pub(crate) nodes: Vec<Node>,
    by_name: HashMap<String, CellId>,
    /// Every cell id in ascending height, computed once. A full evaluation walks this;
    /// a dirty pass walks a filtered subsequence of it.
    pub(crate) order: Vec<CellId>,
}

impl Graph {
    /// An empty builder. The only way to make a graph — [`GraphBuilder::build`] is where the
    /// checks live, and a `Graph` that exists has passed them.
    pub fn builder() -> GraphBuilder {
        GraphBuilder::default()
    }

    /// The number of cells, sources included. Also the length of a session's slot vector and
    /// the denominator of every "evaluated N of M" line in a trace.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the graph has no cells. A legal graph: an app with no cells serves an empty
    /// page rather than failing to build.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The id for a cell name, or `None`. Names are the app's public identifiers, so this is
    /// the entry point for anything arriving from a manifest or off the wire; resolve once
    /// and pass the [`CellId`] thereafter.
    pub fn id(&self, name: &str) -> Option<CellId> {
        self.by_name.get(name).copied()
    }

    /// A cell's declared name.
    ///
    /// # Panics
    ///
    /// If the id is from another graph. Check with [`Graph::contains`] when the id did not
    /// come from this graph in the first place.
    pub fn name(&self, id: CellId) -> &str {
        &self.nodes[id.index()].name
    }

    /// Whether the cell is a source — settable from outside, and the only kind of cell that
    /// can start a pass.
    ///
    /// # Panics
    ///
    /// If the id is from another graph.
    pub fn is_source(&self, id: CellId) -> bool {
        matches!(self.nodes[id.index()].kind, Kind::Source { .. })
    }

    /// The cell's declared inputs, in declaration order — the order its compute sees them in
    /// [`Inputs`].
    ///
    /// # Panics
    ///
    /// If the id is from another graph.
    pub fn inputs_of(&self, id: CellId) -> &[CellId] {
        &self.nodes[id.index()].inputs
    }

    /// The cells that declared this one as an input. These are the reverse edges dirty marks
    /// travel along, and they are why invalidation costs a walk of the affected subtree
    /// rather than a scan of the graph.
    ///
    /// # Panics
    ///
    /// If the id is from another graph.
    pub fn dependents_of(&self, id: CellId) -> &[CellId] {
        &self.nodes[id.index()].dependents
    }

    /// The cell's height: the longest path from any source, computed once at build time.
    /// Ascending height is the evaluation order, and that is the whole of the glitch-freedom
    /// argument — every input of a cell has a strictly lower height, so it is already final
    /// when the cell runs.
    ///
    /// # Panics
    ///
    /// If the id is from another graph.
    pub fn height(&self, id: CellId) -> u32 {
        self.nodes[id.index()].height
    }

    /// Every cell, in evaluation order.
    pub fn order(&self) -> &[CellId] {
        &self.order
    }

    /// Whether this id addresses a cell of this graph. Ids are bare indices, so this is the
    /// check that stops one from another graph reading the wrong cell instead of failing.
    pub fn contains(&self, id: CellId) -> bool {
        id.index() < self.nodes.len()
    }

    /// Every source, in evaluation order. The set a host can set, and the set `dagpane
    /// explain` starts its blast-radius walk from.
    pub fn sources(&self) -> impl Iterator<Item = CellId> + '_ {
        self.order.iter().copied().filter(|id| self.is_source(*id))
    }

    /// The transitive closure of `roots` over the reverse edges, including the roots.
    ///
    /// This is the set a recompute pass may touch, and `dagpane explain` prints it. It is
    /// derived from structure alone — no values, no session — so an app author can see the
    /// blast radius of a slider before shipping it.
    pub fn closure(&self, roots: &[CellId]) -> Vec<CellId> {
        let mut seen = vec![false; self.nodes.len()];
        let mut stack: Vec<CellId> = Vec::new();
        for &r in roots {
            if self.contains(r) && !seen[r.index()] {
                seen[r.index()] = true;
                stack.push(r);
            }
        }
        let mut out = Vec::new();
        while let Some(id) = stack.pop() {
            out.push(id);
            for &d in &self.nodes[id.index()].dependents {
                if !seen[d.index()] {
                    seen[d.index()] = true;
                    stack.push(d);
                }
            }
        }
        out.sort_by_key(|id| (self.nodes[id.index()].height, id.0));
        out
    }
}

impl fmt::Debug for Graph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Graph")
            .field("cells", &self.nodes.len())
            .finish()
    }
}

/// Declares cells and edges, then checks them all at once.
///
/// Cells may be declared in any order — a cell can name an input declared after it — because
/// names are resolved in [`GraphBuilder::build`], not on the way in. That matters for the
/// manifest compiler, which has no reason to topologically sort a TOML file before reading it.
#[derive(Default)]
pub struct GraphBuilder {
    names: Vec<String>,
    kinds: Vec<Kind>,
    input_names: Vec<Vec<String>>,
}

impl fmt::Debug for GraphBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GraphBuilder")
            .field("declared", &self.names.len())
            .finish()
    }
}

impl GraphBuilder {
    /// Declare a source cell: something set from outside, with a starting value.
    pub fn source(&mut self, name: impl Into<String>, initial: Value) -> &mut Self {
        let declared = initial.type_name();
        let outcome = crate::session::Outcome::ok(initial);
        let digest = crate::digest::Digestible::digest(&outcome);
        self.names.push(name.into());
        self.kinds.push(Kind::Source {
            initial: Arc::new(outcome),
            digest,
            declared,
        });
        self.input_names.push(Vec::new());
        self
    }

    /// Declare a computed cell, its inputs by name, and what it does.
    ///
    /// The bound is written out as an `Fn` rather than as `C: Compute`, and that is not
    /// stylistic. With a generic `C: Compute` the compiler has to choose an impl before it
    /// can know a bare closure's argument type, and the blanket impl below is not enough to
    /// break the circle — every call site would need `|i: Inputs<'_>|` spelled out. Naming
    /// the function signature here is what makes `b.cell("x", ["y"], |i| ...)` compile.
    /// [`GraphBuilder::cell_with`] is the door for anything that is not a closure.
    pub fn cell<F>(
        &mut self,
        name: impl Into<String>,
        inputs: impl IntoIterator<Item = impl Into<String>>,
        compute: F,
    ) -> &mut Self
    where
        F: for<'a> Fn(Inputs<'a>) -> Result<Value, CellError> + Send + Sync + 'static,
    {
        self.cell_with(name, inputs, Arc::new(compute))
    }

    /// Declare a computed cell whose behaviour is a type rather than a closure — a struct
    /// holding a compiled manifest step, say, or one shared by several cells.
    pub fn cell_with(
        &mut self,
        name: impl Into<String>,
        inputs: impl IntoIterator<Item = impl Into<String>>,
        compute: Arc<dyn Compute>,
    ) -> &mut Self {
        self.names.push(name.into());
        self.kinds.push(Kind::Computed { compute });
        self.input_names
            .push(inputs.into_iter().map(Into::into).collect());
        self
    }

    /// Resolve names, reject duplicates, unknown inputs and cycles, then compute heights
    /// and reverse edges.
    ///
    /// The order matters. Duplicates are found first because with two cells sharing a name
    /// every later message would be ambiguous; unknown inputs second because an unresolved
    /// name cannot take part in a cycle report; the cycle last, because by then every name
    /// in the reported path is real.
    pub fn build(self) -> Result<Arc<Graph>, BuildError> {
        let GraphBuilder {
            names,
            kinds,
            input_names,
        } = self;

        let mut by_name: HashMap<String, CellId> = HashMap::with_capacity(names.len());
        for (i, name) in names.iter().enumerate() {
            if by_name.insert(name.clone(), CellId(i as u32)).is_some() {
                return Err(BuildError::DuplicateName(name.clone()));
            }
        }

        let mut inputs: Vec<Vec<CellId>> = Vec::with_capacity(names.len());
        for (i, ins) in input_names.iter().enumerate() {
            let mut resolved = Vec::with_capacity(ins.len());
            for input in ins {
                match by_name.get(input) {
                    Some(id) => resolved.push(*id),
                    None => {
                        return Err(BuildError::UnknownInput {
                            cell: names[i].clone(),
                            input: input.clone(),
                        })
                    }
                }
            }
            inputs.push(resolved);
        }

        // Kahn's algorithm. It produces the topological order and detects the cycle in one
        // pass, and the leftover set — the nodes that never reached in-degree zero — is
        // exactly the set the cycle lives in, which is what makes a readable report possible.
        let n = names.len();
        let mut indegree = vec![0usize; n];
        let mut dependents: Vec<Vec<CellId>> = vec![Vec::new(); n];
        for (i, ins) in inputs.iter().enumerate() {
            // Deduplicate: a cell may legitimately name the same input twice (a comparison
            // against itself), and a doubled reverse edge would make the dirty walk do
            // twice the work and the in-degree never reach zero.
            let mut seen: Vec<CellId> = Vec::new();
            for &dep in ins {
                if !seen.contains(&dep) {
                    seen.push(dep);
                    indegree[i] += 1;
                    dependents[dep.index()].push(CellId(i as u32));
                }
            }
        }

        let mut queue: Vec<CellId> = (0..n)
            .filter(|&i| indegree[i] == 0)
            .map(|i| CellId(i as u32))
            .collect();
        // Ascending id among equals, so the order is deterministic and a trace is diffable.
        queue.sort();

        let mut order: Vec<CellId> = Vec::with_capacity(n);
        let mut height = vec![0u32; n];
        let mut head = 0usize;
        while head < queue.len() {
            let id = queue[head];
            head += 1;
            order.push(id);
            let mut newly_ready: Vec<CellId> = Vec::new();
            for &dep in &dependents[id.index()] {
                height[dep.index()] = height[dep.index()].max(height[id.index()] + 1);
                indegree[dep.index()] -= 1;
                if indegree[dep.index()] == 0 {
                    newly_ready.push(dep);
                }
            }
            newly_ready.sort();
            queue.extend(newly_ready);
        }

        if order.len() != n {
            let in_cycle: Vec<usize> = (0..n).filter(|&i| indegree[i] > 0).collect();
            return Err(BuildError::Cycle {
                path: trace_cycle(&in_cycle, &inputs, &names),
            });
        }

        // Height is already final: Kahn visits every predecessor of a node before the node,
        // so the max was taken over all of them. Re-sorting by height is still needed —
        // Kahn's order is *a* topological order, not the height order, and the evaluation
        // loop wants the second one.
        order.sort_by_key(|id| (height[id.index()], id.0));

        let nodes = names
            .into_iter()
            .zip(kinds)
            .zip(inputs)
            .zip(input_names)
            .enumerate()
            .map(|(i, (((name, kind), ins), in_names))| Node {
                name,
                kind,
                inputs: ins,
                input_names: in_names,
                height: height[i],
                dependents: std::mem::take(&mut dependents[i]),
            })
            .collect();

        Ok(Arc::new(Graph {
            nodes,
            by_name,
            order,
        }))
    }
}

/// Walk one concrete cycle out of the set of nodes that never reached in-degree zero.
///
/// Following the *first* unresolved input from any node in the set is enough: every node in
/// the set has at least one input still inside the set, so the walk cannot leave it, and a
/// finite set with no exit must repeat a node. The returned path starts and ends at that
/// repeat, so a reader sees a closed loop rather than a lasso.
fn trace_cycle(in_cycle: &[usize], inputs: &[Vec<CellId>], names: &[String]) -> Vec<String> {
    let member = |i: usize| in_cycle.contains(&i);
    let start = in_cycle[0];
    let mut path = vec![start];
    let mut at = start;
    loop {
        let next = inputs[at]
            .iter()
            .map(|c| c.index())
            .find(|&i| member(i))
            .expect("a node left in the cycle set has an input inside it");
        if let Some(pos) = path.iter().position(|&p| p == next) {
            let mut loop_path: Vec<String> =
                path[pos..].iter().map(|&i| names[i].clone()).collect();
            // Reported the way the edges point — `a -> b` means "a depends on b" reads
            // backwards to most people, so reverse it into "b feeds a".
            loop_path.reverse();
            loop_path.push(loop_path[0].clone());
            return loop_path;
        }
        path.push(next);
        at = next;
    }
}
