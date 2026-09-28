//! What can go wrong between a host header and a compiled app.

use std::fmt;

use dagpane_core::Digest;

use crate::key::AppKey;

/// Every way an open can fail.
///
/// Written out by hand rather than derived, for the same reason `dagpane_core::error` is:
/// each variant's message is the thing an operator reads at three in the morning, and a
/// derived one is written by whoever named the field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostError {
    /// Nothing is routed at this host header. The header is included because the usual cause
    /// is that it is not the one anybody expected — a proxy rewriting it, or a port that
    /// survived normalisation somewhere upstream.
    UnknownHost(String),

    /// The source resolved this key and then had nothing to fetch for it. A registry that
    /// answers two questions inconsistently, most often because a deploy landed between them.
    NotFound(AppKey),

    /// **The source answered with bytes that are not the bytes the key names.**
    ///
    /// The one error here that is a safety property rather than an operational one. The key
    /// is the manifest, so serving a graph compiled from anything else would mean a viewer
    /// reconnecting after a rollback gets the version that was rolled back — silently,
    /// because every log line would still print the key that was asked for. The host
    /// compiles nothing in this case.
    DigestMismatch {
        /// What was asked for.
        key: AppKey,
        /// What the bytes actually digest to.
        found: Digest,
    },

    /// The manifest did not parse or did not compile. Carries the compiler's own message,
    /// which names the cell.
    Manifest {
        /// Which app.
        key: AppKey,
        /// What `dagpane_app` said.
        reason: String,
    },

    /// This app alone is larger than the whole budget, so admitting it could only mean
    /// evicting everything else and still failing. Refused instead.
    OverBudget {
        /// Which app.
        key: AppKey,
        /// What it costs.
        bytes: u64,
        /// What the process will hold in total.
        budget: u64,
    },
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostError::UnknownHost(host) => {
                write!(f, "no app is routed at {host:?}")
            }
            HostError::NotFound(key) => {
                write!(f, "{key} resolved, but the source has no manifest for it")
            }
            HostError::DigestMismatch { key, found } => write!(
                f,
                "{key} was asked for and the source returned a manifest digesting to \
                 {:016x}; nothing was compiled",
                found.short()
            ),
            HostError::Manifest { key, reason } => write!(f, "{key} did not compile: {reason}"),
            HostError::OverBudget { key, bytes, budget } => write!(
                f,
                "{key} holds {bytes} bytes of sources and the whole budget is {budget}; \
                 refused rather than admitted at the cost of every other app"
            ),
        }
    }
}

impl std::error::Error for HostError {}
