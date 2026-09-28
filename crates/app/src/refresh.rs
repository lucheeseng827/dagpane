//! Re-reading an app's sources, and the pass that follows.
//!
//! # The shape, and why it is this shape
//!
//! ```text
//!   for each source:  version() ──same?──> skip, nothing staged
//!                          │
//!                       different
//!                          │
//!                        load() ──> stage the frame on its source cell
//!
//!   commit() ──> the engine compares the frame's DIGEST against what the cell holds
//!                  │
//!            same digest ──> the cell does not move, so nothing below it runs
//!                  │
//!         different digest ──> only the closure below that cell recomputes
//! ```
//!
//! **Two filters, and each catches what the other cannot.** [`crate::BoundSource`]'s version
//! is cheap and approximate: it stops a refresh from reading a file that has not been
//! touched, and it can be wrong in the direction of reading one that has not changed. The
//! engine's digest is exact and expensive — the value has to exist to be digested — and it
//! is what decides whether a single cell recomputes. A refresh over an untouched source
//! costs one `stat`; a refresh over a file that was rewritten with identical content costs
//! one read and then nothing at all, and the trace says `visited: 0` either way.
//!
//! That second case is the one worth stating, because it is where a runtime that re-renders
//! on a schedule differs from this one: a nightly export that produces the same bytes
//! repaints nothing, and a viewer with a dashboard open sees no flicker.

use std::collections::BTreeMap;
use std::fmt;

use dagpane_connect::{SourceError, Version};
use dagpane_core::frame::FrameBuilder;
use dagpane_core::{Session, Trace, Value};

use crate::App;

/// What happened to one source in one refresh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceRefresh {
    /// Its version was unchanged, so it was not read.
    Unchanged {
        /// The version, unchanged.
        version: Version,
    },
    /// It was read, and the rows that came back were staged on its cell.
    ///
    /// Whether anything *recomputed* is the trace's answer, not this one — a reloaded source
    /// whose content is identical stages a value the engine then declines to accept, and
    /// that is the point of the two-filter design rather than a wasted step.
    Reloaded {
        /// The version before.
        was: Version,
        /// The version now.
        now: Version,
        /// Rows read.
        rows: usize,
    },
    /// It could not be read. The refresh continues; see [`Refresh::failed`].
    Failed {
        /// Why.
        error: SourceError,
    },
}

impl SourceRefresh {
    /// Whether this source produced rows to stage.
    pub fn reloaded(&self) -> bool {
        matches!(self, SourceRefresh::Reloaded { .. })
    }
}

/// What one refresh did, source by source, plus the pass it caused.
#[derive(Clone, Debug)]
pub struct Refresh {
    /// One entry per source, in the manifest's order.
    pub sources: Vec<(String, SourceRefresh)>,
    /// The pass. Present even when nothing was staged — an empty trace with `visited == 0`
    /// is the answer to "what did that cost", and omitting it would make the cheap case
    /// indistinguishable from the case where nothing ran the pass at all.
    pub trace: Trace,
    /// The version each source now holds, for the next refresh to compare against.
    ///
    /// A source that **failed** keeps the version it had, so the next refresh tries again.
    /// Recording the failure as a version would make one unreachable tick look like a
    /// successful read of unchanged data for as long as the outage lasted.
    pub versions: BTreeMap<String, Version>,
}

impl Refresh {
    /// Sources that could not be read.
    pub fn failed(&self) -> impl Iterator<Item = (&str, &SourceError)> {
        self.sources
            .iter()
            .filter_map(|(name, outcome)| match outcome {
                SourceRefresh::Failed { error } => Some((name.as_str(), error)),
                _ => None,
            })
    }

    /// Sources that were read.
    pub fn reloaded(&self) -> impl Iterator<Item = &str> {
        self.sources
            .iter()
            .filter(|(_, o)| o.reloaded())
            .map(|(name, _)| name.as_str())
    }

    /// Whether every source answered. A refresh with a failure is still a refresh — the
    /// sources that did answer were applied — and this is how a caller decides what to say
    /// about it.
    pub fn complete(&self) -> bool {
        self.failed().next().is_none()
    }
}

/// How much of the version check to trust.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RefreshOutcome {
    /// Ask each source for its version first, and read only what moved. The default, and
    /// what a schedule does every tick.
    #[default]
    IfChanged,
    /// Read every source whatever its version says.
    ///
    /// For the case the version check cannot cover — a file rewritten within its
    /// filesystem's timestamp granularity at an identical length, a server with no
    /// validator, a table whose row count did not move. Costs a read per source and still
    /// repaints nothing if the content matches, because the engine's digest is the second
    /// filter.
    Force,
}

/// Re-read `app`'s sources into `session` and run the pass.
///
/// `previous` is what each source's version was last time — [`Refresh::versions`] from the
/// last call, or the app's own [`crate::BoundSource::loaded_version`] on the first. Passing
/// it in rather than storing it here is what keeps an `App` immutable and shared: two
/// sessions over one app can refresh independently, and neither can corrupt the other's idea
/// of what it has seen.
///
/// # Errors
///
/// Never. A source that cannot be read is recorded in the result and the others are still
/// applied — a dashboard with four sources should not go blank because one API is down, and
/// the pane whose data did not arrive keeps the value it had, which is the honest thing to
/// show and what a freshness label is for.
pub fn refresh(
    app: &App,
    session: &mut Session,
    previous: &BTreeMap<String, Version>,
    outcome: RefreshOutcome,
    builder: impl Fn() -> Box<dyn FrameBuilder>,
) -> Refresh {
    let mut sources = Vec::with_capacity(app.sources.len());
    let mut versions = BTreeMap::new();

    for bound in &app.sources {
        let was = previous
            .get(&bound.cell)
            .copied()
            .unwrap_or(bound.loaded_version);

        let now = match outcome {
            RefreshOutcome::Force => None,
            RefreshOutcome::IfChanged => match bound.source.version() {
                Ok(now) if now == was => {
                    versions.insert(bound.cell.clone(), was);
                    sources.push((
                        bound.cell.clone(),
                        SourceRefresh::Unchanged { version: was },
                    ));
                    continue;
                }
                Ok(now) => Some(now),
                Err(error) => {
                    // The version could not be read, so the source keeps the one it had and
                    // the next refresh asks again.
                    versions.insert(bound.cell.clone(), was);
                    sources.push((bound.cell.clone(), SourceRefresh::Failed { error }));
                    continue;
                }
            },
        };

        let frame = match bound.source.load(builder()) {
            Ok(frame) => frame,
            Err(error) => {
                versions.insert(bound.cell.clone(), was);
                sources.push((bound.cell.clone(), SourceRefresh::Failed { error }));
                continue;
            }
        };
        let rows = frame.rows();

        // Read AFTER the load under `Force`, for the reason `compile` states: a version read
        // before the rows records the state of a file that may have been rewritten between
        // the two, and every later refresh would then compare against a version the data
        // never had.
        let now = match now {
            Some(now) => now,
            None => match bound.source.version() {
                Ok(now) => now,
                // The rows arrived and the version did not. Keeping the old version means the
                // next refresh reads again, which is a wasted read and never a stale pane.
                Err(_) => was,
            },
        };

        if session.set(&bound.cell, Value::frame(frame)).is_err() {
            // The cell is not in the graph. Only reachable for a hand-built `App` whose
            // `sources` names something its graph does not, so it is reported rather than
            // ignored — silently dropping it would mean a source that never refreshes and
            // no way to find out.
            sources.push((
                bound.cell.clone(),
                SourceRefresh::Failed {
                    error: SourceError::Misconfigured {
                        source: bound.source.describe(),
                        reason: format!("this app has no source cell named {:?}", bound.cell),
                    },
                },
            ));
            versions.insert(bound.cell.clone(), was);
            continue;
        }

        versions.insert(bound.cell.clone(), now);
        sources.push((
            bound.cell.clone(),
            SourceRefresh::Reloaded { was, now, rows },
        ));
    }

    Refresh {
        sources,
        // One commit for every source that moved, not one per source: two sources feeding one
        // cell must not run it twice with a mix of old and new, which is the same reason a
        // client that moves two sliders in one gesture sends one `Set`.
        trace: session.commit(),
        versions,
    }
}

impl fmt::Display for SourceRefresh {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceRefresh::Unchanged { version } => write!(f, "unchanged ({version})"),
            SourceRefresh::Reloaded { was, now, rows } => {
                write!(f, "reloaded {rows} rows ({was} → {now})")
            }
            SourceRefresh::Failed { error } => write!(f, "failed: {error}"),
        }
    }
}
