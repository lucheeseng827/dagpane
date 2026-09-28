//! One viewer: a [`Session`](dagpane_core::Session) plus what that viewer has been shown.
//!
//! The patch is built here, and it is built in two stages on purpose:
//!
//!   1. the engine says which *cells* changed ([`Session::changed_since`]);
//!   2. each pane showing one of those cells is re-rendered, and only a pane whose
//!      **rendered view** differs from the one the viewer already has goes on the wire.
//!
//! Stage 2 is not redundant. A table pane sends its first fifty rows; a change in row nine
//! thousand changes the cell and not the view. Deriving the patch from the trace alone would
//! send that pane anyway and quietly make the engine's numbers look better than the wire.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use dagpane_core::digest::{Digest, Hasher};
use dagpane_core::placement::Frontier;
use dagpane_core::{Placement, Session, SessionError, Trace, Value};

use crate::resume::Dropped;
use crate::view::{render, View};
use crate::wire::{PaneUpdate, PassStats, ServerMessage};
use crate::App;

/// A view's identity on the wire. Taken over the view's JSON encoding, which is exactly the
/// bytes the client would receive — so two views are "the same" here precisely when sending
/// the second one would change nothing on the page.
fn view_digest(view: &View) -> Digest {
    let mut h = Hasher::new();
    match serde_json::to_vec(view) {
        Ok(bytes) => h.bytes(&bytes),
        // Unreachable for the `View` enum as written; if a variant is ever added that cannot
        // serialise, digesting the error text means the pane is resent every pass rather
        // than never — the safe direction.
        Err(e) => h.tag(0xff).str(&e.to_string()),
    };
    h.finish()
}

/// The view for a pane whose cell does not exist. Reachable only through hand-built `App`s;
/// a compiled manifest cannot produce one.
fn missing_cell(pane: &crate::view::Pane, error: &dagpane_core::SessionError) -> View {
    View::Error {
        message: format!("pane `{}` shows `{}`: {error}", pane.id, pane.cell),
        cause: pane.cell.clone(),
    }
}

/// One viewer's state, on one side of the cut.
///
/// # Sides
///
/// An app with no `place` in its manifest has one side, and everything below reads as it did
/// before sides existed: the session runs the whole graph and owns every pane.
///
/// A **split** app has two, and this one type runs either. What decides which panes a session
/// owns is not a new field on `Pane` but a question the cut can already answer — **a pane
/// belongs to the side its cell is PLACED on.**
///
/// Placed on, not present in, and the difference is the boundary. A boundary cell exists in
/// both halves — the server computes it, the page holds a copy fed by the frontier — so
/// "present in this graph" would hand its pane to both sides, and the page would get the same
/// pane id from two writers with nothing to say which wins. `Cut::placement` is single-valued
/// for every cell, so asking it instead makes the partition total by construction, which is
/// what `every_pane_belongs_to_exactly_one_side` checks with a pane on the boundary itself.
///
/// Two sides of one type rather than two types, because the whole promise of a cut is that
/// it does not change what an app computes. Two implementations of "apply, commit, render
/// what moved" would be two places for that promise to stop being true.
#[derive(Debug)]
pub struct AppSession {
    app: Arc<App>,
    /// Which half this session runs, or `None` for the whole graph undivided.
    ///
    /// `None` is not the same as `Some(Server)` on a split app, and the difference is the
    /// export: a static bundle has no server to feed it a frontier, so it runs **everything**
    /// in the page. That is sound for the reason the whole design rests on — a cut says where
    /// cells *may* run, and running all of them in one session is the undivided app whose
    /// values a split is required to reproduce.
    side: Option<Placement>,
    session: Session,
    /// What this viewer currently has on screen, by pane id.
    ///
    /// **Per side.** A page-side pane repainted in the browser must not be recorded here, on
    /// the server, or the server would believe it had sent something it never saw — which is
    /// the exact bookkeeping error that would make the stats block lie.
    on_screen: HashMap<String, Digest>,
}

impl AppSession {
    /// Open a session and compute the first render. Returns the trace so a caller that has a
    /// clock can report what the first pass cost.
    pub fn open(app: Arc<App>) -> (AppSession, Trace) {
        let (me, trace, dropped) = AppSession::resume(app, &BTreeMap::new());
        debug_assert!(dropped.is_empty(), "an empty resume cannot drop anything");
        (me, trace)
    }

    /// Open one side of a split app.
    ///
    /// [`Placement::Server`] is [`AppSession::open`] and is what an unsplit app has.
    /// [`Placement::Client`] is the half that runs in the page — its boundary cells start at
    /// null, so it is not showing anything true until a [`Frontier`] has been delivered.
    /// `dagpane-wasm` is the caller that matters; the same door is open to a test or a host
    /// that wants to run both halves in one process.
    ///
    /// # Panics
    ///
    /// Asking for [`Placement::Client`] on an app that declared no placement. There is no
    /// client half to run, and returning an empty session would silently draw a blank page
    /// rather than say so.
    pub fn open_side(app: Arc<App>, side: Placement) -> (AppSession, Trace) {
        let (me, trace, dropped) = AppSession::resume_side(app, Some(side), &BTreeMap::new());
        debug_assert!(dropped.is_empty(), "an empty resume cannot drop anything");
        (me, trace)
    }

    /// The graph this side runs: the whole app's, or one half of the split.
    fn graph_for(app: &App, side: Option<Placement>) -> Arc<dagpane_core::Graph> {
        match (side, &app.split) {
            (None, _) => Arc::clone(&app.graph),
            (Some(Placement::Server), Some(split)) => Arc::clone(&split.server),
            (Some(Placement::Client), Some(split)) => Arc::clone(&split.client),
            (Some(side), None) => panic!(
                "this app declared no placement, so it has no {side} half to run — open it \
                 undivided instead"
            ),
        }
    }

    /// A session that starts where a viewer left off.
    ///
    /// `values` is what the viewer brought back — see [`crate::resume`] for why they, and not
    /// a server-side store, are holding it. Every value goes through the **same** two
    /// predicates a `Set` does, and the ones that pass are applied **before the first
    /// render**, so restoring a session costs one pass rather than a render at the defaults
    /// followed by a correction.
    ///
    /// Returns what it could not apply rather than failing. A manifest that renamed an input
    /// last week must not make every bookmarked link fail to load — and the viewer has to be
    /// told, because otherwise the same URL quietly shows them different numbers. See the
    /// module docs on why this differs from `Set`, which is all-or-nothing on purpose.
    pub fn resume(
        app: Arc<App>,
        values: &BTreeMap<String, Value>,
    ) -> (AppSession, Trace, Vec<Dropped>) {
        AppSession::resume_side(app, None, values)
    }

    /// [`AppSession::resume`] on a named side. See [`AppSession::open_side`].
    ///
    /// A value naming a control the other side owns is **dropped and reported**, not an
    /// error: a bookmarked link carries every control the app has, and half of them
    /// legitimately belong to the other half. The viewer is told, for the same reason a
    /// renamed input is reported — otherwise the same URL quietly shows them different
    /// numbers.
    pub fn resume_side(
        app: Arc<App>,
        side: Option<Placement>,
        values: &BTreeMap<String, Value>,
    ) -> (AppSession, Trace, Vec<Dropped>) {
        let session = Session::new(AppSession::graph_for(&app, side));
        let mut me = AppSession {
            app,
            side,
            session,
            on_screen: HashMap::new(),
        };

        let mut dropped = Vec::new();
        for (name, value) in values {
            // The same predicate `set` preflights with, so a lock or a validation added
            // there protects a resume without anybody remembering to add it here.
            if let Err(reason) = me.admits(name, value) {
                dropped.push(Dropped {
                    input: name.clone(),
                    reason,
                });
                continue;
            }
            if let Err(e) = me.session.set(name, value.clone()) {
                dropped.push(Dropped {
                    input: name.clone(),
                    reason: e.to_string(),
                });
            }
        }

        // One pass, with the restored values already staged: `refresh` computes every cell
        // once, from the state the viewer left rather than from the app's defaults.
        let trace = me.session.refresh();
        (me, trace, dropped)
    }

    /// Whether this input may take this value: the widget's own bounds, then the cell's.
    ///
    /// Extracted so that [`AppSession::set`] and [`AppSession::resume`] cannot drift. They
    /// differ in what they do about a failure — all-or-nothing against report-and-continue —
    /// and must not differ in what counts as one.
    fn admits(&self, name: &str, value: &Value) -> Result<(), String> {
        // `Widget::accepts` answers "could this control have produced this value"; the core
        // session answers "is this a source cell of a compatible type". Neither implies the
        // other — `App`'s fields are public, so a widget can name a computed or unknown cell.
        match self.app.widget(name) {
            Some(w) => w.accepts(value)?,
            None => return Err(format!("`{name}` is not an input of this app")),
        }
        // A control this app has and this SIDE does not. Worth its own sentence: the generic
        // "unknown cell" the engine would give is true and unhelpful, and the thing to do
        // about it — send it to the other half — is not something the reader would guess.
        if self.session.graph().id(name).is_none() {
            return Err(format!(
                "`{name}` runs on the {} side of this app, not the {} side",
                self.other_side(),
                self.side.map_or("undivided", |p| p.as_str())
            ));
        }
        self.session.can_set(name, value).map_err(|e| e.to_string())
    }

    fn other_side(&self) -> Placement {
        match self.side {
            Some(Placement::Client) => Placement::Server,
            _ => Placement::Client,
        }
    }

    /// The app this session runs. Cloning the `Arc` is how a caller keeps the widget and
    /// pane definitions past the session's borrow — it does not copy the app.
    pub fn app(&self) -> &Arc<App> {
        &self.app
    }

    /// The underlying engine session, for reading cell values and digests directly. Borrowed
    /// immutably on purpose: staging and committing go through this type, which is what keeps
    /// the rendered-view cache honest about what the client has actually been sent.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Which side this session runs, or `None` for the whole graph undivided.
    pub fn side(&self) -> Option<Placement> {
        self.side
    }

    /// Whether this session owns the pane — that is, whether the cut placed its cell here.
    ///
    /// The only rule about pane ownership there is. No `Pane` field says which side a pane is
    /// on, because the cut already decides it and a second answer could disagree with the
    /// first.
    ///
    /// Asked of the **cut** and not of this half's graph, which is not the same question for a
    /// boundary cell: that one is in both graphs, so graph membership would give its pane two
    /// owners. A boundary cell is placed on the server by definition — it is a server cell
    /// with a client dependent — so the server renders its pane and the page does not.
    pub fn owns(&self, pane: &crate::view::Pane) -> bool {
        match self.side {
            // Undivided: this session is the only one there is.
            None => true,
            Some(side) => self
                .app
                .graph
                .id(&pane.cell)
                .is_some_and(|id| self.app.cut.placement(id) == side),
        }
    }

    /// The panes of this app that this side owns, in display order.
    fn my_panes(&self) -> Vec<crate::view::Pane> {
        self.app
            .panes
            .iter()
            .filter(|p| self.owns(p))
            .cloned()
            .collect()
    }

    /// Every pane's current view, and the record of having sent them. What a client gets on
    /// connect, and after a reconnect.
    ///
    /// On a split app this is **this side's** panes. The other half's arrive from the other
    /// half, and the client lays them out together in the order `Init` gave.
    pub fn full_views(&mut self) -> Vec<PaneUpdate> {
        let mine = self.my_panes();
        let mut out = Vec::with_capacity(mine.len());
        for pane in &mine {
            let view = match self.session.get(&pane.cell) {
                Ok(outcome) => render(pane, outcome),
                // `manifest::compile` rejects a pane naming no cell, but `App`'s fields are
                // public, so a caller building one by hand can still get here. A pane that
                // cannot resolve says so in its own place on the page; it does not panic and
                // take the whole app's first render with it.
                Err(e) => missing_cell(pane, &e),
            };
            self.on_screen.insert(pane.id.clone(), view_digest(&view));
            out.push(PaneUpdate {
                id: pane.id.clone(),
                view,
            });
        }
        out
    }

    /// Apply a batch of input values and recompute once.
    ///
    /// Every value is checked against its widget *before* anything is staged, so a batch
    /// containing one bad value changes nothing at all — a half-applied interaction would
    /// leave the viewer's controls and the session disagreeing, with no way to tell which is
    /// right.
    pub fn set(&mut self, values: &BTreeMap<String, Value>) -> Result<(), String> {
        // Preflight EVERYTHING, then apply. Checking and applying in one loop meant a valid
        // earlier entry could be staged before a later one was rejected, leaving the viewer's
        // controls and the session disagreeing with no way to tell which was right.
        for (name, value) in values {
            self.admits(name, value)?;
        }
        for (name, value) in values {
            self.session
                .set(name, value.clone())
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Replace a source cell's value, the way a refresh does.
    ///
    /// Deliberately not [`AppSession::set`]. That one is for *inputs*: it checks every value
    /// against the widget bound to it, and a source loaded from a CSV has no widget, so it
    /// would rightly refuse this. A source's data moving is a different event from a viewer
    /// turning a knob, and the two do not share a door.
    ///
    /// `dagpane refresh` does this to a source when its underlying data changed; `dagpane
    /// explain --change-column` does it to measure what such a change costs.
    pub fn set_source(&mut self, cell: &str, value: Value) -> Result<(), String> {
        self.session.set(cell, value).map_err(|e| e.to_string())
    }

    /// Recompute, then work out what the viewer has to be sent.
    pub fn commit(&mut self) -> (Trace, Vec<PaneUpdate>) {
        let epoch_of_this_pass = self.session.epoch() + 1;
        let trace = self.session.commit();
        let updates = self.patch(epoch_of_this_pass);
        (trace, updates)
    }

    fn patch(&mut self, since: u64) -> Vec<PaneUpdate> {
        let changed: Vec<String> = self
            .session
            .changed_since(since)
            .into_iter()
            // THIS SIDE'S graph, not the app's. `changed_since` returns ids of the graph the
            // session runs, and a split half numbers its cells independently — the server
            // half of `20-placed.toml` calls id 1 `all_time_revenue` where the whole app
            // calls it `min_amount`. Resolving through the app's graph therefore names a
            // different cell, and the pane that actually moved is silently dropped.
            .map(|id| self.session.graph().name(id).to_string())
            .collect();

        let mut out = Vec::new();
        for pane in &self.my_panes() {
            if !changed.contains(&pane.cell) {
                continue;
            }
            let view = match self.session.get(&pane.cell) {
                Ok(outcome) => render(pane, outcome),
                Err(e) => missing_cell(pane, &e),
            };
            let digest = view_digest(&view);
            if self.on_screen.get(&pane.id) == Some(&digest) {
                // The cell moved and the pane did not. Sending it would be a lie about how
                // much this interaction cost.
                continue;
            }
            self.on_screen.insert(pane.id.clone(), digest);
            out.push(PaneUpdate {
                id: pane.id.clone(),
                view,
            });
        }
        out
    }

    // ── the cut ────────────────────────────────────────────────────────────────────────
    //
    // Three methods, and between them they are the whole transport obligation
    // `dagpane_core::placement` names. Everything else about a split app is the code above,
    // unchanged, running over a smaller graph.

    /// What this side has to tell the other one, as boundary cells that moved.
    ///
    /// Empty for an unsplit app, for the client half (nothing flows back — that is the one
    /// rule), and for any pass on the server that moved no boundary cell. That last case is
    /// the common one on a well-cut app and it is why this returns a `Frontier` rather than
    /// sending anything: a pass with an empty frontier puts nothing on the wire.
    ///
    /// `since` is the epoch of the pass that just ran. [`AppSession::full_frontier`] is what
    /// the opening frame carries instead, because a client starting from nulls needs every
    /// boundary cell and not only the ones that happened to move.
    pub fn frontier(&self, since: u64) -> Frontier {
        match (&self.app.split, self.side) {
            (Some(split), Some(Placement::Server)) => split.frontier(&self.session, since),
            _ => Frontier {
                epoch: self.session.epoch(),
                cells: Vec::new(),
            },
        }
    }

    /// Every boundary cell, whatever it did last pass — what the opening frame carries.
    ///
    /// See `dagpane_core::placement::Split::full_frontier` for why this is not a delta, and
    /// for the test that caught the difference mattering.
    pub fn full_frontier(&self) -> Frontier {
        match (&self.app.split, self.side) {
            (Some(split), Some(Placement::Server)) => split.full_frontier(&self.session),
            _ => Frontier {
                epoch: self.session.epoch(),
                cells: Vec::new(),
            },
        }
    }

    /// Stage a whole frontier on this side, ready for one [`AppSession::commit`].
    ///
    /// Staging and not applying: the commit is the caller's, and doing it once after the
    /// whole frontier is staged is the atomicity the correctness argument rests on.
    ///
    /// # Errors
    ///
    /// A message for the caller when the frontier names a cell this half has no source for,
    /// which means the two sides were compiled from different manifests. Failing loudly beats
    /// drawing three-quarters of a page.
    pub fn deliver(&mut self, frontier: &Frontier) -> Result<(), String> {
        // Check everything, then stage — the same preflight `AppSession::set` does, and for a
        // sharper reason. A frontier refused on its last cell that had already staged the
        // others would leave the caller's next commit applying a fragment of an update it was
        // told had failed, which is a mixture of two of the server's passes in one of this
        // side's. See `dagpane_core::placement`.
        let refuse = |name: &str, e: SessionError| {
            format!("the frontier named `{name}`, which this half does not have: {e}")
        };
        for (name, outcome) in &frontier.cells {
            self.session
                .can_set_outcome(name, outcome)
                .map_err(|e| refuse(name, e))?;
        }
        for (name, outcome) in &frontier.cells {
            self.session
                .set_outcome(name, (**outcome).clone())
                .map_err(|e| refuse(name, e))?;
        }
        Ok(())
    }

    /// The `init` message: everything a client needs to draw the page for the first time.
    pub fn init_message(&mut self, trace: &Trace, micros: Option<u64>) -> ServerMessage {
        self.init_message_with(trace, micros, Vec::new())
    }

    /// The opening frame, carrying what a resume could not apply.
    ///
    /// Separate from [`AppSession::init_message`] rather than a parameter on it, because the
    /// overwhelming majority of connections drop nothing and should not have to say so.
    pub fn init_message_with(
        &mut self,
        trace: &Trace,
        micros: Option<u64>,
        dropped: Vec<Dropped>,
    ) -> ServerMessage {
        let views = self.full_views();
        let mut stats = PassStats::from_trace(trace);
        stats.micros = micros;
        // The FULL frontier, not a delta: a client's boundary sources start at null, and a
        // half seeded only with what moved would compute from a value it was never meant to
        // see. Empty for an unsplit app and for the client's own side.
        let frontier = crate::wire::BoundaryValue::of(&self.full_frontier());
        ServerMessage::Init {
            // Only the SERVER half sends this, and only for a split app: it is what the page
            // needs in order to be the other half, so a page sending it to itself would be
            // circular and an undivided session has no other half to describe.
            client_half: match self.side {
                Some(Placement::Server) => self.app.client_half.clone().map(Box::new),
                _ => None,
            },
            frontier,
            title: self.app.title.clone(),
            subtitle: self.app.subtitle.clone(),
            widgets: self.app.widgets.clone(),
            panes: self.app.panes.clone(),
            views,
            renderers: self.app.renderers.iter().map(|r| r.path.clone()).collect(),
            stats,
            dropped,
        }
    }
}
