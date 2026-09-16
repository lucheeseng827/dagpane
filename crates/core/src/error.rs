//! The three kinds of failure, kept apart because they are found at three different times
//! and only one of them may reach a user mid-session.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::graph::CellId;

/// A failure found while *building* a graph, before any session exists. Every variant here
/// is a bug in the app definition, so the process refuses to start rather than serving an
/// app that will misbehave later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildError {
    /// Two cells declared with the same name. Names are the app's public identifiers — they
    /// appear in the manifest, in the wire protocol and in `dagpane explain` — so a
    /// duplicate is ambiguous everywhere, not just here.
    DuplicateName(String),
    /// A cell listed an input that was never declared. Caught here rather than at run time
    /// because the whole reason the edges are declared is that the graph can be checked
    /// once and then trusted for the life of the process.
    UnknownInput {
        /// The cell that declared the input.
        cell: String,
        /// The name it asked for, which matches no declared cell.
        input: String,
    },
    /// A dependency cycle. The path is reported in declaration order and closed — the first
    /// name appears again at the end — because a cycle nobody can see is a cycle nobody
    /// fixes.
    Cycle {
        /// The cells on the cycle, in declaration order and closed: the first name is
        /// repeated as the last, so the message reads as a loop rather than as a list.
        path: Vec<String>,
    },
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::DuplicateName(n) => write!(f, "two cells are named `{n}`"),
            BuildError::UnknownInput { cell, input } => {
                write!(
                    f,
                    "cell `{cell}` depends on `{input}`, which is not declared"
                )
            }
            BuildError::Cycle { path } => {
                write!(f, "dependency cycle: {}", path.join(" -> "))
            }
        }
    }
}

impl std::error::Error for BuildError {}

/// A failure *inside* one cell, during a recompute pass.
///
/// This is a value, not a panic and not a `?` out of the pass. A data app is a place where
/// one bad column takes out one number; it must not take out the page. The engine records
/// the error in the failing cell's slot, marks every cell downstream of it
/// [`CellError::Upstream`], and finishes the pass — so the rest of the app still renders and
/// the user still has the slider that will fix it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CellError {
    /// The cell's own compute returned an error.
    Failed {
        /// What the compute said. Shown verbatim to the user, so it is written for one.
        message: String,
    },
    /// An input to this cell is in error. Carries the name of the cell that actually
    /// failed, not the immediate input, so a user reading a message eight cells downstream
    /// is pointed at the cause rather than at the messenger.
    Upstream {
        /// The cell where the failure actually originated — not this cell's immediate input.
        /// Both this and `message` are digested, because two upstream cells failing with the
        /// same words are still two different states; see the `Digestible` impl below.
        cause: String,
        /// The originating cell's message, carried down unchanged so that every cell in the
        /// affected subtree says the same thing about the same fault.
        message: String,
    },
}

impl CellError {
    /// The error a compute closure returns for its own failure. [`CellError::Upstream`] has
    /// no such constructor on purpose: only the engine may attribute a failure to another
    /// cell, and it does so while propagating.
    pub fn failed(message: impl Into<String>) -> CellError {
        CellError::Failed {
            message: message.into(),
        }
    }

    /// The message shown for this cell.
    pub fn message(&self) -> &str {
        match self {
            CellError::Failed { message } => message,
            CellError::Upstream { message, .. } => message,
        }
    }

    /// The name of the cell where the failure actually originated.
    pub fn cause<'a>(&'a self, self_name: &'a str) -> &'a str {
        match self {
            CellError::Failed { .. } => self_name,
            CellError::Upstream { cause, .. } => cause,
        }
    }
}

/// An error is a value, so it has to digest like one — **including its attribution**.
///
/// Hashing only the message was a real bug, found by the differential oracle in
/// `tests/oracle.rs` and not by any hand-written test: two upstream cells failing with the
/// same words produced the same digest, so a downstream cell whose cause changed from one to
/// the other reused its cached error and went on naming the cell that was no longer the
/// problem. The message is what a user reads; the cause is what they act on. Both are the
/// value.
impl crate::digest::Digestible for CellError {
    fn digest_into(&self, h: &mut crate::digest::Hasher) {
        match self {
            CellError::Failed { message } => {
                h.tag(0).str(message);
            }
            CellError::Upstream { cause, message } => {
                h.tag(1).str(cause).str(message);
            }
        }
    }
}

impl fmt::Display for CellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CellError::Failed { message } => f.write_str(message),
            CellError::Upstream { cause, message } => {
                write!(f, "upstream cell `{cause}` failed: {message}")
            }
        }
    }
}

impl std::error::Error for CellError {}

/// A failure of the *caller*, using the session API wrongly. Distinct from [`CellError`]
/// because none of these can be caused by data — they are all "you asked for a cell that
/// does not exist" or "you set something that is not an input".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionError {
    /// No cell in this graph has that name. Carries the name as given, so a caller can see
    /// the typo rather than be told one exists.
    UnknownCell(String),
    /// Only source cells can be set from outside. Setting a computed cell would make the
    /// graph lie about where its values come from, and the next recompute would silently
    /// overwrite it — a bug that looks like a race.
    NotAnInput {
        /// The computed cell that was set.
        name: String,
    },
    /// The value set does not match the input's declared type.
    TypeMismatch {
        /// The input that was set.
        name: String,
        /// The type the input was declared with: the `type_name` of the initial value the
        /// graph was built with, not of whatever the cell holds now. A source's type is fixed
        /// when the app is compiled, so setting one cannot change what it will accept next.
        expected: &'static str,
        /// The type name of the value offered instead.
        found: &'static str,
    },
    /// A cell id from a different graph. Ids are indices, so using one across graphs would
    /// silently read the wrong cell rather than fail.
    ForeignCell(CellId),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::UnknownCell(n) => write!(f, "no cell named `{n}`"),
            SessionError::NotAnInput { name } => write!(
                f,
                "`{name}` is a computed cell; only inputs can be set from outside"
            ),
            SessionError::TypeMismatch {
                name,
                expected,
                found,
            } => write!(f, "input `{name}` expects {expected}, got {found}"),
            SessionError::ForeignCell(id) => {
                write!(f, "cell id {} does not belong to this graph", id.index())
            }
        }
    }
}

impl std::error::Error for SessionError {}
