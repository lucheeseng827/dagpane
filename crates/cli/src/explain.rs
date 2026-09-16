//! `dagpane explain` — one interaction, and exactly what it cost.
//!
//! Everything this command prints is read out of a [`Trace`] and a patch. There is no
//! instrumentation here and no second code path: the numbers below are the ones the server
//! sends to a browser and the ones `crates/core/tests/oracle.rs` checks against a full
//! recompute. That is the point — a claim about counts belongs somewhere a person can read
//! it without opening a network tab.

use std::collections::BTreeMap;
use std::sync::Arc;

use dagpane_app::view::format_scalar;
use dagpane_app::{App, AppSession, PaneUpdate};
use dagpane_core::{StepOutcome, Trace, Value};
use serde_json::json;

/// One `--set name=value`.
///
/// The value's type is inferred, in this order: integer, float, boolean, then text. That
/// matters for a select whose options are `"1"` and `"2"` — quoting is how an author says
/// text, and `--set region="1"` reaches this function with the quotes already eaten by the
/// shell, so `--set` on such an app is ambiguous and documented as such.
fn parse_set(arg: &str) -> Result<(String, Value), String> {
    let (name, raw) = arg
        .split_once('=')
        .ok_or_else(|| format!("`{arg}` is not `name=value`"))?;
    let value = if let Ok(i) = raw.parse::<i64>() {
        Value::int(i)
    } else if let Ok(f) = raw.parse::<f64>() {
        Value::float(f)
    } else if raw.eq_ignore_ascii_case("true") || raw.eq_ignore_ascii_case("false") {
        Value::bool(raw.eq_ignore_ascii_case("true"))
    } else {
        Value::text(raw)
    };
    Ok((name.to_string(), value))
}

/// What the command found out.
#[derive(Debug)]
pub struct Report {
    title: String,
    total_panes: usize,
    first: Trace,
    interaction: Option<Interaction>,
}

#[derive(Debug)]
struct Interaction {
    set: BTreeMap<String, Value>,
    trace: Trace,
    patch: Vec<PaneUpdate>,
    untouched: Vec<String>,
}

pub fn run(app: Arc<App>, set: &[String]) -> Result<Report, String> {
    let (mut session, first) = AppSession::open(Arc::clone(&app));
    session.full_views();

    let mut report = Report {
        title: app.title.clone(),
        total_panes: app.panes.len(),
        first,
        interaction: None,
    };

    if set.is_empty() {
        return Ok(report);
    }

    let mut values = BTreeMap::new();
    for arg in set {
        let (name, value) = parse_set(arg)?;
        values.insert(name, value);
    }
    session.set(&values)?;
    let (trace, patch) = session.commit();

    // Named, not counted. "three cells were never looked at" is a statistic; "`sales`,
    // `all_time_revenue` and `region` were never looked at" is a thing a reader can check
    // against the manifest they just wrote.
    let touched: Vec<&str> = trace.steps.iter().map(|s| s.cell.as_str()).collect();
    let untouched: Vec<String> = app
        .graph
        .order()
        .iter()
        .map(|&id| app.graph.name(id).to_string())
        .filter(|n| !touched.contains(&n.as_str()))
        .collect();

    report.interaction = Some(Interaction {
        set: values,
        trace,
        patch,
        untouched,
    });
    Ok(report)
}

impl Report {
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "dagpane: {} — {} cells, {} panes\n",
            self.title, self.first.total_cells, self.total_panes
        ));
        out.push_str(&format!(
            "first render: {} of {} cells evaluated\n",
            self.first.evaluated(),
            self.first.total_cells
        ));

        let Some(i) = &self.interaction else {
            out.push_str(
                "\nnothing was set, so nothing recomputed. Pass --set NAME=VALUE to see an\n\
                 interaction; `dagpane graph` lists the names.\n",
            );
            return out;
        };

        let assignments: Vec<String> = i
            .set
            .iter()
            .map(|(k, v)| format!("{k} = {}", format_scalar(v)))
            .collect();
        out.push_str(&format!("\nset {}\n", assignments.join(", ")));

        if i.trace.roots.is_empty() {
            out.push_str(
                "  nothing changed — every value set was the one already held, so no cell ran.\n",
            );
            return out;
        }

        out.push_str(&format!(
            "  epoch {} — looked at {} of {} cells\n",
            i.trace.epoch,
            i.trace.visited(),
            i.trace.total_cells
        ));
        for step in &i.trace.steps {
            let (verb, note) = match &step.outcome {
                StepOutcome::Set => ("set", String::new()),
                StepOutcome::Evaluated { changed: true } => ("ran", "changed".to_string()),
                StepOutcome::Evaluated { changed: false } => {
                    ("ran", "same value — nothing below it ran".to_string())
                }
                StepOutcome::Reused => ("reused", "its inputs had not moved".to_string()),
                StepOutcome::Failed { message } => ("failed", message.clone()),
            };
            out.push_str(format!("    {verb:<8} {:<20} {note}", step.cell).trim_end());
            out.push('\n');
        }

        if !i.untouched.is_empty() {
            out.push_str(&format!(
                "  {} cell(s) never looked at: {}\n",
                i.untouched.len(),
                i.untouched.join(", ")
            ));
        }

        let panes: Vec<&str> = i.patch.iter().map(|p| p.id.as_str()).collect();
        if panes.is_empty() {
            out.push_str(&format!(
                "  patch: 0 of {} panes — nothing to send\n",
                self.total_panes
            ));
        } else {
            out.push_str(&format!(
                "  patch: {} of {} panes — {}\n",
                panes.len(),
                self.total_panes,
                panes.join(", ")
            ));
        }
        out
    }
}

impl serde::Serialize for Report {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let interaction = self.interaction.as_ref().map(|i| {
            json!({
                "set": i.set,
                "trace": i.trace,
                "untouched": i.untouched,
                "panes_sent": i.patch.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
                "panes_total": self.total_panes,
            })
        });
        json!({
            "title": self.title,
            "first_render": self.first,
            "interaction": interaction,
        })
        .serialize(s)
    }
}
