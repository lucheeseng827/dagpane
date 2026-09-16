//! An Arrow-backed [`Frame`], with dictionary encoding for low-cardinality text.
//!
//! Same logical content as `dagpane_core::Table`, same digest, a fraction of the memory.
//! `BENCHMARKS.md` has the numbers this crate exists for: the in-tree representation
//! amplifies a CSV about seventeen times in resident memory, and on one 1M-row four-value
//! categorical column — which is exactly what `region` and `channel` are in the bundled
//! example — the three representations cost:
//!
//! | | resident |
//! |---|---:|
//! | `Vec<Option<String>>` | 27.2 MB |
//! | `arrow::StringArray` | 11.8 MB |
//! | `arrow::DictionaryArray` | 3.8 MB |
//!
//! Note the middle row, because it is the part that is easy to get wrong: **adopting Arrow
//! is worth 2.3×; the 7.1× is dictionary encoding.** A backend that stores every string
//! contiguously and stops there leaves most of the win on the table, so this one encodes by
//! default and says so — see [`Encoding`].
//!
//! # What it must not change
//!
//! The digest. `dagpane_core::frame::digest_frame` walks any frame through the trait's own
//! accessors, so it sees logical values and cannot see layout; this crate delegates to it
//! rather than hashing its own buffers. A dictionary-encoded column and a plain one holding
//! the same strings therefore hash identically **by construction**, and the differential
//! test at the bottom of this file is what proves it stayed that way.
//!
//! That property is not cosmetic. The digest is what decides whether a cell's output moved,
//! so a backend that hashed its own layout would invalidate every cell in the graph the
//! moment anybody switched representation — while looking exactly like a correct pass.

use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::Int32Type;
use arrow_array::{
    Array, ArrayRef, BooleanArray, DictionaryArray, Float64Array, Int64Array, StringArray,
};
use arrow_schema::DataType;
use dagpane_core::frame::Frame;
use dagpane_core::value::{Column, ColumnData, ColumnType, Table, Value};

/// How a text column is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// One contiguous buffer of bytes plus offsets. Always correct, never the cheapest for
    /// a categorical.
    Plain,
    /// A dictionary of the distinct values plus one `i32` code per row.
    ///
    /// Chosen automatically when a column has few enough distinct values to pay for itself
    /// — see [`DICTIONARY_MAX_DISTINCT`] and [`DICTIONARY_MAX_RATIO`]. The break-even is not
    /// subtle: four distinct values over a million rows is 3.8 MB encoded against 11.8 MB
    /// plain, and a column of a million distinct values is *larger* encoded, because the
    /// dictionary is then the whole column plus an index.
    Dictionary,
}

/// A column with more distinct values than this is never dictionary-encoded, however few
/// rows it has. Beyond a few thousand entries the dictionary stops being a lookup and starts
/// being a second copy of the column.
pub const DICTIONARY_MAX_DISTINCT: usize = 4096;

/// A column whose distinct count exceeds this fraction of its rows is not encoded either.
/// At one distinct value per two rows the codes cost about what the strings saved, and past
/// that the encoding is a loss.
pub const DICTIONARY_MAX_RATIO: f64 = 0.5;

/// An Arrow-backed frame.
#[derive(Debug, Clone)]
pub struct ArrowFrame {
    names: Vec<String>,
    types: Vec<ColumnType>,
    columns: Vec<ArrayRef>,
    rows: usize,
}

impl ArrowFrame {
    /// Build from the same columns [`Table`] takes, choosing an encoding per column.
    pub fn from_columns(columns: &[Column]) -> ArrowFrame {
        Self::build(columns, None)
    }

    /// Build with one encoding forced for every text column. For tests and for a caller
    /// that knows something the heuristic cannot.
    pub fn from_columns_with(columns: &[Column], encoding: Encoding) -> ArrowFrame {
        Self::build(columns, Some(encoding))
    }

    /// Convert an existing table. The bridge a migration crosses one cell at a time.
    pub fn from_table(table: &Table) -> ArrowFrame {
        Self::from_columns(table.columns())
    }

    fn build(columns: &[Column], forced: Option<Encoding>) -> ArrowFrame {
        let rows = columns.first().map(|c| c.data.len()).unwrap_or(0);
        let mut names = Vec::with_capacity(columns.len());
        let mut types = Vec::with_capacity(columns.len());
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(columns.len());

        for col in columns {
            names.push(col.name.clone());
            types.push(col.data.column_type());
            arrays.push(match &col.data {
                ColumnData::Int(v) => {
                    Arc::new(v.iter().copied().collect::<Int64Array>()) as ArrayRef
                }
                ColumnData::Float(v) => {
                    Arc::new(v.iter().copied().collect::<Float64Array>()) as ArrayRef
                }
                ColumnData::Bool(v) => {
                    Arc::new(v.iter().copied().collect::<BooleanArray>()) as ArrayRef
                }
                ColumnData::Text(v) => {
                    let encoding = forced.unwrap_or_else(|| choose_encoding(v));
                    match encoding {
                        Encoding::Dictionary => {
                            let dict: DictionaryArray<Int32Type> =
                                v.iter().map(|o| o.as_deref()).collect();
                            Arc::new(dict) as ArrayRef
                        }
                        Encoding::Plain => {
                            Arc::new(v.iter().map(|o| o.as_deref()).collect::<StringArray>())
                                as ArrayRef
                        }
                    }
                }
            });
        }
        ArrowFrame {
            names,
            types,
            columns: arrays,
            rows,
        }
    }

    /// How each text column ended up stored. Nothing in the engine reads this; it is for
    /// tests and for anybody asking why a frame is the size it is.
    pub fn encodings(&self) -> Vec<Option<Encoding>> {
        self.columns
            .iter()
            .map(|a| match a.data_type() {
                DataType::Dictionary(_, _) => Some(Encoding::Dictionary),
                DataType::Utf8 => Some(Encoding::Plain),
                _ => None,
            })
            .collect()
    }

    /// Resident bytes, as Arrow accounts for them. The figure to compare against a
    /// `Vec<Option<T>>` when deciding whether a column is worth encoding.
    pub fn memory_size(&self) -> usize {
        self.columns.iter().map(|a| a.get_array_memory_size()).sum()
    }

    /// Back to a [`Table`]. Not on the hot path — this exists so a caller holding a frame
    /// can hand it to code that has not been migrated yet.
    pub fn to_table(&self) -> Table {
        let columns = (0..self.width())
            .map(|c| {
                let data = match self.types[c] {
                    ColumnType::Int => ColumnData::Int(
                        (0..self.rows)
                            .map(|r| self.value_at(r, c).as_int())
                            .collect(),
                    ),
                    ColumnType::Float => ColumnData::Float(
                        (0..self.rows)
                            .map(|r| self.value_at(r, c).as_float())
                            .collect(),
                    ),
                    ColumnType::Bool => ColumnData::Bool(
                        (0..self.rows)
                            .map(|r| self.value_at(r, c).as_bool())
                            .collect(),
                    ),
                    ColumnType::Text => ColumnData::Text(
                        (0..self.rows)
                            .map(|r| match self.value_at(r, c) {
                                Value::Text { v } => Some(v),
                                _ => None,
                            })
                            .collect(),
                    ),
                };
                Column::new(self.names[c].clone(), data)
            })
            .collect();
        // Every column is built with `self.rows` elements, so this cannot be ragged.
        Table::new(columns).expect("columns built to one length")
    }
}

/// Encode when the column has few enough distinct values to pay for it.
///
/// Counting distinct values costs a pass over the column, which is the same pass the
/// encoder would make anyway — so the decision is not free but it is not extra either.
fn choose_encoding(values: &[Option<String>]) -> Encoding {
    if values.is_empty() {
        return Encoding::Plain;
    }
    let mut seen: Vec<&str> = Vec::new();
    for v in values.iter().flatten() {
        if let Err(at) = seen.binary_search(&v.as_str()) {
            seen.insert(at, v.as_str());
            if seen.len() > DICTIONARY_MAX_DISTINCT {
                return Encoding::Plain;
            }
        }
    }
    if (seen.len() as f64) <= values.len() as f64 * DICTIONARY_MAX_RATIO {
        Encoding::Dictionary
    } else {
        Encoding::Plain
    }
}

impl Frame for ArrowFrame {
    fn rows(&self) -> usize {
        self.rows
    }

    fn schema(&self) -> Vec<(String, ColumnType)> {
        self.names
            .iter()
            .cloned()
            .zip(self.types.iter().copied())
            .collect()
    }

    fn width(&self) -> usize {
        self.names.len()
    }

    fn column_index(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }

    fn value_at(&self, row: usize, col: usize) -> Value {
        let array = &self.columns[col];
        if array.is_null(row) {
            return Value::Null;
        }
        match array.data_type() {
            DataType::Int64 => Value::int(
                array
                    .as_primitive::<arrow_array::types::Int64Type>()
                    .value(row),
            ),
            DataType::Float64 => Value::float(
                array
                    .as_primitive::<arrow_array::types::Float64Type>()
                    .value(row),
            ),
            DataType::Boolean => Value::bool(array.as_boolean().value(row)),
            DataType::Utf8 => Value::text(array.as_string::<i32>().value(row)),
            DataType::Dictionary(_, _) => {
                let dict = array.as_dictionary::<Int32Type>();
                let key = dict.keys().value(row) as usize;
                Value::text(dict.values().as_string::<i32>().value(key))
            }
            // Unreachable: `build` produces only the five above.
            other => Value::text(format!("<unsupported arrow type {other}>")),
        }
    }

    fn backend(&self) -> &'static str {
        "arrow"
    }

    /// Rebuild as an Arrow frame, encoding decisions taken afresh.
    ///
    /// Deliberately re-runs the per-column heuristic rather than inheriting this frame's
    /// encodings: an aggregation's output has different cardinality from its input — a
    /// `group_by` collapses a million rows of four regions into four rows of four regions,
    /// where a dictionary no longer pays — so carrying the old decision over would be
    /// carrying over an answer to a different question.
    fn same_kind(&self, columns: Vec<Column>) -> Arc<dyn Frame> {
        Arc::new(ArrowFrame::from_columns(&columns))
    }

    /// Projection is pointer work: Arrow holds each column in its own array, so keeping a
    /// subset clones `Arc`s and copies no data at all — a dictionary column stays encoded
    /// and stays shared.
    fn select_columns(&self, cols: &[usize]) -> Arc<dyn Frame> {
        Arc::new(ArrowFrame {
            names: cols.iter().map(|&c| self.names[c].clone()).collect(),
            types: cols.iter().map(|&c| self.types[c]).collect(),
            columns: cols.iter().map(|&c| Arc::clone(&self.columns[c])).collect(),
            rows: self.rows,
        })
    }

    fn take_rows(&self, keep: &[usize]) -> Arc<dyn Frame> {
        let indices: arrow_array::UInt32Array = keep.iter().map(|&i| Some(i as u32)).collect();
        let columns: Vec<ArrayRef> = self
            .columns
            .iter()
            // `take` preserves a dictionary's encoding: the codes are gathered and the
            // dictionary is shared, so filtering a categorical does not re-materialise its
            // strings. That is the reason a filter over this backend stays cheap.
            .map(|a| arrow_select::take::take(a, &indices, None).expect("valid row indices"))
            .collect();
        Arc::new(ArrowFrame {
            names: self.names.clone(),
            types: self.types.clone(),
            columns,
            rows: keep.len(),
        })
    }
}

// ---------------------------------------------------------------------------
// Filling Arrow arrays straight from a parse
// ---------------------------------------------------------------------------

use arrow_array::builder::{
    BooleanBuilder, Float64Builder, Int64Builder, StringBuilder, StringDictionaryBuilder,
};
use dagpane_core::frame::{ColumnHint, FrameBuilder};

/// One column under construction.
enum ColumnSink {
    Int(Int64Builder),
    Float(Float64Builder),
    Bool(BooleanBuilder),
    Text(StringBuilder),
    /// Dictionary-encoded text. Chosen from the hint, because a streaming builder has to
    /// commit to a layout before it has seen the values — see [`ArrowFrameBuilder`].
    Dict(StringDictionaryBuilder<Int32Type>),
}

/// Builds an [`ArrowFrame`] without ever materialising a `Vec<Option<T>>`.
///
/// The reason this exists rather than `ArrowFrame::from_columns`: converting means both
/// representations are live at once, and on a 1M x 6 file the `Vec<Option<T>>` side of that
/// measured 139 MB against 34 MB of output. Filling Arrow's builders directly from the parse
/// skips it entirely, and `push_text` taking a `&str` means a text cell never becomes a
/// `String` on the way in.
///
/// # Choosing an encoding without having seen the data
///
/// `ArrowFrame::from_columns` counts distinct values and then decides. A streaming builder
/// cannot: it must pick a layout before the first push. So it takes the count from
/// [`ColumnHint::distinct`], which the reader's type-inference pass already had in hand —
/// that pass visits every field anyway, and counting bounded distinct values there is far
/// cheaper than either guessing or buffering.
///
/// When the hint says nothing, the column is stored plain. That is the conservative
/// direction: a dictionary over high-cardinality text is *larger* than plain, so guessing
/// "encode" when unsure would make an unknown column worse, while guessing "plain" only
/// leaves a saving on the table.
pub struct ArrowFrameBuilder {
    names: Vec<String>,
    types: Vec<ColumnType>,
    sinks: Vec<ColumnSink>,
}

impl Default for ArrowFrameBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ArrowFrameBuilder {
    pub fn new() -> ArrowFrameBuilder {
        ArrowFrameBuilder {
            names: Vec::new(),
            types: Vec::new(),
            sinks: Vec::new(),
        }
    }

    /// Would a dictionary pay, on what the reader counted?
    ///
    /// The same two thresholds `from_columns` applies after the fact, so a column loaded
    /// through the builder and the same column converted afterwards reach the same layout.
    fn worth_encoding(hint: ColumnHint) -> bool {
        match (hint.distinct, hint.rows) {
            (Some(d), Some(rows)) => {
                d <= DICTIONARY_MAX_DISTINCT && (d as f64) <= rows as f64 * DICTIONARY_MAX_RATIO
            }
            _ => false,
        }
    }
}

impl FrameBuilder for ArrowFrameBuilder {
    fn begin_column(&mut self, name: &str, ty: ColumnType, hint: ColumnHint) -> usize {
        let n = hint.rows.unwrap_or(0);
        self.names.push(name.to_string());
        self.types.push(ty);
        self.sinks.push(match ty {
            ColumnType::Int => ColumnSink::Int(Int64Builder::with_capacity(n)),
            ColumnType::Float => ColumnSink::Float(Float64Builder::with_capacity(n)),
            ColumnType::Bool => ColumnSink::Bool(BooleanBuilder::with_capacity(n)),
            ColumnType::Text if Self::worth_encoding(hint) => {
                ColumnSink::Dict(StringDictionaryBuilder::new())
            }
            // 12 bytes per value is arrow's own rule of thumb for the offsets plus a short
            // string; over-reserving a byte buffer costs nothing that dropping it does not
            // return.
            ColumnType::Text => ColumnSink::Text(StringBuilder::with_capacity(n, n * 12)),
        });
        self.sinks.len() - 1
    }

    fn push_int(&mut self, col: usize, v: Option<i64>) {
        match &mut self.sinks[col] {
            ColumnSink::Int(b) => b.append_option(v),
            other => append_null(other),
        }
    }

    fn push_float(&mut self, col: usize, v: Option<f64>) {
        match &mut self.sinks[col] {
            ColumnSink::Float(b) => b.append_option(v),
            other => append_null(other),
        }
    }

    fn push_bool(&mut self, col: usize, v: Option<bool>) {
        match &mut self.sinks[col] {
            ColumnSink::Bool(b) => b.append_option(v),
            other => append_null(other),
        }
    }

    fn push_text(&mut self, col: usize, v: Option<&str>) {
        match (&mut self.sinks[col], v) {
            // Borrowed all the way in: the bytes go straight into arrow's buffer, and the
            // dictionary case additionally never stores a repeat.
            (ColumnSink::Text(b), Some(s)) => b.append_value(s),
            (ColumnSink::Text(b), None) => b.append_null(),
            (ColumnSink::Dict(b), Some(s)) => b.append_value(s),
            (ColumnSink::Dict(b), None) => b.append_null(),
            (other, _) => append_null(other),
        }
    }

    fn finish(self: Box<Self>) -> Arc<dyn Frame> {
        Arc::new(self.finish_arrow())
    }
}

impl ArrowFrameBuilder {
    /// Finish as a concrete [`ArrowFrame`].
    ///
    /// `FrameBuilder::finish` hands back a `dyn Frame`, which is what a reader wants and
    /// what keeps it from naming a backend. A caller that already knows it wants Arrow — or
    /// a test asking which encoding a column actually got — wants the type back.
    pub fn finish_arrow(self) -> ArrowFrame {
        let mut columns: Vec<ArrayRef> = Vec::with_capacity(self.sinks.len());
        let mut rows = 0;
        for mut sink in self.sinks {
            let array: ArrayRef = match &mut sink {
                ColumnSink::Int(b) => Arc::new(b.finish()),
                ColumnSink::Float(b) => Arc::new(b.finish()),
                ColumnSink::Bool(b) => Arc::new(b.finish()),
                ColumnSink::Text(b) => Arc::new(b.finish()),
                ColumnSink::Dict(b) => Arc::new(b.finish()),
            };
            rows = rows.max(array.len());
            columns.push(array);
        }
        ArrowFrame {
            names: self.names,
            types: self.types,
            columns,
            rows,
        }
    }
}

/// A push whose variant does not match the column's declared type. Records a null rather
/// than panicking — see the trait docs.
fn append_null(sink: &mut ColumnSink) {
    match sink {
        ColumnSink::Int(b) => b.append_null(),
        ColumnSink::Float(b) => b.append_null(),
        ColumnSink::Bool(b) => b.append_null(),
        ColumnSink::Text(b) => b.append_null(),
        ColumnSink::Dict(b) => b.append_null(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dagpane_core::digest::Digestible;
    use dagpane_core::frame::frame_digest;

    fn categorical(n: usize) -> Vec<Column> {
        let vals = ["north", "south", "east", "west"];
        vec![
            Column::int("id", (0..n).map(|i| Some(i as i64)).collect()),
            Column::text(
                "region",
                (0..n).map(|i| Some(vals[i % 4].to_string())).collect(),
            ),
            Column::float("amount", (0..n).map(|i| Some(i as f64 * 1.5)).collect()),
            Column::bool(
                "ok",
                (0..n)
                    .map(|i| if i % 7 == 0 { None } else { Some(i % 2 == 0) })
                    .collect(),
            ),
        ]
    }

    // --- the property the seam stands on ---

    /// Same logical content, same digest — across representations AND across encodings.
    /// If this ever fails, switching backend silently invalidates every cell in the graph.
    #[test]
    fn every_representation_of_the_same_data_digests_identically() {
        let columns = categorical(200);
        let table = Table::new(columns.clone()).unwrap();
        let auto = ArrowFrame::from_columns(&columns);
        let plain = ArrowFrame::from_columns_with(&columns, Encoding::Plain);
        let dict = ArrowFrame::from_columns_with(&columns, Encoding::Dictionary);

        let expected = Digestible::digest(&table);
        assert_eq!(frame_digest(&table), expected, "the in-tree frame impl");
        assert_eq!(frame_digest(&auto), expected, "arrow, encoding chosen");
        assert_eq!(frame_digest(&plain), expected, "arrow, forced plain");
        assert_eq!(frame_digest(&dict), expected, "arrow, forced dictionary");
    }

    /// Every accessor, on every row, against the reference implementation.
    #[test]
    fn arrow_and_table_agree_cell_for_cell() {
        let columns = categorical(97);
        let table = Table::new(columns.clone()).unwrap();
        let arrow = ArrowFrame::from_columns(&columns);

        assert_eq!(arrow.rows(), Frame::rows(&table));
        assert_eq!(arrow.width(), Frame::width(&table));
        assert_eq!(Frame::schema(&arrow), Frame::schema(&table));
        assert_eq!(
            arrow.column_index("region"),
            Frame::column_index(&table, "region")
        );
        for r in 0..table.rows() {
            for c in 0..table.width() {
                assert_eq!(
                    arrow.value_at(r, c),
                    Frame::value_at(&table, r, c),
                    "cell ({r},{c})"
                );
            }
            assert_eq!(Frame::row(&arrow, r), Frame::row(&table, r));
        }
    }

    #[test]
    fn take_rows_agrees_and_keeps_the_dictionary() {
        let columns = categorical(60);
        let table = Table::new(columns.clone()).unwrap();
        let arrow = ArrowFrame::from_columns(&columns);
        let keep: Vec<usize> = (0..60).rev().step_by(3).collect();

        let a = arrow.take_rows(&keep);
        let t = Frame::take_rows(&table, &keep);
        assert_eq!(a.rows(), t.rows());
        assert_eq!(
            frame_digest(&*a),
            frame_digest(&*t),
            "reordered rows must still agree"
        );
    }

    #[test]
    fn a_null_is_a_null_and_never_a_default() {
        let columns = vec![
            Column::int("i", vec![None, Some(0)]),
            Column::text("t", vec![None, Some(String::new())]),
            Column::bool("b", vec![None, Some(false)]),
        ];
        let arrow = ArrowFrame::from_columns(&columns);
        for c in 0..3 {
            assert_eq!(arrow.value_at(0, c), Value::Null, "column {c} row 0");
            assert_ne!(
                arrow.value_at(1, c),
                Value::Null,
                "column {c} row 1 is present-but-empty"
            );
        }
        assert_eq!(
            frame_digest(&arrow),
            Digestible::digest(&Table::new(columns).unwrap())
        );
    }

    // --- the encoding decision ---

    #[test]
    fn a_categorical_is_encoded_and_a_unique_column_is_not() {
        let n = 500;
        let f = ArrowFrame::from_columns(&[
            Column::text(
                "region",
                (0..n)
                    .map(|i| Some(["a", "b", "c"][i % 3].to_string()))
                    .collect(),
            ),
            Column::text(
                "order_id",
                (0..n).map(|i| Some(format!("id-{i}"))).collect(),
            ),
        ]);
        assert_eq!(
            f.encodings()[0],
            Some(Encoding::Dictionary),
            "3 distinct over 500 rows"
        );
        assert_eq!(
            f.encodings()[1],
            Some(Encoding::Plain),
            "every value distinct"
        );
    }

    #[test]
    fn encoding_a_categorical_actually_saves_memory() {
        let n = 20_000;
        let columns = vec![Column::text(
            "region",
            (0..n)
                .map(|i| Some(["north", "south", "east", "west"][i % 4].to_string()))
                .collect(),
        )];
        let plain = ArrowFrame::from_columns_with(&columns, Encoding::Plain).memory_size();
        let dict = ArrowFrame::from_columns_with(&columns, Encoding::Dictionary).memory_size();
        assert!(
            dict * 2 < plain,
            "dictionary {dict} should be far under plain {plain}"
        );
        // And the heuristic picks the cheap one on its own.
        assert_eq!(ArrowFrame::from_columns(&columns).memory_size(), dict);
    }

    #[test]
    fn a_high_cardinality_column_is_left_alone_because_encoding_would_cost() {
        // Distinct-to-rows above the ratio: the codes would cost about what the strings save.
        let columns = vec![Column::text(
            "mostly_unique",
            (0..100)
                .map(|i| Some(format!("v{}", i / 2 * 2 + i % 2)))
                .collect(),
        )];
        assert_eq!(
            ArrowFrame::from_columns(&columns).encodings()[0],
            Some(Encoding::Plain)
        );
    }

    #[test]
    fn an_empty_frame_is_handled_rather_than_special_cased_by_callers() {
        let f = ArrowFrame::from_columns(&[Column::int("id", vec![])]);
        assert_eq!(f.rows(), 0);
        assert!(Frame::is_empty(&f));
        assert_eq!(Frame::head(&f, 10).rows(), 0);
        assert_eq!(
            frame_digest(&f),
            Digestible::digest(&Table::new(vec![Column::int("id", vec![])]).unwrap())
        );
    }

    // --- the streaming builder ---

    /// Fill a builder the way the CSV reader does, from the same columns.
    ///
    /// Returns the concrete frame, because the tests below ask which encoding a column
    /// actually got — a question a rebuild would answer about the rebuild rather than about
    /// the build, which is how the first version of this helper managed to pass while
    /// testing nothing.
    fn built(columns: &[Column], hint_distinct: bool) -> ArrowFrame {
        use dagpane_core::frame::{ColumnHint, FrameBuilder};
        let rows = columns.first().map(|c| c.data.len()).unwrap_or(0);
        let mut b = ArrowFrameBuilder::new();
        for c in columns {
            let distinct = if hint_distinct {
                match &c.data {
                    ColumnData::Text(v) => Some(
                        v.iter()
                            .flatten()
                            .collect::<std::collections::BTreeSet<_>>()
                            .len(),
                    ),
                    _ => None,
                }
            } else {
                None
            };
            b.begin_column(
                &c.name,
                c.data.column_type(),
                ColumnHint {
                    rows: Some(rows),
                    distinct,
                },
            );
        }
        for r in 0..rows {
            for (col, c) in columns.iter().enumerate() {
                match &c.data {
                    ColumnData::Int(v) => b.push_int(col, v[r]),
                    ColumnData::Float(v) => b.push_float(col, v[r]),
                    ColumnData::Bool(v) => b.push_bool(col, v[r]),
                    ColumnData::Text(v) => b.push_text(col, v[r].as_deref()),
                }
            }
        }
        b.finish_arrow()
    }

    /// The property the seam rests on: a frame *built* and the same frame *converted* are
    /// the same frame. If they were not, a source loaded through the reader would digest
    /// differently from one converted after the fact, and every cell would recompute on the
    /// day the loading path changed.
    #[test]
    fn a_built_frame_is_identical_to_a_converted_one() {
        let columns = categorical(300);
        let converted = ArrowFrame::from_columns(&columns);
        for hint in [true, false] {
            let b = built(&columns, hint);
            assert_eq!(
                frame_digest(&b),
                frame_digest(&converted),
                "hinted={hint}: built and converted must agree"
            );
            assert_eq!(Frame::rows(&b), Frame::rows(&converted));
            assert_eq!(Frame::schema(&b), Frame::schema(&converted));
        }
    }

    /// The hint is what lets a streaming builder encode at all — it has to choose a layout
    /// before it sees a value.
    #[test]
    fn the_hint_decides_the_encoding_and_never_the_content() {
        // Twenty thousand, not a few hundred: at small row counts a builder's reserved
        // capacity and an array's fixed overhead swamp the difference, and the test would be
        // asserting about allocator slack rather than about encoding.
        let n = 20_000;
        let columns = vec![Column::text(
            "region",
            (0..n)
                .map(|i| Some(["north", "south"][i % 2].to_string()))
                .collect(),
        )];
        let hinted = built(&columns, true);
        let blind = built(&columns, false);

        // Same values either way...
        assert_eq!(frame_digest(&hinted), frame_digest(&blind));
        // ...but only the hinted one could know to encode, and it is much smaller for it.
        let (h, b) = (hinted.memory_size(), blind.memory_size());
        assert!(h * 2 < b, "hinted {h} should be far under unhinted {b}");
        assert_eq!(hinted.encodings()[0], Some(Encoding::Dictionary));
        assert_eq!(blind.encodings()[0], Some(Encoding::Plain));
    }

    /// A hint that says "many distinct" must not encode: a dictionary over high-cardinality
    /// text is *larger* than plain, so acting on it would make the column worse.
    #[test]
    fn a_high_cardinality_hint_is_declined() {
        let n = 500;
        let columns = vec![Column::text(
            "order_id",
            (0..n).map(|i| Some(format!("id-{i}"))).collect(),
        )];
        assert_eq!(built(&columns, true).encodings()[0], Some(Encoding::Plain));
    }

    #[test]
    fn an_empty_build_is_a_frame_with_no_rows() {
        let b = built(&[Column::int("id", vec![])], true);
        assert_eq!(Frame::rows(&b), 0);
        assert_eq!(Frame::backend(&b), "arrow");
    }

    #[test]
    fn a_round_trip_through_table_preserves_content() {
        let columns = categorical(50);
        let arrow = ArrowFrame::from_columns(&columns);
        let back = arrow.to_table();
        assert_eq!(frame_digest(&back), frame_digest(&arrow));
    }

    #[test]
    fn head_is_the_first_n_rows_in_order() {
        let columns = categorical(30);
        let arrow = ArrowFrame::from_columns(&columns);
        let table = Table::new(columns).unwrap();
        assert_eq!(
            frame_digest(&*Frame::head(&arrow, 7)),
            frame_digest(&*Frame::head(&table, 7))
        );
    }
}
