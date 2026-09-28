//! `dagpane-connect` — where a source's rows come from.
//!
//! One trait, three implementations, and no query engine. A source produces a whole frame;
//! the engine's contract with a value is *produce it, digest it, compare the digest*, and
//! nothing here changes that contract. Predicate pushdown, incremental loading and streaming
//! are all deliberately absent: they change how long [`Source::load`] takes, never what it
//! means.
//!
//! # The two questions, and why they are two
//!
//! ```text
//!   version()  — cheap. "has anything changed?"   metadata only, no rows read
//!   load()     — the rows.                        a file, a request, a query
//! ```
//!
//! Splitting them is the whole point of the trait. A scheduled refresh asks the cheap
//! question on every tick and the expensive one only when the answer moved, and when it does
//! not move the refresh costs one `stat` — which is the exit criterion the roadmap sets for
//! refresh, stated as a shape rather than as an optimisation somebody remembers to apply.
//!
//! **A [`Version`] is not a content digest, and confusing the two is the mistake this module
//! is arranged to prevent.** See its own documentation: it is the source's own claim about
//! its own metadata, comparable only against an earlier claim by the same source, and it can
//! be wrong in one direction. What makes that safe is that a version is never the last word —
//! an unequal version causes a *load*, and the engine's digest of the loaded frame is what
//! decides whether anything recomputes. A version that changes when the data did not costs
//! one wasted read; the case it must never do is the other one, and [`Version`] says exactly
//! when it can.
//!
//! # What is not here
//!
//! No async runtime, no socket this crate listens on, and nothing above [`dagpane_core`] in
//! the dependency arrow — `connect` does not know what a pane is. Both network-capable
//! implementations are **off by default**: a default build of this workspace links no HTTP
//! stack and no database driver, and CI checks that this crate's manifest is the only place
//! either can appear.

#![forbid(unsafe_code)]
#![deny(missing_debug_implementations)]
#![deny(missing_docs)]

pub mod csv;
pub mod file;
#[cfg(feature = "http")]
pub mod http;
pub mod memory;
#[cfg(feature = "sql")]
pub mod sql;

mod error;
mod version;

pub use error::SourceError;
pub use file::FileSource;
#[cfg(feature = "http")]
pub use http::HttpSource;
pub use memory::{BytesSource, SchemaSource};
#[cfg(feature = "sql")]
pub use sql::SqlSource;
pub use version::Version;

use std::fmt;
use std::sync::Arc;

use dagpane_core::frame::{Frame, FrameBuilder};
use dagpane_core::ColumnType;

/// Somewhere rows come from.
///
/// # The signature that differs from the plan that asked for this
///
/// The roadmap's sketch had `fn load(&self) -> Result<Table, SourceError>`, and it was
/// written before the `Frame` seam existed. Returning a `Table` would mean every source
/// materialising `Vec<Option<T>>` and a backend converting it — which is exactly the cost
/// the seam was built to remove, and where the measured memory win on the bundled example
/// comes from. So `load` takes the builder it should fill: the caller chooses the
/// representation, the source fills it directly, and nothing in between builds a copy for
/// somebody else to convert.
pub trait Source: Send + Sync + fmt::Debug {
    /// What this source is, for a log line, an error message and `dagpane check`.
    ///
    /// **Must never carry a credential.** A DSN with a password in it, a URL with a token in
    /// its query string, a bearer header — none of them may appear here, because this string
    /// ends up in error text that ends up in a log that ends up in a bug report. Each
    /// implementation states what it redacts.
    fn describe(&self) -> String;

    /// The columns this source will produce, without producing them.
    ///
    /// Not free for every implementation and not promised to be: a CSV's types are decided
    /// by reading it, so [`FileSource`] reads the file. What this method promises is only
    /// that it costs no *more* than a load.
    ///
    /// # Errors
    ///
    /// [`SourceError`] if the source cannot be reached or cannot be understood.
    fn schema(&self) -> Result<Vec<(String, ColumnType)>, SourceError>;

    /// Read every row into `into` and return the frame it built.
    ///
    /// # Errors
    ///
    /// [`SourceError`] if the source cannot be reached, cannot be parsed, or changes shape
    /// mid-read.
    fn load(&self, into: Box<dyn FrameBuilder>) -> Result<Arc<dyn Frame>, SourceError>;

    /// The cheap staleness check. See [`Version`] for what an equal one does and does not
    /// mean.
    ///
    /// # Errors
    ///
    /// [`SourceError`] if the source cannot be reached. A source that is *temporarily*
    /// unreachable returns an error here rather than a made-up version — a refresh that
    /// cannot ask must report that it could not ask, and never that nothing changed.
    fn version(&self) -> Result<Version, SourceError>;
}

/// A boxed source. What a compiled app holds, one per `[[source]]`.
pub type BoxSource = Arc<dyn Source>;
