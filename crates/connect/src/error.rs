//! What can go wrong between a source and a frame.

use std::fmt;

/// Every way a source can refuse.
///
/// One flat enum rather than a chain of wrapped causes. A person reading this is deciding
/// whether the problem is theirs or the source's, and three levels of `source()` to reach
/// "connection refused" is three levels between them and that answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceError {
    /// The source could not be reached at all: a missing file, a refused connection, a DNS
    /// failure, a timeout.
    Unreachable {
        /// The source, as [`crate::Source::describe`] renders it — credentials already
        /// removed.
        source: String,
        /// What the operating system, the HTTP client or the database driver said.
        reason: String,
    },
    /// The source answered and the answer could not be read as rows: a CSV that will not
    /// parse, a status this crate does not treat as success, a column type nothing here maps.
    Unreadable {
        /// The source, redacted.
        source: String,
        /// What is wrong with the answer.
        reason: String,
    },
    /// The source is configured in a way that cannot work — a URL that is not a URL, a query
    /// that is not read-only, a format this build was not compiled with.
    ///
    /// Distinct from the two above because it is the only one a **retry cannot fix**, and a
    /// scheduler that retries everything forever is the usual consequence of merging them.
    Misconfigured {
        /// The source, redacted.
        source: String,
        /// What about it cannot work.
        reason: String,
    },
}

impl SourceError {
    /// Whether retrying this later could plausibly succeed.
    ///
    /// The one question a refresh scheduler actually has, answered here rather than by each
    /// scheduler matching on variants and getting it subtly different.
    pub fn is_retryable(&self) -> bool {
        matches!(self, SourceError::Unreachable { .. })
    }

    /// The source it is about.
    pub fn source_name(&self) -> &str {
        match self {
            SourceError::Unreachable { source, .. }
            | SourceError::Unreadable { source, .. }
            | SourceError::Misconfigured { source, .. } => source,
        }
    }
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceError::Unreachable { source, reason } => {
                write!(f, "{source} could not be reached: {reason}")
            }
            SourceError::Unreadable { source, reason } => {
                write!(f, "{source} answered, but: {reason}")
            }
            SourceError::Misconfigured { source, reason } => {
                write!(f, "{source} cannot work as configured: {reason}")
            }
        }
    }
}

impl std::error::Error for SourceError {}
