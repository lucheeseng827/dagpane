//! A source that is a file on this machine.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dagpane_core::frame::{Frame, FrameBuilder};
use dagpane_core::ColumnType;

use crate::error::SourceError;
use crate::version::{Version, VersionPart};
use crate::{csv, Source};

/// What a file holds.
///
/// One variant today, and it is an enum anyway. The roadmap's own finding was that the
/// manifest had `csv: PathBuf` — "a hardcoded field, not a format enum" — so extending it to
/// Parquet meant changing the manifest type, the compiler and the reader at once. That is
/// the change; this is what it turns into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FileFormat {
    /// RFC 4180-shaped CSV with a header row. See [`crate::csv`] for exactly which shape.
    #[default]
    Csv,
}

impl FileFormat {
    /// The format a path's extension implies, if this build reads it.
    pub fn of_extension(path: &Path) -> Option<FileFormat> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "csv" => Some(FileFormat::Csv),
            _ => None,
        }
    }
}

impl std::fmt::Display for FileFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            FileFormat::Csv => "csv",
        })
    }
}

/// A file on the machine the process is running on.
///
/// The path is **already resolved** when it gets here — against the manifest's own
/// directory, never the process's working directory. That resolution belongs to whoever read
/// the manifest and knows where it was; a source that resolved its own relative path would
/// behave differently depending on which directory somebody started the binary from, which
/// is the bug the rule exists to prevent.
#[derive(Clone, Debug)]
pub struct FileSource {
    path: PathBuf,
    format: FileFormat,
}

impl FileSource {
    /// A file in a known format.
    pub fn new(path: impl Into<PathBuf>, format: FileFormat) -> FileSource {
        FileSource {
            path: path.into(),
            format,
        }
    }

    /// A file whose format its extension names.
    ///
    /// # Errors
    ///
    /// [`SourceError::Misconfigured`] when the extension names nothing this build reads. Not
    /// a guess and not a default: a `.parquet` silently read as CSV produces a column of
    /// binary garbage and a dashboard that looks broken for no stated reason.
    pub fn of_path(path: impl Into<PathBuf>) -> Result<FileSource, SourceError> {
        let path = path.into();
        match FileFormat::of_extension(&path) {
            Some(format) => Ok(FileSource { path, format }),
            None => Err(SourceError::Misconfigured {
                source: format!("file {}", path.display()),
                reason: "this build reads `.csv` and nothing else; name the format explicitly \
                         if the extension is wrong"
                    .to_string(),
            }),
        }
    }

    /// The path, as resolved.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Which format it is read as.
    pub fn format(&self) -> FileFormat {
        self.format
    }

    fn unreachable(&self, e: std::io::Error) -> SourceError {
        SourceError::Unreachable {
            source: self.describe(),
            reason: e.to_string(),
        }
    }
}

impl Source for FileSource {
    /// A path is not a credential, so it is printed whole. A path that *is* sensitive — a
    /// home directory in a shared log — is the operator's own choice of where to put a file.
    fn describe(&self) -> String {
        format!("{} file {}", self.format, self.path.display())
    }

    fn schema(&self) -> Result<Vec<(String, ColumnType)>, SourceError> {
        // A CSV's types are decided by reading it, so this reads it. The trait promises only
        // that `schema` costs no more than a `load`, and for this implementation it costs
        // the same — which is worth saying here rather than leaving a caller to discover it
        // by timing one.
        let columns = csv::load_columns(&self.path).map_err(|e| self.read_error(e))?;
        Ok(columns
            .iter()
            .map(|c| (c.name.clone(), c.data.column_type()))
            .collect())
    }

    fn load(&self, into: Box<dyn FrameBuilder>) -> Result<Arc<dyn Frame>, SourceError> {
        match self.format {
            FileFormat::Csv => csv::load_into(&self.path, into).map_err(|e| self.read_error(e)),
        }
    }

    /// Modification time and length.
    ///
    /// The classic pair, and its failure mode is the one [`Version`] documents: a rewrite
    /// inside the filesystem's timestamp granularity that leaves the length identical. The
    /// nanosecond field is used where the filesystem provides one, which shrinks the window
    /// to something a human editor cannot hit and a program writing fixed-width records in a
    /// loop still can.
    fn version(&self) -> Result<Version, SourceError> {
        let meta = std::fs::metadata(&self.path).map_err(|e| self.unreachable(e))?;
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());

        Ok(Version::of(
            b'f',
            &[
                VersionPart::Num(meta.len()),
                match &modified {
                    // Nanoseconds since the epoch overflow a `u64` in the year 2554, which is
                    // long enough, and seconds alone would put the window back at a second.
                    Some(d) => VersionPart::Num(d.as_nanos().min(u64::MAX as u128) as u64),
                    // A filesystem with no modification time: the length alone is not a
                    // staleness check worth trusting, so the source says it cannot tell and
                    // every refresh reloads. Slow beats wrong.
                    None => VersionPart::Absent,
                },
            ],
        ))
    }
}

impl FileSource {
    fn read_error(&self, e: csv::CsvError) -> SourceError {
        // A missing or unreadable file is `Unreachable` — a retry may find it, and a
        // scheduler that gives up on a file that is being rewritten is worse than one that
        // waits. Anything else the reader complains about is the file's own content.
        match &e {
            csv::CsvError::Io(_) => SourceError::Unreachable {
                source: self.describe(),
                reason: e.to_string(),
            },
            _ => SourceError::Unreadable {
                source: self.describe(),
                reason: e.to_string(),
            },
        }
    }
}
