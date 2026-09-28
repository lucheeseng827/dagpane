//! The viewer's state, carried by the viewer.
//!
//! A session here is **the input values and nothing else** — the computed cells are rebuilt
//! from them in one pass, which is cheaper than storing them and impossible to get stale.
//! The question this module answers is who holds that handful of values between one
//! connection and the next.
//!
//! # The answer is: the viewer holds them
//!
//! Not a server-side store. The reasoning, in the order it actually matters:
//!
//! * **A session id is a bearer token.** A store keyed by one, shipped before there is any
//!   authentication in front of it, is a way to read somebody else's session — which is the
//!   thing `crates/serve`'s own documentation warns about. The values travelling with the
//!   viewer who chose them cannot be fetched by anyone else, because there is nothing to
//!   fetch.
//! * **Nothing to run, replicate or lose.** No eviction policy, no TTL, no garbage
//!   collection, no store to keep alive across a deploy. A process restart costs a reconnect.
//! * **No session affinity.** A viewer who lands on a different replica sees the state they
//!   left, because they brought it. That is the property that makes a second replica
//!   possible at all, and it is free here rather than being a load-balancer feature.
//! * **The state becomes a link.** A dashboard filtered the way you left it is a URL you can
//!   send to somebody. A server-side store gives you that only by also giving them an id
//!   that reads your session.
//!
//! What it costs, stated rather than discovered: a viewer who clears their URL loses their
//! filters, resuming on another device means sending yourself the link, and the values are
//! in a URL, which proxies log. `SECURITY.md` carries the last one.
//!
//! # A resume is an ordinary `Set`, deliberately
//!
//! Restored values go through [`crate::AppSession`]'s **same** two predicates as any other
//! `Set` — the widget must accept the value and the cell must be a source of a compatible
//! type. There is no privileged "seed the session" path, and there must never be one: the
//! moment inputs can be locked by something other than the client (an embed token that fixes
//! `region`, say), the lock has to hold on a resume too, and it holds automatically only
//! because a resume is not a special case.
//!
//! # One difference from `Set`: a resume may be partly applied
//!
//! `Set` is all-or-nothing, because a client that believes it set four values must not be
//! silently left with three. A resume is the opposite: the viewer is *proposing* a remembered
//! state and the app is the authority on what it now has. A manifest that renamed an input
//! last week must not make every bookmarked link fail to load.
//!
//! So a resume applies what it can and **reports what it dropped**, and the report reaches
//! the client in the opening frame. Silently ignoring an input the viewer had saved would
//! show them different numbers under the same URL with nothing to explain it.

use std::collections::BTreeMap;
use std::fmt;

use dagpane_core::Value;
use serde::{Deserialize, Serialize};

/// The most encoded state a connection will carry.
///
/// A URL is not a database. Past this the resume is refused whole, with a message, rather
/// than truncated — half a remembered state is worse than none, because the viewer cannot see
/// which half. Four kilobytes is far more than the handful of scalars an app's controls
/// amount to and far less than the point where proxies start dropping requests.
pub const MAX_ENCODED_BYTES: usize = 4096;

/// One input a resume asked for and did not get.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Dropped {
    /// The input's name, as the resume gave it.
    pub input: String,
    /// Why, in the same words a rejected `Set` would use.
    pub reason: String,
}

/// Why an encoded resume could not be read at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumeError {
    /// Longer than [`MAX_ENCODED_BYTES`].
    TooLong {
        /// What arrived.
        bytes: usize,
        /// What is allowed.
        limit: usize,
    },
    /// Not the JSON object of values this module writes.
    Malformed(String),
}

impl fmt::Display for ResumeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResumeError::TooLong { bytes, limit } => write!(
                f,
                "the saved state is {bytes} bytes and the limit is {limit}; \
                 open the app without it to start fresh"
            ),
            ResumeError::Malformed(why) => write!(f, "the saved state could not be read: {why}"),
        }
    }
}

impl std::error::Error for ResumeError {}

/// The values as they travel: a JSON object of input name to [`Value`].
///
/// The **same** encoding a `Set` puts on the wire, on purpose. One encoding of a value in the
/// system means one place it can be wrong, and it means a saved state can be read by anybody
/// already able to read a `Set` — including a person looking at a URL.
pub fn encode(values: &BTreeMap<String, Value>) -> String {
    // `BTreeMap`, so the same state always encodes to the same bytes and two links to the
    // same view compare equal.
    serde_json::to_string(values).unwrap_or_else(|_| "{}".to_string())
}

/// Read what [`encode`] wrote.
///
/// # Errors
///
/// [`ResumeError::TooLong`] past [`MAX_ENCODED_BYTES`], or [`ResumeError::Malformed`] for
/// anything that is not an object of values. **Nothing here checks whether the app has these
/// inputs** — that is [`crate::AppSession::resume`]'s job, because it is the only thing that
/// knows the app.
pub fn decode(text: &str) -> Result<BTreeMap<String, Value>, ResumeError> {
    if text.len() > MAX_ENCODED_BYTES {
        return Err(ResumeError::TooLong {
            bytes: text.len(),
            limit: MAX_ENCODED_BYTES,
        });
    }
    serde_json::from_str(text).map_err(|e| ResumeError::Malformed(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("min_amount".to_string(), Value::float(400.0)),
            ("region".to_string(), Value::text("north")),
        ])
    }

    #[test]
    fn a_state_round_trips_and_encodes_the_same_bytes_twice() {
        let encoded = encode(&state());
        assert_eq!(decode(&encoded).unwrap(), state());
        assert_eq!(
            encoded,
            encode(&state()),
            "two links to one view must compare equal"
        );
    }

    #[test]
    fn it_is_the_wire_encoding_and_not_a_second_one() {
        // If these ever diverge, a person reading a URL and a person reading a network tab
        // are looking at two different formats for one thing.
        let wire = serde_json::to_string(&state()).unwrap();
        assert_eq!(encode(&state()), wire);
    }

    #[test]
    fn an_oversized_state_is_refused_whole_rather_than_truncated() {
        let long = format!("{{\"a\":{}}}", "1".repeat(MAX_ENCODED_BYTES));
        match decode(&long) {
            Err(ResumeError::TooLong { bytes, limit }) => {
                assert!(bytes > limit);
                assert_eq!(limit, MAX_ENCODED_BYTES);
            }
            other => panic!("expected TooLong, got {other:?}"),
        }
    }

    #[test]
    fn rubbish_is_an_error_and_not_an_empty_state() {
        // An empty state would silently start the viewer fresh with no explanation, which is
        // the failure this whole module is arranged to avoid.
        for bad in ["", "[]", "null", "{\"a\":", "not json"] {
            assert!(
                matches!(decode(bad), Err(ResumeError::Malformed(_))),
                "{bad:?}"
            );
        }
    }
}
