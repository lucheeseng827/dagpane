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
use dagpane_core::{Session, Trace, Value};

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

/// One viewer's state.
#[derive(Debug)]
pub struct AppSession {
    app: Arc<App>,
    session: Session,
    /// What this viewer currently has on screen, by pane id.
    on_screen: HashMap<String, Digest>,
}

impl AppSession {
    /// Open a session and compute the first render. Returns the trace so a caller that has a
    /// clock can report what the first pass cost.
    pub fn open(app: Arc<App>) -> (AppSession, Trace) {
        let session = Session::new(Arc::clone(&app.graph));
        let mut me = AppSession {
            app,
            session,
            on_screen: HashMap::new(),
        };
        let trace = me.session.refresh();
        (me, trace)
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

    /// Every pane's current view, and the record of having sent them. What a client gets on
    /// connect, and after a reconnect.
    pub fn full_views(&mut self) -> Vec<PaneUpdate> {
        let mut out = Vec::with_capacity(self.app.panes.len());
        for pane in &self.app.panes {
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
        // Two preflights, then apply. `Widget::accepts` answers "could this control have
        // produced this value"; the core session answers "is this a source cell of a
        // compatible type". Neither implies the other — `App`'s fields are public, so a
        // widget can name a computed or unknown cell — and checking only the first meant a
        // valid earlier entry could be staged before a later one was rejected, leaving the
        // viewer's controls and the session disagreeing with no way to tell which was right.
        for (name, value) in values {
            match self.app.widget(name) {
                Some(w) => w.accepts(value)?,
                None => return Err(format!("`{name}` is not an input of this app")),
            }
        }
        for (name, value) in values {
            self.session
                .can_set(name, value)
                .map_err(|e| e.to_string())?;
        }
        for (name, value) in values {
            self.session
                .set(name, value.clone())
                .map_err(|e| e.to_string())?;
        }
        Ok(())
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
            .map(|id| self.app.graph.name(id).to_string())
            .collect();

        let mut out = Vec::new();
        for pane in &self.app.panes {
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

    /// The `init` message: everything a client needs to draw the page for the first time.
    pub fn init_message(&mut self, trace: &Trace, micros: Option<u64>) -> ServerMessage {
        let views = self.full_views();
        let mut stats = PassStats::from_trace(trace);
        stats.micros = micros;
        ServerMessage::Init {
            title: self.app.title.clone(),
            subtitle: self.app.subtitle.clone(),
            widgets: self.app.widgets.clone(),
            panes: self.app.panes.clone(),
            views,
            stats,
        }
    }
}
