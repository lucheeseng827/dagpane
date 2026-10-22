//! What an app costs to hold, and how much the process is willing to hold.
//!
//! The eviction policy is a byte budget rather than a count or an LRU guess, and this module
//! is why that is possible at all: `Frame::memory_size` is a number every backend answers
//! about its own storage, so "what does this app cost" has an answer that was measured
//! rather than assumed.

use std::time::Duration;

use dagpane_app::App;
use dagpane_core::frame::FrameLedger;
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
    /// apps can honestly be written in. Nothing in this crate predicts RSS from it. What a
    /// viewer adds is [`SessionFootprint`], counted per session and never added to this.
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

/// What every session of one app starts out holding without owning: the allocations behind
/// its sources.
///
/// Computed once, when the app is admitted, from the same frames [`Footprint::of`] weighs,
/// and kept beside the app so that [`SessionFootprint::of`] can subtract it from every session
/// for as long as the app is resident — rather than rediscovering, on every pass, what every
/// session already shares.
#[derive(Clone, Debug, Default)]
pub struct SharedBaseline(FrameLedger);

impl SharedBaseline {
    /// Everything reachable from a compiled app's sources, marked as paid for.
    ///
    /// Every source, scalars included: a text box's default string is shared until a viewer
    /// types into it, and then the new string is theirs — which is the right answer, and falls
    /// out of excluding the default rather than needing a rule of its own.
    pub fn of(app: &App) -> SharedBaseline {
        let session = Session::new(app.graph.clone());
        let mut ledger = FrameLedger::new();
        for id in app.graph.sources() {
            if let Some(value) = session.get_id(id).value() {
                ledger.exclude(value);
            }
        }
        SharedBaseline(ledger)
    }
}

/// What one live session holds over and above the app it is a session of.
///
/// The term [`Footprint`] leaves out. The sources are shared and the frames a session
/// computes are not, so a row-keeping pipeline costs about its source again per viewer while
/// an aggregating one costs almost nothing. `benches/sessions/` measured that from outside the
/// process as resident memory; this counts it from inside, per session, as it happens.
///
/// **Reported, not enforced.** Nothing in this crate admits, refuses or evicts on it:
/// [`Budget::max_bytes`] is still written in source bytes and still decides alone. A budget
/// over apps that charged them for their viewers would evict an app *because* it was popular,
/// out from under the viewers making it so — and would free nothing by it, because a viewer
/// holds their app's graph, and through it the sources, until they leave. Limiting what viewers
/// hold means refusing or shedding viewers: a different policy from the one implemented here,
/// and one nobody has chosen. What this does is turn the figure such a policy would need from
/// something an operator works out by hand into something the process knows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionFootprint {
    /// Bytes behind the session's slots that its app's sources do not already hold, each
    /// allocation counted once — however many cells, panes or derived frames reach it.
    pub bytes: u64,
    /// How many distinct allocations those bytes are spread over.
    pub allocations: u64,
}

impl SessionFootprint {
    /// Weigh a session against the baseline its app was admitted with.
    ///
    /// Every slot, computed or stale: a stale slot's value is resident until the next pass
    /// replaces it, so leaving it out would under-count exactly when a viewer has just moved
    /// a control. The cost is a walk over the cells and the buffers behind their frames — no
    /// copy, and no hashing of any cell's contents.
    pub fn of(session: &Session, baseline: &SharedBaseline) -> SessionFootprint {
        let mut ledger = baseline.0.clone();
        for &id in session.graph().order() {
            if let Some(value) = session.get_id(id).value() {
                ledger.add(value);
            }
        }
        SessionFootprint {
            bytes: ledger.bytes(),
            allocations: ledger.allocations(),
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
