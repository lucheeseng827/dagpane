//! A CSV reader with column type inference.
//!
//! Written in-tree rather than taken from the `csv` crate, and the reason is the second half
//! of the sentence: **the inference is the work**. A reader hands back strings; every column
//! in this project has a type, and deciding it means a pass over the values anyway. Doing
//! both in one pass is a hundred lines whose failure modes a reviewer can hold in their head,
//! against a dependency that would still leave the interesting half to write.
//!
//! What it supports: RFC 4180's shape — a header row, comma separators, `"` quoting with
//! `""` for a literal quote, `\r\n` or `\n` line endings, and an empty field meaning null.
//! What it does not: alternative delimiters, comment lines, embedded newlines outside quotes,
//! or any encoding other than UTF-8. This is enough for the CSV a data app is pointed at, and
//! the parser says so rather than guessing when it meets something else.
//!
//! Inference, per column, first match wins: every non-empty value parses as `i64` → int;
//! as `f64` → float; every value is `true`/`false` (any case) → bool; otherwise text. A
//! column of nothing but empty fields is text, which is the choice that loses the least.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use dagpane_core::frame::{ColumnHint, Frame, FrameBuilder, FrameRef, TableBuilder};
use dagpane_core::{Column, ColumnType, Table};
// Only the `#[cfg(test)]` inference oracle names it now — the reader pushes into a builder
// rather than constructing columns itself.
#[cfg(test)]
use dagpane_core::ColumnData;

/// What can go wrong reading a CSV. Every variant that names a line names it 1-based and
/// counted over the file as written, blank lines included, so it matches what an editor shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CsvError {
    /// The file has no header row — it is empty, or contains only blank lines. There is no
    /// zero-column table to fall back to, so this is an error rather than an empty result.
    Empty,
    /// A row with a different number of fields from the header. Reported with the line
    /// number, because a CSV nobody can find the bad line in is a CSV nobody fixes.
    Ragged {
        /// The offending line.
        line: usize,
        /// How many fields the header declared.
        expected: usize,
        /// How many this line has.
        found: usize,
    },
    /// A field opens a `"` that the line never closes. A newline inside quotes is one of the
    /// RFC 4180 features this reader does not support, and this is how it says so instead of
    /// silently truncating the row.
    UnterminatedQuote {
        /// The line the unclosed quote is on.
        line: usize,
    },
    /// The file could not be read, or the parsed columns could not form a table. Carries the
    /// underlying message, already prefixed with the path where there is one.
    Io(String),
}

impl fmt::Display for CsvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CsvError::Empty => f.write_str("the file has no header row"),
            CsvError::Ragged {
                line,
                expected,
                found,
            } => write!(
                f,
                "line {line} has {found} fields; the header has {expected}"
            ),
            CsvError::UnterminatedQuote { line } => {
                write!(f, "line {line} opens a quote and never closes it")
            }
            CsvError::Io(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for CsvError {}

/// Read a file and parse it.
///
/// # Errors
///
/// [`CsvError::Io`] if the file cannot be read or is not UTF-8, or any [`parse`] error.
pub fn load(path: &Path) -> Result<Table, CsvError> {
    parse(&read(path)?)
}

/// Read a file into columns. What a source load actually calls.
pub fn load_columns(path: &Path) -> Result<Vec<Column>, CsvError> {
    parse_columns(&read(path)?)
}

fn read(path: &Path) -> Result<String, CsvError> {
    std::fs::read_to_string(path).map_err(|e| CsvError::Io(format!("{}: {e}", path.display())))
}

/// Parse CSV text into a table, inferring each column's type.
///
/// The first non-blank line is the header and names the columns; blank lines anywhere are
/// skipped. See the module docs for the inference rules and for what of RFC 4180 is and is
/// not supported.
///
/// # Errors
///
/// [`CsvError::Empty`] if there is no header, [`CsvError::Ragged`] if a row's field count
/// differs from the header's, or [`CsvError::UnterminatedQuote`] for an unclosed quote.
pub fn parse(text: &str) -> Result<Table, CsvError> {
    let columns = parse_columns(text)?;
    // Every column was built from the same number of rows by construction, so `new` cannot
    // actually fail here. It is still mapped rather than unwrapped: a panic in a CSV reader
    // takes down whatever is loading the app, and an error that cannot happen costs one line.
    Table::new(columns).map_err(|e| CsvError::Io(e.to_string()))
}

/// Parse into columns, without choosing a representation for them.
///
/// The split exists so a caller can build whichever [`dagpane_core::frame::Frame`] it wants
/// from the same parse. Type inference — which is the part with the interesting decisions in
/// it, and the part every published count depends on — happens here and only here, so no
/// backend can move a number by reading the file differently.
/// Parse into core's own columns.
///
/// Kept for `parse` and for the tests that diff this reader against the buffered one it
/// replaced. A source load goes through [`load_into`] instead, which is the path that avoids
/// building a representation the backend will only convert.
pub fn parse_columns(text: &str) -> Result<Vec<Column>, CsvError> {
    let frame = parse_into(text, Box::new(TableBuilder::new()))?;
    Ok(FrameRef::new(frame).to_table().columns().to_vec())
}

/// Parse into whichever frame the builder builds.
///
/// The entry point a source load uses. The caller chooses the representation and the reader
/// never learns which one it filled.
pub fn parse_into(
    text: &str,
    mut builder: Box<dyn FrameBuilder>,
) -> Result<Arc<dyn Frame>, CsvError> {
    let header = header_of(text)?;
    let probes = infer_types(text, &header)?;
    read_into(text, &header, &probes, &mut *builder)?;
    Ok(builder.finish())
}

/// Read a file into whichever frame the builder builds.
pub fn load_into(path: &Path, builder: Box<dyn FrameBuilder>) -> Result<Arc<dyn Frame>, CsvError> {
    parse_into(&read(path)?, builder)
}

/// Two passes over the text, holding nothing but the answer.
///
/// The obvious reader buffers every field into a `Vec<Vec<String>>` and infers afterwards,
/// and that is what this used to do. It costs one live `String` per cell: a 1M x 6 file held
/// six million of them at once and peaked at 393 MB to produce 34 MB of columns. Type
/// inference genuinely needs to see every value before it can name a column's type, so the
/// buffering looked forced.
///
/// It is not. Inference does not need the *values*, only three booleans per column — does
/// everything parse as an integer, as a float, as a bool — and those fold one row at a time.
/// So the first pass carries [`ColumnProbe`]s and discards every field as it goes, the second
/// parses straight into the typed column the first decided on, and peak memory becomes the
/// output plus one row.
///
/// The price is parsing the text twice. That is CPU against an order of magnitude of memory,
/// on a path that runs once at load rather than once per interaction.
fn header_of(text: &str) -> Result<Vec<String>, CsvError> {
    let (_, line) = rows(text).next().ok_or(CsvError::Empty)?;
    split_line(line, 1)
}

/// Data rows, numbered as the file numbers them, with blank lines skipped — the header is
/// the caller's to drop.
fn rows(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| (i + 1, l))
}

/// What a column could still be, after everything seen so far.
///
/// Each flag starts true and only ever falls, so the fold is order-independent and one row
/// can never resurrect a possibility an earlier row ruled out.
#[derive(Clone)]
struct ColumnProbe {
    /// Any non-empty value at all. A column of nothing but blanks is text, matching the
    /// `!filled.is_empty()` guard the buffered version used on every branch.
    filled: bool,
    int: bool,
    float: bool,
    boolean: bool,
    /// Distinct non-empty values, abandoned once there are more than a backend could use.
    ///
    /// Counted here because this pass already visits every field, and a streaming builder
    /// has to choose a physical layout before it sees any. Carrying the count forward is
    /// cheaper than the alternatives: guessing wrong makes a wide column larger, and
    /// counting later means buffering the file again.
    distinct: Option<BTreeSet<String>>,
}

impl ColumnProbe {
    fn new() -> ColumnProbe {
        ColumnProbe {
            filled: false,
            int: true,
            float: true,
            boolean: true,
            distinct: Some(BTreeSet::new()),
        }
    }

    /// How many distinct values, if the count stayed small enough to be worth acting on.
    fn distinct_count(&self) -> Option<usize> {
        self.distinct.as_ref().map(BTreeSet::len)
    }

    fn see(&mut self, field: &str) {
        if field.is_empty() {
            return; // an empty field is a null in every type, so it rules nothing out
        }
        self.filled = true;
        self.int &= field.parse::<i64>().is_ok();
        self.float &= field.parse::<f64>().is_ok();
        self.boolean &= matches!(field.to_ascii_lowercase().as_str(), "true" | "false");

        // Bounded: past the cap no backend would encode the column anyway, so the set is
        // dropped rather than grown into a second copy of it.
        if let Some(seen) = &mut self.distinct {
            if seen.len() > DISTINCT_CAP {
                self.distinct = None;
            } else if !seen.contains(field) {
                seen.insert(field.to_string());
            }
        }
    }

    /// The same ladder the buffered version walked, in the same order: int, then float, then
    /// bool, then text. Order matters — `1` parses as both an integer and a float, and the
    /// column is an integer.
    fn decide(&self) -> ColumnType {
        if !self.filled {
            ColumnType::Text
        } else if self.int {
            ColumnType::Int
        } else if self.float {
            ColumnType::Float
        } else if self.boolean {
            ColumnType::Bool
        } else {
            ColumnType::Text
        }
    }
}

/// Pass one: decide each column's type, keeping no values.
fn infer_types(text: &str, header: &[String]) -> Result<Vec<ColumnProbe>, CsvError> {
    let mut probes = vec![ColumnProbe::new(); header.len()];
    for (number, line) in rows(text).skip(1) {
        let fields = split_line(line, number)?;
        check_width(&fields, header, number)?;
        for (probe, field) in probes.iter_mut().zip(&fields) {
            probe.see(field);
        }
    }
    Ok(probes)
}

/// Above this many distinct values a column is never dictionary-encoded, so counting further
/// buys nothing and the set is dropped. Matches the backend's own ceiling.
const DISTINCT_CAP: usize = 4096;

/// Pass two: parse straight into a builder's arrays.
///
/// Takes the builder rather than returning columns, so the reader never materialises a
/// representation the backend is only going to convert. Before this, loading an Arrow source
/// built the whole file as `Vec<Option<T>>` and converted, with both live at once — 139 MB
/// of intermediate against 34 MB of output on a 1M x 6 file.
///
/// A field that does not parse becomes a null rather than an error, exactly as the buffered
/// reader did. It can only happen for a value pass one never saw, which cannot occur while
/// both passes read the same text — but a reader that panicked on it would trade a null cell
/// for a dead process, which is the wrong trade in a file loader.
fn read_into(
    text: &str,
    header: &[String],
    probes: &[ColumnProbe],
    builder: &mut dyn FrameBuilder,
) -> Result<(), CsvError> {
    let rows_total = rows(text).skip(1).count();
    let types: Vec<ColumnType> = probes.iter().map(ColumnProbe::decide).collect();

    for (i, name) in header.iter().enumerate() {
        builder.begin_column(
            name,
            types[i],
            ColumnHint {
                rows: Some(rows_total),
                // Only meaningful for a column that stayed text; harmless elsewhere, since a
                // backend only consults it when choosing a text layout.
                distinct: probes[i].distinct_count(),
            },
        );
    }

    for (number, line) in rows(text).skip(1) {
        let fields = split_line(line, number)?;
        check_width(&fields, header, number)?;
        for (col, field) in fields.iter().enumerate() {
            let empty = field.is_empty();
            match types[col] {
                ColumnType::Int => {
                    builder.push_int(col, if empty { None } else { field.parse().ok() })
                }
                ColumnType::Float => {
                    builder.push_float(col, if empty { None } else { field.parse().ok() })
                }
                ColumnType::Bool => builder.push_bool(
                    col,
                    match field.to_ascii_lowercase().as_str() {
                        "true" => Some(true),
                        "false" => Some(false),
                        _ => None,
                    },
                ),
                // Borrowed, never cloned: the backend copies into its own storage if it
                // needs to, and an Arrow column copies into a shared byte buffer instead of
                // allocating a `String` per cell.
                ColumnType::Text => {
                    builder.push_text(col, if empty { None } else { Some(field.as_str()) })
                }
            }
        }
    }
    Ok(())
}

fn check_width(fields: &[String], header: &[String], number: usize) -> Result<(), CsvError> {
    if fields.len() != header.len() {
        return Err(CsvError::Ragged {
            line: number,
            expected: header.len(),
            found: fields.len(),
        });
    }
    Ok(())
}

fn split_line(line: &str, number: usize) -> Result<Vec<String>, CsvError> {
    // Each field carries whether it was quoted, because that decides whether it may be
    // trimmed. Tracking it only in a loop-local `quoted` flag was a bug: the flag was already
    // false by the time the trim ran, so `" x "` came back as `"x"` — the exact case the
    // comment below says quoting exists to express.
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut field = String::new();
    let mut chars = line.chars().peekable();
    let mut quoted = false;
    let mut closed_quote = false;
    let mut was_quoted = false;

    while let Some(c) = chars.next() {
        match c {
            '"' if !quoted && field.is_empty() && !closed_quote => {
                quoted = true;
                was_quoted = true;
            }
            '"' if quoted => {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                    closed_quote = true;
                }
            }
            ',' if !quoted => {
                out.push((std::mem::take(&mut field), was_quoted));
                closed_quote = false;
                was_quoted = false;
            }
            c => field.push(c),
        }
    }
    if quoted {
        return Err(CsvError::UnterminatedQuote { line: number });
    }
    out.push((field, was_quoted));

    // An unquoted field is trimmed; a quoted one is taken exactly as written, which is the
    // only way to express a value with leading or trailing spaces.
    Ok(out
        .into_iter()
        .map(|(f, was_quoted)| {
            if !was_quoted && (f.starts_with(' ') || f.ends_with(' ')) {
                f.trim().to_string()
            } else {
                f
            }
        })
        .collect())
}

/// The original buffered inference, kept **only** as a test oracle.
///
/// It is what every published count was measured against, so the two-pass reader that
/// replaced it has to agree with it exactly rather than approximately. Deleting it would
/// have left that agreement asserted by nothing. `#[cfg(test)]` so it is not compiled into
/// the binary it was removed from.
#[cfg(test)]
fn infer(values: &[String]) -> ColumnData {
    let filled: Vec<&String> = values.iter().filter(|v| !v.is_empty()).collect();

    if !filled.is_empty() && filled.iter().all(|v| v.parse::<i64>().is_ok()) {
        return ColumnData::Int(
            values
                .iter()
                .map(|v| if v.is_empty() { None } else { v.parse().ok() })
                .collect(),
        );
    }
    if !filled.is_empty() && filled.iter().all(|v| v.parse::<f64>().is_ok()) {
        return ColumnData::Float(
            values
                .iter()
                .map(|v| if v.is_empty() { None } else { v.parse().ok() })
                .collect(),
        );
    }
    if !filled.is_empty()
        && filled
            .iter()
            .all(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "false"))
    {
        return ColumnData::Bool(
            values
                .iter()
                .map(|v| match v.to_ascii_lowercase().as_str() {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                })
                .collect(),
        );
    }
    ColumnData::Text(
        values
            .iter()
            .map(|v| if v.is_empty() { None } else { Some(v.clone()) })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use dagpane_core::{ColumnType, Value};

    #[test]
    fn types_are_inferred_per_column() {
        let t = parse("a,b,c,d\n1,1.5,true,x\n2,2.5,false,y\n").unwrap();
        let types: Vec<ColumnType> = t.columns().iter().map(|c| c.data.column_type()).collect();
        assert_eq!(
            types,
            vec![
                ColumnType::Int,
                ColumnType::Float,
                ColumnType::Bool,
                ColumnType::Text
            ]
        );
    }

    #[test]
    fn one_non_numeric_value_makes_the_whole_column_text() {
        let t = parse("n\n1\n2\nn/a\n").unwrap();
        assert_eq!(t.column("n").unwrap().data.column_type(), ColumnType::Text);
    }

    #[test]
    fn an_empty_field_is_null_not_a_zero() {
        let t = parse("n\n1\n\n3\n").unwrap();
        assert_eq!(t.rows(), 2, "a blank line is not a row");
        let t = parse("n,m\n1,\n3,4\n").unwrap();
        assert_eq!(t.column("m").unwrap().data.value_at(0), Value::Null);
    }

    #[test]
    fn a_column_of_nothing_is_text_rather_than_a_guess() {
        let t = parse("a,b\n1,\n2,\n").unwrap();
        assert_eq!(t.column("b").unwrap().data.column_type(), ColumnType::Text);
    }

    #[test]
    fn quotes_protect_commas_and_double_up_for_a_literal_quote() {
        let t = parse("a,b\n\"x,y\",\"say \"\"hi\"\"\"\n").unwrap();
        assert_eq!(t.column("a").unwrap().data.value_at(0), Value::text("x,y"));
        assert_eq!(
            t.column("b").unwrap().data.value_at(0),
            Value::text("say \"hi\"")
        );
    }

    #[test]
    fn a_quoted_field_keeps_its_spaces_and_an_unquoted_one_does_not() {
        // The whole point of quoting. This was broken: the quoted flag was loop-local, so the
        // trim could not see it and `" x "` came back trimmed.
        let t = parse("a,b\n\" x \", y \n").unwrap();
        assert_eq!(t.column("a").unwrap().data.value_at(0), Value::text(" x "));
        assert_eq!(t.column("b").unwrap().data.value_at(0), Value::text("y"));
    }

    #[test]
    fn a_short_row_is_an_error_that_names_the_line() {
        assert_eq!(
            parse("a,b\n1,2\n3\n"),
            Err(CsvError::Ragged {
                line: 3,
                expected: 2,
                found: 1
            })
        );
    }

    #[test]
    fn an_unterminated_quote_is_an_error() {
        assert!(matches!(
            parse("a\n\"oops\n"),
            Err(CsvError::UnterminatedQuote { .. })
        ));
    }

    #[test]
    fn an_empty_file_is_an_error_not_an_empty_table() {
        assert_eq!(parse(""), Err(CsvError::Empty));
    }

    #[test]
    fn a_header_with_no_rows_is_a_real_empty_table() {
        let t = parse("a,b\n").unwrap();
        assert_eq!((t.rows(), t.width()), (0, 2));
    }
}

#[cfg(test)]
mod streaming_equivalence {
    //! The two-pass reader against the buffered one it replaced.
    //!
    //! Type inference decides what every published count is a count *of*, so "the new reader
    //! is faster and uses less memory" is worth nothing unless it reads identically. These
    //! run both implementations over the same fields and compare the columns they produce.

    use super::*;

    /// Build a one-column CSV from raw fields and read it the new way.
    fn streamed(fields: &[&str]) -> ColumnData {
        let mut text = String::from("c\n");
        for f in fields {
            text.push_str(f);
            text.push('\n');
        }
        // A blank field on its own line is a blank *row*, which `rows` skips — so pad the
        // file to two columns and read the first, keeping empty fields reachable.
        let mut two = String::from("c,pad\n");
        for f in fields {
            two.push_str(f);
            two.push_str(",x\n");
        }
        parse_columns(&two).expect("valid csv").swap_remove(0).data
    }

    /// The buffered reader, fed **post-split** fields.
    ///
    /// `split_line` trims, so handing raw text to `infer` would compare the old reader's
    /// inference against the new reader's inference *plus* the splitter — and the first
    /// version of this test failed on `" 4"` for exactly that reason. Both sides must see
    /// what the splitter produces, because in production both did.
    fn buffered(fields: &[&str]) -> ColumnData {
        let split: Vec<String> = fields
            .iter()
            .map(|f| split_line(&format!("{f},x"), 1).expect("one field and a pad")[0].clone())
            .collect();
        infer(&split)
    }

    /// Equal, with `NaN` treated as equal to itself.
    ///
    /// `nan` is a legitimate thing to find in a CSV and worth covering, but `NaN != NaN`, so
    /// a derived comparison reports two identical columns as different. That is a property of
    /// floats rather than of either reader.
    fn same(a: &ColumnData, b: &ColumnData) -> bool {
        match (a, b) {
            (ColumnData::Float(x), ColumnData::Float(y)) => {
                x.len() == y.len()
                    && x.iter().zip(y).all(|(p, q)| match (p, q) {
                        (Some(p), Some(q)) => p == q || (p.is_nan() && q.is_nan()),
                        (None, None) => true,
                        _ => false,
                    })
            }
            _ => a == b,
        }
    }

    fn agree(fields: &[&str]) {
        let (s, b) = (streamed(fields), buffered(fields));
        assert!(
            same(&s, &b),
            "the two readers disagreed on {fields:?}\n  new: {s:?}\n  old: {b:?}"
        );
    }

    #[test]
    fn the_four_column_types() {
        agree(&["1", "2", "3"]);
        agree(&["1.5", "2.25"]);
        agree(&["true", "false"]);
        agree(&["north", "south"]);
    }

    #[test]
    fn an_integer_wins_over_a_float_because_the_ladder_is_ordered() {
        // `1` parses as both; the column is an integer. Getting the order wrong here would
        // retype half the columns in every app.
        agree(&["1", "2"]);
        agree(&["1", "2.5"]);
    }

    #[test]
    fn empty_fields_are_nulls_in_every_type() {
        agree(&["1", "", "3"]);
        agree(&["1.5", "", "2.5"]);
        agree(&["true", "", "false"]);
        agree(&["a", "", "b"]);
    }

    #[test]
    fn a_column_of_nothing_but_blanks_is_text() {
        agree(&["", "", ""]);
    }

    #[test]
    fn one_stray_value_demotes_the_whole_column() {
        agree(&["1", "2", "x"]);
        agree(&["1.5", "oops"]);
        agree(&["true", "maybe"]);
    }

    #[test]
    fn bools_are_case_insensitive_and_nothing_else_is_a_bool() {
        agree(&["TRUE", "False", "true"]);
        agree(&["yes", "no"]);
        agree(&["1", "0"]);
    }

    #[test]
    fn numbers_at_the_awkward_edges() {
        agree(&["-1", "-2"]);
        agree(&["+1", "2"]);
        agree(&["1e3", "2e4"]);
        agree(&["0.0", "-0.0"]);
        agree(&["inf", "nan"]);
        // Too big for an i64, fine as an f64 — so a Float column, not an Int one.
        agree(&["99999999999999999999", "1"]);
        agree(&[" 1", "2"]);
        agree(&["1 ", "2"]);
    }

    #[test]
    fn a_generated_corpus_agrees_row_for_row() {
        // Deterministic, and wide enough that a rule missed in one branch shows up.
        let pool = [
            "1",
            "2",
            "-3",
            "1.5",
            "1e2",
            "true",
            "FALSE",
            "",
            "x",
            "north",
            "0",
            "99999999999999999999",
            " 4",
            "nan",
        ];
        let mut seed = 0x2545F491_4F6CDD1Du64;
        for case in 0..400 {
            let len = 1 + (case % 7);
            let fields: Vec<&str> = (0..len)
                .map(|_| {
                    seed = seed
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    pool[(seed >> 33) as usize % pool.len()]
                })
                .collect();
            let (new, old) = (streamed(&fields), buffered(&fields));
            assert!(
                same(&new, &old),
                "case {case} disagreed on {fields:?}\n  new: {new:?}\n  old: {old:?}"
            );
        }
    }
}
