//! Where each cell runs, and what that costs to keep correct.
//!
//! `ROADMAP.md` §4 asked for one thing and got another. What shipped moves the *whole*
//! graph into a browser; what it asked for was that **the placement of each cell is a
//! deployment decision rather than a rewrite** — some cells on a server, some in the page,
//! chosen in the manifest. This module is that, and it is mostly an argument about why the
//! obvious version of it is wrong.
//!
//! # The obvious version, and why it is not here
//!
//! A graph evaluates in ascending height ([`crate::session`] has the argument). So the
//! obvious way to split one across a wire is to keep the height order and hop: run every
//! cell at height 0 wherever it lives, then every cell at height 1, and so on. It is
//! correct, and it costs **a round trip per height boundary the cut crosses**. An app whose
//! placements alternate would pay six round trips for one slider move, which is worse than
//! the thing being replaced and would be sold as an improvement.
//!
//! # What is here: the cut is a frontier, not a sieve
//!
//! A placement is admitted only when it is **monotone along every edge** — a cell on the
//! client may not be an input to a cell on the server. Values flow one way across the cut
//! and never back. [`CutError::Backflow`] names the edge that broke it, and this is the
//! only rule; everything else follows from it.
//!
//! What follows is worth spelling out, because it is the whole design:
//!
//! 1. **The server's cells are closed under inputs.** No server cell reads a client cell,
//!    so the server can evaluate its side with no knowledge that the other side exists.
//! 2. **The frontier is a set, not a sequence.** The cells whose values must cross are
//!    exactly the server cells with at least one client dependent — the [`Cut::boundary`].
//!    Every one of them has its final value for a pass by the time the server's pass ends,
//!    because the server's pass is itself in ascending height.
//! 3. **So one message carries the whole frontier**, once per pass, and the client's side
//!    is a graph whose sources are those boundary cells.
//!
//! That third point is the part that makes this cheap to *believe*. The client does not run
//! a special distributed evaluator. It runs [`crate::Session`], unmodified, over a graph
//! that [`Graph::split`] built for it — one in which every boundary cell is an ordinary
//! source. Ascending height still holds on each side, and there is no new code in the pass
//! loop where a glitch could hide.
//!
//! # The correctness argument, stated so it can be attacked
//!
//! Glitch freedom is "no cell ever observes a mixture of old and new upstream values".
//! Within one side it holds for the reason it always did. Across the cut it needs one more
//! thing: that the client applies a pass's boundary values **all together or not at all**.
//! Given that, a client cell reading two boundary cells sees both from pass *N* or both
//! from pass *N-1*, never one of each — which is the same statement as the local one, with
//! "the server's pass" where "a lower height" used to be.
//!
//! Atomicity is therefore the whole obligation the transport carries, and it is the only
//! one. It is not asserted here in prose alone: `crates/core/tests/oracle.rs` generates
//! random graphs, cuts them at random admissible places, runs both halves, and requires the
//! split session to agree **cell for cell** with an unsplit one after every interaction.
//!
//! # What it buys
//!
//! An interaction whose entire dirty closure lies on the client sends nothing at all. That
//! is the point, and it is why the rule above is a rule rather than a warning: the property
//! is checkable ([`Cut::is_local`]) precisely because backflow is impossible.

use std::sync::Arc;

use crate::error::SessionError;
use crate::graph::{CellId, Graph, Kind};
use crate::session::{Outcome, Session};
use crate::value::Value;

/// The `Kind` a boundary cell takes on the client side: a source holding nothing yet.
///
/// Declared `"null"`, which `Session::set` treats as "accepts anything". That is not
/// laziness about types — a boundary cell is a *computed* cell on the other side, and a
/// computed cell has no declared type to copy. What it will hold is whatever its compute
/// produced, and the client has no way to know that before the first message arrives.
fn frontier_source() -> Kind {
    let outcome = Outcome::ok(Value::Null);
    let digest = crate::digest::Digestible::digest(&outcome);
    Kind::source_parts(Arc::new(outcome), digest, None, "null")
}

/// Which side of the cut a cell runs on.
///
/// `Server` is the default everywhere, and deliberately: an app with no placement written
/// down anywhere is the app this project already had, evaluated where it always was.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    /// Evaluated by whoever holds the sources. The default.
    #[default]
    Server,
    /// Evaluated in the page, from values that crossed the wire once.
    Client,
}

impl Placement {
    /// The name used in a manifest and on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Placement::Server => "server",
            Placement::Client => "client",
        }
    }
}

impl std::fmt::Display for Placement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a placement was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CutError {
    /// A client cell feeds a server cell. See the module docs: this is the one rule.
    Backflow {
        /// The cell on the client.
        from: String,
        /// The cell on the server that reads it.
        to: String,
    },
    /// A placement was given for a name the graph does not have.
    UnknownCell(String),
    /// A placement vector whose length is not the graph's.
    Arity {
        /// How many cells the graph has.
        expected: usize,
        /// How many placements were offered.
        found: usize,
    },
}

impl std::fmt::Display for CutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Says which way the edge runs, because "these two disagree" is not actionable
            // and "move one of them" is. Both fixes are named rather than implied.
            CutError::Backflow { from, to } => write!(
                f,
                "`{to}` runs on the server and reads `{from}`, which runs on the client: \
                 values cross the cut once and never come back. Move `{to}` to the client, \
                 or `{from}` to the server."
            ),
            CutError::UnknownCell(name) => {
                write!(f, "no cell named `{name}` to place")
            }
            CutError::Arity { expected, found } => write!(
                f,
                "this graph has {expected} cells and {found} placements were given"
            ),
        }
    }
}

impl std::error::Error for CutError {}

/// A checked placement of every cell, and the frontier it implies.
///
/// The only way to make one is [`Cut::new`] or [`Cut::of_client`], so a `Cut` that exists is
/// monotone — which is what lets [`Graph::split`] be total.
#[derive(Clone, Debug)]
pub struct Cut {
    /// By cell index.
    placement: Vec<Placement>,
    /// Server cells with at least one client dependent, ascending height.
    boundary: Vec<CellId>,
}

impl Cut {
    /// Check a placement per cell, in cell-id order.
    ///
    /// # Errors
    ///
    /// [`CutError::Arity`] if the vector is not the graph's length, or
    /// [`CutError::Backflow`] naming the first edge that runs from client to server.
    pub fn new(graph: &Graph, placement: Vec<Placement>) -> Result<Cut, CutError> {
        if placement.len() != graph.len() {
            return Err(CutError::Arity {
                expected: graph.len(),
                found: placement.len(),
            });
        }

        // Walked in the graph's own order rather than by index, so the edge a message names
        // is the first one a reader would reach going down the app, not the first by id.
        for &id in graph.order() {
            if placement[id.index()] != Placement::Server {
                continue;
            }
            for &input in graph.inputs_of(id) {
                if placement[input.index()] == Placement::Client {
                    return Err(CutError::Backflow {
                        from: graph.name(input).to_string(),
                        to: graph.name(id).to_string(),
                    });
                }
            }
        }

        let mut boundary: Vec<CellId> = graph
            .order()
            .iter()
            .copied()
            .filter(|&id| placement[id.index()] == Placement::Server)
            .filter(|&id| {
                graph
                    .dependents_of(id)
                    .iter()
                    .any(|d| placement[d.index()] == Placement::Client)
            })
            .collect();
        // `order()` is already ascending height; the sort is belt and braces against a
        // future change to what `order()` guarantees, and costs nothing at these sizes.
        boundary.sort_by_key(|id| (graph.height(*id), id.0));

        Ok(Cut {
            placement,
            boundary,
        })
    }

    /// Place these cells on the client and everything else on the server.
    ///
    /// The shape a manifest actually produces — a short list of names, not a vector the
    /// length of the app.
    ///
    /// # Errors
    ///
    /// [`CutError::UnknownCell`] for a name the graph does not have, or
    /// [`CutError::Backflow`] as [`Cut::new`].
    pub fn of_client<S: AsRef<str>>(graph: &Graph, client: &[S]) -> Result<Cut, CutError> {
        let mut placement = vec![Placement::Server; graph.len()];
        for name in client {
            let id = graph
                .id(name.as_ref())
                .ok_or_else(|| CutError::UnknownCell(name.as_ref().to_string()))?;
            placement[id.index()] = Placement::Client;
        }
        Cut::new(graph, placement)
    }

    /// Everything on the server: the app this project had before this module existed.
    pub fn whole(graph: &Graph) -> Cut {
        Cut {
            placement: vec![Placement::Server; graph.len()],
            boundary: Vec::new(),
        }
    }

    /// Where this cell runs.
    pub fn placement(&self, id: CellId) -> Placement {
        self.placement[id.index()]
    }

    /// The cells whose values cross the wire, ascending height.
    ///
    /// Its length is the thing to look at when judging a cut: it is how much has to be
    /// serialised once per pass that changes any of it.
    pub fn boundary(&self) -> &[CellId] {
        &self.boundary
    }

    /// How many cells run on the client.
    pub fn client_cells(&self) -> usize {
        self.placement
            .iter()
            .filter(|p| **p == Placement::Client)
            .count()
    }

    /// Whether anything at all runs on the client.
    ///
    /// `false` means [`Graph::split`] would hand back the original graph and an empty one,
    /// and a caller should keep doing what it already does rather than start a protocol.
    pub fn is_split(&self) -> bool {
        self.client_cells() > 0
    }

    /// Whether setting `root` can be answered without the server.
    ///
    /// True when `root` and **every cell downstream of it** run on the client. This is the
    /// property the whole module exists for, and it is decidable exactly because backflow
    /// is impossible: a closure that starts on the client can only leave it by an edge that
    /// [`Cut::new`] would have refused.
    pub fn is_local(&self, graph: &Graph, root: CellId) -> bool {
        self.placement(root) == Placement::Client
            && graph
                .closure(&[root])
                .into_iter()
                .all(|id| self.placement(id) == Placement::Client)
    }
}

/// The two graphs a cut produces, plus the names that travel between them.
#[derive(Debug)]
pub struct Split {
    /// Every server cell, with its edges. Closed under inputs, so it runs alone.
    pub server: Arc<Graph>,
    /// Every client cell, plus each boundary cell rewritten as a **source**.
    ///
    /// Those sources start at [`crate::Value::Null`] and are declared untyped, so the first
    /// boundary message a client applies is accepted whatever the cells turn out to hold.
    /// A client that commits before receiving one gets nulls, which is why a transport
    /// sends the frontier with the opening frame rather than after it.
    pub client: Arc<Graph>,
    /// The boundary cells by name, ascending height — what a message has to carry, and the
    /// same names in both graphs.
    pub boundary: Vec<String>,
}

impl Graph {
    /// Split this graph in two along a cut.
    ///
    /// Both halves are ordinary graphs: same computes, same names, heights recomputed for
    /// their own sources. Nothing is cloned that a session would have shared — a loaded
    /// table crosses as the same `Arc` the original held, because the rebuilt source keeps
    /// the original's already-digested value rather than re-deriving one.
    ///
    /// A cut with nothing on the client gives back an equivalent server graph and an empty
    /// client one; callers should check [`Cut::is_split`] first and skip the protocol.
    pub fn split(&self, cut: &Cut) -> Split {
        let mut server = Graph::builder();
        let mut client = Graph::builder();

        let boundary: Vec<String> = cut
            .boundary()
            .iter()
            .map(|&id| self.name(id).to_string())
            .collect();

        for &id in self.order() {
            let node = &self.nodes[id.index()];
            let is_boundary = cut.boundary().contains(&id);
            match cut.placement(id) {
                Placement::Server => {
                    server.push_node(
                        node.name.clone(),
                        node.clone_kind(),
                        node.input_names.clone(),
                    );
                    // A boundary cell appears on BOTH sides: computed on the server, and a
                    // source on the client waiting for what the server computed.
                    if is_boundary {
                        client.push_node(node.name.clone(), frontier_source(), Vec::new());
                    }
                }
                Placement::Client => {
                    client.push_node(
                        node.name.clone(),
                        node.clone_kind(),
                        node.input_names.clone(),
                    );
                }
            }
        }

        Split {
            // Both sides are subgraphs of one that already built, with every input either
            // kept or replaced by a source of the same name — so no name is unresolved, no
            // name is duplicated, and no edge was added that could close a cycle.
            server: server.build().expect("a subgraph of a built graph"),
            client: client.build().expect("boundary cells became sources"),
            boundary,
        }
    }
}

/// One pass's worth of boundary values, as they cross.
///
/// **This type is the atomicity obligation.** The correctness argument in the module docs
/// reduces to "a pass's boundary values are applied together or not at all", and a
/// `Frontier` is that batch given a name so a transport cannot accidentally deliver half of
/// one. [`Split::deliver`] stages the whole of it before anything commits.
///
/// It holds `Arc<Outcome>` rather than `Outcome` so that moving a frontier inside one
/// process — a test, a host running both sides, the same-process bench — is a refcount bump
/// rather than a copy of a loaded table. A transport that has to serialise it pays for that
/// at the edge, where it is unavoidable and visible.
#[derive(Clone, Debug)]
pub struct Frontier {
    /// The server pass this came from. Carried so a client can refuse a frontier it has
    /// already applied, or one that arrived out of order — two deliveries interleaved would
    /// be exactly the mixture of old and new the engine exists to prevent.
    pub epoch: u64,
    /// The boundary cells that moved, by name, ascending height.
    pub cells: Vec<(String, Arc<Outcome>)>,
}

impl Frontier {
    /// Whether anything at all has to cross.
    ///
    /// An empty frontier is the common case on a well-cut app: a client-side slider moved,
    /// the server ran nothing, and there is no message to send.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
}

impl Split {
    /// Every boundary cell, whatever it did last pass — what a client needs to *start*.
    ///
    /// The client's boundary sources begin at `Null`, so a client that is sent only what
    /// moved begins with holes: a boundary cell that happened not to change during the
    /// server's first pass would never be sent, and every client cell reading it would
    /// compute from a null it was never supposed to see. That is not hypothetical — it is
    /// what `a_split_graph_agrees_cell_for_cell_with_an_unsplit_one` caught on seed 4, where
    /// a source whose initial value the caller re-set was legitimately "unchanged" and left
    /// a client cell reading `null`.
    ///
    /// So a transport pairs these the way `AppSession` already pairs `full_views` with
    /// `patch`: this one with the opening frame, [`Split::frontier`] with every pass after.
    /// `a_client_that_only_ever_gets_deltas_starts_with_holes` pins the distinction.
    ///
    /// # Panics
    ///
    /// If `server` is not a session over [`Split::server`].
    pub fn full_frontier(&self, server: &Session) -> Frontier {
        let graph = server.graph();
        let cells = self
            .boundary
            .iter()
            .map(|name| {
                let id = graph
                    .id(name)
                    .expect("a boundary name is a cell of the server graph");
                (name.clone(), server.share(id))
            })
            .collect();
        Frontier {
            epoch: server.epoch(),
            cells,
        }
    }

    /// What the server's last pass has to tell a client that is already up to date.
    ///
    /// `since` is the epoch to report changes from — the same argument
    /// [`Session::changed_since`] takes, and normally the epoch of the pass that just ran.
    /// A boundary cell that did not move is not in the result, because a cut's whole
    /// economy is that a pass which changes nothing on the frontier sends nothing.
    ///
    /// # Panics
    ///
    /// If `server` is not a session over [`Split::server`]. The boundary names come from
    /// this split, so a session over some other graph is a caller error rather than a
    /// runtime condition to report.
    pub fn frontier(&self, server: &Session, since: u64) -> Frontier {
        let graph = server.graph();
        let moved: Vec<CellId> = server.changed_since(since);
        let mut cells = Vec::new();
        // Walked in boundary order — ascending height — rather than in `changed_since`
        // order, so a frontier is byte-identical for the same pass however the trace came
        // out, which is what makes one diffable in a test and cacheable on a wire.
        for name in &self.boundary {
            let id = graph
                .id(name)
                .expect("a boundary name is a cell of the server graph");
            if moved.contains(&id) {
                cells.push((name.clone(), server.share(id)));
            }
        }
        Frontier {
            epoch: server.epoch(),
            cells,
        }
    }

    /// Stage a whole frontier on the client, ready for one [`Session::commit`].
    ///
    /// Staging rather than applying is the point: every boundary cell is staged, then the
    /// caller commits **once**, so the client's pass sees all of them or none. Committing
    /// between deliveries is what would produce a glitch, and the only way to do it here is
    /// to call `commit` yourself in the middle of a loop this method does not have.
    ///
    /// # Errors
    ///
    /// [`SessionError::UnknownCell`] if the frontier names a cell this client graph has no
    /// source for — which means the two sides were cut from different manifests, and is
    /// worth failing loudly rather than drawing three-quarters of a page.
    pub fn deliver(&self, client: &mut Session, frontier: &Frontier) -> Result<(), SessionError> {
        // CHECK EVERYTHING, THEN STAGE. Staging as it goes would mean a frontier refused on
        // its last cell had already staged the others, and the caller's next commit would
        // apply a fragment of an update it was told had failed — a mixture of two passes,
        // which is the exact thing the module docs above argue cannot happen. `AppSession::set`
        // splits its loop for the same reason and says so in the same words.
        for (name, outcome) in &frontier.cells {
            client.can_set_outcome(name, outcome)?;
        }
        for (name, outcome) in &frontier.cells {
            client.set_outcome(name, (**outcome).clone())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CellError;

    /// `a -> b -> c`, plus `d` reading `a`. Small enough to reason about by hand and wide
    /// enough to have a cell that is not on the path being cut.
    fn graph() -> Arc<Graph> {
        let mut b = Graph::builder();
        b.source("a", Value::int(1));
        b.cell("b", ["a"], |i| Ok(Value::int(i.int(0)? * 2)));
        b.cell("c", ["b"], |i| Ok(Value::int(i.int(0)? + 1)));
        b.cell("d", ["a"], |i| Ok(Value::int(i.int(0)? + 100)));
        b.build().expect("acyclic")
    }

    #[test]
    fn a_client_cell_may_not_feed_a_server_cell() {
        let g = graph();
        // `b` in the page, `c` on the server — `c` reads `b`, so a value would have to come
        // back across the cut.
        let e = Cut::of_client(&g, &["b"]).unwrap_err();
        assert_eq!(
            e,
            CutError::Backflow {
                from: "b".to_string(),
                to: "c".to_string()
            }
        );
        // The message names both ways out, because "these disagree" is not a fix.
        let text = e.to_string();
        assert!(text.contains("Move `c` to the client"), "{text}");
        assert!(text.contains("or `b` to the server"), "{text}");

        // The same pair the other way round is fine: values may always flow outward.
        assert!(Cut::of_client(&g, &["b", "c"]).is_ok());
    }

    #[test]
    fn the_boundary_is_the_server_cells_the_client_reads_and_nothing_else() {
        let g = graph();
        let cut = Cut::of_client(&g, &["b", "c"]).expect("monotone");
        let names: Vec<&str> = cut.boundary().iter().map(|&id| g.name(id)).collect();
        // `a` crosses because `b` reads it. `d` is a server cell that no client cell reads,
        // so it stays where it is and never goes on the wire — which is the whole economy
        // of a cut and the thing a "send everything" implementation would get wrong.
        assert_eq!(names, vec!["a"]);
        assert_eq!(cut.client_cells(), 2);
        assert!(cut.is_split());
    }

    #[test]
    fn an_app_with_no_placement_is_the_app_this_project_already_had() {
        let g = graph();
        let cut = Cut::whole(&g);
        assert!(!cut.is_split());
        assert!(cut.boundary().is_empty());
        let split = g.split(&cut);
        assert_eq!(split.server.len(), g.len());
        assert!(split.client.is_empty());
    }

    #[test]
    fn a_split_shares_the_original_source_allocation() {
        // The `Kind::Source` docs promise that a loaded table is held once and shared. A
        // split rebuilds graphs, which is exactly where that promise would quietly break —
        // one `Value::clone` in `clone_kind` and every split app holds a second copy of its
        // own data with nothing to show for it.
        let g = graph();
        let split = g.split(&Cut::of_client(&g, &["d"]).expect("monotone"));
        let mut original = crate::Session::new(Arc::clone(&g));
        let mut server = crate::Session::new(Arc::clone(&split.server));
        original.refresh();
        server.refresh();
        let a = g.id("a").expect("a source");
        assert!(
            Arc::ptr_eq(&original.share(a), &server.share(a)),
            "the split copied a source it should have shared"
        );
    }

    #[test]
    fn a_client_that_only_ever_gets_deltas_starts_with_holes() {
        // Why `full_frontier` exists. A boundary cell that did not move during the server's
        // first pass is absent from a delta, and the client then computes from the `Null` its
        // sources start at — a wrong answer that looks like a legitimate empty one.
        let g = graph();
        let cut = Cut::of_client(&g, &["b", "c"]).expect("monotone");
        let split = g.split(&cut);
        let mut server = crate::Session::new(Arc::clone(&split.server));
        server.refresh();

        // `a` never moved off its initial value, so a delta since this pass is empty.
        let delta = split.frontier(&server, server.epoch());
        assert!(delta.is_empty(), "nothing moved, so nothing should be sent");

        let mut starved = crate::Session::new(Arc::clone(&split.client));
        split.deliver(&mut starved, &delta).expect("one graph");
        starved.refresh();
        assert!(
            starved.get("c").expect("a client cell").is_err(),
            "a client seeded with a delta read a null it was never meant to see"
        );

        // The full frontier is what an opening frame carries, and it is complete.
        let mut fed = crate::Session::new(Arc::clone(&split.client));
        split
            .deliver(&mut fed, &split.full_frontier(&server))
            .expect("one graph");
        fed.refresh();
        assert_eq!(
            fed.get("c").expect("a client cell").value(),
            Some(&Value::int(3))
        );
    }

    #[test]
    fn delivering_a_frontier_runs_nothing_until_the_caller_commits() {
        // THE ATOMICITY OBLIGATION, as an assertion. The module docs reduce glitch freedom
        // across the cut to "a pass's boundary values are applied together or not at all",
        // and `deliver` keeps that by staging. If it ever applied instead, a client cell
        // reading two boundary cells could see one of each — and this is the check that
        // would fail, because staging is exactly "the epoch does not move".
        let g = graph();
        let split = g.split(&Cut::of_client(&g, &["b", "c"]).expect("monotone"));
        let mut server = crate::Session::new(Arc::clone(&split.server));
        server.refresh();
        let mut client = crate::Session::new(Arc::clone(&split.client));
        client.refresh();

        let before = client.epoch();
        split
            .deliver(&mut client, &split.full_frontier(&server))
            .expect("one graph");
        assert_eq!(client.epoch(), before, "deliver ran a pass of its own");
        client.commit();
        assert_eq!(client.epoch(), before + 1);
    }

    #[test]
    fn an_error_crosses_the_cut_as_an_error() {
        // A null would be drawn as a legitimate empty answer. `set_outcome` exists so the
        // client learns the cell is broken and says so, in the place that cell occupies.
        let mut b = Graph::builder();
        b.source("n", Value::int(0));
        b.cell("risky", ["n"], |i| {
            let n = i.int(0)?;
            if n == 0 {
                Err(CellError::failed("no"))
            } else {
                Ok(Value::int(n))
            }
        });
        b.cell("shown", ["risky"], |i| Ok(Value::int(i.int(0)? + 1)));
        let g = b.build().expect("acyclic");

        let split = g.split(&Cut::of_client(&g, &["shown"]).expect("monotone"));
        let mut server = crate::Session::new(Arc::clone(&split.server));
        server.refresh();
        let mut client = crate::Session::new(Arc::clone(&split.client));
        split
            .deliver(&mut client, &split.full_frontier(&server))
            .expect("one graph");
        client.refresh();

        let shown = client.get("shown").expect("a client cell");
        assert!(shown.is_err(), "the failure did not cross: {shown:?}");
        assert!(
            shown
                .error()
                .expect("an error")
                .to_string()
                .contains("risky"),
            "the message stopped naming where the failure started: {shown:?}"
        );
    }

    #[test]
    fn a_frontier_that_is_refused_stages_none_of_itself() {
        // The atomicity obligation has a second half, and this is it. `deliver` staging
        // rather than applying stops a glitch BETWEEN boundary cells; it does nothing about a
        // frontier that is refused halfway through, which would leave the earlier cells
        // staged and let the caller's next `commit` apply a fragment of a rejected update.
        //
        // A frontier naming a cell this half has no source for means the two sides were
        // compiled from different manifests — so the refusal is right, and applying two
        // thirds of it is not.
        let g = graph();
        let split = g.split(&Cut::of_client(&g, &["b", "c"]).expect("monotone"));
        let mut server = crate::Session::new(Arc::clone(&split.server));
        server.refresh();
        let mut client = crate::Session::new(Arc::clone(&split.client));

        let mut poisoned = split.full_frontier(&server);
        assert!(!poisoned.cells.is_empty(), "there is something to stage");
        poisoned.cells.push((
            "not_a_boundary_cell".to_string(),
            Arc::new(Outcome::ok(Value::int(1))),
        ));

        let before = client.epoch();
        split
            .deliver(&mut client, &poisoned)
            .expect_err("the frontier names a cell this half has no source for");

        // Nothing ran — `deliver` never commits — and nothing is waiting to run either.
        assert_eq!(client.epoch(), before);
        client.commit();
        assert_eq!(
            client.get("a").expect("the boundary source").value(),
            Some(&Value::Null),
            "a refused frontier left part of itself staged, and a commit applied it"
        );
    }

    #[test]
    fn a_placement_for_a_cell_that_does_not_exist_is_refused_by_name() {
        let g = graph();
        assert_eq!(
            Cut::of_client(&g, &["nope"]).unwrap_err(),
            CutError::UnknownCell("nope".to_string())
        );
        assert_eq!(
            Cut::new(&g, vec![Placement::Client; 2]).unwrap_err(),
            CutError::Arity {
                expected: 4,
                found: 2
            }
        );
    }

    #[test]
    fn locality_is_decided_from_structure_before_anything_runs() {
        let mut b = Graph::builder();
        b.source("shared", Value::int(1));
        b.source("local_knob", Value::int(1));
        b.cell("served", ["shared"], |i| Ok(Value::int(i.int(0)?)));
        b.cell("drawn", ["served", "local_knob"], |i| {
            Ok(Value::int(i.int(0)? + i.int(1)?))
        });
        let g = b.build().expect("acyclic");

        let cut = Cut::of_client(&g, &["local_knob", "drawn"]).expect("monotone");
        let knob = g.id("local_knob").expect("a source");
        let shared = g.id("shared").expect("a source");
        // The knob's whole closure is `{local_knob, drawn}`, both in the page: no wire.
        assert!(cut.is_local(&g, knob));
        // `shared` feeds `served`, which is on the server: this one costs a round trip.
        assert!(!cut.is_local(&g, shared));
    }
}
