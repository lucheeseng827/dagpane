//! What an app costs to hold, and how much the process is willing to hold.
//!
//! The eviction policy is a byte budget rather than a count or an LRU guess, and this module
//! is why that is possible at all: `Frame::memory_size` is a number every backend answers
//! about its own storage, so "what does this app cost" has an answer that was measured
//! rather than assumed.

use std::time::Duration;

use dagpane_app::App;
use dagpane_core::Session;

/// What one compiled app costs to keep resident.
///
/// Three numbers rather than one, because the one that matters for admission is not the one
/// that is legible in a log line. `source_bytes` decides; `rows` and `cells` are what a
/// person reads when they want to know *why* an app is expensive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Footprint {
    /// Resident bytes held by the app's loaded sources, summed over
    /// [`dagpane_core::frame::Frame::memory_size`].
    ///
    /// **This is the app's data, not the process's.** It excludes the graph's own nodes, the
    /// compiled pipelines, every session's slot vector, and the allocator's slack. Those are
    /// real and they are not here, for the reason the whole crate exists: the sources are
    /// what a second app adds and a second *viewer* does not, so they are what a budget over
    /// apps can honestly be written in. Nothing in this crate predicts RSS from it.
    pub source_bytes: u64,
    /// Rows across those sources.
    pub source_rows: u64,
    /// Cells in the graph.
    pub cells: u32,
}

impl Footprint {
    /// Measure a compiled app.
    ///
    /// Cheap: [`Session::new`] seeds sources by `Arc::clone` and evaluates nothing, so this
    /// reads the frames the compile already built and asks each one its own size. It runs
    /// once per load, never per session.
    pub fn of(app: &App) -> Footprint {
        let session = Session::new(app.graph.clone());
        let mut source_bytes = 0u64;
        let mut source_rows = 0u64;

        for id in app.graph.sources() {
            if let Some(frame) = session.get_id(id).value().and_then(|v| v.as_frame()) {
                source_bytes += frame.memory_size() as u64;
                source_rows += frame.rows() as u64;
            }
        }

        Footprint {
            source_bytes,
            source_rows,
            cells: app.graph.len() as u32,
        }
    }
}

/// How much the process will hold, and how long it will hold something nobody is opening.
///
/// Configuration, and stated as such: there is no default that is right for two machines,
/// and a host that picks one silently is a host whose eviction is a surprise. [`Budget::new`]
/// makes both numbers explicit at the call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// The ceiling on the sum of every resident app's [`Footprint::source_bytes`].
    ///
    /// An app whose own footprint exceeds this is **refused**, not admitted and then
    /// immediately evicted: admitting it would evict every other app first and still fail,
    /// which turns one oversized deploy into an outage for everything else on the node.
    pub max_bytes: u64,
    /// How long an app may go unopened before a sweep may drop it.
    ///
    /// Only a sweep acts on this — nothing is evicted for being idle on the request path,
    /// because the request path is the one place where the cost of being wrong is a user
    /// waiting for a recompile.
    pub idle_after: Duration,
}

impl Budget {
    /// Both numbers, spelled out.
    pub fn new(max_bytes: u64, idle_after: Duration) -> Budget {
        Budget {
            max_bytes,
            idle_after,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dagpane_core::{Column, Table, Value};

    fn app_with_source(rows: usize) -> App {
        let mut b = dagpane_core::Graph::builder();
        let table = Table::new(vec![
            Column::int("n", (0..rows).map(|i| Some(i as i64)).collect()),
            Column::text("s", (0..rows).map(|i| Some(format!("row-{i}"))).collect()),
        ])
        .unwrap();
        b.source("data", Value::frame(std::sync::Arc::new(table)));
        b.cell("n_rows", ["data"], |i| {
            Ok(Value::int(i.frame(0)?.rows() as i64))
        });
        let graph = b.build().unwrap();
        App {
            title: "t".into(),
            subtitle: None,
            // Hand-built and undivided: nothing here is placed anywhere but the server.
            cut: dagpane_core::Cut::whole(&graph),
            split: None,
            client_half: None,
            graph,
            widgets: Vec::new(),
            panes: Vec::new(),
            // Hand-built: the frames are already here, so there is nothing to re-read.
            sources: Vec::new(),
            renderers: Vec::new(),
        }
    }

    #[test]
    fn a_footprint_grows_with_the_data_and_not_with_the_graph() {
        let small = Footprint::of(&app_with_source(10));
        let large = Footprint::of(&app_with_source(1_000));

        assert_eq!(small.cells, large.cells, "the same graph either way");
        assert_eq!(small.source_rows, 10);
        assert_eq!(large.source_rows, 1_000);
        assert!(
            large.source_bytes > small.source_bytes * 10,
            "a hundred times the rows is not fewer bytes: {small:?} vs {large:?}"
        );
    }

    #[test]
    fn a_graph_with_no_loaded_source_costs_no_bytes() {
        let mut b = dagpane_core::Graph::builder();
        b.source("min_amount", Value::int(0));
        b.cell("double", ["min_amount"], |i| Ok(Value::int(i.int(0)? * 2)));
        let graph = b.build().unwrap();
        let app = App {
            title: "t".into(),
            subtitle: None,
            cut: dagpane_core::Cut::whole(&graph),
            split: None,
            client_half: None,
            graph,
            widgets: Vec::new(),
            panes: Vec::new(),
            // Hand-built: the frames are already here, so there is nothing to re-read.
            sources: Vec::new(),
            renderers: Vec::new(),
        };
        let f = Footprint::of(&app);
        assert_eq!(f.source_bytes, 0);
        assert_eq!(f.source_rows, 0);
        assert_eq!(f.cells, 2);
    }
}
