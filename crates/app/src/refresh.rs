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

use crate::{App, BoundSource};

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

/// What re-reading some of an app's sources found, before it has reached any session.
///
/// Reading and applying are separate because a server has many sessions and one set of
/// sources. The rows are read **once**, here, and then either staged on the one session a
/// command-line [`refresh`] has, or turned by [`App::with_sources`] into the graph every live
/// session moves onto — which is where a new table is weighed, once, rather than per viewer.
#[derive(Clone, Debug)]
pub struct Reading {
    /// One entry per source that was asked about, in the manifest's order.
    pub sources: Vec<(String, SourceRefresh)>,
    /// The rows that came back, by source cell, for every source that [`reloaded`].
    ///
    /// [`reloaded`]: SourceRefresh::reloaded
    pub values: Vec<(String, Value)>,
    /// The version each source that was asked about now holds, for the next read to compare
    /// against. A source that **failed** keeps the version it had — see [`Refresh::versions`].
    pub versions: BTreeMap<String, Version>,
}

impl Reading {
    /// Sources that could not be read, or were refused once read.
    pub fn failed(&self) -> impl Iterator<Item = (&str, &SourceError)> {
        self.sources
            .iter()
            .filter_map(|(name, outcome)| match outcome {
                SourceRefresh::Failed { error } => Some((name.as_str(), error)),
                _ => None,
            })
    }
}

/// Re-read the sources of `app` that `due` picks, without applying anything.
///
/// `previous` is what each source's version was last time, as [`refresh`] takes it. A source
/// whose version has not moved is not read; one that has is loaded through `builder` and
/// checked against the data the app holds now.
///
/// **A refresh changes rows, never the shape the app was checked against.** New rows whose
/// columns differ from the ones the app holds — a column gone, renamed, reordered, or read
/// as another type — are refused and reported as a failure, and the app keeps the rows it
/// had. Every cell was type-checked against those columns when the manifest compiled, and
/// `dagpane check` in front of a deploy is the gate that a change of shape is supposed to go
/// through; a refresh that applied it would hand every cell a table it was never checked
/// against, and the first viewer to find out would be looking at a broken pane.
pub fn read(
    app: &App,
    previous: &BTreeMap<String, Version>,
    outcome: RefreshOutcome,
    due: impl Fn(&BoundSource) -> bool,
    builder: impl Fn() -> Box<dyn FrameBuilder>,
) -> Reading {
    let mut sources = Vec::new();
    let mut values = Vec::new();
    let mut versions = BTreeMap::new();

    for bound in app.sources.iter().filter(|b| due(b)) {
        let was = previous
            .get(&bound.cell)
            .copied()
            .unwrap_or(bound.loaded_version);
        // A source that fails keeps the version it had, so the next read tries again.
        let fail = |sources: &mut Vec<(String, SourceRefresh)>,
                    versions: &mut BTreeMap<String, Version>,
                    error: SourceError| {
            versions.insert(bound.cell.clone(), was);
            sources.push((bound.cell.clone(), SourceRefresh::Failed { error }));
        };

        // The cell this source fills, and the rows it holds now. Only a hand-built `App` can
        // name a cell its graph does not have as a source, so that is reported rather than
        // ignored — dropping it silently would mean a source that never refreshes and no way
        // to find out.
        let Some(held) = app
            .graph
            .id(&bound.cell)
            .and_then(|id| app.graph.initial(id))
        else {
            fail(
                &mut sources,
                &mut versions,
                SourceError::Misconfigured {
                    source: bound.source.describe(),
                    reason: format!("this app has no source cell named {:?}", bound.cell),
                },
            );
            continue;
        };

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
                // The version could not be read, so the source keeps the one it had and the
                // next refresh asks again.
                Err(error) => {
                    fail(&mut sources, &mut versions, error);
                    continue;
                }
            },
        };

        let frame = match bound.source.load(builder()) {
            Ok(frame) => frame,
            Err(error) => {
                fail(&mut sources, &mut versions, error);
                continue;
            }
        };

        // Compared against the frame the app holds rather than against the manifest: the two
        // agree — the app was compiled from that frame, and every refresh since was refused
        // unless it agreed too — and the frame is what a cell will actually be handed.
        if let Some(had) = held.value().and_then(Value::as_frame) {
            let (before, after) = (had.schema(), frame.schema());
            if before != after {
                fail(
                    &mut sources,
                    &mut versions,
                    SourceError::Unreadable {
                        source: bound.source.describe(),
                        reason: format!(
                            "its columns changed from {} to {}; this app was checked against the \
                         first, and a refresh changes rows, never their shape — redeploy to \
                         change it",
                            describe_columns(&before),
                            describe_columns(&after)
                        ),
                    },
                );
                continue;
            }
        }
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

        versions.insert(bound.cell.clone(), now);
        sources.push((
            bound.cell.clone(),
            SourceRefresh::Reloaded { was, now, rows },
        ));
        values.push((bound.cell.clone(), Value::frame(frame)));
    }

    Reading {
        sources,
        values,
        versions,
    }
}

/// `region: text, amount: float`, for a refusal a person has to read.
fn describe_columns(columns: &[(String, dagpane_core::ColumnType)]) -> String {
    let parts: Vec<String> = columns
        .iter()
        .map(|(name, ty)| format!("{name}: {ty}"))
        .collect();
    format!("[{}]", parts.join(", "))
}

/// Re-read `app`'s sources into `session` and run the pass.
///
/// `previous` is what each source's version was last time — [`Refresh::versions`] from the
/// last call, or the app's own [`crate::BoundSource::loaded_version`] on the first. Passing
/// it in rather than storing it here is what keeps an `App` immutable and shared: two
/// sessions over one app can refresh independently, and neither can corrupt the other's idea
/// of what it has seen.
///
/// The one-session form of [`read`]: every source is asked, and the rows that came back are
/// staged on `session` and committed together. A server with many sessions reads once and
/// moves them all onto [`App::with_sources`] instead.
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
    let Reading {
        mut sources,
        values,
        mut versions,
    } = read(app, previous, outcome, |_| true, builder);

    for (cell, value) in values {
        if let Err(e) = session.set(&cell, value) {
            // `read` checked the cell is one of the app's sources, so this is a session over
            // some other graph than the app's. Reported for the same reason as above.
            let Some(entry) = sources.iter_mut().find(|(name, _)| *name == cell) else {
                continue;
            };
            let SourceRefresh::Reloaded { was, .. } = entry.1 else {
                continue;
            };
            versions.insert(cell.clone(), was);
            entry.1 = SourceRefresh::Failed {
                error: SourceError::Misconfigured {
                    source: cell.clone(),
                    reason: e.to_string(),
                },
            };
        }
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
