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
use dagpane_core::frame::{column_digest, Frame};
use dagpane_core::value::{Column, ColumnType, Table};
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
    cause: Cause,
    trace: Trace,
    patch: Vec<PaneUpdate>,
    untouched: Vec<String>,
}

/// What this interaction was.
///
/// The two are kept apart, and the command refuses to do both at once, because the whole
/// value of the output is that it attributes a cost to one cause. "I moved a slider and
/// rewrote a column and eleven cells ran" answers nothing.
#[derive(Debug)]
enum Cause {
    /// Controls were set — what a viewer does.
    Set(BTreeMap<String, Value>),
    /// One column of one source frame was rewritten — what a refresh of a wide table
    /// usually is.
    Column {
        cell: String,
        column: String,
        /// How wide the frame is. The denominator of the sub-node claim.
        width: usize,
        /// Controls set beforehand, in their own pass, to put the app in the state the
        /// measurement is about. Empty when the app was measured at its defaults.
        settled: BTreeMap<String, Value>,
    },
}

/// One column of a frame, rewritten; every other column keeps its bytes.
///
/// The edit is synthetic and deliberately small — each value nudged, nulls left null — so
/// that what is being measured is *a column moved*, with the frame's shape, its row count
/// and every other column held fixed. Anything larger would measure something else.
fn with_column_changed(frame: &dyn Frame, col: usize) -> Value {
    let schema = frame.schema();
    let columns: Vec<Column> = schema
        .iter()
        .enumerate()
        .map(|(c, (name, ty))| {
            let nudge = c == col;
            match ty {
                ColumnType::Int => Column::int(
                    name.clone(),
                    (0..frame.rows())
                        .map(|r| {
                            frame.value_at(r, c).as_int().map(|v| {
                                if nudge {
                                    v.wrapping_add(1)
                                } else {
                                    v
                                }
                            })
                        })
                        .collect(),
                ),
                ColumnType::Float => Column::float(
                    name.clone(),
                    (0..frame.rows())
                        .map(|r| {
                            frame
                                .value_at(r, c)
                                .as_float()
                                .map(|v| if nudge { v + 1.0 } else { v })
                        })
                        .collect(),
                ),
                ColumnType::Bool => Column::bool(
                    name.clone(),
                    (0..frame.rows())
                        .map(|r| {
                            frame
                                .value_at(r, c)
                                .as_bool()
                                .map(|v| if nudge { !v } else { v })
                        })
                        .collect(),
                ),
                ColumnType::Text => Column::text(
                    name.clone(),
                    (0..frame.rows())
                        .map(|r| match frame.value_at(r, c) {
                            Value::Text { v } => Some(if nudge { format!("{v}*") } else { v }),
                            _ => None,
                        })
                        .collect(),
                ),
            }
        })
        .collect();
    Value::table(Table::new(columns).expect("one row count throughout"))
}

/// Apply `CELL.COLUMN`, returning what was changed.
fn change_one_column(
    app: &Arc<App>,
    session: &mut AppSession,
    spec: &str,
) -> Result<Cause, String> {
    let (cell, column) = spec
        .rsplit_once('.')
        .ok_or_else(|| format!("`{spec}` is not `CELL.COLUMN`"))?;
    let id = app
        .graph
        .id(cell)
        .ok_or_else(|| format!("no cell `{cell}`; `dagpane graph` lists them"))?;
    let outcome = session.session().get_id(id).clone();
    let value = outcome
        .value()
        .ok_or_else(|| format!("`{cell}` is in error, so there is no column to change"))?;
    let frame = value
        .as_frame()
        .ok_or_else(|| format!("`{cell}` does not hold a table"))?;
    let at = frame.column_index(column).ok_or_else(|| {
        format!(
            "`{cell}` has no column `{column}`. It has: {}",
            frame.column_names().join(", ")
        )
    })?;

    let before = column_digest(frame, at);
    let next = with_column_changed(frame, at);
    let after = {
        let f = next.as_frame().expect("just built a frame");
        column_digest(f, at)
    };
    if before == after {
        return Err(format!(
            "nudging `{column}` of `{cell}` did not change it — every value in it is null, \
             so there is nothing here to measure"
        ));
    }
    let width = frame.width();
    session.set_source(cell, next)?;
    Ok(Cause::Column {
        cell: cell.to_string(),
        column: column.to_string(),
        width,
        settled: BTreeMap::new(),
    })
}

pub fn run(app: Arc<App>, set: &[String], change_column: Option<&str>) -> Result<Report, String> {
    let (mut session, first) = AppSession::open(Arc::clone(&app));
    session.full_views();

    let mut report = Report {
        title: app.title.clone(),
        total_panes: app.panes.len(),
        first,
        interaction: None,
    };

    if set.is_empty() && change_column.is_none() {
        return Ok(report);
    }

    let mut values = BTreeMap::new();
    for arg in set {
        let (name, value) = parse_set(arg)?;
        values.insert(name, value);
    }

    let cause = match change_column {
        // Both given: the controls **settle** the app in its own pass, and the column change
        // is the one measured afterwards. That is not two causes in one interaction — it is a
        // starting state and then the thing being measured, which is the only way to ask what
        // a data change costs at a threshold other than the manifest's default.
        Some(spec) => {
            if !values.is_empty() {
                session.set(&values)?;
                session.commit();
            }
            let settled = values;
            let Cause::Column {
                cell,
                column,
                width,
                ..
            } = change_one_column(&app, &mut session, spec)?
            else {
                unreachable!("change_one_column returns a Column cause")
            };
            Cause::Column {
                cell,
                column,
                width,
                settled,
            }
        }
        None => {
            session.set(&values)?;
            Cause::Set(values)
        }
    };
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
        cause,
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

        match &i.cause {
            Cause::Set(values) => {
                let assignments: Vec<String> = values
                    .iter()
                    .map(|(k, v)| format!("{k} = {}", format_scalar(v)))
                    .collect();
                out.push_str(&format!("\nset {}\n", assignments.join(", ")));
            }
            Cause::Column {
                cell,
                column,
                width,
                settled,
            } => {
                if !settled.is_empty() {
                    let assignments: Vec<String> = settled
                        .iter()
                        .map(|(k, v)| format!("{k} = {}", format_scalar(v)))
                        .collect();
                    out.push_str(&format!(
                        "\nsettled first, in its own pass: {}\n",
                        assignments.join(", ")
                    ));
                }
                out.push_str(&format!(
                    "\nchanged column `{column}` of `{cell}` — 1 of {width} columns \
                     (a synthetic edit: every value in it nudged, the frame's shape held)\n"
                ));
            }
        }

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

        // The sub-node claim, in the one form that is checkable: a column, a width, and a
        // count of cells that had to run because of it.
        if let Cause::Column { width, .. } = &i.cause {
            out.push_str(&format!(
                "  changed 1 column of a {width}-column frame; recomputed {} of {} cells\n",
                i.trace.evaluated(),
                i.trace.total_cells
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
            let (set, changed_column) = match &i.cause {
                Cause::Set(v) => (Some(v.clone()), None),
                Cause::Column {
                    cell,
                    column,
                    width,
                    settled,
                } => (
                    (!settled.is_empty()).then(|| settled.clone()),
                    Some(json!({ "cell": cell, "column": column, "of_columns": width })),
                ),
            };
            json!({
                "set": set,
                "changed_column": changed_column,
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
