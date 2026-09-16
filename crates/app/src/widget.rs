//! The controls that drive an app's source cells.
//!
//! A widget is a *description*, not a component: the server sends it once, the client draws
//! whatever it draws, and what comes back is a value for a source cell. That indirection is
//! why the same app definition can be rendered by the bundled client, by a different
//! front-end, or by a test that never opens a browser.

use dagpane_core::Value;
use serde::{Deserialize, Serialize};

/// The kinds of control an app can declare, and the bounds each one promises.
///
/// The bounds are carried here rather than left to the client because they are checked on
/// the server by [`Widget::accepts`]. A browser is not trusted; these fields are what the
/// refusal is made of.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "widget", rename_all = "snake_case")]
pub enum WidgetKind {
    /// A bounded, stepped numeric control. Sets a `Float` cell.
    Slider {
        /// Lowest accepted value, inclusive.
        min: f64,
        /// Highest accepted value, inclusive.
        max: f64,
        /// The client's increment. Not enforced server-side — a value between two steps is
        /// still in range, and rejecting it would make the control's usability a correctness
        /// question.
        step: f64,
    },
    /// A free numeric entry, optionally bounded. Sets a `Float` cell.
    Number {
        /// Lowest accepted value, or unbounded below.
        min: Option<f64>,
        /// Highest accepted value, or unbounded above.
        max: Option<f64>,
    },
    /// A choice from a fixed list. Sets a `Text` cell.
    Select {
        /// The permitted values, in the order the client shows them. Anything else is
        /// refused, so a cell downstream can match on them exhaustively.
        options: Vec<String>,
    },
    /// A boolean toggle. Sets a `Bool` cell, and needs no bounds — both values are legal.
    Checkbox,
    /// Free text entry. Sets a `Text` cell.
    Text {
        /// Shown by the client when the field is empty. Presentation only: it is never a
        /// value, and an empty field sets an empty string.
        placeholder: String,
    },
}

/// One control, bound to one source cell by name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Widget {
    /// The source cell this control sets. Also its identity on the wire.
    pub cell: String,
    /// What the client shows beside the control. Presentation only — the cell name is the
    /// identity, so a label can be changed without breaking a client's state.
    pub label: String,
    /// The value the bound source cell starts at. Compiled into the graph as the source's
    /// initial value, so it is also what the cell holds before any client connects.
    pub default: Value,
    /// Which control this is, and its bounds.
    #[serde(flatten)]
    pub kind: WidgetKind,
}

impl Widget {
    /// Whether a value is one this control could have produced.
    ///
    /// The server checks it before touching the session. A client is not trusted — it is a
    /// browser — and a slider that reports 10^9 because someone edited the DOM should be
    /// refused at the edge with a message, not fed into somebody's compute function.
    pub fn accepts(&self, value: &Value) -> Result<(), String> {
        match (&self.kind, value) {
            (WidgetKind::Slider { min, max, .. }, v) => {
                let n = v.as_float().ok_or_else(|| {
                    format!(
                        "`{}` is a slider; {} is not a number",
                        self.cell,
                        v.type_name()
                    )
                })?;
                if n < *min || n > *max {
                    return Err(format!(
                        "`{}` is {n}, outside its range {min}..={max}",
                        self.cell
                    ));
                }
                Ok(())
            }
            (WidgetKind::Number { min, max }, v) => {
                let n = v.as_float().ok_or_else(|| {
                    format!("`{}` takes a number, got {}", self.cell, v.type_name())
                })?;
                if min.is_some_and(|m| n < m) || max.is_some_and(|m| n > m) {
                    return Err(format!("`{}` is {n}, outside its bounds", self.cell));
                }
                Ok(())
            }
            (WidgetKind::Select { options }, Value::Text { v }) => {
                if options.contains(v) {
                    Ok(())
                } else {
                    Err(format!("`{}` has no option `{v}`", self.cell))
                }
            }
            (WidgetKind::Select { .. }, v) => Err(format!(
                "`{}` is a selection; {} is not one of its options",
                self.cell,
                v.type_name()
            )),
            (WidgetKind::Checkbox, Value::Bool { .. }) => Ok(()),
            (WidgetKind::Checkbox, v) => Err(format!(
                "`{}` is a checkbox, got {}",
                self.cell,
                v.type_name()
            )),
            (WidgetKind::Text { .. }, Value::Text { .. }) => Ok(()),
            (WidgetKind::Text { .. }, v) => {
                Err(format!("`{}` takes text, got {}", self.cell, v.type_name()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slider() -> Widget {
        Widget {
            cell: "t".into(),
            label: "Threshold".into(),
            default: Value::float(1.0),
            kind: WidgetKind::Slider {
                min: 0.0,
                max: 10.0,
                step: 1.0,
            },
        }
    }

    #[test]
    fn a_slider_refuses_a_value_outside_its_range() {
        assert!(slider().accepts(&Value::float(5.0)).is_ok());
        let err = slider().accepts(&Value::float(1e9)).unwrap_err();
        assert!(err.contains("outside its range"), "{err}");
    }

    #[test]
    fn a_slider_takes_a_whole_number_from_a_browser() {
        assert!(slider().accepts(&Value::int(5)).is_ok());
    }

    #[test]
    fn a_select_refuses_an_option_it_does_not_have() {
        let w = Widget {
            cell: "r".into(),
            label: "Region".into(),
            default: Value::text("all"),
            kind: WidgetKind::Select {
                options: vec!["all".into(), "north".into()],
            },
        };
        assert!(w.accepts(&Value::text("north")).is_ok());
        assert!(w.accepts(&Value::text("mars")).is_err());
    }
}
