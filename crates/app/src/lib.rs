//! `dagpane-app` — the app model on top of the engine.
//!
//! [`dagpane_core`] knows about cells and values. This crate knows what a slider is, what a
//! pane shows, and what goes on the wire — and it knows nothing about sockets, runtimes or
//! files beyond the CSV it reads. `cargo test -p dagpane-app` runs the whole patch protocol
//! with no server in the process, which is the property that makes the protocol testable at
//! all.
//!
//! # The invariant this crate carries alone
//!
//! **A pane is not a cell.** Two panes can show one cell, a cell can drive no pane, and a
//! cell whose *value* changed can leave its pane's *view* identical — a table pane sending
//! its first fifty rows does not change when row nine thousand does. So a patch is computed
//! from rendered views, not from the trace: the trace says which cells to re-render, and the
//! rendered view's own digest decides whether it goes on the wire. Skipping that second step
//! would put the engine's honesty at the mercy of the renderer.

#![forbid(unsafe_code)]
#![deny(missing_debug_implementations)]
#![deny(missing_docs)]

pub mod csv;
pub mod manifest;
pub mod view;
pub mod widget;
pub mod wire;

mod session;

pub use manifest::{compile, load, Manifest, ManifestError};
pub use session::AppSession;
pub use view::{render, Pane, PaneKind, View};
pub use widget::{Widget, WidgetKind};
pub use wire::{ClientMessage, PaneUpdate, PassStats, ServerMessage};

use std::sync::Arc;

use dagpane_core::Graph;

/// A compiled app: the graph, the controls, and the panes. Immutable, and shared by every
/// session — one `Arc<App>` however many viewers there are.
#[derive(Debug)]
pub struct App {
    /// The app's title, shown by the client and used as the page title.
    pub title: String,
    /// An optional line under the title.
    pub subtitle: Option<String>,
    /// The reactive graph. Shared with every [`AppSession`] and with any other `App` compiled
    /// from the same manifest — a session holds this `Arc`, never a copy.
    pub graph: Arc<Graph>,
    /// The controls, each bound to a source cell. [`compile`] guarantees every one names a
    /// source that exists; a hand-built `App` does not, since these fields are public.
    pub widgets: Vec<Widget>,
    /// The panes, in display order. Not a one-to-one map onto cells in either direction —
    /// see the invariant in this module's own docs.
    pub panes: Vec<Pane>,
}

impl App {
    /// The control bound to this source cell, if there is one. A cell may have no widget:
    /// a source loaded from a CSV is set by the host, not by a user.
    pub fn widget(&self, cell: &str) -> Option<&Widget> {
        self.widgets.iter().find(|w| w.cell == cell)
    }

    /// Panes showing a given cell. More than one is legal — the same number as a metric and
    /// inside a table, say — which is exactly why panes have their own identity.
    pub fn panes_for<'a>(&'a self, cell: &'a str) -> impl Iterator<Item = &'a Pane> + 'a {
        self.panes.iter().filter(move |p| p.cell == cell)
    }
}
