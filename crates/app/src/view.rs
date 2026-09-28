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
    /// A pane this crate does not know how to draw, handed to a renderer the **client**
    /// supplies.
    ///
    /// The four variants above are closed: adding a treemap meant editing this enum, [`View`],
    /// [`render`], the manifest compiler and the client's own `switch` — five places in two
    /// crates, and a rebuilt server binary. That is the right cost for a vocabulary the
    /// runtime has to understand, and the wrong cost for a drawing.
    ///
    /// This variant is the seam. The engine does what it is actually good at — decide which
    /// rows exist, decide whether anything moved, and put the answer on the wire — and hands
    /// the drawing to a named function the page registered. A new visualisation costs **one
    /// JavaScript function and one manifest stanza**, and no Rust at all.
    ///
    /// What this crate still guarantees: the renderer's data is typed and schema-carrying,
    /// truncation is reported rather than hidden, and a pane whose *view* did not change is
    /// still absent from the patch. A custom renderer cannot make the product claim untrue,
    /// because it never touches the path that decides what goes on the wire.
    Custom {
        /// The renderer's name, as the client registered it. Not resolved here: this crate
        /// has no opinion about what draws a `"sankey"`, and a server that refused to start
        /// because a browser had not loaded a script yet would be checking the wrong thing in
        /// the wrong process.
        renderer: String,
        /// Which columns to send, in this order. `None` sends every column.
        ///
        /// A **wire** economy, not a compute one. The cell still computes every column it
        /// was written to compute; this decides how much of the answer crosses the socket. To
        /// narrow the *computation*, put a `select` in the cell — that is what sub-node
        /// invalidation reads.
        #[serde(default)]
        columns: Option<Vec<String>>,
        /// How many rows to send, with the true height carried beside them exactly as a
        /// [`PaneKind::Table`] carries it.
        ///
        /// The default is higher than a table's because the shapes differ in what they are
        /// for: fifty rows is a page of a table and a fiftieth of a sparkline.
        #[serde(default = "default_custom_max_rows")]
        max_rows: usize,
        /// Whatever the renderer wants, passed through from the manifest untouched.
        ///
        /// **Opaque JSON, deliberately.** Not [`Value`]: that is the engine's type, it is
        /// internally tagged for the wire, and a renderer's colour scale is not a cell's
        /// value. Typing this would mean every new option a drawing wants is a change to
        /// this enum — which is the cost this variant exists to remove.
        ///
        /// Carried on the **pane**, which the client is sent once, and never on the
        /// [`View`], which it is sent on every change. A renderer's configuration is static;
        /// putting it in the view would grow every patch by a constant for no reason, and
        /// this crate's whole argument is about what a patch weighs.
        #[serde(default)]
        options: std::collections::BTreeMap<String, serde_json::Value>,
    },
}

fn default_max_rows() -> usize {
    50
}

fn default_custom_max_rows() -> usize {
    500
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
    /// Data for a renderer this crate does not know about. See [`PaneKind::Custom`].
    ///
    /// The renderer name travels with the data rather than being looked up from the pane id,
    /// so a client can dispatch on the message alone — and so a pane that changed renderer
    /// between two deploys cannot be drawn by the old one.
    Custom {
        /// Which registered renderer draws this.
        renderer: String,
        /// The cell's value, in whichever shape it has.
        #[serde(flatten)]
        data: CustomData,
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

/// What a custom renderer is handed, in the shape the cell's value actually has.
///
/// Two shapes rather than one, because a renderer for a gauge and a renderer for a treemap
/// want different things and neither should have to unwrap the other's. A scalar wrapped in a
/// one-by-one table would be a lie told for the sake of uniformity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "data", rename_all = "snake_case")]
pub enum CustomData {
    /// The cell holds a frame: a schema, the rows sent, and the true height.
    Table {
        /// One header per column sent, in the order they were sent.
        head: Vec<Head>,
        /// The rows sent, each in the same column order. At most the pane's `max_rows`.
        rows: Vec<Vec<Value>>,
        /// The frame's real height, whatever was sent.
        total_rows: usize,
    },
    /// The cell holds a scalar — a number, a string, a null.
    Scalar {
        /// The value, unformatted. A custom renderer is assumed to want the number rather
        /// than this crate's rendering of it; [`format_scalar`] is available to a client that
        /// wants the same text a [`View::Text`] would have carried.
        value: Value,
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
        PaneKind::Custom {
            renderer,
            columns,
            max_rows,
            options: _,
        } => match custom_view(renderer, value, columns.as_deref(), *max_rows) {
            Ok(v) => v,
            Err(e) => error_view(pane, e),
        },
    }
}

/// A custom pane's data: a projection of a frame, or the scalar as it stands.
///
/// A named column that the frame does not have is an **error**, not an omission. A chart that
/// silently drew three of its four series because somebody renamed a column is the failure
/// this whole project is arranged to make impossible, and a renderer has no way to notice.
fn custom_view(
    renderer: &str,
    value: &Value,
    columns: Option<&[String]>,
    max_rows: usize,
) -> Result<View, CellError> {
    let Some(t) = value.as_frame() else {
        return Ok(View::Custom {
            renderer: renderer.to_string(),
            data: CustomData::Scalar {
                value: value.clone(),
            },
        });
    };

    let picked: Vec<usize> = match columns {
        Some(names) => names
            .iter()
            .map(|n| {
                t.column_index(n)
                    .ok_or_else(|| CellError::failed(format!("no column `{n}` to send")))
            })
            .collect::<Result<_, _>>()?,
        None => (0..t.width()).collect(),
    };

    let schema = t.schema();
    let shown = t.head(max_rows);
    Ok(View::Custom {
        renderer: renderer.to_string(),
        data: CustomData::Table {
            head: picked
                .iter()
                .map(|c| {
                    let (name, column_type) = schema[*c].clone();
                    Head { name, column_type }
                })
                .collect(),
            rows: (0..shown.rows())
                .map(|r| picked.iter().map(|c| shown.value_at(r, *c)).collect())
                .collect(),
            total_rows: t.rows(),
        },
    })
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

    fn custom(columns: Option<Vec<String>>, max_rows: usize) -> Pane {
        pane(PaneKind::Custom {
            renderer: "sankey".into(),
            columns,
            max_rows,
            options: Default::default(),
        })
    }

    #[test]
    fn a_custom_pane_sends_a_schema_and_the_true_height() {
        let big = Table::new(vec![Column::int("n", (0..100).map(Some).collect())]).unwrap();
        let View::Custom {
            renderer,
            data:
                CustomData::Table {
                    head,
                    rows,
                    total_rows,
                },
        } = render(&custom(None, 10), &Outcome::ok(Value::table(big)))
        else {
            panic!("expected a custom table view")
        };
        assert_eq!(renderer, "sankey");
        assert_eq!(head.len(), 1);
        assert_eq!(rows.len(), 10);
        assert_eq!(
            total_rows, 100,
            "a custom pane tells the truth about truncation too"
        );
    }

    #[test]
    fn a_custom_pane_sends_the_columns_it_named_in_the_order_it_named_them() {
        let View::Custom {
            data: CustomData::Table { head, rows, .. },
            ..
        } = render(
            &custom(Some(vec!["v".into(), "k".into()]), 50),
            &Outcome::ok(Value::table(t())),
        )
        else {
            panic!("expected a custom table view")
        };
        assert_eq!(
            head.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
            vec!["v", "k"],
            "the projection is ordered by the manifest, not by the frame"
        );
        assert_eq!(rows[0], vec![Value::float(1.5), Value::text("a")]);
    }

    #[test]
    fn a_custom_pane_naming_a_column_that_is_gone_is_an_error_not_an_omission() {
        // The failure this rules out is silent: a renderer handed three of its four series
        // draws a chart that looks fine and is wrong, and has no way to notice.
        let v = render(
            &custom(Some(vec!["k".into(), "renamed".into()]), 50),
            &Outcome::ok(Value::table(t())),
        );
        let View::Error { message, .. } = v else {
            panic!("expected an error view, got {v:?}")
        };
        assert!(message.contains("`renamed`"), "{message}");
    }

    #[test]
    fn a_custom_pane_over_a_scalar_hands_the_value_over_unformatted() {
        let v = render(&custom(None, 50), &Outcome::ok(Value::float(0.82)));
        assert_eq!(
            v,
            View::Custom {
                renderer: "sankey".into(),
                data: CustomData::Scalar {
                    value: Value::float(0.82)
                }
            },
            "a gauge wants the number, not this crate's rendering of it"
        );
    }

    #[test]
    fn a_custom_view_round_trips_through_the_wire_encoding() {
        // `CustomData` is flattened into `View`, and a flattened internally-tagged enum is
        // exactly the serde shape that silently stops round-tripping. Nothing else would
        // catch it: the server would send bytes the client could not read back.
        for v in [
            render(&custom(None, 50), &Outcome::ok(Value::table(t()))),
            render(&custom(None, 50), &Outcome::ok(Value::int(3))),
        ] {
            let json = serde_json::to_string(&v).unwrap();
            assert!(json.contains("\"view\":\"custom\""), "{json}");
            assert!(json.contains("\"renderer\":\"sankey\""), "{json}");
            assert_eq!(
                serde_json::from_str::<View>(&json).unwrap(),
                v,
                "custom views must survive the trip they exist to make: {json}"
            );
        }
    }

    #[test]
    fn a_custom_panes_options_never_reach_the_view() {
        // Options are static and the view is sent on every change. Putting them in the view
        // would add a constant to every patch, which is the one thing this crate argues about.
        let mut options = std::collections::BTreeMap::new();
        options.insert("unit".to_string(), serde_json::json!("USD"));
        let p = pane(PaneKind::Custom {
            renderer: "sankey".into(),
            columns: None,
            max_rows: 50,
            options,
        });
        let json = serde_json::to_string(&render(&p, &Outcome::ok(Value::table(t())))).unwrap();
        assert!(!json.contains("USD"), "{json}");
        // …and they do reach the pane, which is sent once.
        assert!(serde_json::to_string(&p).unwrap().contains("USD"));
    }

    #[test]
    fn a_whole_float_prints_without_a_trailing_point() {
        assert_eq!(format_scalar(&Value::float(3.0)), "3");
        assert_eq!(format_scalar(&Value::float(3.25)), "3.25");
    }
}
