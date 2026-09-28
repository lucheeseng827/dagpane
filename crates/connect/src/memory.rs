//! A source whose bytes the caller already has.
//!
//! Every other source in this crate reaches for something — a file, a URL, a database. This
//! one is handed its bytes and holds them, and it exists because **there are hosts with no
//! filesystem to reach into**. A browser is the one that motivated it: `dagpane-wasm`
//! compiles the same manifest the server compiles, but `[[source]] csv = "sales.csv"` has to
//! resolve to bytes the page supplied rather than to a path, because wasm32 has neither a
//! working directory nor an open(2).
//!
//! It is not a browser-only thing. A test that wants a source without a `tempfile`, a host
//! embedding an app in a binary, and a control plane that already fetched the bytes for its
//! own reasons all want the same type.
//!
//! # What a version means here
//!
//! A file's version is its mtime and length; a URL's is its `ETag`. This one has no external
//! thing to ask, so its version is a digest of **the bytes themselves**. That is stronger
//! than either — it cannot report "unchanged" for content that changed — and it costs a pass
//! over the data, which is the trade the other two avoid by asking somebody else. For a
//! source that is already in memory there is nothing to avoid.

use std::sync::Arc;

use dagpane_core::frame::{ColumnHint, Frame, FrameBuilder};
use dagpane_core::ColumnType;

use crate::error::SourceError;
use crate::file::FileFormat;
use crate::version::{Version, VersionPart};
use crate::{csv, Source};

/// Bytes the caller already holds, read as one of the formats this build understands.
#[derive(Clone, Debug)]
pub struct BytesSource {
    /// What to call this in an error message. The manifest's path, usually — so a browser
    /// reports `no column `x` in sales.csv` exactly as the server does, even though nothing
    /// opened a file.
    name: String,
    format: FileFormat,
    text: Arc<str>,
}

impl BytesSource {
    /// Bytes in a known format, named for whatever the error messages should call them.
    ///
    /// `Arc<str>` rather than `String`: a host holding one app open for many sessions hands
    /// the same bytes to each, and cloning a source must not copy the data.
    pub fn new(name: impl Into<String>, format: FileFormat, text: impl Into<Arc<str>>) -> Self {
        BytesSource {
            name: name.into(),
            format,
            text: text.into(),
        }
    }

    /// Bytes whose format the name's extension implies.
    ///
    /// # Errors
    ///
    /// [`SourceError::Misconfigured`] when the extension names nothing this build reads —
    /// the same refusal [`crate::FileSource::of_path`] makes, and for the same reason: a
    /// `.parquet` read as CSV is a column of binary garbage and a dashboard that looks
    /// broken for no stated reason.
    pub fn of_name(
        name: impl Into<String>,
        text: impl Into<Arc<str>>,
    ) -> Result<Self, SourceError> {
        let name = name.into();
        match FileFormat::of_extension(std::path::Path::new(&name)) {
            Some(format) => Ok(BytesSource::new(name, format, text)),
            None => Err(SourceError::Misconfigured {
                source: format!("bytes named {name}"),
                reason: "this build reads `.csv` and nothing else; name the format explicitly \
                         if the extension is wrong"
                    .to_string(),
            }),
        }
    }

    /// The text, as given.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Which format it is read as.
    pub fn format(&self) -> FileFormat {
        self.format
    }

    fn unreadable(&self, e: csv::CsvError) -> SourceError {
        // Never `Unreachable`. That variant means "ask again later", and there is no later:
        // the bytes are already here and they will not improve. A scheduler that retried
        // this would retry forever.
        SourceError::Unreadable {
            source: self.describe(),
            reason: e.to_string(),
        }
    }
}

impl Source for BytesSource {
    /// The given name and the format. No bytes: a source's description ends up in logs, and
    /// an in-memory CSV is as likely to hold something private as a file is.
    fn describe(&self) -> String {
        format!("{} bytes {}", self.format, self.name)
    }

    fn schema(&self) -> Result<Vec<(String, ColumnType)>, SourceError> {
        let columns = csv::parse_columns(&self.text).map_err(|e| self.unreadable(e))?;
        Ok(columns
            .iter()
            .map(|c| (c.name.clone(), c.data.column_type()))
            .collect())
    }

    fn load(&self, into: Box<dyn FrameBuilder>) -> Result<Arc<dyn Frame>, SourceError> {
        match self.format {
            FileFormat::Csv => csv::parse_into(&self.text, into).map_err(|e| self.unreadable(e)),
        }
    }

    /// A digest of the bytes.
    ///
    /// See the module docs: this is the one source that can answer the staleness question
    /// exactly, because it is the one holding the thing the question is about.
    fn version(&self) -> Result<Version, SourceError> {
        Ok(Version::of(b'm', &[VersionPart::Text(&self.text)]))
    }
}

/// A source's **shape**, with no rows behind it.
///
/// # Why this exists
///
/// A split app's page half has to compile the same manifest the server compiled, and
/// `manifest::compile_with` loads every `[[source]]` to do it — a CSV's column types are
/// decided by reading it, so there is no compiling without data of some kind. That is a
/// problem precisely for the cut worth making: place the filter in the page, leave the rows
/// on the server, and now the page must type-check a pipeline over data it is not supposed
/// to have.
///
/// The rows are not what the compiler wants, though. It wants **types**, and those are a
/// column name and a [`ColumnType`] each — a few dozen bytes for a table of any size. So the
/// server sends the shape, the page compiles against a frame with that shape and no rows in
/// it, and the real values arrive afterwards as the frontier, which is where they were always
/// going to come from.
///
/// # What it is not
///
/// Not an empty table you would show anybody. A pane over one renders as zero rows, which is
/// correct and useless — this type exists for the seconds between "the page has the manifest"
/// and "the page has the first frontier", and a session that stays in that state is a bug in
/// its host rather than a use of this.
#[derive(Clone, Debug)]
pub struct SchemaSource {
    name: String,
    columns: Vec<(String, ColumnType)>,
}

impl SchemaSource {
    /// A source that will answer with these columns and no rows.
    pub fn new(name: impl Into<String>, columns: Vec<(String, ColumnType)>) -> Self {
        SchemaSource {
            name: name.into(),
            columns,
        }
    }

    /// The columns, as given.
    pub fn columns(&self) -> &[(String, ColumnType)] {
        &self.columns
    }
}

impl Source for SchemaSource {
    /// Names the source and says plainly that it carries no rows, because an error message
    /// reading `no column ˋxˋ in sales` is confusing when the reason is that nothing was
    /// loaded at all.
    fn describe(&self) -> String {
        format!("the shape of {} (no rows)", self.name)
    }

    fn schema(&self) -> Result<Vec<(String, ColumnType)>, SourceError> {
        Ok(self.columns.clone())
    }

    fn load(&self, mut into: Box<dyn FrameBuilder>) -> Result<Arc<dyn Frame>, SourceError> {
        for (name, ty) in &self.columns {
            // `rows: Some(0)` is information and not a guess: a backend that preallocates can
            // allocate nothing, which is the whole point of this source.
            into.begin_column(
                name,
                *ty,
                ColumnHint {
                    rows: Some(0),
                    distinct: Some(0),
                },
            );
        }
        Ok(into.finish())
    }

    /// A digest of the shape.
    ///
    /// Two schema sources with the same columns are the same source, and one whose columns
    /// moved is a different one — which is right, because a column appearing or changing type
    /// is exactly the event that should recompile an app rather than quietly re-run it.
    fn version(&self) -> Result<Version, SourceError> {
        let mut parts: Vec<String> = Vec::with_capacity(self.columns.len());
        for (name, ty) in &self.columns {
            parts.push(format!("{name}:{ty}"));
        }
        let joined = parts.join(",");
        Ok(Version::of(b's', &[VersionPart::Text(&joined)]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dagpane_core::frame::TableBuilder;
    use dagpane_core::ColumnType;

    const SALES: &str = "region,amount\nnorth,10\nsouth,30\n";

    fn source() -> BytesSource {
        BytesSource::new("sales.csv", FileFormat::Csv, SALES)
    }

    #[test]
    fn it_loads_the_bytes_it_was_given() {
        let frame = source().load(Box::new(TableBuilder::new())).unwrap();
        assert_eq!(frame.rows(), 2);
        assert_eq!(frame.column_names(), vec!["region", "amount"]);
    }

    #[test]
    fn its_schema_agrees_with_what_it_loads() {
        let s = source();
        let schema = s.schema().unwrap();
        let frame = s.load(Box::new(TableBuilder::new())).unwrap();
        assert_eq!(schema, frame.schema());
    }

    #[test]
    fn the_version_moves_with_the_bytes_and_only_with_them() {
        let a = BytesSource::new("x.csv", FileFormat::Csv, SALES);
        let same = BytesSource::new("other-name.csv", FileFormat::Csv, SALES);
        let edited = BytesSource::new("x.csv", FileFormat::Csv, "region,amount\nnorth,11\n");

        assert_eq!(
            a.version().unwrap(),
            same.version().unwrap(),
            "the name is not the content"
        );
        assert_ne!(a.version().unwrap(), edited.version().unwrap());
    }

    #[test]
    fn an_unreadable_body_is_never_worth_retrying() {
        let s = BytesSource::new("bad.csv", FileFormat::Csv, "");
        let e = s.schema().unwrap_err();
        assert!(
            matches!(e, SourceError::Unreadable { .. }),
            "bytes already in hand cannot become reachable later: {e:?}"
        );
    }

    #[test]
    fn an_extension_this_build_cannot_read_is_refused_rather_than_guessed() {
        assert!(BytesSource::of_name("sales.parquet", SALES).is_err());
        assert!(BytesSource::of_name("sales.csv", SALES).is_ok());
    }

    #[test]
    fn a_schema_source_has_the_columns_and_no_rows() {
        let s = SchemaSource::new(
            "sales",
            vec![
                ("region".to_string(), ColumnType::Text),
                ("amount".to_string(), ColumnType::Float),
            ],
        );
        let frame = s.load(Box::new(TableBuilder::new())).unwrap();
        assert_eq!(frame.rows(), 0);
        assert_eq!(frame.column_names(), vec!["region", "amount"]);
        // The property the whole type is for: what a compiler asks a source, it answers the
        // same way the real one would.
        assert_eq!(s.schema().unwrap(), frame.schema());
    }

    #[test]
    fn a_schema_source_agrees_with_the_bytes_it_stands_in_for() {
        // The substitution has to be invisible to a compiler, so the shape this reports must
        // be the shape the real source would have reported. If CSV inference ever changed
        // what it decides, this is where a page compiling against a stand-in would start
        // type-checking a different app from the one the server compiled.
        let real = BytesSource::new("sales.csv", FileFormat::Csv, SALES);
        let stand_in = SchemaSource::new("sales", real.schema().unwrap());
        assert_eq!(real.schema().unwrap(), stand_in.schema().unwrap());

        let a = real.load(Box::new(TableBuilder::new())).unwrap();
        let b = stand_in.load(Box::new(TableBuilder::new())).unwrap();
        assert_eq!(a.schema(), b.schema(), "same columns");
        assert_ne!(a.rows(), b.rows(), "and the rows are the difference");
    }

    #[test]
    fn a_schema_sources_version_moves_with_the_shape_and_only_with_it() {
        let cols = |ty| vec![("a".to_string(), ty)];
        let int = SchemaSource::new("x", cols(ColumnType::Int));
        let same = SchemaSource::new("other-name", cols(ColumnType::Int));
        let retyped = SchemaSource::new("x", cols(ColumnType::Float));
        let renamed = SchemaSource::new("x", vec![("b".to_string(), ColumnType::Int)]);

        assert_eq!(
            int.version().unwrap(),
            same.version().unwrap(),
            "the source's own name is not its shape"
        );
        // Both of these should recompile an app rather than quietly re-run it.
        assert_ne!(int.version().unwrap(), retyped.version().unwrap());
        assert_ne!(int.version().unwrap(), renamed.version().unwrap());
    }

    #[test]
    fn the_description_says_there_are_no_rows() {
        // `no column `x` in sales` is a confusing message when the reason is that nothing was
        // loaded at all, so the description says which kind of source this is.
        let d = SchemaSource::new("sales", vec![]).describe();
        assert!(d.contains("sales"), "{d}");
        assert!(d.contains("no rows"), "{d}");
    }

    #[test]
    fn the_description_carries_no_bytes() {
        // The literal below holds a credential ON PURPOSE: this test exists to assert that
        // `describe` does **not** carry it. A source's description ends up in error text that
        // ends up in a log, and an in-memory CSV is as likely to hold something private as a
        // file is.
        //
        // The binding is `private_rows` rather than `secret` because the repository's own
        // scanner matches `secret = "…"` — a *credential-shaped name* assigned a literal —
        // and this is a CSV fixture, not a credential this program uses. Renaming it stops a
        // standing false positive without weakening the assertion by one character; the
        // scanner would be right about `let secret = "…"` in almost any other file.
        let private_rows = "name,token\nada,hunter2\n";
        let d = BytesSource::new("people.csv", FileFormat::Csv, private_rows).describe();
        assert!(!d.contains("hunter2"), "{d}");
    }
}
