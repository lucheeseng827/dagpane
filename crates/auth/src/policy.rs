//! What counts as a valid token here, and who it lets in.

use std::collections::BTreeSet;
use std::fmt;

use jsonwebtoken::{decode, decode_header, Validation};
use serde::Deserialize;

use crate::error::AuthError;
use crate::jwks::{Family, Jwks};

/// The signature algorithms this crate will verify.
///
/// **Asymmetric only, and that is the type system doing the work.** There is no variant for
/// `none` and none for any `HS*`, so the two classic JWT attacks — a token that asks not to
/// be verified, and a token that asks to be HMAC'd with the verifier's own public key — are
/// not reachable through a configuration mistake. An operator cannot allowlist them because
/// this enum cannot name them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Algorithm {
    /// RSASSA-PKCS1-v1_5 with SHA-256.
    RS256,
    /// RSASSA-PKCS1-v1_5 with SHA-384.
    RS384,
    /// RSASSA-PKCS1-v1_5 with SHA-512.
    RS512,
    /// RSASSA-PSS with SHA-256.
    PS256,
    /// RSASSA-PSS with SHA-384.
    PS384,
    /// RSASSA-PSS with SHA-512.
    PS512,
    /// ECDSA with P-256 and SHA-256.
    ES256,
    /// ECDSA with P-384 and SHA-384.
    ES384,
}

impl Algorithm {
    /// The name as it appears in a token header.
    pub fn name(self) -> &'static str {
        match self {
            Algorithm::RS256 => "RS256",
            Algorithm::RS384 => "RS384",
            Algorithm::RS512 => "RS512",
            Algorithm::PS256 => "PS256",
            Algorithm::PS384 => "PS384",
            Algorithm::PS512 => "PS512",
            Algorithm::ES256 => "ES256",
            Algorithm::ES384 => "ES384",
        }
    }

    /// Parse one, for a command line.
    ///
    /// # Errors
    ///
    /// The offending string, when it is not an allowlistable algorithm. `HS256` and `none`
    /// land here, which is the point.
    pub fn parse(name: &str) -> Result<Algorithm, String> {
        Ok(match name {
            "RS256" => Algorithm::RS256,
            "RS384" => Algorithm::RS384,
            "RS512" => Algorithm::RS512,
            "PS256" => Algorithm::PS256,
            "PS384" => Algorithm::PS384,
            "PS512" => Algorithm::PS512,
            "ES256" => Algorithm::ES256,
            "ES384" => Algorithm::ES384,
            other => {
                return Err(format!(
                    "{other:?} is not an algorithm this verifies with. Asymmetric only: \
                     RS256/384/512, PS256/384/512, ES256/384. `HS*` would mean verifying a \
                     token with a shared secret, and `none` would mean not verifying it."
                ))
            }
        })
    }

    fn to_jwt(self) -> jsonwebtoken::Algorithm {
        use jsonwebtoken::Algorithm as A;
        match self {
            Algorithm::RS256 => A::RS256,
            Algorithm::RS384 => A::RS384,
            Algorithm::RS512 => A::RS512,
            Algorithm::PS256 => A::PS256,
            Algorithm::PS384 => A::PS384,
            Algorithm::PS512 => A::PS512,
            Algorithm::ES256 => A::ES256,
            Algorithm::ES384 => A::ES384,
        }
    }

    fn family(self) -> Family {
        match self {
            Algorithm::ES256 | Algorithm::ES384 => Family::Ec,
            _ => Family::Rsa,
        }
    }
}

/// How a token says which apps its holder may open.
///
/// **There is no default, and the absence is on purpose.** A server that silently let any
/// valid token open any app would be one whose access control nobody noticed was missing; a
/// server that silently required a claim nobody mints would refuse everyone on the first day
/// and look broken. So the operator chooses, in the open, and a server given neither refuses
/// to start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access {
    /// Any token this policy accepts may open any app this process serves.
    ///
    /// The right answer for a single-app `dagpane run` behind a front door that already
    /// decided who may reach it, and the wrong one for a fleet.
    AnyApp,
    /// The named claim lists the app ids its holder may open. `"*"` means all of them.
    ///
    /// The claim must be **present and an array of strings**. A token without it is refused
    /// rather than treated as empty — an absent claim is far more likely to be a
    /// misconfigured identity provider than a deliberate grant of nothing, and refusing is
    /// the direction that fails safely.
    Claim(String),
}

/// Everything that has to be true for a token to open an app.
///
/// No `Default`. Every field below has a wrong value that looks reasonable, and a struct
/// update from a default is how one of them gets left behind.
#[derive(Clone, Debug)]
pub struct Policy {
    /// The `iss` a token must carry, exactly.
    pub issuer: String,
    /// The `aud` a token must carry. Not optional: a token minted for another service by the
    /// same provider is a valid token, and it is not for this one.
    pub audience: String,
    /// The algorithms that may verify it. Empty is refused at construction.
    pub algorithms: BTreeSet<Algorithm>,
    /// Seconds of clock skew allowed on `exp` and `nbf`.
    ///
    /// Stated rather than inherited: `jsonwebtoken`'s own default is 60 seconds, which is a
    /// reasonable number nobody chose. Small, because the cost of being strict is a retry and
    /// the cost of being generous is a token that outlives its revocation.
    pub leeway_seconds: u64,
    /// How to decide which apps the holder may open.
    pub access: Access,
}

impl Policy {
    /// Check a policy before anything depends on it.
    ///
    /// # Errors
    ///
    /// A message naming what is missing. Called at start-up so that a server with an
    /// unusable policy **refuses to start** rather than refusing every viewer later, which
    /// looks the same from outside and is much harder to diagnose.
    pub fn check(&self) -> Result<(), String> {
        if self.issuer.trim().is_empty() {
            return Err("the policy has no issuer".into());
        }
        if self.audience.trim().is_empty() {
            return Err("the policy has no audience".into());
        }
        if self.algorithms.is_empty() {
            return Err("the policy allowlists no algorithm, so it refuses every token".into());
        }
        if let Access::Claim(name) = &self.access {
            if name.trim().is_empty() {
                return Err("the access claim has no name".into());
            }
        }
        Ok(())
    }

    /// Verify a token and decide whether its holder may open `app`.
    ///
    /// # Errors
    ///
    /// [`AuthError`], with the detail for a log. Give a caller [`AuthError::public`].
    pub fn admit(&self, jwks: &Jwks, token: &str, app: &str) -> Result<Viewer, AuthError> {
        let header = decode_header(token).map_err(|e| AuthError::Malformed(e.to_string()))?;

        // The allowlist is consulted BEFORE the key, so a token asking for something outside
        // it is refused by name rather than by a signature failure that says nothing useful.
        let algorithm = self
            .algorithms
            .iter()
            .copied()
            .find(|a| a.to_jwt() == header.alg)
            .ok_or_else(|| AuthError::Algorithm {
                asked: format!("{:?}", header.alg),
            })?;

        let (key, family) =
            jwks.find(header.kid.as_deref())
                .ok_or_else(|| AuthError::UnknownKey {
                    kid: header.kid.clone(),
                })?;

        // The header's algorithm and the key's family must agree. `jsonwebtoken` would fail
        // the signature anyway; failing here says which of the two is wrong, and makes the
        // "an EC key is not an RSA key" rule something a reader can find.
        if algorithm.family() != family {
            return Err(AuthError::UnknownKey {
                kid: header.kid.clone(),
            });
        }

        let mut validation = Validation::new(algorithm.to_jwt());
        // Named one by one rather than taking the defaults. `jsonwebtoken` does require
        // `exp` by default and does check it — but a reader cannot tell that from a call
        // site that says nothing, and the next version's defaults are not this version's.
        validation.algorithms = vec![algorithm.to_jwt()];
        validation.leeway = self.leeway_seconds;
        validation.validate_exp = true;
        validation.validate_nbf = true;
        validation.required_spec_claims = ["exp", "iss", "aud"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);

        let data = decode::<Claims>(token, key, &validation)
            .map_err(|e| AuthError::Rejected(e.to_string()))?;
        let claims = data.claims;

        let subject = claims.sub.clone().unwrap_or_default();
        let apps = match &self.access {
            Access::AnyApp => Apps::All,
            Access::Claim(name) => match claims.extra.get(name) {
                Some(serde_json::Value::Array(items)) => {
                    let listed: BTreeSet<String> = items
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect();
                    if listed.iter().any(|a| a == "*") {
                        Apps::All
                    } else {
                        Apps::Only(listed)
                    }
                }
                // Present and not an array, or absent. Both refuse: an absent claim is far
                // more likely a misconfigured provider than a deliberate grant of nothing.
                _ => Apps::Only(BTreeSet::new()),
            },
        };

        let allowed = match &apps {
            Apps::All => true,
            Apps::Only(listed) => listed.contains(app),
        };
        if !allowed {
            return Err(AuthError::Forbidden {
                subject,
                app: app.to_string(),
            });
        }

        Ok(Viewer {
            subject,
            name: claims.name,
            email: claims.email,
            apps,
        })
    }
}

/// Which apps a viewer may open.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Apps {
    All,
    Only(BTreeSet<String>),
}

/// Who got in.
#[derive(Clone, PartialEq, Eq)]
pub struct Viewer {
    /// The `sub` claim. Stable for one person at one provider, and the thing to log.
    pub subject: String,
    /// The `name` claim, when the provider sent one.
    pub name: Option<String>,
    /// The `email` claim, when the provider sent one.
    pub email: Option<String>,
    apps: Apps,
}

impl Viewer {
    /// Whether this viewer may open that app, without re-verifying the token.
    ///
    /// For a connection that outlives the request it was authorised on, and for a host that
    /// wants to answer the question again without keeping the token around.
    pub fn may_open(&self, app: &str) -> bool {
        match &self.apps {
            Apps::All => true,
            Apps::Only(listed) => listed.contains(app),
        }
    }
}

impl fmt::Debug for Viewer {
    /// The subject and nothing else by default. A name and an email are personal data and a
    /// `{:?}` in a log line is the least deliberate way for them to leave the process.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Viewer({})", self.subject)
    }
}

/// The claims this crate reads. Everything else is kept in `extra` for [`Access::Claim`].
#[derive(Debug, Deserialize)]
struct Claims {
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

/// Pull a bearer token out of the two places a browser can put one.
///
/// **Neither is a URL.** A query string is logged by proxies and lands in access logs, and a
/// token there is a credential in a log file — which is why `?t=` is not supported here even
/// though it would be the easiest thing to write.
///
/// * `Authorization: Bearer <jwt>` — for anything that can set a header: a proxy, a script,
///   the load generator, a test.
/// * A `Sec-WebSocket-Protocol` entry of `dagpane.auth.<jwt>` — for a browser, which cannot
///   set headers on `new WebSocket()`. Ugly, and the ugliness is the standard practice.
pub const AUTH_SUBPROTOCOL_PREFIX: &str = "dagpane.auth.";

/// Find a token in an `Authorization` value and a `Sec-WebSocket-Protocol` value.
///
/// # Errors
///
/// [`AuthError::Missing`] when neither carries one.
pub fn bearer<'a>(
    authorization: Option<&'a str>,
    protocols: Option<&'a str>,
) -> Result<&'a str, AuthError> {
    if let Some(value) = authorization {
        // `get`, not `value[..7]`. Slicing a `&str` by byte panics when the index lands
        // inside a multi-byte character, and this function is `pub`: the callers in this
        // workspace hand it `HeaderValue::to_str()`, which is ASCII, but a panic reachable
        // from outside the crate is a panic in somebody's auth path.
        //
        // ASCII-case-insensitive, because the scheme is a keyword and clients differ.
        if let (Some(scheme), Some(rest)) = (value.get(..7), value.get(7..)) {
            if scheme.eq_ignore_ascii_case("bearer ") {
                let token = rest.trim();
                if !token.is_empty() {
                    return Ok(token);
                }
            }
        }
    }
    if let Some(list) = protocols {
        for entry in list.split(',') {
            if let Some(token) = entry.trim().strip_prefix(AUTH_SUBPROTOCOL_PREFIX) {
                if !token.is_empty() {
                    return Ok(token);
                }
            }
        }
    }
    Err(AuthError::Missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_algorithm_that_would_undo_the_verification_cannot_be_named() {
        for bad in ["HS256", "HS384", "HS512", "none", "None", "", "RS128"] {
            let err = Algorithm::parse(bad).unwrap_err();
            assert!(err.contains("Asymmetric only"), "{bad}: {err}");
        }
        for good in ["RS256", "PS512", "ES256", "ES384"] {
            assert_eq!(Algorithm::parse(good).unwrap().name(), good);
        }
    }

    #[test]
    fn a_policy_that_would_refuse_everyone_or_admit_everyone_is_refused_at_start_up() {
        let base = Policy {
            issuer: "https://idp.test".into(),
            audience: "dagpane".into(),
            algorithms: BTreeSet::from([Algorithm::ES256]),
            leeway_seconds: 5,
            access: Access::AnyApp,
        };
        assert!(base.check().is_ok());

        let mut no_alg = base.clone();
        no_alg.algorithms.clear();
        assert!(no_alg.check().unwrap_err().contains("refuses every token"));

        for (field, policy) in [
            (
                "issuer",
                Policy {
                    issuer: "  ".into(),
                    ..base.clone()
                },
            ),
            (
                "audience",
                Policy {
                    audience: String::new(),
                    ..base.clone()
                },
            ),
            (
                "claim",
                Policy {
                    access: Access::Claim(" ".into()),
                    ..base.clone()
                },
            ),
        ] {
            assert!(policy.check().is_err(), "an empty {field} was accepted");
        }
    }

    #[test]
    fn a_token_is_taken_from_a_header_or_a_subprotocol_and_never_from_a_url() {
        assert_eq!(bearer(Some("Bearer abc"), None).unwrap(), "abc");
        assert_eq!(bearer(Some("bearer abc"), None).unwrap(), "abc");
        assert_eq!(bearer(None, Some("dagpane.auth.abc")).unwrap(), "abc");
        // A browser sends the list it is willing to speak; ours may not be first.
        assert_eq!(bearer(None, Some("json, dagpane.auth.xyz")).unwrap(), "xyz");
        // The header wins when both are present: it is the one a proxy sets deliberately.
        assert_eq!(
            bearer(Some("Bearer header"), Some("dagpane.auth.proto")).unwrap(),
            "header"
        );

        for (a, p) in [
            (None, None),
            (Some("Bearer "), None),
            (Some("Basic abc"), None),
            (None, Some("json")),
            (None, Some("dagpane.auth.")),
        ] {
            assert_eq!(bearer(a, p), Err(AuthError::Missing), "{a:?} {p:?}");
        }
    }

    #[test]
    fn a_header_that_is_not_ascii_is_refused_rather_than_a_panic() {
        // This function is `pub`, so it is reachable with something no HTTP header would
        // carry. Slicing `&str` by byte panics when the index lands inside a character, and
        // every value below puts a character boundary somewhere other than byte 7 — the
        // first two used to take the whole process down rather than return an error.
        for value in ["ééééxxxx", "𝔟𝔢𝔞𝔯𝔢𝔯 abc", "Béarer abc", "→→→ token"]
        {
            assert_eq!(
                bearer(Some(value), None),
                Err(AuthError::Missing),
                "{value:?}"
            );
        }
        // And a non-ASCII value does not stop the subprotocol from being read.
        assert_eq!(
            bearer(Some("ééééxxxx"), Some("dagpane.auth.abc")).unwrap(),
            "abc"
        );
    }

    #[test]
    fn what_a_caller_is_told_is_coarser_than_what_is_logged() {
        let detailed = AuthError::Rejected("aud is `other`, expected `dagpane`".into());
        assert!(detailed.to_string().contains("expected"));
        assert_eq!(detailed.public(), "this app requires a valid token");
        assert_eq!(detailed.status(), 401);

        let forbidden = AuthError::Forbidden {
            subject: "alice".into(),
            app: "payroll".into(),
        };
        assert_eq!(
            forbidden.status(),
            403,
            "a real viewer who may not is not a 401"
        );
        assert!(!forbidden.public().contains("payroll"));
    }
}
