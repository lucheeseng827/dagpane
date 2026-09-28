//! Why a token was refused.

use std::fmt;

/// Every way the front door says no.
///
/// # What a caller may show a viewer, and what it must not
///
/// These messages are for a **log**. [`AuthError::public`] is what may reach a browser, and
/// it is deliberately coarse: telling an unauthenticated caller that a token's *audience* was
/// wrong, or that a key id was unknown, tells them what to change. The distinctions below
/// exist so an operator reading a log can tell a clock-skew problem from a misconfigured
/// audience without turning the front door into an oracle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthError {
    /// No token at all: no `Authorization: Bearer`, no auth subprotocol.
    Missing,
    /// Not three base64url segments, or the header did not parse.
    Malformed(String),
    /// The header named an algorithm that is not on the policy's allowlist.
    ///
    /// **Includes `none` and every `HS*`**, which is the point of having an allowlist rather
    /// than trusting the header.
    Algorithm {
        /// What the token asked for, as it wrote it.
        asked: String,
    },
    /// The header's `kid` names no key in the JWKS, or there is no `kid` and the JWKS holds
    /// more than one key so there is nothing to guess with.
    UnknownKey {
        /// The key id, when there was one.
        kid: Option<String>,
    },
    /// The signature did not verify, or a registered claim failed: `exp`, `nbf`, `iss`,
    /// `aud`. One variant on purpose — see this module's docs.
    Rejected(String),
    /// The token is valid and its holder may not open this app.
    ///
    /// Distinct from every variant above because it is the only one where the *viewer* is
    /// real: a log line here is a person who logged in and was told no, which an operator
    /// usually wants to see and sometimes wants to act on.
    Forbidden {
        /// Who.
        subject: String,
        /// Which app they asked for.
        app: String,
    },
}

impl AuthError {
    /// What may be said to an unauthenticated caller.
    ///
    /// Two outcomes and no detail. Anything finer is a probing oracle: a caller who can tell
    /// "wrong audience" from "expired" from "unknown key" can iterate towards a token that
    /// works. The detail goes in the log, where the operator is.
    pub fn public(&self) -> &'static str {
        match self {
            AuthError::Forbidden { .. } => "you do not have access to this app",
            _ => "this app requires a valid token",
        }
    }

    /// The HTTP status a caller should get: `401` for "authenticate", `403` for "not you".
    pub fn status(&self) -> u16 {
        match self {
            AuthError::Forbidden { .. } => 403,
            _ => 401,
        }
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::Missing => f.write_str("no bearer token was presented"),
            AuthError::Malformed(why) => write!(f, "the token is not a JWT: {why}"),
            AuthError::Algorithm { asked } => write!(
                f,
                "the token asks to be verified with {asked:?}, which is not on the allowlist"
            ),
            AuthError::UnknownKey { kid: Some(kid) } => {
                write!(f, "no key in the JWKS has kid {kid:?}")
            }
            AuthError::UnknownKey { kid: None } => f.write_str(
                "the token has no `kid` and the JWKS holds more than one key; there is \
                 nothing to choose with",
            ),
            AuthError::Rejected(why) => write!(f, "the token was refused: {why}"),
            AuthError::Forbidden { subject, app } => {
                write!(f, "{subject} has no access to app {app:?}")
            }
        }
    }
}

impl std::error::Error for AuthError {}
