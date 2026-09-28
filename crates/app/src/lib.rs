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

pub mod manifest;
pub mod refresh;
pub mod resume;
pub mod sql;
pub mod view;
pub mod widget;
pub mod wire;

mod reads;
mod session;

/// The CSV reader, which now lives in `dagpane-connect` beside the other sources.
///
/// Re-exported at the path it used to occupy: it is the same reader, and a caller who had
/// `dagpane_app::csv::parse` should not have to care that the file moved.
pub use dagpane_connect::csv;

/// The source vocabulary, re-exported so a caller of this crate needs one dependency and not
/// two. `dagpane-connect` is where these are defined and where a new source is added.
pub use dagpane_connect::{
    file::FileFormat, BytesSource, SchemaSource, Source, SourceError, Version,
};

pub use manifest::{compile, compile_with, load, Manifest, ManifestError, Renderer, Sources};
pub use refresh::{Refresh, RefreshOutcome, SourceRefresh};
pub use resume::{Dropped, ResumeError};
pub use session::AppSession;
pub use view::{render, CustomData, Pane, PaneKind, View};
pub use widget::{Widget, WidgetKind};
pub use wire::{
    BoundaryValue, ClientHalf, ClientMessage, ColumnSpec, PaneUpdate, PassStats, ServerMessage,
};

use std::sync::Arc;

use dagpane_core::{Cut, Graph, Split};

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
    /// Where each `[[source]]` cell's data came from, kept so it can be read **again**.
    ///
    /// The graph holds the rows; this holds the thing that produced them. Without it a
    /// compiled app is a photograph — every session over it sees whatever was on disk when
    /// the process started, forever — and `dagpane refresh`, a schedule and a control plane
    /// all become impossible for the same reason. Empty for an app built by hand.
    pub sources: Vec<BoundSource>,
    /// Scripts a client loads before its first paint, each registering one or more drawings
    /// that a `custom` pane can name. Paths relative to the manifest, already checked to stay
    /// inside the app's own directory.
    ///
    /// Read when the app was compiled, so serving one is a map lookup and never a file read
    /// a request influenced — the same decision `dagpane-serve` makes about its own client.
    pub renderers: Vec<Renderer>,
    /// Where each cell runs, checked when the app compiled.
    ///
    /// [`Cut::whole`] for an app that named no placement, which is every app this project
    /// had before placement existed and still most of them. A caller decides what to do with
    /// a real cut by asking [`Cut::is_split`] — see `dagpane_core::placement` for the
    /// protocol obligation it takes on when the answer is yes.
    pub cut: Cut,
    /// The two halves, built once when the app compiled — `None` unless [`App::cut`] splits.
    ///
    /// Built here rather than per session, and that is the whole reason this field exists on
    /// an immutable shared `App`: [`Graph::split`] rebuilds graphs, and a viewer that built
    /// its own would hold its own copy of every loaded source. One `Arc<Split>` behind every
    /// session keeps the sharing the `Kind::Source` docs promise.
    ///
    /// `None` for an unsplit app rather than a `Split` with an empty client half, so the
    /// common case allocates nothing and a caller cannot accidentally open a side that has
    /// no cells in it.
    pub split: Option<Arc<Split>>,
    /// What a page needs to compile this app's other half — `None` unless [`App::cut`] splits.
    ///
    /// Built once, here, for the same reason [`App::split`] is: every connection would
    /// otherwise re-emit the manifest and re-derive the schemas, and the schemas in
    /// particular come from frames this process already holds, so deriving them per viewer
    /// would be paying repeatedly for an answer that cannot change.
    pub client_half: Option<ClientHalf>,
}

/// One `[[source]]` cell and the thing that fills it.
#[derive(Clone, Debug)]
pub struct BoundSource {
    /// The source cell's name, as the graph knows it.
    pub cell: String,
    /// Where its rows come from.
    pub source: Arc<dyn dagpane_connect::Source>,
    /// The version that was current when the app was compiled. What a refresh compares
    /// against on its first tick.
    pub loaded_version: dagpane_connect::Version,
    /// How often a scheduled refresh should re-read it, from the manifest. Nothing in this
    /// crate owns a clock, so nothing in this crate acts on it.
    pub refresh_secs: Option<u64>,
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
