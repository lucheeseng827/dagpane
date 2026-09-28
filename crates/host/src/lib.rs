//! `dagpane-host` — many apps in one process.
//!
//! `dagpane run` binds a port and serves one manifest. Hosting four hundred apps that way
//! means four hundred processes, which is already cheaper than four hundred Python
//! containers — and "cheaper" is a word, not a number. This crate is what makes it a number:
//! one process, `N` apps, one compiled graph each, and a byte budget that decides which ones
//! stay resident.
//!
//! # The shape of it
//!
//! ```text
//!   host header ──> AppSource::resolve ──> AppKey{app_id, manifest_digest}
//!                                             │
//!                                     hit ────┴──── miss ──> fetch bytes ──> digest check
//!                                      │                                          │
//!                                 Arc<App>  <───────────── compile <──────────────┘
//! ```
//!
//! [`AppKey`] is the whole design. It names an app *and the manifest it was compiled from*,
//! so a redeploy is a different key rather than a mutation of the same one. Three things fall
//! out of that and none of them needed a mechanism of their own:
//!
//! * a viewer who reconnects after a deploy cannot be served the graph that was deployed
//!   over, because that graph is under a key nothing resolves to any more;
//! * a rollback is a key change, so the host does the work of a rollback by being told a
//!   different digest — there is no rollback path to get wrong;
//! * two processes holding one key hold the same graph, because the key names the bytes.
//!
//! # Isolation
//!
//! One process serving many parties' apps makes isolation a property of a data structure
//! rather than of a database. The rule is one line: **the graph is shared, the session is
//! not.** [`Host::open`] hands out `Arc<App>` — immutable, compiled once — and every
//! connection builds its own [`dagpane_core::Session`] over it. There is deliberately no API
//! here that returns a session somebody else is also holding, because that is not a cache
//! hit, it is one viewer reading another viewer's inputs. `tests/isolation.rs` is the
//! adversarial version of that sentence.
//!
//! # What is not here
//!
//! No socket, no runtime, no clock beyond [`std::time::Instant`]. Multiplexing is a decision
//! about a map — which key, which lifetime, which budget — and every one of those decisions
//! is testable with nothing listening on anything. `crates/serve` owns the socket and drives
//! this; that split is what keeps the isolation test from being the kind that needs a browser
//! and therefore gets skipped.

#![forbid(unsafe_code)]
#![deny(missing_debug_implementations)]
#![deny(missing_docs)]

pub mod budget;
pub mod error;
pub mod host;
pub mod key;
pub mod source;

pub use budget::{Budget, Footprint};
pub use error::HostError;
pub use host::{AppSlot, EvictReason, Eviction, Host};
pub use key::{manifest_digest, AppId, AppKey, InvalidAppId};
pub use source::{normalise_host, AppSource, DirAppSource, ManifestBytes, MemAppSource};
