//! `dagpane refresh` — re-read the sources, and print what it cost.
//!
//! The command exists to make one number visible: **a refresh over an unchanged source
//! visits nothing.** Not "is fast" — visits nothing, and the trace says so in the same
//! counts `dagpane explain` prints and the same ones the server sends a browser.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use dagpane_app::refresh::{refresh as run_refresh, Refresh, RefreshOutcome, SourceRefresh};
use dagpane_app::App;
use dagpane_core::{Session, StepOutcome};
use serde_json::json;

/// Load the app's sources again and report.
///
/// The app was compiled a moment ago, which means its sources were read a moment ago — so a
/// plain `dagpane refresh` on a static file is the unchanged case, and that is deliberate:
/// it is the case the roadmap's exit criterion names, and it should be the one a person gets
/// by running the command with no arguments.
pub fn run(app: Arc<App>, force: bool) -> Report {
    let mut session = Session::new(app.graph.clone());
    // The first render, so every cell holds a value. Without it the refresh below would walk
    // into cells that had never computed and the counts would describe a cold start rather
    // than a refresh.
    let first = session.refresh();

    let previous: BTreeMap<_, _> = BTreeMap::new();
    let outcome = if force {
        RefreshOutcome::Force
    } else {
        RefreshOutcome::IfChanged
    };
    let refreshed = run_refresh(&app, &mut session, &previous, outcome, source_builder);

    Report {
        title: app.title.clone(),
        first_visited: first.visited(),
        refreshed,
    }
}

#[cfg(feature = "arrow-sources")]
fn source_builder() -> Box<dyn dagpane_core::frame::FrameBuilder> {
    Box::new(dagpane_frame_arrow::ArrowFrameBuilder::new())
}

#[cfg(not(feature = "arrow-sources"))]
fn source_builder() -> Box<dyn dagpane_core::frame::FrameBuilder> {
    Box::new(dagpane_core::frame::TableBuilder::new())
}

/// What the command found out.
#[derive(Debug)]
pub struct Report {
    title: String,
    first_visited: usize,
    refreshed: Refresh,
}

impl Report {
    /// Non-zero when a source could not be read. The refresh still applied the ones that
    /// could, so this is "something needs looking at" and never "nothing happened".
    pub fn exit_code(&self) -> u8 {
        u8::from(!self.refreshed.complete())
    }

    /// For a CI gate that wants to assert on the counts rather than grep the prose.
    pub fn json(&self) -> serde_json::Value {
        let trace = &self.refreshed.trace;
        json!({
            "title": self.title,
            "first_render": { "visited": self.first_visited },
            "sources": self.refreshed.sources.iter().map(|(name, outcome)| {
                let mut row = json!({ "name": name, "outcome": kind(outcome) });
                if let SourceRefresh::Reloaded { rows, .. } = outcome {
                    row["rows"] = json!(rows);
                }
                if let SourceRefresh::Failed { error } = outcome {
                    row["error"] = json!(error.to_string());
                    row["retryable"] = json!(error.is_retryable());
                }
                row
            }).collect::<Vec<_>>(),
            "refresh": {
                "total_cells": trace.total_cells,
                "roots": trace.roots,
                "visited": trace.visited(),
                "evaluated": trace.steps.iter()
                    .filter(|s| matches!(s.outcome, StepOutcome::Evaluated { .. })).count(),
                // `changed: false` is the value-equality short-circuit: the cell ran and
                // produced what it already held, so nothing below it woke. Counted
                // separately because it is the number that says an edge is over-declared.
                "unchanged_evaluations": trace.steps.iter()
                    .filter(|s| matches!(s.outcome, StepOutcome::Evaluated { changed: false })).count(),
                "reused": trace.steps.iter()
                    .filter(|s| s.outcome == StepOutcome::Reused).count(),
            },
            "complete": self.refreshed.complete(),
        })
    }

    /// The human form.
    pub fn render(&self) -> String {
        let mut out = format!("{}\n\n", self.title);

        if self.refreshed.sources.is_empty() {
            out.push_str("  no [[source]] entries — nothing to re-read\n");
            return out;
        }

        let width = self
            .refreshed
            .sources
            .iter()
            .map(|(name, _)| name.len())
            .max()
            .unwrap_or(0);
        for (name, outcome) in &self.refreshed.sources {
            out.push_str(&format!("  {name:<width$}  {outcome}\n"));
        }

        let trace = &self.refreshed.trace;
        out.push('\n');
        if trace.roots.is_empty() {
            // The sentence the whole command exists to be able to print. A source that was
            // read and produced identical bytes lands here too — the engine compared the
            // digests and declined the value — which is why this says "no cell moved" rather
            // than "nothing was read".
            out.push_str(&format!(
                "  no cell moved: {} of {} cells visited\n",
                trace.visited(),
                trace.total_cells
            ));
        } else {
            let evaluated = trace
                .steps
                .iter()
                .filter(|s| matches!(s.outcome, StepOutcome::Evaluated { .. }))
                .count();
            let reused = trace
                .steps
                .iter()
                .filter(|s| s.outcome == StepOutcome::Reused)
                .count();
            out.push_str(&format!(
                "  {} changed: {} of {} cells visited, {evaluated} evaluated, {reused} reused\n",
                trace.roots.join(", "),
                trace.visited(),
                trace.total_cells,
            ));
        }

        for (name, error) in self.refreshed.failed() {
            out.push_str(&format!(
                "\n  {name}: {error}\n    {}\n",
                if error.is_retryable() {
                    "this may succeed later; the pane keeps the value it had"
                } else {
                    "retrying will not fix this"
                }
            ));
        }
        out
    }
}

fn kind(outcome: &SourceRefresh) -> &'static str {
    match outcome {
        SourceRefresh::Unchanged { .. } => "unchanged",
        SourceRefresh::Reloaded { .. } => "reloaded",
        SourceRefresh::Failed { .. } => "failed",
    }
}

/// Re-read on an interval until the process is interrupted.
///
/// A plain `std::thread::sleep` loop and no runtime: this command has one thing to do at a
/// time and the whole point of it is to be readable. One session is kept across every pass,
/// which is what makes the counts mean anything — a fresh session per tick would recompute
/// the whole app each time and the trace would say so.
///
/// # Errors
///
/// Never returns `Ok`: it runs until interrupted. The signature matches the other commands'
/// so the caller does not need a special case.
pub fn watch(app: Arc<App>, every: Duration, json: bool) -> Result<(), String> {
    let mut session = Session::new(app.graph.clone());
    session.refresh();

    // Seeded from the app's own load, so the first pass compares against what compiling it
    // actually read rather than against nothing.
    let mut previous: BTreeMap<String, dagpane_app::Version> = app
        .sources
        .iter()
        .map(|b| (b.cell.clone(), b.loaded_version))
        .collect();

    if !json {
        println!(
            "{}: watching {} source(s) every {}s — ^C to stop",
            app.title,
            app.sources.len(),
            every.as_secs()
        );
    }

    loop {
        std::thread::sleep(every);
        let refreshed = run_refresh(
            &app,
            &mut session,
            &previous,
            RefreshOutcome::IfChanged,
            source_builder,
        );
        previous = refreshed.versions.clone();

        if json {
            let report = Report {
                title: app.title.clone(),
                first_visited: 0,
                refreshed,
            };
            // One object per line rather than an array: a watch has no end, so a consumer
            // has to be able to read a pass without waiting for the last one.
            println!("{}", report.json());
            continue;
        }

        // Quiet when nothing moved. A watcher that prints a line per tick is a watcher
        // somebody redirects to /dev/null, and then the line that mattered goes there too.
        for (name, error) in refreshed.failed() {
            println!("  {name}: {error}");
        }
        if !refreshed.trace.roots.is_empty() {
            let trace = &refreshed.trace;
            let rows: Vec<String> = refreshed
                .sources
                .iter()
                .filter(|(_, o)| o.reloaded())
                .map(|(name, o)| format!("{name} {o}"))
                .collect();
            println!(
                "  {} — {} of {} cells visited, {} evaluated",
                rows.join("; "),
                trace.visited(),
                trace.total_cells,
                trace
                    .steps
                    .iter()
                    .filter(|s| matches!(s.outcome, StepOutcome::Evaluated { .. }))
                    .count(),
            );
        }
    }
}
