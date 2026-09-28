//! The seam a table lives behind, so the representation can change without the engine
//! changing with it.
//!
//! `ARCHITECTURE.md` §6 names this: the engine needs `rows()`, `schema()`, `head(n)` and
//! `digest()` from a table, and nothing else. Everything above this trait — the seven
//! verbs, the render path, the digest short-circuit — is written against those, so a
//! different backend is a different implementation rather than a different engine.
//!
//! # Why this trait exists now, when the roadmap said not yet
//!
//! `ROADMAP.md` §2 deliberately refused to add this trait while it would have had one
//! implementation and no second caller, on the grounds that a design with one instance is
//! a design fitted to that instance. That objection is answered rather than waived: the
//! trait ships with **two** implementations from its first commit — [`Table`], the
//! `Vec<Option<T>>` representation that has always been here, and an Arrow-backed one in
//! `dagpane-frame-arrow`. The two are held to each other by a differential test, which is
//! the only way to find out whether a seam is really a seam.
//!
//! # The one rule a backend cannot bend
//!
//! **Two frames holding the same logical content must produce the same digest**, whatever
//! they do with it physically. The digest is what decides whether a cell's output moved,
//! so a backend that digests its own layout instead of its values would invalidate the
//! whole graph the moment anybody switched representation — and worse, would do it while
//! looking like a correct recomputation.
//!
//! That is not left to each backend to get right, and deliberately not even offered to
//! them: **there is no `digest` method on this trait.** [`digest_frame`] is a free function
//! over the trait's own accessors, and `Value`'s `Digestible` impl calls it directly, so a
//! backend has no hook with which to disagree. A dictionary-encoded column and a plain one
//! holding the same strings hash identically by construction.
//!
//! An earlier draft did put an overridable `digest_into` on the trait. It was dead — nothing
//! called it, because `Value` reaches for the free function — and a dead override is worse
//! than none: it invites a backend author to "fix" a digest there and believe it took
//! effect. Found by mutating it and watching the differential test below stay green.

use std::fmt;
use std::sync::Arc;

use crate::digest::{Digest, Hasher};
use crate::value::{Column, ColumnType, Table, Value};

/// A rectangular, columnar table, however it is stored underneath.
///
/// `Send + Sync` because one `Arc<App>` is shared by every connection; `Debug` because a
/// cell error that cannot print the value it choked on is a bug report nobody can act on.
pub trait Frame: fmt::Debug + Send + Sync {
    /// Row count. The denominator of everything.
    fn rows(&self) -> usize;

    /// Column names and types, left to right. A client renders its header from this, so it
    /// is part of the value rather than metadata about it.
    fn schema(&self) -> Vec<(String, ColumnType)>;

    /// One cell, as a [`Value`]. A null reads as [`Value::Null`] and never as a zero.
    ///
    /// Callers must pass `row < rows()` and `col < width()`; both are internal invariants
    /// of this crate, never reachable from a manifest.
    fn value_at(&self, row: usize, col: usize) -> Value;

    /// A new frame holding `keep`'s rows, in `keep`'s order.
    ///
    /// This is the workhorse: `filter`, `sort` and `limit` all reduce to choosing row
    /// indices and calling this, which is why a backend only has to make one operation
    /// fast to make all three fast.
    fn take_rows(&self, keep: &[usize]) -> Arc<dyn Frame>;

    /// Resident bytes this frame holds, as its own backend accounts for them.
    ///
    /// Required rather than defaulted, and that is the whole point of the method. A default
    /// would have to guess — rows times a nominal width per column type — and a guess is
    /// exactly what a caller admitting apps against a byte budget must not be handed. Every
    /// backend can answer this exactly about its own storage and none can answer it about
    /// anybody else's, so the trait asks and does not assume.
    ///
    /// What it counts: the buffers holding this frame's cells, including the heap behind
    /// each string. What it does not count: the frame's own struct, its schema strings, or
    /// anything a caller wrapped it in. It is therefore a figure for **comparing frames and
    /// summing them**, never an RSS prediction — a process holds allocator slack, a graph,
    /// a client and a socket buffer besides.
    fn memory_size(&self) -> usize;

    /// Which backend this is — `"table"`, `"arrow"`, whatever a future one calls itself.
    ///
    /// Required rather than defaulted, because a default would let a new backend silently
    /// report itself as somebody else. It exists so a test can assert that a chain of verbs
    /// stayed in one representation, which is a property no amount of comparing values can
    /// show: two frames holding identical content are equal here by design, so equality is
    /// exactly the wrong tool for asking "did this fall back to a `Table` halfway through?"
    fn backend(&self) -> &'static str;

    /// A new frame of **this frame's own kind**, holding these columns.
    ///
    /// The seam's constructor. A verb that builds rows rather than selecting them —
    /// `group_by` is the only one — cannot name a backend, because `transform.rs` lives in
    /// a crate that must not depend on any. It asks its input to build the result instead,
    /// so an aggregation over an Arrow frame yields an Arrow frame and one over a `Table`
    /// yields a `Table`: **a chain stays in whatever representation it started in**, with no
    /// crate above the seam choosing on its behalf.
    ///
    /// Takes owned columns because the caller has just built them and nobody else wants
    /// them; a backend that stores something else converts once, here.
    fn same_kind(&self, columns: Vec<Column>) -> Arc<dyn Frame>;

    /// A new frame with only these columns, in this order.
    ///
    /// The one verb `take_rows` cannot express: `select` moves along the other axis. A
    /// backend that stores columns separately — both of ours do — answers it by cloning
    /// pointers rather than data.
    fn select_columns(&self, cols: &[usize]) -> Arc<dyn Frame>;

    /// Is this cell null?
    ///
    /// Defaulted through [`Frame::value_at`], and overridden by any backend that can answer
    /// from a validity bitmap without materialising the value.
    fn is_null(&self, row: usize, col: usize) -> bool {
        matches!(self.value_at(row, col), Value::Null)
    }

    /// Order two rows within one column. Nulls are not considered here — `sort` handles
    /// their placement — so an implementation may assume both are present.
    ///
    /// On the trait because the default allocates: comparing two text cells via
    /// [`Frame::value_at`] builds two `String`s, and a sort does that `n log n` times. A
    /// backend that can compare in place should, and [`Table`] does.
    fn compare_in_column(&self, col: usize, a: usize, b: usize) -> std::cmp::Ordering {
        compare_values(&self.value_at(a, col), &self.value_at(b, col))
    }

    /// Order one cell against a literal, or `None` when the two are not comparable — a text
    /// column against a number, say, which is a filter that matches nothing rather than an
    /// error.
    fn compare_to_value(&self, row: usize, col: usize, rhs: &Value) -> Option<std::cmp::Ordering> {
        let lhs = self.value_at(row, col);
        if matches!(lhs, Value::Null) {
            return None;
        }
        match (&lhs, rhs) {
            // Two integers compare as integers. Widening both to `f64` first — which the
            // mixed arm below has to do — loses every integer above 2^53, so `2^53` and
            // `2^53 + 1` compare EQUAL and a `filter` silently keeps a different set of rows.
            // `Table` overrides this method with an exact compare, so without this arm the
            // backends disagree on the same data: a representation the user never chose
            // changing the answer, which is the one thing the seam exists to prevent.
            (Value::Int { v: a }, Value::Int { v: b }) => Some(a.cmp(b)),
            // Mixed, or two floats: `f64` is the only common ground, and it is what the
            // in-tree column path does too — so the backends still agree with each other.
            (Value::Int { .. } | Value::Float { .. }, Value::Int { .. } | Value::Float { .. }) => {
                lhs.as_float()?.partial_cmp(&rhs.as_float()?)
            }
            (Value::Text { v: a }, Value::Text { v: b }) => Some(a.as_str().cmp(b.as_str())),
            (Value::Bool { v: a }, Value::Bool { v: b }) => Some(a.cmp(b)),
            _ => None,
        }
    }

    // --- derived; a backend overrides one only when it can genuinely do better ---

    /// Column count. Derived from [`Frame::schema`] so no backend can report a width
    /// its schema does not have.
    fn width(&self) -> usize {
        self.schema().len()
    }

    /// The column names, in order.
    fn column_names(&self) -> Vec<String> {
        self.schema().into_iter().map(|(n, _)| n).collect()
    }

    /// The index of a named column, or `None`. First match wins, which is the rule the
    /// in-tree `Table` has always used for a frame with duplicate names.
    fn column_index(&self, name: &str) -> Option<usize> {
        self.schema().iter().position(|(n, _)| n == name)
    }

    /// The declared type of a column, or `None` if the index is out of range.
    fn column_type(&self, col: usize) -> Option<ColumnType> {
        self.schema().get(col).map(|(_, t)| *t)
    }

    /// One row, left to right.
    fn row(&self, row: usize) -> Vec<Value> {
        (0..self.width()).map(|c| self.value_at(row, c)).collect()
    }

    /// The first `n` rows. Clamped, so `head` past the end is the whole frame rather than
    /// a panic — a manifest can ask for a hundred rows of a table with three.
    fn head(&self, n: usize) -> Arc<dyn Frame> {
        let keep: Vec<usize> = (0..n.min(self.rows())).collect();
        self.take_rows(&keep)
    }

    /// Whether the frame has no rows. A frame with columns but no rows is empty; so is
    /// one with neither.
    fn is_empty(&self) -> bool {
        self.rows() == 0
    }
}

/// Order two values of the same column, nulls aside. Total within a column type; across
/// types it falls back to equal, which only a malformed frame could reach.
fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Int { v: x }, Value::Int { v: y }) => x.cmp(y),
        (Value::Float { v: x }, Value::Float { v: y }) => {
            x.partial_cmp(y).unwrap_or(Ordering::Equal)
        }
        (Value::Text { v: x }, Value::Text { v: y }) => x.cmp(y),
        (Value::Bool { v: x }, Value::Bool { v: y }) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

/// The canonical content hash for one column: its name, its type tag, and every element,
/// tagged present-or-null.
///
/// Split out of [`digest_frame`] because sub-node invalidation compares *columns* and not
/// frames. A cell that reads three of two hundred columns records the digests of those
/// three; a pass that rewrites a fourth finds all three equal and the cell reuses. The
/// frame digest is then composed from these rather than taken separately, so a frame's
/// columns and its whole are one walk over the data and never two.
fn digest_column_parts(
    frame: &(impl Frame + ?Sized),
    col: usize,
    name: &str,
    ty: ColumnType,
    h: &mut Hasher,
) {
    h.str(name);
    h.tag(match ty {
        ColumnType::Int => 1,
        ColumnType::Float => 2,
        ColumnType::Text => 3,
        ColumnType::Bool => 4,
    });
    for row in 0..frame.rows() {
        match frame.value_at(row, col) {
            Value::Null => {
                h.tag(0);
            }
            Value::Int { v } => {
                h.tag(1).i64(v);
            }
            Value::Float { v } => {
                h.tag(1).f64(v);
            }
            Value::Text { v } => {
                h.tag(1).str(&v);
            }
            Value::Bool { v } => {
                // One byte, not a u64. The existing scheme hashes `bytes(&[x as u8])`
                // and the two are not the same eight bytes — getting this wrong would
                // have changed every digest of every table containing a bool, which is
                // to say invalidated every cached value in every live session on the
                // day this landed. The test below is what caught it.
                h.tag(1).bytes(&[u8::from(v)]);
            }
            // A frame cell is one of the four column types by construction; the other
            // Value variants cannot appear here. Hash a distinct tag rather than
            // panicking, so a future column type shows up as "different" instead of
            // taking a connection down.
            other => {
                h.tag(9).str(other.type_name());
            }
        }
    }
}

/// One column's digest, on its own.
pub fn column_digest(frame: &(impl Frame + ?Sized), col: usize) -> Digest {
    let schema = frame.schema();
    let (name, ty) = &schema[col];
    let mut h = Hasher::new();
    digest_column_parts(frame, col, name, *ty, &mut h);
    h.finish()
}

/// Every column's digest, left to right.
///
/// The unit sub-node invalidation compares. Computed once when a value is produced, beside
/// the frame digest it composes into, so the granular comparison costs a slice lookup per
/// column read and not a second walk over the table.
pub fn column_digests(frame: &(impl Frame + ?Sized)) -> Vec<Digest> {
    frame
        .schema()
        .iter()
        .enumerate()
        .map(|(col, (name, ty))| {
            let mut h = Hasher::new();
            digest_column_parts(frame, col, name, *ty, &mut h);
            h.finish()
        })
        .collect()
}

/// The digest of a frame's **shape**: its row count and its whole schema, and none of its
/// data.
///
/// This is what makes a column-granular reuse decision safe rather than merely narrow. A
/// cell that reads no columns at all — `count` over an unfiltered table is exactly that —
/// has an empty set of column digests to compare, and would reuse forever. Its key carries
/// this as well, so a row appearing, a column being added, or a column changing type
/// invalidates the cell even though nothing it named has moved. Cheap: O(width), never
/// O(cells).
pub fn shape_digest(frame: &(impl Frame + ?Sized)) -> Digest {
    let schema = frame.schema();
    let mut h = Hasher::new();
    h.tag(7).u64(frame.rows() as u64).u64(schema.len() as u64);
    for (name, ty) in &schema {
        h.str(name);
        h.tag(match ty {
            ColumnType::Int => 1,
            ColumnType::Float => 2,
            ColumnType::Text => 3,
            ColumnType::Bool => 4,
        });
    }
    h.finish()
}

/// The canonical content hash for any [`Frame`].
///
/// One definition rather than one per backend: tag, row count, column count, then each
/// column's own digest — name, type tag, and every element tagged present-or-null. Column
/// names and types are hashed as well as the data, so a rename or a retype is a change even
/// when every cell is identical.
///
/// It walks values through the trait, so it sees logical content and cannot see layout.
/// That is the property that lets a dictionary-encoded frame and a plain one agree.
///
/// **Composed from the column digests rather than taken in one stream.** The bytes this
/// absorbs are therefore not the bytes the pre-sub-node scheme absorbed, and that is fine
/// in a way worth stating: a digest is only ever compared against another taken by the same
/// process, never against one written down earlier. What must not change is the *rule* —
/// equal content, equal digest — and composing a parent from its children's digests is the
/// standard way to keep that while making the children separately comparable.
pub fn digest_frame(frame: &(impl Frame + ?Sized), h: &mut Hasher) {
    let columns = column_digests(frame);
    digest_frame_from_columns(frame.rows(), &columns, h);
}

/// [`digest_frame`] for a caller that already holds the column digests, so a value that
/// needs both pays for the data walk once.
pub fn digest_frame_from_columns(rows: usize, columns: &[Digest], h: &mut Hasher) {
    h.tag(6).u64(rows as u64).u64(columns.len() as u64);
    for d in columns {
        h.digest(*d);
    }
}

/// The digest of any frame, for callers that want the value rather than a hasher.
pub fn frame_digest(frame: &(impl Frame + ?Sized)) -> Digest {
    let mut h = Hasher::new();
    digest_frame(frame, &mut h);
    h.finish()
}

// ---------------------------------------------------------------------------
// The in-tree implementation
// ---------------------------------------------------------------------------

impl Frame for Table {
    fn rows(&self) -> usize {
        Table::rows(self)
    }

    fn schema(&self) -> Vec<(String, ColumnType)> {
        Table::schema(self)
    }

    fn value_at(&self, row: usize, col: usize) -> Value {
        self.columns()[col].data.value_at(row)
    }

    fn take_rows(&self, keep: &[usize]) -> Arc<dyn Frame> {
        Arc::new(Table::take_rows(self, keep))
    }

    fn memory_size(&self) -> usize {
        self.columns().iter().map(|c| c.data.memory_size()).sum()
    }

    fn backend(&self) -> &'static str {
        "table"
    }

    fn same_kind(&self, columns: Vec<Column>) -> Arc<dyn Frame> {
        // `new` cannot fail: a caller builds every column to one row count.
        Arc::new(Table::new(columns).expect("columns built to one length"))
    }

    fn select_columns(&self, cols: &[usize]) -> Arc<dyn Frame> {
        let picked: Vec<_> = cols.iter().map(|&c| self.columns()[c].clone()).collect();
        // Every column came from one table, so they are all the same length.
        Arc::new(Table::new(picked).expect("columns of one table share a length"))
    }

    // `width` and `column_index` are answered from the column vector directly rather than
    // by building a schema and searching it, which the default would do on every call.
    fn width(&self) -> usize {
        Table::width(self)
    }

    fn column_index(&self, name: &str) -> Option<usize> {
        Table::column_index(self, name)
    }

    fn row(&self, row: usize) -> Vec<Value> {
        Table::row(self, row)
    }

    // These three read the column vector directly, which is what `transform.rs` did before
    // the seam existed. Keeping them exact means the swap changed no work on the path that
    // production actually takes today.
    fn is_null(&self, row: usize, col: usize) -> bool {
        crate::transform::column_is_null(&self.columns()[col].data, row)
    }

    fn compare_in_column(&self, col: usize, a: usize, b: usize) -> std::cmp::Ordering {
        crate::transform::order_within_column(&self.columns()[col].data, a, b)
    }

    fn compare_to_value(&self, row: usize, col: usize, rhs: &Value) -> Option<std::cmp::Ordering> {
        crate::transform::compare_column_to(&self.columns()[col].data, row, rhs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::Digestible;
    use crate::value::Column;

    fn sample() -> Table {
        Table::new(vec![
            Column::int("id", vec![Some(1), Some(2), None]),
            Column::text(
                "region",
                vec![
                    Some("north".into()),
                    Some("south".into()),
                    Some("north".into()),
                ],
            ),
            Column::bool("ok", vec![Some(true), None, Some(false)]),
        ])
        .unwrap()
    }

    /// The seam must not have changed any existing digest. If it had, every cached value in
    /// every running session would be invalidated by the refactor alone.
    #[test]
    fn the_canonical_walk_matches_the_hand_written_table_digest() {
        let t = sample();
        assert_eq!(frame_digest(&t), Digestible::digest(&t));
    }

    #[test]
    fn schema_and_cells_read_the_same_through_the_trait() {
        let t = sample();
        let f: &dyn Frame = &t;
        assert_eq!(f.rows(), 3);
        assert_eq!(f.width(), 3);
        assert_eq!(f.column_names(), vec!["id", "region", "ok"]);
        assert_eq!(f.column_index("region"), Some(1));
        assert_eq!(f.value_at(0, 0), Value::int(1));
        assert_eq!(
            f.value_at(2, 0),
            Value::Null,
            "a null is a null, never a zero"
        );
        assert_eq!(f.value_at(1, 1), Value::text("south"));
    }

    #[test]
    fn take_rows_reorders_and_selects() {
        let t = sample();
        let f: &dyn Frame = &t;
        let taken = f.take_rows(&[2, 0]);
        assert_eq!(taken.rows(), 2);
        assert_eq!(taken.value_at(0, 0), Value::Null);
        assert_eq!(taken.value_at(1, 0), Value::int(1));
    }

    #[test]
    fn head_past_the_end_is_the_whole_frame() {
        let t = sample();
        let f: &dyn Frame = &t;
        assert_eq!(f.head(100).rows(), 3);
        assert_eq!(f.head(0).rows(), 0);
    }

    /// A rename is a change even when every cell is identical, because a client renders its
    /// header from the schema.
    #[test]
    fn memory_size_counts_the_buffer_and_the_strings_behind_it() {
        // Two int columns of two rows: `Option<i64>` is 16 bytes on every target this
        // builds for, and nothing else is counted.
        let ints = Table::new(vec![
            Column::int("a", vec![Some(1), Some(2)]),
            Column::int("b", vec![None, Some(4)]),
        ])
        .unwrap();
        assert_eq!(
            Frame::memory_size(&ints),
            4 * std::mem::size_of::<Option<i64>>()
        );

        // A text column costs its `Option<String>` slots plus the bytes each string holds,
        // which is the part a rows-times-nominal-width guess cannot see.
        let short = Table::new(vec![Column::text("s", vec![Some("a".into())])]).unwrap();
        let long = Table::new(vec![Column::text("s", vec![Some("a".repeat(1000))])]).unwrap();
        assert_eq!(
            Frame::memory_size(&long) - Frame::memory_size(&short),
            999,
            "the heap behind a string is the difference between these two"
        );
    }

    #[test]
    fn a_renamed_column_digests_differently() {
        let a = Table::new(vec![Column::int("id", vec![Some(1)])]).unwrap();
        let b = Table::new(vec![Column::int("ident", vec![Some(1)])]).unwrap();
        assert_ne!(frame_digest(&a), frame_digest(&b));
    }
}

// ---------------------------------------------------------------------------
// The handle `Value` carries
// ---------------------------------------------------------------------------

/// A frame inside a [`Value`].
///
/// `Value` derives `Clone`, `PartialEq`, `Serialize` and `Deserialize`, and `Arc<dyn Frame>`
/// supports none of the last three. Rather than hand-write the whole enum — six other
/// variants that only exist to be forwarded, and a wire format that must not move — the
/// manual work is confined to this newtype and `Value` keeps its derives.
///
/// Cloning is an `Arc` bump: one loaded source is shared by every session over one
/// `Arc<App>`, and the reason that is affordable is that copying a `Value` never copies a
/// frame.
#[derive(Clone, Debug)]
pub struct FrameRef(Arc<dyn Frame>);

impl FrameRef {
    /// Wrap a frame. An `Arc` bump, never a copy.
    pub fn new(frame: Arc<dyn Frame>) -> FrameRef {
        FrameRef(frame)
    }

    /// Borrow the frame behind this handle.
    pub fn as_frame(&self) -> &dyn Frame {
        &*self.0
    }

    /// Clone out the shared handle, for a caller that needs to keep the frame alive
    /// beyond this borrow.
    pub fn arc(&self) -> Arc<dyn Frame> {
        Arc::clone(&self.0)
    }

    /// Materialise as a [`Table`]. O(cells) — for code that has not moved to the trait yet,
    /// never on a path that runs per interaction.
    pub fn to_table(&self) -> Table {
        let f = self.as_frame();
        let schema = f.schema();
        let columns = schema
            .iter()
            .enumerate()
            .map(|(c, (name, ty))| {
                let data = match ty {
                    ColumnType::Int => crate::value::ColumnData::Int(
                        (0..f.rows()).map(|r| f.value_at(r, c).as_int()).collect(),
                    ),
                    ColumnType::Float => crate::value::ColumnData::Float(
                        (0..f.rows()).map(|r| f.value_at(r, c).as_float()).collect(),
                    ),
                    ColumnType::Bool => crate::value::ColumnData::Bool(
                        (0..f.rows()).map(|r| f.value_at(r, c).as_bool()).collect(),
                    ),
                    ColumnType::Text => crate::value::ColumnData::Text(
                        (0..f.rows())
                            .map(|r| match f.value_at(r, c) {
                                Value::Text { v } => Some(v),
                                _ => None,
                            })
                            .collect(),
                    ),
                };
                crate::value::Column::new(name.clone(), data)
            })
            .collect();
        Table::new(columns).expect("a frame's columns share its row count")
    }
}

impl From<Table> for FrameRef {
    fn from(t: Table) -> FrameRef {
        FrameRef(Arc::new(t))
    }
}

/// Content equality, walked cell by cell.
///
/// Deliberately **not** a digest comparison, even though the engine decides invalidation
/// with digests. A 128-bit hash is the right tool for "did this move?" on a hot path and
/// the wrong one for `assert_eq!` in a test, where a collision would read as a passing
/// test rather than as a rare event. Two frames are equal here when they are the same
/// values, whatever holds them.
impl PartialEq for FrameRef {
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (self.as_frame(), other.as_frame());
        if a.rows() != b.rows() || a.schema() != b.schema() {
            return false;
        }
        (0..a.rows()).all(|r| (0..a.width()).all(|c| a.value_at(r, c) == b.value_at(r, c)))
    }
}

/// Serialises as a [`Table`], byte for byte what this field has always emitted.
///
/// The wire is a published contract — the replay in the product site and every socket test
/// read it — so the representation change must be invisible to a client. A backend that
/// serialised its own layout would be a protocol change wearing a refactor's clothes.
impl serde::Serialize for FrameRef {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // A frame that already is a `Table` serialises without a copy; anything else
        // materialises first. Serialisation is O(cells) either way.
        self.to_table().serialize(s)
    }
}

/// Deserialises into a [`Table`] — the default backend, and the only one core knows.
///
/// A client sends values; it does not choose a representation, and it must not be able to.
/// `Table`'s own hand-written `Deserialize` rejects a ragged table, and going through it
/// keeps that check on the path a WebSocket message actually takes.
impl<'de> serde::Deserialize<'de> for FrameRef {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<FrameRef, D::Error> {
        Table::deserialize(d).map(FrameRef::from)
    }
}

// ---------------------------------------------------------------------------
// Filling a frame without knowing which one
// ---------------------------------------------------------------------------

/// What a reader learned about a column while deciding its type, offered to the builder.
///
/// **Advisory.** A backend is free to ignore all of it — [`TableBuilder`] does — and must
/// produce identical content either way. It exists because a streaming builder has to commit
/// to a physical layout before it has seen the data, while the pass that chose the column's
/// type has already seen all of it. Passing that knowledge forward is cheaper than either
/// guessing or buffering.
#[derive(Clone, Copy, Debug, Default)]
pub struct ColumnHint {
    /// Rows the column will receive, when the caller knows.
    pub rows: Option<usize>,
    /// Distinct non-null values, when the caller counted them and the count stayed small
    /// enough to be worth carrying. `None` means "unknown or too many to matter", never
    /// "zero" — a backend must treat it as no information rather than as a low cardinality.
    pub distinct: Option<usize>,
}

/// Somewhere to put a column of a given type, without naming a backend.
///
/// The reader owns the parse and the backend owns the layout, and before this trait existed
/// they could only meet through core's `Vec<Option<T>>` columns — so loading an Arrow source
/// built the whole file in one representation and converted it into another, with both live
/// at the moment of conversion. On a 1M x 6 file that intermediate measured 139 MB against
/// 34 MB of output.
///
/// A builder is filled column by column: [`begin_column`](FrameBuilder::begin_column) for
/// each, then a push per row into the column's index, then [`finish`](FrameBuilder::finish).
/// Pushing a variant that does not match the column's declared type is a caller bug; an
/// implementation records a null rather than panicking, because a file loader that takes the
/// process down over one field is the wrong trade.
pub trait FrameBuilder {
    /// Declare the next column. Returns its index, which every push then names.
    fn begin_column(&mut self, name: &str, ty: ColumnType, hint: ColumnHint) -> usize;

    /// Append one `Int` cell to column `col`. `None` is a null, never a zero.
    fn push_int(&mut self, col: usize, v: Option<i64>);
    /// Append one `Float` cell to column `col`. `None` is a null; NaN is a value.
    fn push_float(&mut self, col: usize, v: Option<f64>);
    /// Append one `Bool` cell to column `col`. `None` is a null, never `false`.
    fn push_bool(&mut self, col: usize, v: Option<bool>);

    /// Text is pushed **borrowed**. That is the point of the method: an implementation that
    /// stores bytes contiguously copies into its own buffer and never allocates a `String`
    /// per cell, which is most of what this trait is for.
    fn push_text(&mut self, col: usize, v: Option<&str>);

    /// Consume the builder and hand back the frame.
    ///
    /// By value, so a backend can move its buffers into the finished arrays rather than
    /// copying them out — the whole reason this trait is worth having.
    fn finish(self: Box<Self>) -> Arc<dyn Frame>;
}

/// The in-tree builder: fills `Vec<Option<T>>` columns and hands back a [`Table`].
///
/// Ignores every hint, which is the reference behaviour a backend's own builder is checked
/// against.
#[derive(Debug, Default)]
pub struct TableBuilder {
    columns: Vec<Column>,
}

impl TableBuilder {
    /// A builder with no columns yet.
    pub fn new() -> TableBuilder {
        TableBuilder::default()
    }
}

impl FrameBuilder for TableBuilder {
    fn begin_column(&mut self, name: &str, ty: ColumnType, hint: ColumnHint) -> usize {
        let n = hint.rows.unwrap_or(0);
        let data = match ty {
            ColumnType::Int => crate::value::ColumnData::Int(Vec::with_capacity(n)),
            ColumnType::Float => crate::value::ColumnData::Float(Vec::with_capacity(n)),
            ColumnType::Bool => crate::value::ColumnData::Bool(Vec::with_capacity(n)),
            ColumnType::Text => crate::value::ColumnData::Text(Vec::with_capacity(n)),
        };
        self.columns.push(Column::new(name, data));
        self.columns.len() - 1
    }

    fn push_int(&mut self, col: usize, v: Option<i64>) {
        if let crate::value::ColumnData::Int(x) = &mut self.columns[col].data {
            x.push(v);
        }
    }

    fn push_float(&mut self, col: usize, v: Option<f64>) {
        if let crate::value::ColumnData::Float(x) = &mut self.columns[col].data {
            x.push(v);
        }
    }

    fn push_bool(&mut self, col: usize, v: Option<bool>) {
        if let crate::value::ColumnData::Bool(x) = &mut self.columns[col].data {
            x.push(v);
        }
    }

    fn push_text(&mut self, col: usize, v: Option<&str>) {
        if let crate::value::ColumnData::Text(x) = &mut self.columns[col].data {
            x.push(v.map(str::to_owned));
        }
    }

    fn finish(self: Box<Self>) -> Arc<dyn Frame> {
        // `new` cannot fail: a caller pushes the same number of rows into every column.
        Arc::new(Table::new(self.columns).expect("columns filled to one length"))
    }
}

// ---------------------------------------------------------------------------
// One more column, without copying the ones already there
// ---------------------------------------------------------------------------

/// A frame with one column appended.
///
/// Every other verb in [`crate::transform`] rewrites its input; `derive` *keeps* it and adds
/// to it, and that difference is worth a type. Materialising the base to append would copy
/// every column of it — which is the mistake `group_by` was rewritten to remove, and the
/// reason the comment in `dagpane_app`'s pipeline says "an `Arc` bump, not a copy". A
/// two-column derive over a million-row frame allocates one column, which is the one it
/// actually computed.
///
/// The new column is always last, and [`Frame::backend`] still reports the base's: this is an
/// adapter, not a representation, so a derive over an Arrow frame is still Arrow and
/// `same_kind` still builds Arrow.
///
/// # Panics
///
/// If `column` is not exactly as long as `base` has rows. Every caller inside this crate
/// builds the column by walking `0..base.rows()`.
pub fn with_column(base: Arc<dyn Frame>, column: Column) -> Arc<dyn Frame> {
    assert_eq!(
        column.data.len(),
        base.rows(),
        "a derived column must have one element per row"
    );
    Arc::new(WithColumn {
        base_width: base.width(),
        base,
        extra: column,
    })
}

#[derive(Debug)]
struct WithColumn {
    base: Arc<dyn Frame>,
    extra: Column,
    /// Cached, because `width()` walks `schema()` and `value_at` is called once per cell.
    base_width: usize,
}

impl WithColumn {
    fn take_extra(&self, keep: &[usize]) -> Column {
        let mut data = self.extra.data.empty_like();
        for &r in keep {
            self.extra.data.push_from(&mut data, r);
        }
        Column::new(self.extra.name.clone(), data)
    }
}

impl Frame for WithColumn {
    fn rows(&self) -> usize {
        self.base.rows()
    }

    fn schema(&self) -> Vec<(String, ColumnType)> {
        let mut s = self.base.schema();
        s.push((self.extra.name.clone(), self.extra.data.column_type()));
        s
    }

    fn value_at(&self, row: usize, col: usize) -> Value {
        if col == self.base_width {
            self.extra.data.value_at(row)
        } else {
            self.base.value_at(row, col)
        }
    }

    fn take_rows(&self, keep: &[usize]) -> Arc<dyn Frame> {
        Arc::new(WithColumn {
            base_width: self.base_width,
            base: self.base.take_rows(keep),
            extra: self.take_extra(keep),
        })
    }

    fn memory_size(&self) -> usize {
        self.base.memory_size() + self.extra.data.memory_size()
    }

    fn backend(&self) -> &'static str {
        self.base.backend()
    }

    fn same_kind(&self, columns: Vec<Column>) -> Arc<dyn Frame> {
        self.base.same_kind(columns)
    }

    fn select_columns(&self, cols: &[usize]) -> Arc<dyn Frame> {
        let extra_at = self.base_width;
        let selects_extra = cols.contains(&extra_at);
        if !selects_extra {
            // The common shape of `derive` then `select`: the derived column was scaffolding
            // for a filter and the projection drops it again.
            return self.base.select_columns(cols);
        }
        if cols == [extra_at] {
            // Nothing of the base survives. Handing it an empty projection would ask it for a
            // frame with no columns, and a backend that takes its row count from its first
            // column has none to take it from — `Table` reports zero rows while `extra` still
            // holds one element per row, and `rows()` here delegates to the base. The two
            // backends then disagree about the same data, which is the one thing this seam
            // exists to stop. Reachable from `select amount * 2 as x from sales`.
            return self.base.same_kind(vec![self.extra.clone()]);
        }
        if cols.last() == Some(&extra_at) && !cols[..cols.len() - 1].contains(&extra_at) {
            // The other common shape: keep some of the base and put the derived column last,
            // which is where `derive` put it. Still no copy of the base.
            return Arc::new(WithColumn {
                base_width: cols.len() - 1,
                base: self.base.select_columns(&cols[..cols.len() - 1]),
                extra: self.extra.clone(),
            });
        }
        // A projection that moves the derived column or repeats it. Rare, and the only path
        // here that materialises — through the base's builder, so the result is still the
        // base's representation.
        let schema = self.schema();
        let mut out = Vec::with_capacity(cols.len());
        for &c in cols {
            let mut data = crate::value::ColumnData::with_capacity(schema[c].1, self.rows());
            for row in 0..self.rows() {
                data.push(self.value_at(row, c));
            }
            out.push(Column::new(schema[c].0.clone(), data));
        }
        self.base.same_kind(out)
    }

    fn width(&self) -> usize {
        self.base_width + 1
    }

    fn column_type(&self, col: usize) -> Option<ColumnType> {
        if col == self.base_width {
            Some(self.extra.data.column_type())
        } else {
            self.base.column_type(col)
        }
    }

    fn is_null(&self, row: usize, col: usize) -> bool {
        if col == self.base_width {
            crate::transform::column_is_null(&self.extra.data, row)
        } else {
            self.base.is_null(row, col)
        }
    }

    fn compare_in_column(&self, col: usize, a: usize, b: usize) -> std::cmp::Ordering {
        if col == self.base_width {
            crate::transform::order_within_column(&self.extra.data, a, b)
        } else {
            self.base.compare_in_column(col, a, b)
        }
    }

    fn compare_to_value(&self, row: usize, col: usize, rhs: &Value) -> Option<std::cmp::Ordering> {
        if col == self.base_width {
            crate::transform::compare_column_to(&self.extra.data, row, rhs)
        } else {
            self.base.compare_to_value(row, col, rhs)
        }
    }
}
