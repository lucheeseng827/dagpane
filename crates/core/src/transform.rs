//! The built-in table operations.
//!
//! These exist so that an app can be written **without a compiler**: the manifest path in
//! `dagpane-app` wires a slider to a filter and a filter to a chart, and every one of those
//! steps is a function in this module. A Rust app can call them too, or ignore them entirely
//! and return a [`Table`] it built any way it likes — the engine does not care where a value
//! came from.
//!
//! Deliberately small: filter, select, sort, limit, and group-by with five aggregates.
//! That is the set the example app needs and the set whose null-handling can be stated in
//! one paragraph. It is not a query engine and the README does not claim one.
//!
//! **Nulls follow SQL, not Rust.** A null compares equal to nothing, including another
//! null, so a filter never keeps a null row. `count` counts rows; `sum`, `mean`, `min` and
//! `max` skip nulls and return null for a group with no non-null values. Sorting puts nulls
//! last in both directions, because "last" is where a reader looks for missing data.

use serde::{Deserialize, Serialize};

use crate::error::CellError;
use std::sync::Arc;

use crate::frame::Frame;
use crate::value::{Column, ColumnData, ColumnType, Value};

/// The comparisons a filter can make.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    /// Equal. A null is equal to nothing, itself included, so this never keeps a null row.
    Eq,
    /// Not equal — and still not true of a null, which is where SQL's three-valued logic
    /// surprises people and why the module docs state the rule once for all seven.
    Ne,
    /// Less than.
    Lt,
    /// Less than or equal.
    Le,
    /// Greater than.
    Gt,
    /// Greater than or equal.
    Ge,
    /// Substring, text columns only. Case-sensitive; a case-insensitive variant is a
    /// deliberate omission rather than an oversight — it needs a locale answer this crate
    /// does not have.
    Contains,
}

impl Comparison {
    /// Whether this comparison holds, given how the two sides ordered.
    fn holds(self, ord: std::cmp::Ordering) -> bool {
        use std::cmp::Ordering::*;
        match (self, ord) {
            (Comparison::Eq, Equal) => true,
            (Comparison::Ne, Equal) => false,
            (Comparison::Ne, _) => true,
            (Comparison::Lt, Less) => true,
            (Comparison::Le, Less | Equal) => true,
            (Comparison::Gt, Greater) => true,
            (Comparison::Ge, Greater | Equal) => true,
            _ => false,
        }
    }
}

/// Keep the rows where `column op value` holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    /// The column to test. Missing columns are an error at filter time, not at build time —
    /// a table's schema is data, so it cannot be checked when the graph is.
    pub column: String,
    /// The comparison.
    pub op: Comparison,
    /// The right-hand side, compared against each element. `Int` and `Float` compare
    /// against each other, so a slider that lands on a whole number still filters a float
    /// column; every other cross-type pairing simply never compares equal, and the filter
    /// returns no rows rather than an error. Worth knowing when a filter unexpectedly
    /// empties a table.
    pub value: Value,
}

fn missing_column(table: &dyn Frame, name: &str) -> CellError {
    CellError::failed(format!(
        "no column `{name}`; the table has {}",
        if table.width() == 0 {
            "no columns".to_string()
        } else {
            table
                .column_names()
                .iter()
                .map(|c| format!("`{c}`"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    ))
}

/// Compare one element against a literal. `None` means "not comparable", which a filter
/// treats as "do not keep" — a null row, or a comparison between a number and a word.
pub(crate) fn compare_column_to(
    data: &ColumnData,
    row: usize,
    rhs: &Value,
) -> Option<std::cmp::Ordering> {
    match (data, rhs) {
        (ColumnData::Int(v), _) => {
            let lhs = v[row]?;
            match rhs {
                Value::Int { v } => Some(lhs.cmp(v)),
                Value::Float { v } => (lhs as f64).partial_cmp(v),
                _ => None,
            }
        }
        (ColumnData::Float(v), _) => {
            let lhs = v[row]?;
            let rhs = rhs.as_float()?;
            lhs.partial_cmp(&rhs)
        }
        (ColumnData::Text(v), Value::Text { v: rhs }) => {
            let lhs = v[row].as_ref()?;
            Some(lhs.as_str().cmp(rhs.as_str()))
        }
        (ColumnData::Bool(v), Value::Bool { v: rhs }) => {
            let lhs = v[row]?;
            Some(lhs.cmp(rhs))
        }
        _ => None,
    }
}

/// Keep the rows where the filter holds, in their original order.
///
/// Row order is preserved because a filter feeding a [`limit`] would otherwise return an
/// arbitrary slice — "the first ten" has to mean something.
///
/// A row whose value is null, or whose type cannot be compared with the filter's, is
/// dropped rather than reported: SQL's rule, and the reason an all-wrong-type filter yields
/// an empty frame instead of an error.
///
/// # Errors
///
/// [`CellError::Failed`] if the column does not exist, or if [`Comparison::Contains`] is
/// used with a non-text needle or against a non-text column — the two cases where the spec
/// itself, rather than a row, is the thing that does not make sense.
pub fn filter(table: &dyn Frame, spec: &Filter) -> Result<Arc<dyn Frame>, CellError> {
    let idx = table
        .column_index(&spec.column)
        .ok_or_else(|| missing_column(table, &spec.column))?;

    if spec.op == Comparison::Contains {
        let needle = spec.value.as_text().ok_or_else(|| {
            CellError::failed(format!(
                "`contains` needs text to look for, got {}",
                spec.value.type_name()
            ))
        })?;
        let column_type = table.column_type(idx).expect("an index from column_index");
        if column_type != ColumnType::Text {
            return Err(CellError::failed(format!(
                "`contains` needs a text column; `{}` is {column_type}",
                spec.column
            )));
        }
        let keep: Vec<usize> = (0..table.rows())
            .filter(|&r| match table.value_at(r, idx) {
                Value::Text { v } => v.contains(needle),
                _ => false,
            })
            .collect();
        return Ok(table.take_rows(&keep));
    }

    let keep: Vec<usize> = (0..table.rows())
        .filter(|&r| {
            table
                .compare_to_value(r, idx, &spec.value)
                .map(|ord| spec.op.holds(ord))
                .unwrap_or(false)
        })
        .collect();
    Ok(table.take_rows(&keep))
}

/// Keep these columns, in this order. A name that is not there is an error rather than a
/// silent omission: a chart that quietly loses its y-axis is worse than one that says so.
pub fn select(table: &dyn Frame, columns: &[String]) -> Result<Arc<dyn Frame>, CellError> {
    let mut idx = Vec::with_capacity(columns.len());
    for name in columns {
        idx.push(
            table
                .column_index(name)
                .ok_or_else(|| missing_column(table, name))?,
        );
    }
    Ok(table.select_columns(&idx))
}

/// Sort by one column. Nulls go last in both directions.
pub fn sort(
    table: &dyn Frame,
    column: &str,
    descending: bool,
) -> Result<Arc<dyn Frame>, CellError> {
    let idx = table
        .column_index(column)
        .ok_or_else(|| missing_column(table, column))?;

    let mut rows: Vec<usize> = (0..table.rows()).collect();
    rows.sort_by(|&a, &b| {
        let (na, nb) = (table.is_null(a, idx), table.is_null(b, idx));
        match (na, nb) {
            (true, true) => a.cmp(&b),
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => {
                let ord = table.compare_in_column(idx, a, b);
                // A stable tiebreak on row index keeps the output deterministic, which is
                // what lets a digest short-circuit a re-sort of unchanged data.
                let ord = if descending { ord.reverse() } else { ord };
                ord.then(a.cmp(&b))
            }
        }
    });
    Ok(table.take_rows(&rows))
}

pub(crate) fn column_is_null(data: &ColumnData, row: usize) -> bool {
    match data {
        ColumnData::Int(v) => v[row].is_none(),
        ColumnData::Float(v) => v[row].is_none(),
        ColumnData::Text(v) => v[row].is_none(),
        ColumnData::Bool(v) => v[row].is_none(),
    }
}

pub(crate) fn order_within_column(data: &ColumnData, a: usize, b: usize) -> std::cmp::Ordering {
    match data {
        ColumnData::Int(v) => v[a].cmp(&v[b]),
        // Nulls are already separated by the caller and NaN is the only remaining
        // incomparable case; treat it as equal so the stable tiebreak decides.
        ColumnData::Float(v) => v[a].partial_cmp(&v[b]).unwrap_or(std::cmp::Ordering::Equal),
        ColumnData::Text(v) => v[a].cmp(&v[b]),
        ColumnData::Bool(v) => v[a].cmp(&v[b]),
    }
}

/// The first `n` rows.
pub fn limit(table: &dyn Frame, n: usize) -> Arc<dyn Frame> {
    table.head(n)
}

/// What to compute per group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Agg {
    /// Rows in the group, as an integer column. The only aggregate that ignores its column
    /// and the only one that counts nulls, because a missing measurement is still a row.
    Count,
    /// Sum of the non-null values; null for a group with none. Produces a float even over
    /// an integer column: an `i64` sum that silently wrapped would be a wrong number
    /// rendered confidently, which is the one thing a dashboard must not do.
    Sum,
    /// Arithmetic mean of the non-null values; null for a group with none.
    Mean,
    /// Smallest non-null value; null for a group with none. Over a text column this keeps
    /// the column's own type and compares lexicographically; over a numeric one it produces
    /// a float, like every other numeric aggregate here.
    Min,
    /// Largest non-null value; null for a group with none. Same typing as [`Agg::Min`].
    Max,
}

/// One output column of a group-by.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AggSpec {
    /// The column to aggregate. Ignored by [`Agg::Count`], which counts rows.
    pub column: String,
    /// Which aggregate to compute.
    pub agg: Agg,
    /// The output column's name. Defaults to `<agg>_<column>` when a manifest leaves it out.
    pub as_name: String,
}

/// Group by zero or more columns and aggregate.
///
/// Zero grouping columns is legal and means "the whole table is one group", which is how a
/// summary metric is written.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupBy {
    /// The grouping columns, in order. That order is the output's column order and its sort
    /// order, so it is a choice the app author makes rather than one this function makes for
    /// them. Empty means one group covering the whole table.
    pub by: Vec<String>,
    /// The columns to compute per group. May be empty, which yields the distinct key
    /// combinations and nothing else.
    pub aggs: Vec<AggSpec>,
}

/// Group the frame and aggregate, returning the key columns followed by one column per
/// [`AggSpec`].
///
/// Groups come out in first-appearance order rather than sorted: it is stable, it costs
/// nothing, and it means a frame that arrived sorted leaves sorted.
///
/// A group with no non-null values for an aggregated column gets a null there, not a zero.
/// Grouping by nothing over an empty frame still yields one row, so a whole-table metric
/// reads `0` rather than disappearing from the page.
///
/// # Errors
///
/// [`CellError::Failed`] if a grouping column or an aggregated column does not exist, or if
/// an aggregate is applied to a column type it has no meaning for — every aggregate but
/// [`Agg::Count`] over a boolean column, and [`Agg::Sum`] or [`Agg::Mean`] over text.
pub fn group_by(table: &dyn Frame, spec: &GroupBy) -> Result<Arc<dyn Frame>, CellError> {
    // Reads its inputs through the trait, one cell at a time, and materialises nothing.
    //
    // It used to pull every input column into an owned `ColumnData` first — `Option<String>`
    // per text cell and all. That was measured: on the bundled app at a million rows it set
    // the whole process peak at 294 MB against a 76 MB load, and it did so even for a
    // group-by that reads ONE column, because the loop ran over `0..table.width()`. It was
    // also the same copy the CSV reader had just been taught not to make — the same mistake,
    // twice, in two files, with the second one invisible until the first was gone.
    //
    // The output still goes back through the seam via `same_kind`, so a chain over an Arrow
    // frame stays Arrow. Only the *inputs* changed.
    let key_idx: Vec<usize> = spec
        .by
        .iter()
        .map(|name| {
            table
                .column_index(name)
                .ok_or_else(|| missing_column(table, name))
        })
        .collect::<Result<_, _>>()?;

    // Group keys are rendered to strings. Slower than a typed key and immune to the bug
    // where two different types hash into the same bucket; the group count is small by
    // construction (it is what a human is going to look at) so the cost is not where the
    // time goes.
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    for row in 0..table.rows() {
        let key = key_idx
            .iter()
            .map(|&i| render_key(&table.value_at(row, i)))
            .collect::<Vec<_>>()
            .join("\u{1f}");
        groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            Vec::new()
        });
        groups.get_mut(&key).expect("just inserted").push(row);
    }
    if spec.by.is_empty() && table.rows() == 0 {
        // A whole-table aggregate of an empty table is still one group, so a metric reads
        // `0` rather than vanishing from the page.
        order.push(String::new());
        groups.insert(String::new(), Vec::new());
    }

    let mut columns: Vec<Column> = Vec::with_capacity(spec.by.len() + spec.aggs.len());
    for (n, &i) in key_idx.iter().enumerate() {
        let ty = table
            .column_type(i)
            .expect("a column index the frame itself resolved");
        let mut out = empty_column_of(ty);
        for key in &order {
            let first = groups[key][0];
            push_value(&mut out, table.value_at(first, i));
        }
        columns.push(Column::new(spec.by[n].clone(), out));
    }

    for agg in &spec.aggs {
        let column = match agg.agg {
            Agg::Count => {
                ColumnData::Int(order.iter().map(|k| Some(groups[k].len() as i64)).collect())
            }
            _ => {
                let idx = table
                    .column_index(&agg.column)
                    .ok_or_else(|| missing_column(table, &agg.column))?;
                aggregate_from_frame(table, idx, &order, &groups, agg)?
            }
        };
        columns.push(Column::new(agg.as_name.clone(), column));
    }

    // Built through the seam, so an aggregation over an Arrow frame stays Arrow. Every
    // column here was built from `order`, so they share a length and `same_kind` cannot be
    // handed a ragged set.
    Ok(table.same_kind(columns))
}

/// The bucket key for one cell.
///
/// Type-tagged, so an `Int` 1 and the text "1" cannot collide, and rendered rather than typed
/// because the group count is small by construction — it is what a human is going to look at —
/// so the cost is not where the time goes.
fn render_key(value: &Value) -> String {
    match value {
        Value::Null => "\u{0}null".to_string(),
        Value::Int { v } => format!("i{v}"),
        Value::Float { v } => format!("f{v}"),
        Value::Text { v } => format!("s{v}"),
        Value::Bool { v } => format!("b{v}"),
        other => format!("?{}", other.type_name()),
    }
}

/// An empty owned column of a given type — the start of an output column, which is small:
/// one row per group, not one per input row.
fn empty_column_of(ty: ColumnType) -> ColumnData {
    match ty {
        ColumnType::Int => ColumnData::Int(Vec::new()),
        ColumnType::Float => ColumnData::Float(Vec::new()),
        ColumnType::Bool => ColumnData::Bool(Vec::new()),
        ColumnType::Text => ColumnData::Text(Vec::new()),
    }
}

/// Append one [`Value`] to an output column.
///
/// A value whose type does not match the column records a null rather than panicking. That
/// case is unreachable through `group_by`, which builds the column from the same
/// `column_type` the value came from; it is written this way so that a future backend
/// reporting a type it does not store degrades to a visible null instead of taking the
/// process down.
fn push_value(out: &mut ColumnData, value: Value) {
    match (out, value) {
        (ColumnData::Int(v), Value::Int { v: x }) => v.push(Some(x)),
        (ColumnData::Float(v), Value::Float { v: x }) => v.push(Some(x)),
        (ColumnData::Bool(v), Value::Bool { v: x }) => v.push(Some(x)),
        (ColumnData::Text(v), Value::Text { v: x }) => v.push(Some(x)),
        (ColumnData::Int(v), _) => v.push(None),
        (ColumnData::Float(v), _) => v.push(None),
        (ColumnData::Bool(v), _) => v.push(None),
        (ColumnData::Text(v), _) => v.push(None),
    }
}

/// Aggregate one column of the input, reading every cell through the trait.
///
/// min/max on text keeps the column's own type; every numeric aggregate produces a float,
/// including `sum` over ints — a sum that silently wrapped an i64 would be a wrong number
/// rendered confidently, which is the one thing a dashboard must not do.
///
/// Reads through `value_at` rather than over an owned copy of the column. For a numeric
/// column that allocates nothing at all; for a text min/max it allocates one `String` per
/// cell and drops it again — the same allocation the old path made on its way to keeping
/// every one of them alive at once.
fn aggregate_from_frame(
    frame: &dyn Frame,
    col: usize,
    order: &[String],
    groups: &std::collections::HashMap<String, Vec<usize>>,
    spec: &AggSpec,
) -> Result<ColumnData, CellError> {
    let ty = frame
        .column_type(col)
        .expect("a column index the frame itself resolved");
    match (ty, spec.agg) {
        (ColumnType::Text, Agg::Min | Agg::Max) => {
            let out = order
                .iter()
                .map(|k| {
                    let mut best: Option<String> = None;
                    for &r in &groups[k] {
                        // A null, which min/max skips rather than propagates.
                        let Value::Text { v: s } = frame.value_at(r, col) else {
                            continue;
                        };
                        best = Some(match (best, spec.agg) {
                            (None, _) => s,
                            (Some(b), Agg::Min) => b.min(s),
                            (Some(b), _) => b.max(s),
                        });
                    }
                    best
                })
                .collect();
            Ok(ColumnData::Text(out))
        }
        (ColumnType::Int | ColumnType::Float, _) => {
            // Folded, not collected. Buffering the group's values first would put back a
            // per-row allocation of exactly the kind this function was rewritten to remove:
            // a whole-table aggregate is ONE group, so the buffer would be one `f64` per
            // input row — 8 MB at a million.
            //
            // `acc` stays `None` until the first non-null value, which is what carries the
            // two rules the buffered version got from `vals.is_empty()` and from an unseeded
            // `reduce`. A group with no non-null values yields null rather than a zero. And
            // min/max are seeded from the data rather than from ±infinity, because
            // `f64::min`/`max` ignore a lone NaN — seeding from infinity would return that
            // seed for a NaN-only group, an infinity the data never contained. NaN is not
            // null in this module (sorting and digests treat it as a value, and Sum and Mean
            // propagate it), so a NaN-only group must yield NaN.
            let out = order
                .iter()
                .map(|k| {
                    let mut acc: Option<(f64, usize)> = None;
                    for &r in &groups[k] {
                        let Some(v) = frame.value_at(r, col).as_float() else {
                            continue;
                        };
                        acc = Some(match acc {
                            None => (v, 1),
                            Some((a, n)) => (
                                match spec.agg {
                                    Agg::Sum | Agg::Mean => a + v,
                                    Agg::Min => f64::min(a, v),
                                    Agg::Max => f64::max(a, v),
                                    Agg::Count => {
                                        unreachable!("count does not read a column")
                                    }
                                },
                                n + 1,
                            ),
                        });
                    }
                    acc.map(|(a, n)| match spec.agg {
                        Agg::Mean => a / n as f64,
                        _ => a,
                    })
                })
                .collect();
            Ok(ColumnData::Float(out))
        }
        (ty, agg) => Err(CellError::failed(format!(
            "cannot compute {agg:?} over a {ty} column (`{}`)",
            spec.column
        ))),
    }
}

/// The type an aggregate produces, for a manifest checker that wants to reject a bad app
/// before it runs rather than after.
pub fn agg_output_type(input: ColumnType, agg: Agg) -> Option<ColumnType> {
    match (input, agg) {
        (_, Agg::Count) => Some(ColumnType::Int),
        (ColumnType::Text, Agg::Min | Agg::Max) => Some(ColumnType::Text),
        (ColumnType::Int | ColumnType::Float, Agg::Sum | Agg::Mean | Agg::Min | Agg::Max) => {
            Some(ColumnType::Float)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    // Only the fixtures name it now: the verbs build their outputs through `same_kind`,
    // so nothing above builds a `Table` directly any more.
    use crate::value::Table;

    /// Read one cell by column name. The verbs return `Arc<dyn Frame>` now, so a test that
    /// wants a value asks the frame for it rather than reaching into a `Column`.
    fn cell(f: &dyn Frame, column: &str, row: usize) -> Value {
        f.value_at(
            row,
            f.column_index(column).expect("a column the test named"),
        )
    }

    /// Content equality for two frames, which `Arc<dyn Frame>` cannot derive.
    fn same(a: &Arc<dyn Frame>, b: &Arc<dyn Frame>) -> bool {
        crate::frame::FrameRef::new(Arc::clone(a)) == crate::frame::FrameRef::new(Arc::clone(b))
    }

    use super::*;

    fn sales() -> Table {
        Table::new(vec![
            Column::text(
                "region",
                vec![
                    Some("north".into()),
                    Some("south".into()),
                    Some("north".into()),
                    None,
                ],
            ),
            Column::float("amount", vec![Some(10.0), Some(30.0), Some(5.0), Some(1.0)]),
            Column::int("units", vec![Some(1), Some(3), None, Some(2)]),
        ])
        .unwrap()
    }

    #[test]
    fn filter_keeps_matching_rows() {
        let t = filter(
            &sales(),
            &Filter {
                column: "amount".into(),
                op: Comparison::Ge,
                value: Value::float(10.0),
            },
        )
        .unwrap();
        assert_eq!(t.rows(), 2);
    }

    #[test]
    fn filter_compares_an_int_literal_against_a_float_column() {
        let t = filter(
            &sales(),
            &Filter {
                column: "amount".into(),
                op: Comparison::Ge,
                value: Value::int(10),
            },
        )
        .unwrap();
        assert_eq!(t.rows(), 2, "a slider on a whole number must still filter");
    }

    #[test]
    fn a_null_never_matches_a_filter() {
        let t = filter(
            &sales(),
            &Filter {
                column: "units".into(),
                op: Comparison::Ne,
                value: Value::int(999),
            },
        )
        .unwrap();
        assert_eq!(t.rows(), 3, "the null row is not kept by `!=`");
    }

    #[test]
    fn filter_on_a_missing_column_names_the_columns_that_exist() {
        let err = filter(
            &sales(),
            &Filter {
                column: "nope".into(),
                op: Comparison::Eq,
                value: Value::int(1),
            },
        )
        .unwrap_err();
        assert!(err.message().contains("`region`"), "{}", err.message());
    }

    #[test]
    fn contains_needs_a_text_column() {
        let err = filter(
            &sales(),
            &Filter {
                column: "amount".into(),
                op: Comparison::Contains,
                value: Value::text("x"),
            },
        )
        .unwrap_err();
        assert!(err.message().contains("text column"));
    }

    #[test]
    fn select_reorders_and_rejects_unknown_columns() {
        let t = select(&sales(), &["units".into(), "region".into()]).unwrap();
        assert_eq!(t.column_names(), vec!["units", "region"]);
        assert!(select(&sales(), &["ghost".into()]).is_err());
    }

    #[test]
    fn sort_puts_nulls_last_in_both_directions() {
        for descending in [false, true] {
            let t = sort(&sales(), "units", descending).unwrap();
            let last = cell(&*t, "units", 3);
            assert_eq!(last, Value::Null, "descending = {descending}");
        }
    }

    #[test]
    fn sort_is_stable_so_an_unchanged_sort_digests_alike() {
        let a = sort(&sales(), "region", false).unwrap();
        let b = sort(&sales(), "region", false).unwrap();
        assert!(same(&a, &b));
    }

    #[test]
    fn group_by_counts_and_sums() {
        let t = group_by(
            &sales(),
            &GroupBy {
                by: vec!["region".into()],
                aggs: vec![
                    AggSpec {
                        column: String::new(),
                        agg: Agg::Count,
                        as_name: "n".into(),
                    },
                    AggSpec {
                        column: "amount".into(),
                        agg: Agg::Sum,
                        as_name: "total".into(),
                    },
                ],
            },
        )
        .unwrap();
        assert_eq!(t.rows(), 3, "north, south, and the null region");
        assert_eq!(t.column_names(), vec!["region", "n", "total"]);
        assert_eq!(cell(&*t, "total", 0), Value::float(15.0));
    }

    #[test]
    fn group_by_nothing_is_one_group() {
        let t = group_by(
            &sales(),
            &GroupBy {
                by: vec![],
                aggs: vec![AggSpec {
                    column: "amount".into(),
                    agg: Agg::Mean,
                    as_name: "avg".into(),
                }],
            },
        )
        .unwrap();
        assert_eq!(t.rows(), 1);
        assert_eq!(cell(&*t, "avg", 0), Value::float(11.5));
    }

    #[test]
    fn an_aggregate_over_only_nulls_is_null_not_zero() {
        let t = Table::new(vec![
            Column::text("k", vec![Some("a".into())]),
            Column::int("v", vec![None]),
        ])
        .unwrap();
        let out = group_by(
            &t,
            &GroupBy {
                by: vec!["k".into()],
                aggs: vec![AggSpec {
                    column: "v".into(),
                    agg: Agg::Sum,
                    as_name: "s".into(),
                }],
            },
        )
        .unwrap();
        assert_eq!(cell(&*out, "s", 0), Value::Null);
    }

    #[test]
    fn an_empty_table_still_produces_one_summary_row() {
        let t = Table::new(vec![Column::float("amount", vec![])]).unwrap();
        let out = group_by(
            &t,
            &GroupBy {
                by: vec![],
                aggs: vec![AggSpec {
                    column: String::new(),
                    agg: Agg::Count,
                    as_name: "n".into(),
                }],
            },
        )
        .unwrap();
        assert_eq!(out.rows(), 1);
        assert_eq!(cell(&*out, "n", 0), Value::int(0));
    }

    #[test]
    fn a_nan_only_group_aggregates_to_nan_not_to_infinity() {
        let t = Table::new(vec![
            Column::text("k", vec![Some("a".into())]),
            Column::float("v", vec![Some(f64::NAN)]),
        ])
        .unwrap();
        for agg in [Agg::Min, Agg::Max, Agg::Sum] {
            let out = group_by(
                &t,
                &GroupBy {
                    by: vec!["k".into()],
                    aggs: vec![AggSpec {
                        column: "v".into(),
                        agg,
                        as_name: "r".into(),
                    }],
                },
            )
            .unwrap();
            match cell(&*out, "r", 0) {
                Value::Float { v } => assert!(v.is_nan(), "{agg:?} produced {v}, not NaN"),
                other => panic!("{agg:?} produced {other:?}"),
            }
        }
    }

    #[test]
    fn summing_text_is_an_error_rather_than_a_zero() {
        let err = group_by(
            &sales(),
            &GroupBy {
                by: vec![],
                aggs: vec![AggSpec {
                    column: "region".into(),
                    agg: Agg::Sum,
                    as_name: "s".into(),
                }],
            },
        )
        .unwrap_err();
        assert!(err.message().contains("text"));
    }

    #[test]
    fn limit_is_head() {
        assert_eq!(limit(&sales(), 2).rows(), 2);
    }

    /// The three rules the numeric fold has to carry, none of which a value-equality test
    /// over ordinary data would notice.
    ///
    /// Written when `aggregate_from_frame` stopped buffering each group into a `Vec<f64>`
    /// and started folding it. The buffered version got rule 1 from `vals.is_empty()` and
    /// rule 2 from `reduce`'s lack of a seed; the fold has to get both from `acc` starting
    /// as `None`, and nothing else in the suite would have failed if it did not.
    #[test]
    fn a_numeric_aggregate_keeps_its_null_and_nan_rules() {
        use crate::value::Column;

        let frame = Table::new(vec![
            Column::text(
                "g",
                vec![
                    Some("allnull".into()),
                    Some("allnull".into()),
                    Some("nan".into()),
                    Some("nan".into()),
                    Some("mixed".into()),
                    Some("mixed".into()),
                    Some("mixed".into()),
                ],
            ),
            Column::float(
                "v",
                vec![
                    None,
                    None,
                    Some(f64::NAN),
                    Some(f64::NAN),
                    None,
                    Some(2.0),
                    Some(4.0),
                ],
            ),
        ])
        .unwrap();

        let agg = |a: Agg| {
            let out = group_by(
                &frame,
                &GroupBy {
                    by: vec!["g".into()],
                    aggs: vec![AggSpec {
                        column: "v".into(),
                        agg: a,
                        as_name: "r".into(),
                    }],
                },
            )
            .unwrap();
            let g = out.column_index("g").unwrap();
            let r = out.column_index("r").unwrap();
            (0..out.rows())
                .map(|i| {
                    let name = match out.value_at(i, g) {
                        Value::Text { v } => v,
                        other => panic!("group key is {other:?}"),
                    };
                    (name, out.value_at(i, r))
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };

        // 1. A group with no non-null values is null, never a zero — a missing measurement
        //    must not arrive at a metric as `0`.
        for a in [Agg::Sum, Agg::Mean, Agg::Min, Agg::Max] {
            assert_eq!(
                agg(a)["allnull"],
                Value::Null,
                "{a:?} over an all-null group"
            );
        }

        // 2. A group whose every value is NaN yields NaN. `f64::min`/`max` ignore a lone
        //    NaN, so a fold seeded from ±infinity would return that infinity here — a
        //    number the data never contained, rendered confidently.
        for a in [Agg::Sum, Agg::Mean, Agg::Min, Agg::Max] {
            let got = agg(a)["nan"].clone();
            match got {
                Value::Float { v } => assert!(v.is_nan(), "{a:?} over a NaN group gave {v}"),
                other => panic!("{a:?} over a NaN group gave {other:?}"),
            }
        }

        // 3. Nulls are skipped, not counted: mean is 3.0 over two values, not 2.0 over three.
        assert_eq!(agg(Agg::Sum)["mixed"], Value::float(6.0));
        assert_eq!(agg(Agg::Mean)["mixed"], Value::float(3.0));
        assert_eq!(agg(Agg::Min)["mixed"], Value::float(2.0));
        assert_eq!(agg(Agg::Max)["mixed"], Value::float(4.0));
    }
}
