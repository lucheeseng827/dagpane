//! The built-in table operations.
//!
//! These exist so that an app can be written **without a compiler**: the manifest path in
//! `dagpane-app` wires a slider to a filter and a filter to a chart, and every one of those
//! steps is a function in this module. A Rust app can call them too, or ignore them entirely
//! and return a [`Table`](crate::value::Table) it built any way it likes — the engine does not
//! care where a value came from.
//!
//! Deliberately small: filter, derive, select, sort, limit, and group-by with five
//! aggregates. That is the set the example app needs and the set whose null-handling can be
//! stated in one paragraph. It is not a query engine and the README does not claim one.
//!
//! **Nulls follow SQL, not Rust.** A null compares equal to nothing, including another
//! null, so a filter never keeps a null row. `count` counts rows; `sum`, `mean`, `min` and
//! `max` skip nulls and return null for a group with no non-null values. Sorting puts nulls
//! last in both directions, because "last" is where a reader looks for missing data.
//!
//! [`derive()`] is the exception, and [`crate::expr`] says why at length: inside an expression
//! a null propagates through everything, `and` and `or` included, because one rule an author
//! can hold in their head beats a three-valued truth table they have to look up.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::CellError;
use std::sync::Arc;

use crate::expr::{Expr, Row, Scope, Ty};
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
    let names = table.column_names();
    // The same "did you mean" the expression checker spells, through the same function: a
    // misspelt column is a misspelt column whether it was written in a `sort` or inside a
    // `derive`, and two implementations of that sentence is how the two drift apart.
    let hint = crate::expr::nearest(name, names.iter().map(String::as_str))
        .map(|n| format!(" — did you mean `{n}`?"))
        .unwrap_or_default();
    CellError::failed(format!(
        "no column `{name}`{hint}; the table has {}",
        if names.is_empty() {
            "no columns".to_string()
        } else {
            names
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
    Ok(table.take_rows(&selection(table, spec)?))
}

/// The rows a filter keeps, in their original order — [`filter`] without the taking.
///
/// Split out because a **predicate constraint** needs the selection and not the frame. A cell
/// that filters on one column and then aggregates others depends on which rows the predicate
/// picked, and not on the values it picked them by: move a value from 500 to 600 under
/// `>= 400` and the same rows survive, so the cell's answer cannot have changed. Recording
/// that means recording this.
///
/// `filter` is `take_rows` over this, so a pipeline that needs both the frame and the
/// selection pays for one pass and not two.
///
/// # Errors
///
/// The same two as [`filter`]: a column that does not exist, and [`Comparison::Contains`]
/// against anything but text.
pub fn selection(table: &dyn Frame, spec: &Filter) -> Result<Vec<usize>, CellError> {
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
        return Ok((0..table.rows())
            .filter(|&r| match table.value_at(r, idx) {
                Value::Text { v } => v.contains(needle),
                _ => false,
            })
            .collect());
    }

    Ok((0..table.rows())
        .filter(|&r| {
            table
                .compare_to_value(r, idx, &spec.value)
                .map(|ord| spec.op.holds(ord))
                .unwrap_or(false)
        })
        .collect())
}

/// The digest of a row selection.
///
/// What a predicate constraint stores and compares. Length-prefixed and in order, so a
/// selection that gained a row, lost one, or reordered is a different digest.
pub fn selection_digest(keep: &[usize]) -> crate::digest::Digest {
    rows_digest(0xc1, keep)
}

/// The digest of a row **ordering**.
///
/// What a sort constraint stores. Tagged apart from [`selection_digest`] on purpose: a
/// selection and a permutation can be the same list of integers while meaning entirely
/// different things, and two constraints that could compare equal across that boundary would
/// be a way for one to be validated by the other's answer.
pub fn ordering_digest(order: &[usize]) -> crate::digest::Digest {
    rows_digest(0xc2, order)
}

fn rows_digest(tag: u8, rows: &[usize]) -> crate::digest::Digest {
    let mut h = crate::digest::Hasher::new();
    h.tag(tag).u64(rows.len() as u64);
    for r in rows {
        h.u64(*r as u64);
    }
    h.finish()
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
    Ok(table.take_rows(&ordering(table, column, descending)?))
}

/// The order a sort puts the rows in — [`sort`] without the taking.
///
/// Split out for the same reason [`selection`] was: a cell downstream of a `sort` depends on
/// the **order the rows ended up in**, and not on the values that decided it. A column whose
/// values all move by the same amount produces the same permutation, and a ranking that does
/// not change cannot change any answer computed from it.
///
/// Stable, and that matters here rather than only aesthetically: an unstable sort could return
/// a different permutation for the same data, and a constraint recorded against one would fail
/// against the other for no reason a reader could see.
///
/// # Errors
///
/// [`CellError::Failed`] if the column does not exist.
pub fn ordering(
    table: &dyn Frame,
    column: &str,
    descending: bool,
) -> Result<Vec<usize>, CellError> {
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
    Ok(rows)
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

/// Add one column, computed from each row by an expression.
///
/// The eighth verb, and the only one that *adds* to its input rather than reshaping it —
/// which is why it takes an `Arc` where its siblings take a `&dyn`. The result shares every
/// existing column with `table` (see [`crate::frame::with_column`]) and allocates exactly the
/// one column it computed, so a derive over a million rows costs one column and not a frame.
///
/// `params` supplies the `$name` references, in an order the caller chooses and the
/// expression does not: binding resolves each name to its position here. In a manifest that
/// order is the order the steps first mention them, so a compiled index is stable.
///
/// Both the schema check and the type check happen once, before the first row is read. After
/// that, evaluation cannot fail — every way an expression can fail to produce a number yields
/// null instead, so one bad row is a gap in a column rather than a broken pane.
///
/// # Errors
///
/// [`CellError::Failed`] if a column the expression names does not exist here, if a parameter
/// holds a table or a list, if an operator is applied to types it has no meaning for, or if
/// `name` is already a column — a second column of the same name would shadow the first for
/// every later step that looks one up by name, which is a wrong app that looks like a working
/// one.
pub fn derive(
    table: &Arc<dyn Frame>,
    name: &str,
    expr: &Expr,
    params: &[(String, Value)],
) -> Result<Arc<dyn Frame>, CellError> {
    let schema = table.schema();
    if let Some((existing, _)) = schema.iter().find(|(c, _)| c == name) {
        return Err(CellError::failed(format!(
            "this table already has a column `{existing}`; a derived column needs a name of \
             its own, because every step after this one that names `{existing}` would still \
             find the original"
        )));
    }

    let mut scope = Scope::new().with_columns(schema);
    for (param, value) in params {
        let ty = Ty::of_value(value).ok_or_else(|| {
            CellError::failed(format!(
                "`${param}` holds a {}, and an expression works on one row at a time; \
                 a {} is not a value it can read",
                value.type_name(),
                value.type_name()
            ))
        })?;
        scope = scope.with_param(param, ty);
    }

    let program = expr
        .bind(&scope)
        .map_err(|e| CellError::failed(format!("`{name} = {expr}`: {e}")))?;

    // The parameter values, positionally, exactly as `scope` recorded their names.
    let values: Vec<Value> = params.iter().map(|(_, v)| v.clone()).collect();
    let mut data = ColumnData::with_capacity(program.output_type(), table.rows());
    for row in 0..table.rows() {
        data.push(program.eval(&FrameRow {
            frame: &**table,
            row,
            params: &values,
        }));
    }
    Ok(crate::frame::with_column(
        table.clone(),
        Column::new(name, data),
    ))
}

/// One row of a frame, as the expression evaluator reads it.
///
/// Built per row; three fields and no allocation. `param` clones the parameter's value on
/// every read, which is free for a number and one short string for text — the alternative is
/// a lifetime on [`crate::expr::Row`] that every caller would carry to buy back an allocation
/// a manifest makes at most a handful of per row.
struct FrameRow<'a> {
    frame: &'a dyn Frame,
    row: usize,
    params: &'a [Value],
}

impl Row for FrameRow<'_> {
    fn column(&self, at: usize) -> Value {
        self.frame.value_at(self.row, at)
    }

    fn param(&self, at: usize) -> Value {
        self.params[at].clone()
    }
}

// ── joining ────────────────────────────────────────────────────────────────────────────

/// Which rows a join keeps, and what it carries across.
///
/// Four, and the two that are missing are missing on purpose. **`right` is `left` with the
/// two cells swapped**, which a manifest can write and this enum therefore does not need.
/// **`full` is not built**: nothing has needed it yet, and its one real decision — which side
/// a key column's value comes from on a row that matched only one of them — is better made
/// against an app that wants it than invented here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum How {
    /// Keep the left rows that matched, widened by the right's columns.
    Inner,
    /// Keep every left row, widened by the right's columns — nulls where nothing matched.
    Left,
    /// Keep the left rows that matched. **Adds no columns and no rows**, so it is a filter
    /// that happens to read another table: "the accounts that have a ticket".
    Semi,
    /// Keep the left rows that did *not* match. The other half of the same filter: "the
    /// accounts that have no ticket".
    Anti,
}

impl fmt::Display for How {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            How::Inner => "inner",
            How::Left => "left",
            How::Semi => "semi",
            How::Anti => "anti",
        })
    }
}

impl How {
    /// Whether this kind carries the right-hand table's columns across. `semi` and `anti` do
    /// not, which is why they cost nothing but the hash map.
    fn widens(self) -> bool {
        matches!(self, How::Inner | How::Left)
    }
}

/// Match rows of one table against another on equal keys.
///
/// The ninth verb. Its edge was never the hard part — both tables are cells the manifest
/// names, so `dagpane graph` draws them — and every interesting decision here is about what
/// happens when the *data* does not fit the join the author had in mind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Join {
    /// The key columns on the left, in order.
    pub left_on: Vec<String>,
    /// The key columns on the right, positionally paired with `left_on`. The right's key
    /// columns never appear in the output: they are equal to the left's by construction, and
    /// a second copy of a column is a collision waiting to be found by somebody else.
    pub right_on: Vec<String>,
    /// Which rows survive.
    pub how: How,
    /// Appended to every non-key column carried over from the right. Empty means none, and
    /// then a name that appears on both sides is an error rather than a second column with
    /// the same name.
    pub suffix: String,
    /// Whether one left row may match several right rows.
    ///
    /// **Default `false`, and that is the load-bearing decision in this file.** A right-hand
    /// table with a duplicated key silently multiplies rows, and a chart whose total doubled
    /// because a dimension table gained a duplicate is exactly the failure this project
    /// exists to not have — it renders confidently and it is wrong. So the ordinary join is a
    /// *lookup*, a duplicate key is an error that names the key, and an author who really
    /// does mean one-to-many writes `multiple = true` and thereby tells the next reader that
    /// the output may be longer than the left.
    pub multiple: bool,
}

/// The columns a join produces, or why it cannot produce any.
///
/// **One function, two callers**: [`join`] itself, and the manifest checker, which knows both
/// schemas before the app runs. That is the whole reason it is public — a second
/// implementation of these rules is how the two come to disagree, and the disagreement would
/// show up as an app that `dagpane check` passed and the run time refused.
///
/// # Errors
///
/// A sentence, for the caller to wrap in whatever error type it reports: a key column that is
/// not there, a key pair of different types, no keys at all, or two output columns that would
/// share a name.
pub fn join_schema(
    left: &[(String, ColumnType)],
    right: &[(String, ColumnType)],
    spec: &Join,
) -> Result<Vec<(String, ColumnType)>, String> {
    if spec.left_on.is_empty() {
        return Err("a join needs at least one key column".to_string());
    }
    if spec.left_on.len() != spec.right_on.len() {
        return Err(format!(
            "a join's keys pair up positionally: {} on the left against {} on the right",
            spec.left_on.len(),
            spec.right_on.len()
        ));
    }

    for (l, r) in spec.left_on.iter().zip(&spec.right_on) {
        let lt = look_up(left, l, "the left-hand table")?;
        let rt = look_up(right, r, "the right-hand table")?;
        // Not widened. An `int` key against a `float` one is a data-modelling mistake, and
        // the two ways of being lenient about it are both worse than saying so: matching
        // across the types makes a join that works until a value stops being whole, and
        // refusing silently makes an empty table with no explanation.
        if lt != rt {
            return Err(format!(
                "key `{l}` is {lt} on the left and `{r}` is {rt} on the right; a join matches \
                 values of one type"
            ));
        }
    }

    let mut out: Vec<(String, ColumnType)> = left.to_vec();
    if spec.how.widens() {
        for (name, ty) in right {
            if spec.right_on.iter().any(|k| k == name) {
                continue;
            }
            out.push((format!("{name}{}", spec.suffix), *ty));
        }
    }

    // A duplicate name would leave every later step that looks one up finding the first,
    // which is a wrong app that looks like a working one — the same rule `derive` enforces
    // when it refuses to shadow.
    for i in 0..out.len() {
        if out[..i].iter().any(|(n, _)| *n == out[i].0) {
            return Err(format!(
                "both tables have a column `{}`; give the right-hand one a `suffix`, or \
                 `select` it away before joining",
                out[i].0
            ));
        }
    }
    Ok(out)
}

fn look_up(
    schema: &[(String, ColumnType)],
    name: &str,
    where_: &str,
) -> Result<ColumnType, String> {
    if let Some((_, ty)) = schema.iter().find(|(c, _)| c == name) {
        return Ok(*ty);
    }
    let hint = crate::expr::nearest(name, schema.iter().map(|(c, _)| c.as_str()))
        .map(|n| format!(" — did you mean `{n}`?"))
        .unwrap_or_default();
    Err(format!(
        "no column `{name}` in {where_}{hint}; it has {}",
        if schema.is_empty() {
            "no columns".to_string()
        } else {
            schema
                .iter()
                .map(|(c, _)| format!("`{c}`"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    ))
}

/// Match `left` against `right` on equal keys.
///
/// Row order is the left's, always: a left row's position in the output is its position in
/// the input, and under `multiple` its several matches appear together in the right's own row
/// order. That determinism is not a nicety — an output that reordered itself between two runs
/// over identical data would digest differently and repaint a page that did not change.
///
/// **A null key matches nothing**, including another null, which is SQL's rule and the one
/// [`crate::transform`] already follows everywhere else. Under [`How::Left`] a left row with a
/// null key keeps its row and gets nulls across; under [`How::Inner`] it is dropped.
///
/// [`How::Semi`] and [`How::Anti`] return `left.take_rows(…)` and allocate nothing but the
/// hash map — they choose rows and never build a column, so a semi-join over an Arrow frame
/// is the same Arrow frame with fewer rows.
///
/// # Errors
///
/// [`CellError::Failed`] for anything [`join_schema`] rejects, and for a duplicated right-hand
/// key when `multiple` is false.
pub fn join(left: &dyn Frame, right: &dyn Frame, spec: &Join) -> Result<Arc<dyn Frame>, CellError> {
    let left_schema = left.schema();
    let right_schema = right.schema();
    let out_schema = join_schema(&left_schema, &right_schema, spec).map_err(CellError::failed)?;

    let left_keys: Vec<usize> = spec
        .left_on
        .iter()
        .map(|n| left.column_index(n).expect("join_schema resolved it"))
        .collect();
    let right_keys: Vec<usize> = spec
        .right_on
        .iter()
        .map(|n| right.column_index(n).expect("join_schema resolved it"))
        .collect();

    // The right-hand side, bucketed by key. Rendered rather than typed, like `group_by`'s:
    // the tag in front of each part means an `Int` 1 and the text "1" cannot land in one
    // bucket, which matters more here than the cost of formatting does.
    let mut index: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::with_capacity(right.rows());
    for row in 0..right.rows() {
        let Some(key) = key_of(right, row, &right_keys) else {
            // A null key matches nothing, so it never enters the index at all.
            continue;
        };
        index.entry(key).or_default().push(row);
    }

    if !spec.multiple && spec.how.widens() {
        if let Some((_, rows)) = index.iter().find(|(_, rows)| rows.len() > 1) {
            return Err(CellError::failed(format!(
                "the right-hand table has {} rows for one key ({}), so this join would make \
                 the table longer than it is; aggregate the right-hand side first, or say you \
                 meant it with `multiple = true` on a `join` step",
                rows.len(),
                readable_key(right, rows[0], &right_keys)
            )));
        }
    }

    let mut pairs: Vec<(usize, Option<usize>)> = Vec::with_capacity(left.rows());
    let mut keep: Vec<usize> = Vec::new();
    for row in 0..left.rows() {
        let matched = key_of(left, row, &left_keys).and_then(|k| index.get(&k));
        match spec.how {
            How::Semi => {
                if matched.is_some() {
                    keep.push(row);
                }
            }
            How::Anti => {
                if matched.is_none() {
                    keep.push(row);
                }
            }
            How::Inner => {
                if let Some(rows) = matched {
                    pairs.extend(rows.iter().map(|&r| (row, Some(r))));
                }
            }
            How::Left => match matched {
                Some(rows) => pairs.extend(rows.iter().map(|&r| (row, Some(r)))),
                None => pairs.push((row, None)),
            },
        }
    }

    if !spec.how.widens() {
        // A filter, so it is one: every column shared with the input, nothing built.
        return Ok(left.take_rows(&keep));
    }

    let mut columns: Vec<Column> = Vec::with_capacity(out_schema.len());
    for (i, (name, ty)) in out_schema.iter().enumerate().take(left_schema.len()) {
        let mut data = ColumnData::with_capacity(*ty, pairs.len());
        for &(l, _) in &pairs {
            data.push(left.value_at(l, i));
        }
        columns.push(Column::new(name.clone(), data));
    }
    let mut at = left_schema.len();
    for (j, (_, ty)) in right_schema.iter().enumerate() {
        if right_keys.contains(&j) {
            continue;
        }
        let mut data = ColumnData::with_capacity(*ty, pairs.len());
        for &(_, r) in &pairs {
            // `None` is an unmatched left row under `How::Left`, and a null is what it gets:
            // "there was no row over there" is missing data, not a zero.
            data.push(r.map(|r| right.value_at(r, j)).unwrap_or(Value::Null));
        }
        columns.push(Column::new(out_schema[at].0.clone(), data));
        at += 1;
    }

    // Built through the seam, so a join over an Arrow left-hand side stays Arrow. Every column
    // came from `pairs`, so they share a length.
    Ok(left.same_kind(columns))
}

/// The bucket key for one row, or `None` if any part of it is null.
fn key_of(frame: &dyn Frame, row: usize, cols: &[usize]) -> Option<String> {
    let mut key = String::new();
    for (n, &c) in cols.iter().enumerate() {
        let value = frame.value_at(row, c);
        if matches!(value, Value::Null) {
            return None;
        }
        if n > 0 {
            key.push('\u{1f}');
        }
        key.push_str(&render_key(&value));
    }
    Some(key)
}

/// One row's key as a person would write it, for the duplicate-key error. The rendered bucket
/// key carries type tags and a separator nobody can type, which is right for a `HashMap` and
/// wrong for a message somebody has to act on.
fn readable_key(frame: &dyn Frame, row: usize, cols: &[usize]) -> String {
    cols.iter()
        .map(|&c| match frame.value_at(row, c) {
            Value::Text { v } => format!("`{v}`"),
            other => format!("`{}`", &render_key(&other)[1..]),
        })
        .collect::<Vec<_>>()
        .join(", ")
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

impl fmt::Display for Agg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Agg::Count => "count",
            Agg::Sum => "sum",
            Agg::Mean => "mean",
            Agg::Min => "min",
            Agg::Max => "max",
        })
    }
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
        let mut out = ColumnData::with_capacity(ty, order.len());
        for key in &order {
            let first = groups[key][0];
            out.push(table.value_at(first, i));
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
            "cannot compute `{agg}` over a {ty} column (`{}`)",
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

    /// `derive` takes an `Arc` because it keeps its input rather than rewriting it.
    fn frame(t: Table) -> Arc<dyn Frame> {
        Arc::new(t)
    }

    fn derived(text: &str, params: &[(String, Value)]) -> Result<Arc<dyn Frame>, CellError> {
        let expr = Expr::parse(text).expect("the test wrote a parseable expression");
        derive(&frame(sales()), "out", &expr, params)
    }

    #[test]
    fn derive_appends_one_column_computed_per_row() {
        let f = derived("amount * 2", &[]).unwrap();
        assert_eq!(f.rows(), 4);
        assert_eq!(f.width(), 4);
        assert_eq!(f.column_names().last().map(String::as_str), Some("out"));
        assert_eq!(f.column_type(3), Some(ColumnType::Float));
        assert_eq!(cell(&*f, "out", 1).as_float(), Some(60.0));
        // And the columns it was given are still there, unchanged.
        assert_eq!(cell(&*f, "region", 0).as_text(), Some("north"));
    }

    #[test]
    fn derive_reads_a_parameter_by_name() {
        let f = derived(
            "amount * (1 - $rate)",
            &[("rate".to_string(), Value::float(0.5))],
        )
        .unwrap();
        assert_eq!(cell(&*f, "out", 1).as_float(), Some(15.0));
    }

    #[test]
    fn a_null_row_derives_a_null_and_not_a_zero() {
        // `units` is null in row 2. A missing measurement must not arrive at a page as 0.
        let f = derived("units + 1", &[]).unwrap();
        assert!(matches!(cell(&*f, "out", 2), Value::Null));
        assert_eq!(cell(&*f, "out", 3).as_int(), Some(3));
    }

    #[test]
    fn derive_does_not_copy_the_columns_it_was_given() {
        // The whole reason `with_column` exists: adding a column to a wide frame must cost
        // one column, not a frame. Two `f64` columns' worth of slack is the `Option<f64>`
        // buffer this derive actually allocated.
        let base = frame(sales());
        let before = base.memory_size();
        let f = derive(&base, "out", &Expr::parse("amount * 2").unwrap(), &[]).unwrap();
        let one_column = 4 * std::mem::size_of::<Option<f64>>();
        assert_eq!(f.memory_size(), before + one_column);
        // An adapter, not a representation: a derive over an Arrow frame is still Arrow.
        assert_eq!(f.backend(), base.backend());
    }

    #[test]
    fn a_derived_column_filters_sorts_and_groups_like_any_other() {
        let f = derived("amount * 2", &[]).unwrap();
        let kept = filter(
            &*f,
            &Filter {
                column: "out".to_string(),
                op: Comparison::Gt,
                value: Value::float(20.0),
            },
        )
        .unwrap();
        assert_eq!(kept.rows(), 1);
        assert_eq!(cell(&*kept, "region", 0).as_text(), Some("south"));

        let sorted = sort(&*f, "out", true).unwrap();
        assert_eq!(cell(&*sorted, "out", 0).as_float(), Some(60.0));

        let grouped = group_by(
            &*f,
            &GroupBy {
                by: vec!["region".to_string()],
                aggs: vec![AggSpec {
                    column: "out".to_string(),
                    agg: Agg::Sum,
                    as_name: "total".to_string(),
                }],
            },
        )
        .unwrap();
        assert_eq!(cell(&*grouped, "total", 0).as_float(), Some(30.0));
    }

    #[test]
    fn select_after_derive_keeps_the_derived_column_or_drops_it() {
        let f = derived("amount * 2", &[]).unwrap();
        let dropped = select(&*f, &["region".to_string()]).unwrap();
        assert_eq!(dropped.column_names(), vec!["region".to_string()]);

        let kept = select(&*f, &["region".to_string(), "out".to_string()]).unwrap();
        assert_eq!(
            kept.column_names(),
            vec!["region".to_string(), "out".to_string()]
        );
        assert_eq!(cell(&*kept, "out", 1).as_float(), Some(60.0));

        // The reordering path, which is the one that materialises. Same content either way.
        let moved = select(&*f, &["out".to_string(), "region".to_string()]).unwrap();
        assert_eq!(cell(&*moved, "out", 1).as_float(), Some(60.0));
        assert_eq!(cell(&*moved, "region", 1).as_text(), Some("south"));
    }

    #[test]
    fn selecting_only_the_derived_column_keeps_its_rows() {
        let f = derived("amount * 2", &[]).unwrap();
        let only = select(&*f, &["out".to_string()]).unwrap();
        assert_eq!(only.rows(), 4, "the derived column lost its rows");
        assert_eq!(only.column_names(), vec!["out"]);
        assert_eq!(cell(&*only, "out", 1).as_float(), Some(60.0));
    }

    #[test]
    fn two_derives_stack() {
        let one = derived("amount * 2", &[]).unwrap();
        let two = derive(&one, "twice", &Expr::parse("out + units").unwrap(), &[]).unwrap();
        assert_eq!(two.width(), 5);
        assert_eq!(cell(&*two, "twice", 1).as_float(), Some(63.0));
        assert!(matches!(cell(&*two, "twice", 2), Value::Null));
    }

    #[test]
    fn a_derived_column_may_not_shadow_an_existing_one() {
        // Two columns of one name would leave every later step finding the original, which
        // is a wrong app that looks like a working one.
        let expr = Expr::parse("amount * 2").unwrap();
        let e = derive(&frame(sales()), "amount", &expr, &[]).unwrap_err();
        assert!(
            e.to_string().contains("already has a column `amount`"),
            "{e}"
        );
    }

    #[test]
    fn a_parameter_holding_a_table_is_refused_by_name() {
        let expr = Expr::parse("$other + 1").unwrap();
        let e = derive(
            &frame(sales()),
            "out",
            &expr,
            &[("other".to_string(), Value::table(sales()))],
        )
        .unwrap_err();
        assert!(e.to_string().contains("`$other`"), "{e}");
        assert!(e.to_string().contains("table"), "{e}");
    }

    #[test]
    fn a_bad_column_reference_names_the_columns_that_are_there() {
        let e = derived("amonut * 2", &[]).unwrap_err();
        let message = e.to_string();
        assert!(message.contains("did you mean `amount`?"), "{message}");
        assert!(message.contains("`units`"), "{message}");
        // And it quotes the expression back, because the author is looking at a manifest.
        assert!(message.contains("out = amonut * 2"), "{message}");
    }

    #[test]
    fn deriving_over_an_empty_frame_produces_an_empty_column() {
        let empty: Arc<dyn Frame> = Arc::new(Table::empty());
        let f = derive(&empty, "out", &Expr::parse("1 + 1").unwrap(), &[]).unwrap();
        assert_eq!(f.rows(), 0);
        assert_eq!(f.width(), 1);
    }

    // ── joining ────────────────────────────────────────────────────────────────────────

    /// A dimension table: one row per region, and an `east` that `sales` never mentions.
    fn regions() -> Table {
        Table::new(vec![
            Column::text(
                "region",
                vec![
                    Some("north".into()),
                    Some("south".into()),
                    Some("east".into()),
                ],
            ),
            Column::int("head_count", vec![Some(3), Some(5), Some(2)]),
            Column::text("lead", vec![Some("ana".into()), Some("bo".into()), None]),
        ])
        .unwrap()
    }

    fn on_region(how: How) -> Join {
        Join {
            left_on: vec!["region".to_string()],
            right_on: vec!["region".to_string()],
            how,
            suffix: String::new(),
            multiple: false,
        }
    }

    #[test]
    fn an_inner_join_widens_the_left_and_keeps_its_row_order() {
        let f = join(&sales(), &regions(), &on_region(How::Inner)).unwrap();
        // Row 3 of `sales` has a null region and matches nothing; `east` has no sales.
        assert_eq!(f.rows(), 3);
        assert_eq!(
            f.column_names(),
            vec!["region", "amount", "units", "head_count", "lead"]
        );
        assert_eq!(cell(&*f, "amount", 0).as_float(), Some(10.0));
        assert_eq!(cell(&*f, "amount", 1).as_float(), Some(30.0));
        assert_eq!(cell(&*f, "head_count", 0).as_int(), Some(3));
        assert_eq!(cell(&*f, "head_count", 1).as_int(), Some(5));
        assert_eq!(cell(&*f, "head_count", 2).as_int(), Some(3));
    }

    #[test]
    fn the_right_hand_key_column_does_not_appear_twice() {
        // It is equal to the left's by construction, and a second column of one name is a
        // collision waiting to be found by somebody else.
        let f = join(&sales(), &regions(), &on_region(How::Inner)).unwrap();
        assert_eq!(
            f.column_names().iter().filter(|n| *n == "region").count(),
            1
        );
    }

    #[test]
    fn a_left_join_keeps_every_left_row_and_fills_nulls() {
        let f = join(&sales(), &regions(), &on_region(How::Left)).unwrap();
        assert_eq!(f.rows(), 4);
        // Row 3's region is null, which matches nothing — so it keeps its row and gets
        // nothing across, rather than being dropped or matching the other null.
        assert!(matches!(cell(&*f, "region", 3), Value::Null));
        assert!(matches!(cell(&*f, "head_count", 3), Value::Null));
        assert_eq!(cell(&*f, "amount", 3).as_float(), Some(1.0));
    }

    #[test]
    fn a_null_key_matches_nothing_including_another_null() {
        let nulls = Table::new(vec![
            Column::text("region", vec![None]),
            Column::int("n", vec![Some(9)]),
        ])
        .unwrap();
        let inner = join(&sales(), &nulls, &on_region(How::Inner)).unwrap();
        assert_eq!(inner.rows(), 0, "two nulls are not a match");
        let anti = join(&sales(), &nulls, &on_region(How::Anti)).unwrap();
        assert_eq!(anti.rows(), 4, "so every left row is unmatched");
    }

    #[test]
    fn semi_and_anti_are_filters_that_add_nothing() {
        let semi = join(&sales(), &regions(), &on_region(How::Semi)).unwrap();
        assert_eq!(semi.schema(), sales().schema(), "no column was added");
        assert_eq!(semi.rows(), 3);
        assert_eq!(cell(&*semi, "amount", 2).as_float(), Some(5.0));

        let anti = join(&sales(), &regions(), &on_region(How::Anti)).unwrap();
        assert_eq!(anti.schema(), sales().schema());
        assert_eq!(anti.rows(), 1, "only the null-region row is unmatched");
        assert_eq!(cell(&*anti, "amount", 0).as_float(), Some(1.0));

        // They are `take_rows` and nothing else, which is what makes them free.
        let by_hand = Frame::take_rows(&sales(), &[0, 1, 2]);
        assert!(same(&semi, &by_hand));
    }

    #[test]
    fn a_duplicated_right_hand_key_is_refused_and_names_the_key() {
        // The failure this default exists to prevent: a dimension table that gained a
        // duplicate silently doubles a total, and the page looks fine.
        let dupes = Table::new(vec![
            Column::text("region", vec![Some("north".into()), Some("north".into())]),
            Column::int("head_count", vec![Some(3), Some(4)]),
        ])
        .unwrap();
        let e = join(&sales(), &dupes, &on_region(How::Inner)).unwrap_err();
        let message = e.to_string();
        assert!(message.contains("`north`"), "{message}");
        assert!(message.contains("multiple = true"), "{message}");
    }

    #[test]
    fn multiple_admits_the_fan_out_and_orders_it() {
        let dupes = Table::new(vec![
            Column::text("region", vec![Some("north".into()), Some("north".into())]),
            Column::int("head_count", vec![Some(3), Some(4)]),
        ])
        .unwrap();
        let mut spec = on_region(How::Inner);
        spec.multiple = true;
        let f = join(&sales(), &dupes, &spec).unwrap();
        // Two north sales rows, two north dimension rows: four rows, the left's order
        // outermost and the right's within it. Deterministic, because a join that reordered
        // itself between runs would digest differently and repaint a page that did not move.
        assert_eq!(f.rows(), 4);
        let got: Vec<(f64, i64)> = (0..4)
            .map(|r| {
                (
                    cell(&*f, "amount", r).as_float().unwrap(),
                    cell(&*f, "head_count", r).as_int().unwrap(),
                )
            })
            .collect();
        assert_eq!(got, vec![(10.0, 3), (10.0, 4), (5.0, 3), (5.0, 4)]);
    }

    #[test]
    fn semi_never_fans_out_so_it_never_needs_permission_to() {
        let dupes = Table::new(vec![
            Column::text("region", vec![Some("north".into()), Some("north".into())]),
            Column::int("head_count", vec![Some(3), Some(4)]),
        ])
        .unwrap();
        let f = join(&sales(), &dupes, &on_region(How::Semi)).unwrap();
        assert_eq!(f.rows(), 2, "the two north rows, once each");
    }

    #[test]
    fn a_key_of_two_different_types_is_refused_rather_than_matched_across() {
        let by_number = Table::new(vec![
            Column::int("region", vec![Some(1)]),
            Column::int("n", vec![Some(1)]),
        ])
        .unwrap();
        let e = join(&sales(), &by_number, &on_region(How::Inner)).unwrap_err();
        assert!(
            e.to_string().contains("text on the left")
                && e.to_string().contains("int on the right"),
            "{e}"
        );
    }

    #[test]
    fn a_missing_key_column_says_which_side_it_is_missing_from() {
        let mut spec = on_region(How::Inner);
        spec.left_on = vec!["regoin".to_string()];
        let e = join(&sales(), &regions(), &spec).unwrap_err();
        let message = e.to_string();
        assert!(message.contains("the left-hand table"), "{message}");
        assert!(message.contains("did you mean `region`?"), "{message}");

        let mut spec = on_region(How::Inner);
        spec.right_on = vec!["head_cont".to_string()];
        let e = join(&sales(), &regions(), &spec).unwrap_err();
        let message = e.to_string();
        assert!(message.contains("the right-hand table"), "{message}");
        assert!(message.contains("did you mean `head_count`?"), "{message}");
    }

    #[test]
    fn a_column_on_both_sides_is_an_error_until_a_suffix_resolves_it() {
        let clash = Table::new(vec![
            Column::text("region", vec![Some("north".into())]),
            Column::float("amount", vec![Some(7.0)]),
        ])
        .unwrap();
        let e = join(&sales(), &clash, &on_region(How::Inner)).unwrap_err();
        assert!(
            e.to_string().contains("both tables have a column `amount`"),
            "{e}"
        );

        let mut spec = on_region(How::Inner);
        spec.suffix = "_plan".to_string();
        let f = join(&sales(), &clash, &spec).unwrap();
        assert_eq!(
            f.column_names(),
            vec!["region", "amount", "units", "amount_plan"]
        );
        assert_eq!(cell(&*f, "amount_plan", 0).as_float(), Some(7.0));
    }

    #[test]
    fn keys_may_have_different_names_on_the_two_sides() {
        let lookup = Table::new(vec![
            Column::text("name", vec![Some("north".into()), Some("south".into())]),
            Column::int("head_count", vec![Some(3), Some(5)]),
        ])
        .unwrap();
        let spec = Join {
            left_on: vec!["region".to_string()],
            right_on: vec!["name".to_string()],
            how: How::Inner,
            suffix: String::new(),
            multiple: false,
        };
        let f = join(&sales(), &lookup, &spec).unwrap();
        // The left's name survives; the right's key is dropped, not carried and not renamed.
        assert_eq!(
            f.column_names(),
            vec!["region", "amount", "units", "head_count"]
        );
    }

    #[test]
    fn a_composite_key_matches_on_every_part() {
        let left = Table::new(vec![
            Column::text("a", vec![Some("x".into()), Some("x".into())]),
            Column::int("b", vec![Some(1), Some(2)]),
        ])
        .unwrap();
        let right = Table::new(vec![
            Column::text("a", vec![Some("x".into())]),
            Column::int("b", vec![Some(2)]),
            Column::text("tag", vec![Some("hit".into())]),
        ])
        .unwrap();
        let spec = Join {
            left_on: vec!["a".to_string(), "b".to_string()],
            right_on: vec!["a".to_string(), "b".to_string()],
            how: How::Left,
            suffix: String::new(),
            multiple: false,
        };
        let f = join(&left, &right, &spec).unwrap();
        assert!(
            matches!(cell(&*f, "tag", 0), Value::Null),
            "x/1 does not match x/2"
        );
        assert_eq!(cell(&*f, "tag", 1).as_text(), Some("hit"));
    }

    #[test]
    fn join_schema_is_the_schema_the_join_produces() {
        // The invariant that lets `dagpane check` reject a bad join before the app runs:
        // the checker and the run time call the same function.
        for how in [How::Inner, How::Left, How::Semi, How::Anti] {
            let spec = on_region(how);
            let predicted = join_schema(&sales().schema(), &regions().schema(), &spec).unwrap();
            let actual = join(&sales(), &regions(), &spec).unwrap().schema();
            assert_eq!(predicted, actual, "{how:?}");
        }
    }

    #[test]
    fn an_empty_right_hand_table_is_not_an_error() {
        let empty = Table::new(vec![
            Column::text("region", vec![]),
            Column::int("head_count", vec![]),
        ])
        .unwrap();
        assert_eq!(
            join(&sales(), &empty, &on_region(How::Inner))
                .unwrap()
                .rows(),
            0
        );
        assert_eq!(
            join(&sales(), &empty, &on_region(How::Left))
                .unwrap()
                .rows(),
            4
        );
        assert_eq!(
            join(&sales(), &empty, &on_region(How::Anti))
                .unwrap()
                .rows(),
            4
        );
    }

    #[test]
    fn a_join_reads_a_derived_column_like_any_other() {
        let derived = derived("amount * 2", &[]).unwrap();
        let f = join(&*derived, &regions(), &on_region(How::Inner)).unwrap();
        assert_eq!(cell(&*f, "out", 1).as_float(), Some(60.0));
        assert_eq!(cell(&*f, "head_count", 1).as_int(), Some(5));
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
    fn ordering_is_what_sort_took_and_a_shifted_column_ranks_alike() {
        // The two halves of what a sort constraint rests on. First: `ordering` is exactly the
        // permutation `sort` applied, so recording one and taking the other cannot drift.
        let t = sales();
        let order = ordering(&t, "amount", false).unwrap();
        let taken = <Table as Frame>::take_rows(&t, &order);
        assert!(same(&taken, &sort(&t, "amount", false).unwrap()));

        // Second, and the reason the constraint pays at all: a column every value of which
        // moved by the same amount ranks the rows identically, so the ordering digest is
        // unchanged while the column's own digest is not.
        let shifted = Table::new(vec![Column::float(
            "amount",
            (0..t.rows())
                .map(|r| cell(&t, "amount", r).as_float().map(|v| v + 1000.0))
                .collect(),
        )])
        .unwrap();
        assert_eq!(
            ordering_digest(&ordering(&shifted, "amount", false).unwrap()),
            ordering_digest(&order),
            "shifting every value by the same amount must not move the ranking"
        );
        assert_ne!(
            crate::frame::column_digest(&t, 1),
            crate::frame::column_digest(&shifted, 0),
            "...while the column itself has demonstrably moved"
        );
    }

    #[test]
    fn a_selection_and_an_ordering_of_the_same_rows_digest_differently() {
        // A permutation and a selection can be the same list of integers while meaning
        // entirely different things. Tagging them apart is what stops one constraint's answer
        // from ever validating the other's question.
        let rows = [0usize, 2, 5];
        assert_ne!(selection_digest(&rows), ordering_digest(&rows));
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
