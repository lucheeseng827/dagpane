//! The value model: what travels along an edge in the graph.
//!
//! Two rules decide everything in this file.
//!
//! **Every value is digestible.** The engine never compares two values; it compares two
//! [`Digest`](crate::digest::Digest)s taken when each was produced (see [`crate::digest`]). So a type may only
//! enter this enum if its content can be hashed deterministically — which is why there is
//! no `Value::Any(Box<dyn Any>)` escape hatch, tempting as one is.
//!
//! **The table is deliberately small.** [`Table`] is a plain columnar container:
//! `Vec<Option<T>>` per column, sixteen bytes for an `i64`, no chunking, no dictionary
//! encoding, no SIMD. It is not Arrow and does not pretend to be. It exists because taking
//! a real dataframe dependency in v0.1.0 would have decided the wasm question by accident
//! and would have implied a capability the rest of the code does not have. The seam where a
//! real engine replaces it is described in ARCHITECTURE.md §6 — deliberately as prose and
//! not as a `Frame` trait, because a trait with one implementation and no second caller is
//! speculative generality. The honest limit is stated in the README rather than hidden here: this
//! layout is fine for the hundreds-of-thousands-of-rows apps the runtime targets and is the
//! wrong tool above that.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::digest::{Digestible, Hasher};

/// The scalar and tabular values a cell can produce.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Value {
    /// No value — and a successful one. A cell that returns `Null` has computed; a cell
    /// that has not computed is not a `Value` at all but an [`crate::session::Outcome`],
    /// and a failed one carries a [`crate::error::CellError`]. `Null` is also what a null
    /// table element reads as, so a missing measurement never arrives at a cell as a zero.
    Null,
    /// A boolean.
    Bool {
        /// The boolean. Tagged before it is hashed, so it cannot digest as the `Int` 0 or 1
        /// and silently fail to wake the cells below it.
        v: bool,
    },
    /// A 64-bit signed integer. Widens to `f64` through [`Value::as_float`], so a slider
    /// resting on a whole number still reads as a number; the reverse does not hold.
    Int {
        /// The integer. The full `i64` range is representable, which is more than JSON
        /// numbers survive in a browser — a cell producing values above 2^53 should send
        /// text if the client is going to display them.
        v: i64,
    },
    /// A 64-bit float.
    Float {
        /// The number. NaN and `-0.0` are allowed and are canonicalised before hashing, so a
        /// cell that recomputes NaN from a different NaN, or `-0.0` where it had `0.0`, does
        /// not wake its dependents. [`crate::digest::Hasher::f64`] has the argument.
        v: f64,
    },
    /// UTF-8 text.
    Text {
        /// The string, hashed length-prefixed so that a boundary moving between two adjacent
        /// strings is a change. Unbounded: a cell may return a megabyte, and will pay for it
        /// once in digest cost when it does.
        v: String,
    },
    /// An ordered, heterogeneous list. Nothing requires the elements to share a type; the
    /// engine only requires that they digest, and every `Value` does.
    List {
        /// The elements, in order. Order is content: swapping two of them changes the digest
        /// and therefore recomputes everything downstream.
        v: Vec<Value>,
    },
    /// A columnar table — the only bulk container in the value model, and the one the
    /// transforms in [`crate::transform`] operate on — held behind
    /// [`crate::frame::Frame`] so the representation is pluggable.
    ///
    /// The variant is `Frame` and the **wire name stays `table`**. That rename is load
    /// bearing: the patch protocol is a published contract — the product site's replay and
    /// every socket test read it — so swapping the representation must be invisible to a
    /// client. A representation change that moved the wire would be a protocol change
    /// wearing a refactor's clothes.
    #[serde(rename = "table")]
    Frame {
        /// The frame. Digested column by column over every element through the trait's own
        /// accessors, so two backends holding the same logical content digest identically;
        /// a cell that appends one row to a large frame pays a full rehash, and that cost,
        /// and why it is accepted, is in [`crate::digest`].
        v: crate::frame::FrameRef,
    },
}

impl Value {
    /// Wraps a boolean. These six constructors exist because the variants are struct-like
    /// for serde's sake, and `Value::Bool { v: true }` is noise at every call site in a
    /// compute closure.
    pub fn bool(v: bool) -> Value {
        Value::Bool { v }
    }
    /// Wraps an integer.
    pub fn int(v: i64) -> Value {
        Value::Int { v }
    }
    /// Wraps a float. No canonicalisation happens here — NaN stays the NaN you passed, and
    /// is only folded when the value is digested.
    pub fn float(v: f64) -> Value {
        Value::Float { v }
    }
    /// Wraps text, taking anything that becomes a `String` so `format!` results and string
    /// literals both work without a turn at the call site.
    pub fn text(v: impl Into<String>) -> Value {
        Value::Text { v: v.into() }
    }
    /// Wraps a list. The elements need not share a type.
    pub fn list(v: Vec<Value>) -> Value {
        Value::List { v }
    }
    /// Wrap an existing frame without copying it — an `Arc` bump.
    pub fn frame(v: std::sync::Arc<dyn crate::frame::Frame>) -> Value {
        Value::Frame {
            v: crate::frame::FrameRef::new(v),
        }
    }
    /// Wraps a table. Use [`Table::new`] to build one: it is the constructor that rejects
    /// columns of unequal length.
    pub fn table(v: Table) -> Value {
        Value::Frame {
            v: crate::frame::FrameRef::from(v),
        }
    }

    /// The boolean, or `None` for any other variant. No coercion: a non-empty string and a
    /// non-zero integer are both `None`, because a cell that meant to branch on a checkbox
    /// and was handed a number should say so rather than guess.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool { v } => Some(*v),
            _ => None,
        }
    }

    /// The integer, or `None`. A `Float` does *not* read as an `Int` even when it holds a
    /// whole number: the rounding rule would have to be invented here and would be wrong for
    /// somebody.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int { v } => Some(*v),
            _ => None,
        }
    }

    /// An `Int` reads as a `Float`, because a slider that happens to be sitting on a whole
    /// number must not change a cell's arithmetic. The reverse is not true.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Float { v } => Some(*v),
            Value::Int { v } => Some(*v as f64),
            _ => None,
        }
    }

    /// The string, or `None`. Nothing is rendered on the way out — [`Value::type_name`] is
    /// what a mismatch message wants, not a stringified value.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text { v } => Some(v),
            _ => None,
        }
    }

    /// The elements, or `None`. Borrowed, because a compute closure usually wants to iterate
    /// and count rather than own.
    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List { v } => Some(v),
            _ => None,
        }
    }

    /// The frame behind a table value, or `None`. Borrowed for the same reason as
    /// [`Value::as_list`], and more so: cloning a frame to read its row count is the one
    /// mistake in this API that costs real time.
    ///
    /// Replaces the old `as_table`, which handed back a `&Table` — a borrow no longer
    /// available now that the representation is behind a trait. Callers that genuinely need
    /// an owned `Table` go through [`crate::frame::FrameRef::to_table`], which copies and
    /// says so; everything on an interaction path should read through the trait instead.
    pub fn as_frame(&self) -> Option<&dyn crate::frame::Frame> {
        match self {
            Value::Frame { v } => Some(v.as_frame()),
            _ => None,
        }
    }

    /// The name used in type-mismatch messages. Short on purpose: it ends up in a cell
    /// error a user reads in a browser.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool { .. } => "bool",
            Value::Int { .. } => "int",
            Value::Float { .. } => "float",
            Value::Text { .. } => "text",
            Value::List { .. } => "list",
            Value::Frame { .. } => "table",
        }
    }
}

impl Digestible for Value {
    fn digest_into(&self, h: &mut Hasher) {
        match self {
            Value::Null => {
                h.tag(0);
            }
            Value::Bool { v } => {
                h.tag(1).bytes(&[*v as u8]);
            }
            Value::Int { v } => {
                h.tag(2).i64(*v);
            }
            Value::Float { v } => {
                h.tag(3).f64(*v);
            }
            Value::Text { v } => {
                h.tag(4).str(v);
            }
            Value::List { v } => {
                h.tag(5).u64(v.len() as u64);
                for item in v {
                    item.digest_into(h);
                }
            }
            Value::Frame { v } => {
                // No extra tag here: `digest_frame` opens with tag(6) itself, which is what
                // `Table::digest_into` always did. Adding one would shift every table
                // digest ever computed.
                crate::frame::digest_frame(v.as_frame(), h);
            }
        }
    }
}

/// A column's element type. Kept to four because every one of them has to be digestible,
/// renderable in a browser, and expressible in a manifest without a type grammar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnType {
    /// 64-bit signed integers.
    Int,
    /// 64-bit floats.
    Float,
    /// UTF-8 strings.
    Text,
    /// Booleans.
    Bool,
}

impl fmt::Display for ColumnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ColumnType::Int => "int",
            ColumnType::Float => "float",
            ColumnType::Text => "text",
            ColumnType::Bool => "bool",
        };
        f.write_str(s)
    }
}

/// One column's data. `Option<T>` per element rather than a validity bitmap: the bitmap is
/// the right layout and the wrong amount of code for a first cut, and every operation in
/// [`crate::transform`] would have to carry it correctly for the saving to be real.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ColumnData {
    /// Integers, `None` for null.
    Int(Vec<Option<i64>>),
    /// Floats, `None` for null. A null is not a NaN and the two are not interchangeable: an
    /// aggregate skips the first and propagates the second.
    Float(Vec<Option<f64>>),
    /// Strings, `None` for null — which is distinct from `Some(String::new())`, so a blank
    /// cell in a CSV and a missing one can be told apart.
    Text(Vec<Option<String>>),
    /// Booleans, `None` for null.
    Bool(Vec<Option<bool>>),
}

impl ColumnData {
    /// The element count, nulls included. This is the number every column in a [`Table`]
    /// must agree on.
    pub fn len(&self) -> usize {
        match self {
            ColumnData::Int(v) => v.len(),
            ColumnData::Float(v) => v.len(),
            ColumnData::Text(v) => v.len(),
            ColumnData::Bool(v) => v.len(),
        }
    }

    /// Whether the column has no elements. Present because clippy asks for it beside
    /// [`ColumnData::len`], and because a zero-row table is a normal result of a filter.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The element type, recovered from the variant. This is how a transform checks that a
    /// comparison makes sense before it runs over every row.
    pub fn column_type(&self) -> ColumnType {
        match self {
            ColumnData::Int(_) => ColumnType::Int,
            ColumnData::Float(_) => ColumnType::Float,
            ColumnData::Text(_) => ColumnType::Text,
            ColumnData::Bool(_) => ColumnType::Bool,
        }
    }

    /// Resident bytes this column holds: its allocated buffer, plus the heap behind every
    /// string in it.
    ///
    /// `capacity`, not `len`. A column built by a filter keeps whatever the growth policy
    /// gave it, and those bytes are resident whether or not an element occupies them — a
    /// budget that counts only the occupied ones under-admits nothing and over-admits
    /// steadily.
    ///
    /// Exact for this representation and comparable across backends, which is the property
    /// [`crate::frame::Frame::memory_size`] needs. It counts no `ColumnData` enum header and
    /// no column name; those are per-column constants, not per-row ones, and the caller
    /// summing frames is deciding about rows.
    pub fn memory_size(&self) -> usize {
        match self {
            ColumnData::Int(v) => v.capacity() * std::mem::size_of::<Option<i64>>(),
            ColumnData::Float(v) => v.capacity() * std::mem::size_of::<Option<f64>>(),
            ColumnData::Bool(v) => v.capacity() * std::mem::size_of::<Option<bool>>(),
            ColumnData::Text(v) => {
                v.capacity() * std::mem::size_of::<Option<String>>()
                    + v.iter().flatten().map(String::capacity).sum::<usize>()
            }
        }
    }

    /// One element as a [`Value`], for rendering and for the scalar path in transforms.
    /// A null element reads as [`Value::Null`], never as a zero.
    pub fn value_at(&self, row: usize) -> Value {
        match self {
            ColumnData::Int(v) => v[row].map(Value::int).unwrap_or(Value::Null),
            ColumnData::Float(v) => v[row].map(Value::float).unwrap_or(Value::Null),
            ColumnData::Text(v) => v[row]
                .as_ref()
                .map(|s| Value::text(s.clone()))
                .unwrap_or(Value::Null),
            ColumnData::Bool(v) => v[row].map(Value::bool).unwrap_or(Value::Null),
        }
    }

    /// A new, empty column of a given type, with room for `rows` elements.
    ///
    /// The capacity is not a micro-optimisation: [`ColumnData::memory_size`] counts capacity
    /// rather than length, so a column grown by doubling reports up to twice the bytes it
    /// needs and a host budget admits proportionally fewer apps.
    pub fn with_capacity(ty: ColumnType, rows: usize) -> ColumnData {
        match ty {
            ColumnType::Int => ColumnData::Int(Vec::with_capacity(rows)),
            ColumnType::Float => ColumnData::Float(Vec::with_capacity(rows)),
            ColumnType::Text => ColumnData::Text(Vec::with_capacity(rows)),
            ColumnType::Bool => ColumnData::Bool(Vec::with_capacity(rows)),
        }
    }

    /// Append one [`Value`], recording a null for anything this column cannot hold.
    ///
    /// The one conversion it does make is `Int` into a `Float` column, which is the widening
    /// [`Value::as_float`] already performs and the reason an expression whose static type is
    /// `float` may still evaluate to an `Int` on a row where both of its branches were
    /// integers.
    ///
    /// Every other mismatch is a null rather than a panic. That case is unreachable through
    /// the transforms in [`crate::transform`], which build their output columns from the same
    /// type the values came from; it is written this way so that a future backend reporting a
    /// type it does not store degrades to a visible null instead of taking the process down.
    pub fn push(&mut self, value: Value) {
        match (self, value) {
            (ColumnData::Int(v), Value::Int { v: x }) => v.push(Some(x)),
            (ColumnData::Float(v), Value::Float { v: x }) => v.push(Some(x)),
            (ColumnData::Float(v), Value::Int { v: x }) => v.push(Some(x as f64)),
            (ColumnData::Bool(v), Value::Bool { v: x }) => v.push(Some(x)),
            (ColumnData::Text(v), Value::Text { v: x }) => v.push(Some(x)),
            (ColumnData::Int(v), _) => v.push(None),
            (ColumnData::Float(v), _) => v.push(None),
            (ColumnData::Bool(v), _) => v.push(None),
            (ColumnData::Text(v), _) => v.push(None),
        }
    }

    /// A new, empty column of the same type — the starting point of every filtering
    /// transform, which appends the rows it keeps.
    pub fn empty_like(&self) -> ColumnData {
        match self {
            ColumnData::Int(_) => ColumnData::Int(Vec::new()),
            ColumnData::Float(_) => ColumnData::Float(Vec::new()),
            ColumnData::Text(_) => ColumnData::Text(Vec::new()),
            ColumnData::Bool(_) => ColumnData::Bool(Vec::new()),
        }
    }

    /// Append element `row` of `self` onto `dst`. Panics if the two are different types,
    /// which is a programming error inside this crate and never reachable from a manifest:
    /// every transform builds its output columns with [`ColumnData::empty_like`].
    pub fn push_from(&self, dst: &mut ColumnData, row: usize) {
        match (self, dst) {
            (ColumnData::Int(src), ColumnData::Int(out)) => out.push(src[row]),
            (ColumnData::Float(src), ColumnData::Float(out)) => out.push(src[row]),
            (ColumnData::Text(src), ColumnData::Text(out)) => out.push(src[row].clone()),
            (ColumnData::Bool(src), ColumnData::Bool(out)) => out.push(src[row]),
            (a, b) => unreachable!(
                "push_from across column types: {} into {}",
                a.column_type(),
                b.column_type()
            ),
        }
    }
}

/// A named column.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Column {
    /// The column's name. Uniqueness within a table is not enforced here — a manifest that
    /// produces two columns called `total` gets two, and the transforms that look a column up
    /// by name find the first.
    pub name: String,
    /// The elements. Its length is what [`Table::new`] checks against every other column.
    pub data: ColumnData,
}

impl Column {
    /// Pairs a name with data. Nothing is validated: a column is only ever wrong relative to
    /// the other columns of a table, and [`Table::new`] is where that is caught.
    pub fn new(name: impl Into<String>, data: ColumnData) -> Column {
        Column {
            name: name.into(),
            data,
        }
    }

    /// An integer column. The four typed constructors below spare callers — tests above all —
    /// from naming [`ColumnData`] for every literal column they build.
    pub fn int(name: impl Into<String>, v: Vec<Option<i64>>) -> Column {
        Column::new(name, ColumnData::Int(v))
    }
    /// A float column.
    pub fn float(name: impl Into<String>, v: Vec<Option<f64>>) -> Column {
        Column::new(name, ColumnData::Float(v))
    }
    /// A text column.
    pub fn text(name: impl Into<String>, v: Vec<Option<String>>) -> Column {
        Column::new(name, ColumnData::Text(v))
    }
    /// A boolean column.
    pub fn bool(name: impl Into<String>, v: Vec<Option<bool>>) -> Column {
        Column::new(name, ColumnData::Bool(v))
    }
}

/// A rectangular, columnar table. See the module docs for what it deliberately is not.
///
/// `Deserialize` is written out rather than derived, and that is a correctness requirement
/// rather than a style choice: `ClientMessage::Set` deserialises a `Value` straight off a
/// WebSocket, so a derived impl would let a client construct a `Table` whose columns have
/// different lengths — bypassing [`Table::new`], the only constructor that rejects one. Every
/// method here then indexes columns by a row number this type promises is valid, so
/// `Table::row`, `head` and `take_rows` panic on a ragged table and take the connection with
/// them. Reproduced before it was fixed: two `int` columns of lengths 2 and 1 with
/// `"rows": 2` deserialised fine and panicked in `row(1)`.
#[derive(Clone, Debug, PartialEq, Default, Serialize)]
pub struct Table {
    columns: Vec<Column>,
    rows: usize,
}

impl<'de> Deserialize<'de> for Table {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Table, D::Error> {
        /// The serialised shape, with no invariants attached to it.
        #[derive(Deserialize)]
        struct TableWire {
            #[serde(default)]
            columns: Vec<Column>,
            #[serde(default)]
            rows: usize,
        }

        let wire = TableWire::deserialize(deserializer)?;
        let declared_rows = wire.rows;
        let table = Table::new(wire.columns).map_err(serde::de::Error::custom)?;
        // `rows` is derived from the columns, so a value that disagrees with them is a
        // malformed message rather than a table with a wrong count. Rejecting it keeps the
        // encoding round-trippable: anything this type serialises deserialises back.
        if table.rows != declared_rows {
            return Err(serde::de::Error::custom(format!(
                "table says it has {declared_rows} rows, but its columns have {}",
                table.rows
            )));
        }
        Ok(table)
    }
}

/// Constructing a table with columns of unequal length is the one way to produce a
/// [`Table`] that no transform could cope with, so it is rejected at construction rather
/// than defended against in twelve later places.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RaggedTable {
    /// The first column whose length disagreed. Reported by name because the caller built
    /// these columns one at a time and that is how they will find the one that is wrong.
    pub column: String,
    /// The length the table was going to have — the first column's, since that is the one
    /// that sets the width every later column is measured against.
    pub expected: usize,
    /// The offending column's own length.
    pub found: usize,
}

impl fmt::Display for RaggedTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "column `{}` has {} rows, but the table has {}",
            self.column, self.found, self.expected
        )
    }
}

impl std::error::Error for RaggedTable {}

impl Table {
    /// A table with no columns and no rows. What a transform returns when it has nothing to
    /// return, so that a downstream cell sees an empty table rather than an error.
    pub fn empty() -> Table {
        Table::default()
    }

    /// The only constructor that establishes the rectangularity invariant, and therefore the
    /// only way to obtain a `Table` from outside this module — the hand-written
    /// `Deserialize` above routes through it for exactly that reason.
    ///
    /// # Errors
    ///
    /// [`RaggedTable`] if any column's length differs from the first column's.
    pub fn new(columns: Vec<Column>) -> Result<Table, RaggedTable> {
        let rows = columns.first().map(|c| c.data.len()).unwrap_or(0);
        for c in &columns {
            if c.data.len() != rows {
                return Err(RaggedTable {
                    column: c.name.clone(),
                    expected: rows,
                    found: c.data.len(),
                });
            }
        }
        Ok(Table { columns, rows })
    }

    /// The row count. Stored rather than recomputed, and true of every column by
    /// construction, so indexing any column with a row below this cannot panic.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// The column count.
    pub fn width(&self) -> usize {
        self.columns.len()
    }

    /// The columns in declaration order, which is the order [`Table::row`] and the wire
    /// encoding both use.
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// The first column with this name, or `None`. Linear: tables here are tens of columns
    /// wide, and an index would be another invariant to keep true across every transform.
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// The position of the first column with this name, for callers that then want to index
    /// several columns in lockstep down the rows.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// Every column name, in order — what an error message lists when a transform is asked
    /// for a column that is not there.
    pub fn column_names(&self) -> Vec<&str> {
        self.columns.iter().map(|c| c.name.as_str()).collect()
    }

    /// The schema as (name, type) pairs — what a `TableView` sends on the wire so a client
    /// can right-align numbers without inspecting every cell.
    pub fn schema(&self) -> Vec<(String, ColumnType)> {
        self.columns
            .iter()
            .map(|c| (c.name.clone(), c.data.column_type()))
            .collect()
    }

    /// One row as values, in column order. Used by rendering and by nothing hot.
    pub fn row(&self, row: usize) -> Vec<Value> {
        self.columns.iter().map(|c| c.data.value_at(row)).collect()
    }

    /// The first `n` rows as a new table. This is how a table reaches a browser: a view
    /// sends a page, never a million rows, and the pane records the true row count beside
    /// it so the user is told what they are not seeing.
    pub fn head(&self, n: usize) -> Table {
        let n = n.min(self.rows);
        let columns = self
            .columns
            .iter()
            .map(|c| {
                let mut out = c.data.empty_like();
                for row in 0..n {
                    c.data.push_from(&mut out, row);
                }
                Column::new(c.name.clone(), out)
            })
            .collect();
        Table { columns, rows: n }
    }

    /// Build a table from a subset of rows, preserving column order and types.
    pub fn take_rows(&self, keep: &[usize]) -> Table {
        let columns = self
            .columns
            .iter()
            .map(|c| {
                let mut out = c.data.empty_like();
                for &row in keep {
                    c.data.push_from(&mut out, row);
                }
                Column::new(c.name.clone(), out)
            })
            .collect();
        Table {
            columns,
            rows: keep.len(),
        }
    }
}

impl Digestible for Table {
    /// Column names and types are hashed as well as the data, so a rename or a retype is a
    /// change even when every cell is identical — a client renders the header from the
    /// schema, so it is part of the value.
    ///
    /// **Composed from per-column digests**, exactly as [`crate::frame::digest_frame`]
    /// composes them. This impl exists beside that one because it reads [`ColumnData`]
    /// directly and so hashes a text column without building a `String` per cell, which a
    /// walk through [`crate::frame::Frame::value_at`] cannot avoid. The two producing the
    /// same bytes is a requirement, not a coincidence:
    /// `the_canonical_walk_matches_the_hand_written_table_digest` in `frame.rs` is the test
    /// that catches this pair drifting apart, and it earned its keep when the column digests
    /// went in.
    fn digest_into(&self, h: &mut Hasher) {
        let columns: Vec<crate::digest::Digest> = self
            .columns
            .iter()
            .map(|c| {
                let mut ch = Hasher::new();
                ch.str(&c.name);
                ch.tag(match c.data.column_type() {
                    ColumnType::Int => 1,
                    ColumnType::Float => 2,
                    ColumnType::Text => 3,
                    ColumnType::Bool => 4,
                });
                match &c.data {
                    ColumnData::Int(v) => {
                        for e in v {
                            match e {
                                Some(x) => {
                                    ch.tag(1).i64(*x);
                                }
                                None => {
                                    ch.tag(0);
                                }
                            }
                        }
                    }
                    ColumnData::Float(v) => {
                        for e in v {
                            match e {
                                Some(x) => {
                                    ch.tag(1).f64(*x);
                                }
                                None => {
                                    ch.tag(0);
                                }
                            }
                        }
                    }
                    ColumnData::Text(v) => {
                        for e in v {
                            match e {
                                Some(x) => {
                                    ch.tag(1).str(x);
                                }
                                None => {
                                    ch.tag(0);
                                }
                            }
                        }
                    }
                    ColumnData::Bool(v) => {
                        for e in v {
                            match e {
                                Some(x) => {
                                    ch.tag(1).bytes(&[*x as u8]);
                                }
                                None => {
                                    ch.tag(0);
                                }
                            }
                        }
                    }
                }
                ch.finish()
            })
            .collect();
        crate::frame::digest_frame_from_columns(self.rows, &columns, h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Table {
        Table::new(vec![
            Column::int("id", vec![Some(1), Some(2), Some(3)]),
            Column::text("name", vec![Some("a".into()), None, Some("c".into())]),
        ])
        .unwrap()
    }

    #[test]
    fn ragged_columns_are_rejected_at_construction() {
        let err = Table::new(vec![
            Column::int("a", vec![Some(1), Some(2)]),
            Column::int("b", vec![Some(1)]),
        ])
        .unwrap_err();
        assert_eq!(err.column, "b");
        assert_eq!((err.expected, err.found), (2, 1));
    }

    #[test]
    fn nulls_read_as_null_not_as_a_zero() {
        assert_eq!(t().column("name").unwrap().data.value_at(1), Value::Null);
    }

    #[test]
    fn head_truncates_and_keeps_the_schema() {
        let h = t().head(2);
        assert_eq!(h.rows(), 2);
        assert_eq!(h.column_names(), vec!["id", "name"]);
    }

    #[test]
    fn head_past_the_end_is_the_whole_table() {
        assert_eq!(t().head(99).rows(), 3);
    }

    #[test]
    fn identical_tables_digest_alike() {
        assert_eq!(t().digest(), t().digest());
    }

    #[test]
    fn a_renamed_column_is_a_different_value() {
        let a = t();
        let b = Table::new(vec![
            Column::int("id", vec![Some(1), Some(2), Some(3)]),
            Column::text("label", vec![Some("a".into()), None, Some("c".into())]),
        ])
        .unwrap();
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn a_null_is_not_the_same_as_a_missing_row() {
        let a = Table::new(vec![Column::int("x", vec![Some(1), None])]).unwrap();
        let b = Table::new(vec![Column::int("x", vec![Some(1)])]).unwrap();
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn int_reads_as_float_but_not_the_reverse() {
        assert_eq!(Value::int(3).as_float(), Some(3.0));
        assert_eq!(Value::float(3.0).as_int(), None);
    }

    #[test]
    fn a_ragged_table_is_rejected_at_the_edge_rather_than_panicking_later() {
        // A client sends `Value` straight off a socket. Before this was a hand-written
        // `Deserialize`, the derive built a table whose columns had different lengths and
        // `Table::row` then indexed past the end of the short one.
        let json = r#"{"columns":[
            {"name":"a","data":{"type":"int","data":[1,2]}},
            {"name":"b","data":{"type":"int","data":[1]}}],"rows":2}"#;
        let err = serde_json::from_str::<Table>(json).unwrap_err().to_string();
        assert!(err.contains("column `b`"), "{err}");
    }

    #[test]
    fn a_table_whose_row_count_disagrees_with_its_columns_is_rejected() {
        let json = r#"{"columns":[{"name":"a","data":{"type":"int","data":[1,2]}}],"rows":9}"#;
        let err = serde_json::from_str::<Table>(json).unwrap_err().to_string();
        assert!(err.contains("says it has 9 rows"), "{err}");
    }

    #[test]
    fn value_round_trips_through_json() {
        let v = Value::table(t());
        let s = serde_json::to_string(&v).unwrap();
        let back: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v, back);
    }
}
