//! What a cell looks like on a page, and how a value becomes one.
//!
//! A [`Pane`] is static: it comes from the app definition and never changes. A [`View`] is
//! what a pane currently shows, and it is derived from one cell's value. Keeping the two
//! apart is what makes a patch small — the client is told the panes once and the changed
//! views many times.

use std::fmt::Write as _;

use dagpane_core::frame::Frame;
use dagpane_core::{CellError, ColumnType, Outcome, Value};
use serde::{Deserialize, Serialize};

/// How a pane presents its cell.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "pane", rename_all = "snake_case")]
pub enum PaneKind {
    /// The value, formatted.
    Text,
    /// A headline number with a label under it.
    Metric {
        /// The caption under the number.
        label: String,
        /// Decimal places for a float. `None` prints the number as it is.
        #[serde(default)]
        decimals: Option<usize>,
        /// Written before the number — a currency symbol, typically.
        #[serde(default)]
        prefix: String,
        /// Written after it — a unit or a `%`.
        #[serde(default)]
        suffix: String,
    },
    /// A table, truncated to `max_rows` with the true row count kept beside it — a user is
    /// always told what they are not being shown.
    Table {
        /// How many rows to send. The cap is on the *wire*, not on the cell: the table is
        /// computed in full and the view carries the true height beside the rows it sends.
        #[serde(default = "default_max_rows")]
        max_rows: usize,
    },
    /// One bar per row.
    Bar {
        /// The column holding each bar's label.
        label_column: String,
        /// The column holding each bar's height. Must be numeric.
        value_column: String,
    },
    /// One point per row, in row order.
    Line {
        /// The column holding each point's x. Must be numeric — a categorical axis is a
        /// [`PaneKind::Bar`].
        x_column: String,
        /// The column holding each point's y. Must be numeric.
        y_column: String,
    },
}

fn default_max_rows() -> usize {
    50
}

/// A pane of the app: a cell, and how to show it. Static for the life of the process.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pane {
    /// Stable identity on the wire. Defaults to the cell name; distinct only when one cell
    /// is shown twice.
    pub id: String,
    /// The cell this pane shows. Resolved by name at render time, so two panes naming one
    /// cell each get their own view of it.
    pub cell: String,
    /// A heading above the pane, or none.
    #[serde(default)]
    pub title: Option<String>,
    /// How to present the cell's value.
    #[serde(flatten)]
    pub kind: PaneKind,
}

/// A column header, so a client can right-align numbers without inspecting every row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Head {
    /// The column's name, as the table carries it.
    pub name: String,
    /// Its element type. Sent so the client can align and format a whole column without
    /// looking at the rows — which it could not do anyway for a column that is all nulls.
    #[serde(rename = "type")]
    pub column_type: ColumnType,
}

/// What a pane currently shows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "view", rename_all = "snake_case")]
pub enum View {
    /// The value, already formatted for display.
    Text {
        /// The rendered text. Formatting happens here rather than in the client so that
        /// `dagpane explain` and a browser show a number the same way.
        text: String,
    },
    /// A headline number.
    Metric {
        /// The caption, copied from the pane.
        label: String,
        /// The number, already formatted with the pane's decimals, prefix and suffix.
        value: String,
    },
    /// A table, possibly truncated.
    Table {
        /// One header per column, in column order.
        head: Vec<Head>,
        /// The rows sent, each in column order. At most the pane's `max_rows`.
        rows: Vec<Vec<Value>>,
        /// The table's real height. `rows.len()` is what was sent.
        total_rows: usize,
    },
    /// A bar chart, one bar per row.
    Bar {
        /// Each bar's label, in row order.
        labels: Vec<String>,
        /// Each bar's height, in the same order and the same length as `labels`.
        values: Vec<f64>,
        /// The value column's name, for the axis.
        value_label: String,
    },
    /// A line chart, one point per row.
    Line {
        /// The x coordinates, in row order.
        x: Vec<f64>,
        /// The y coordinates, same order and same length as `x`.
        y: Vec<f64>,
        /// The x column's name, for the axis.
        x_label: String,
        /// The y column's name, for the axis.
        y_label: String,
    },
    /// The cell is in error. A pane in error is still a pane: it keeps its place on the
    /// page and says what went wrong, rather than disappearing and taking the layout with it.
    Error {
        /// What went wrong, as the failing cell put it.
        message: String,
        /// The cell the failure originated in — not necessarily this pane's cell, which is
        /// the point: a user is pointed at the cause rather than at the messenger.
        cause: String,
    },
}

/// Render one cell's current outcome through one pane.
pub fn render(pane: &Pane, outcome: &Outcome) -> View {
    let value = match outcome {
        Outcome::Error { error } => {
            return View::Error {
                message: error.message().to_string(),
                cause: error.cause(&pane.cell).to_string(),
            }
        }
        Outcome::Value { value } => value,
    };

    match &pane.kind {
        PaneKind::Text => View::Text {
            text: format_scalar(value),
        },
        PaneKind::Metric {
            label,
            decimals,
            prefix,
            suffix,
        } => {
            let body = match (value.as_float(), decimals) {
                (Some(n), Some(d)) => format!("{n:.*}", *d),
                _ => format_scalar(value),
            };
            View::Metric {
                label: label.clone(),
                value: format!("{prefix}{body}{suffix}"),
            }
        }
        PaneKind::Table { max_rows } => match value.as_frame() {
            Some(t) => table_view(t, *max_rows),
            None => wrong_shape(pane, "a table", value),
        },
        PaneKind::Bar {
            label_column,
            value_column,
        } => match value.as_frame() {
            Some(t) => match bar_view(t, label_column, value_column) {
                Ok(v) => v,
                Err(e) => error_view(pane, e),
            },
            None => wrong_shape(pane, "a table", value),
        },
        PaneKind::Line { x_column, y_column } => match value.as_frame() {
            Some(t) => match line_view(t, x_column, y_column) {
                Ok(v) => v,
                Err(e) => error_view(pane, e),
            },
            None => wrong_shape(pane, "a table", value),
        },
    }
}

fn error_view(pane: &Pane, e: CellError) -> View {
    View::Error {
        message: e.message().to_string(),
        cause: e.cause(&pane.cell).to_string(),
    }
}

fn wrong_shape(pane: &Pane, expected: &str, got: &Value) -> View {
    View::Error {
        message: format!(
            "pane `{}` needs {expected}; cell `{}` produced {}",
            pane.id,
            pane.cell,
            got.type_name()
        ),
        cause: pane.cell.clone(),
    }
}

/// Scalars are formatted here rather than in the browser so that the same number reads the
/// same way in `dagpane explain`, in a JSON dump and on the page.
pub fn format_scalar(v: &Value) -> String {
    match v {
        Value::Null => "—".to_string(),
        Value::Bool { v } => (if *v { "yes" } else { "no" }).to_string(),
        Value::Int { v } => v.to_string(),
        Value::Float { v } => {
            if v.fract() == 0.0 && v.abs() < 1e15 {
                format!("{v:.0}")
            } else {
                format!("{v}")
            }
        }
        Value::Text { v } => v.clone(),
        Value::List { v } => {
            let mut s = String::new();
            for (i, item) in v.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                let _ = write!(s, "{}", format_scalar(item));
            }
            s
        }
        Value::Frame { v } => {
            let f = v.as_frame();
            format!("{} rows × {} columns", f.rows(), f.width())
        }
    }
}

fn table_view(t: &dyn Frame, max_rows: usize) -> View {
    let shown = t.head(max_rows);
    View::Table {
        head: t
            .schema()
            .into_iter()
            .map(|(name, column_type)| Head { name, column_type })
            .collect(),
        rows: (0..shown.rows()).map(|r| shown.row(r)).collect(),
        total_rows: t.rows(),
    }
}

fn numeric_column(t: &dyn Frame, name: &str) -> Result<Vec<f64>, CellError> {
    let c = t
        .column_index(name)
        .ok_or_else(|| CellError::failed(format!("no column `{name}` to plot")))?;
    let column_type = t.column_type(c).expect("an index from column_index");

    // A wrong type is wrong for the whole column; a null is wrong for one row. Reporting a
    // null as "column `x` is float and cannot be plotted" sent readers to look at a column
    // whose type was fine — and an empty CSV field becomes a null in a numeric column, so
    // this is the common case rather than the exotic one.
    if !matches!(column_type, ColumnType::Int | ColumnType::Float) {
        return Err(CellError::failed(format!(
            "column `{name}` is {column_type} and cannot be plotted"
        )));
    }

    (0..t.rows())
        .map(|r| {
            t.value_at(r, c).as_float().ok_or_else(|| {
                CellError::failed(format!(
                    "column `{name}` has no value in row {r} and cannot be plotted"
                ))
            })
        })
        .collect()
}

fn bar_view(t: &dyn Frame, label_column: &str, value_column: &str) -> Result<View, CellError> {
    let labels_col = t
        .column_index(label_column)
        .ok_or_else(|| CellError::failed(format!("no column `{label_column}` to label bars")))?;
    Ok(View::Bar {
        labels: (0..t.rows())
            .map(|r| format_scalar(&t.value_at(r, labels_col)))
            .collect(),
        values: numeric_column(t, value_column)?,
        value_label: value_column.to_string(),
    })
}

fn line_view(t: &dyn Frame, x_column: &str, y_column: &str) -> Result<View, CellError> {
    Ok(View::Line {
        x: numeric_column(t, x_column)?,
        y: numeric_column(t, y_column)?,
        x_label: x_column.to_string(),
        y_label: y_column.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    // `Table` is only a test fixture now — the module itself reads frames through the trait.
    use dagpane_core::{Column, Table};

    fn pane(kind: PaneKind) -> Pane {
        Pane {
            id: "p".into(),
            cell: "c".into(),
            title: None,
            kind,
        }
    }

    fn t() -> Table {
        Table::new(vec![
            Column::text("k", vec![Some("a".into()), Some("b".into())]),
            Column::float("v", vec![Some(1.5), Some(2.0)]),
        ])
        .unwrap()
    }

    #[test]
    fn a_table_pane_reports_the_rows_it_did_not_send() {
        let big = Table::new(vec![Column::int("n", (0..100).map(Some).collect())]).unwrap();
        let View::Table {
            rows, total_rows, ..
        } = render(
            &pane(PaneKind::Table { max_rows: 10 }),
            &Outcome::ok(Value::table(big)),
        )
        else {
            panic!("expected a table view")
        };
        assert_eq!(rows.len(), 10);
        assert_eq!(total_rows, 100, "the user is told what they are not seeing");
    }

    #[test]
    fn a_cell_in_error_renders_as_a_pane_not_as_a_hole() {
        let v = render(
            &pane(PaneKind::Text),
            &Outcome::err(CellError::failed("boom")),
        );
        assert_eq!(
            v,
            View::Error {
                message: "boom".into(),
                cause: "c".into()
            }
        );
    }

    #[test]
    fn a_metric_formats_and_decorates() {
        let v = render(
            &pane(PaneKind::Metric {
                label: "Revenue".into(),
                decimals: Some(2),
                prefix: "$".into(),
                suffix: String::new(),
            }),
            &Outcome::ok(Value::float(1234.5)),
        );
        assert_eq!(
            v,
            View::Metric {
                label: "Revenue".into(),
                value: "$1234.50".into()
            }
        );
    }

    #[test]
    fn a_pane_given_the_wrong_shape_says_so_and_names_both_ends() {
        let v = render(
            &pane(PaneKind::Table { max_rows: 10 }),
            &Outcome::ok(Value::int(3)),
        );
        let View::Error { message, .. } = v else {
            panic!("expected an error view")
        };
        assert!(
            message.contains("`p`") && message.contains("`c`"),
            "{message}"
        );
    }

    #[test]
    fn plotting_a_text_column_is_an_error_with_the_column_named() {
        let v = render(
            &pane(PaneKind::Bar {
                label_column: "k".into(),
                value_column: "k".into(),
            }),
            &Outcome::ok(Value::table(t())),
        );
        let View::Error { message, .. } = v else {
            panic!("expected an error view")
        };
        assert!(message.contains("`k`"), "{message}");
    }

    #[test]
    fn a_null_in_a_plotted_column_names_the_row_not_the_column_type() {
        let t = Table::new(vec![
            Column::text("k", vec![Some("a".into()), Some("b".into())]),
            Column::float("v", vec![Some(1.0), None]),
        ])
        .unwrap();
        for kind in [
            PaneKind::Bar {
                label_column: "k".into(),
                value_column: "v".into(),
            },
            PaneKind::Line {
                x_column: "v".into(),
                y_column: "v".into(),
            },
        ] {
            let View::Error { message, .. } =
                render(&pane(kind), &Outcome::ok(Value::table(t.clone())))
            else {
                panic!("a null must not be plotted")
            };
            assert!(message.contains("row 1"), "{message}");
            assert!(!message.contains("is float"), "{message}");
        }
    }

    #[test]
    fn a_bar_chart_takes_its_labels_from_any_column_type() {
        let v = render(
            &pane(PaneKind::Bar {
                label_column: "k".into(),
                value_column: "v".into(),
            }),
            &Outcome::ok(Value::table(t())),
        );
        assert_eq!(
            v,
            View::Bar {
                labels: vec!["a".into(), "b".into()],
                values: vec![1.5, 2.0],
                value_label: "v".into()
            }
        );
    }

    #[test]
    fn a_whole_float_prints_without_a_trailing_point() {
        assert_eq!(format_scalar(&Value::float(3.0)), "3");
        assert_eq!(format_scalar(&Value::float(3.25)), "3.25");
    }
}
