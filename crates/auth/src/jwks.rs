//! The keys, as the operator supplies them.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use jsonwebtoken::DecodingKey;
use serde::Deserialize;

/// A set of public keys, read from a JWKS document.
///
/// **From a file, never from a URL.** See the crate docs: fetching it would mean an HTTP
/// client in the runtime, a request that can fail at start-up, and a thing to redirect.
/// Rotation is a copy and a reload.
pub struct Jwks {
    keys: BTreeMap<String, Key>,
    /// The one key, when there is exactly one — so a token with no `kid` still has an
    /// unambiguous answer. More than one and a missing `kid` is refused rather than guessed.
    only: Option<String>,
}

struct Key {
    key: DecodingKey,
    /// What the JWK says this key is for. A token whose header names a different family is
    /// refused before any signature is attempted.
    family: Family,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Family {
    Rsa,
    Ec,
}

impl fmt::Debug for Jwks {
    /// Key ids and nothing else. These are public keys and printing them would be harmless;
    /// printing them in a log line nobody asked for is still noise.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Jwks")
            .field("kids", &self.keys.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Why a JWKS could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JwksError {
    /// The file could not be read.
    Unreadable(String),
    /// It is not a JWKS document.
    Malformed(String),
    /// It parsed and contains no key this crate can verify with.
    ///
    /// A separate variant because it is the failure that would otherwise be silent: a JWKS
    /// full of `oct` keys, or of RSA keys missing `n`, parses fine and refuses everyone.
    /// Starting a server with it would be starting a door that never opens.
    NoUsableKeys {
        /// How many entries the document had.
        found: usize,
        /// Why each was skipped, in the document's order.
        reasons: Vec<String>,
    },
}

impl fmt::Display for JwksError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JwksError::Unreadable(why) => write!(f, "the JWKS could not be read: {why}"),
            JwksError::Malformed(why) => write!(f, "the JWKS is not a JWKS: {why}"),
            JwksError::NoUsableKeys { found, reasons } => write!(
                f,
                "the JWKS has {found} key(s) and none can be used: {}",
                reasons.join("; ")
            ),
        }
    }
}

impl std::error::Error for JwksError {}

#[derive(Deserialize)]
struct Document {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kty: String,
    #[serde(default)]
    kid: Option<String>,
    /// What the key may be used for. `sig` or absent; `enc` is skipped.
    #[serde(default, rename = "use")]
    usage: Option<String>,
    // RSA
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
    // EC
    #[serde(default)]
    x: Option<String>,
    #[serde(default)]
    y: Option<String>,
}

impl Jwks {
    /// Read a JWKS from a file.
    ///
    /// # Errors
    ///
    /// [`JwksError`]. Note [`JwksError::NoUsableKeys`] in particular: a document that parses
    /// and yields nothing is an error here rather than an empty key set, because an empty key
    /// set is a front door that refuses everybody and says nothing about why.
    pub fn from_file(path: &Path) -> Result<Jwks, JwksError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| JwksError::Unreadable(format!("{}: {e}", path.display())))?;
        Jwks::parse(&text)
    }

    /// Read a JWKS from a document already in memory.
    ///
    /// Named `parse` rather than `from_str` so it is not confused with
    /// [`std::str::FromStr`], which this type deliberately does not implement: `"".parse()`
    /// inferring a key set is not a thing a reader should have to discover.
    ///
    /// # Errors
    ///
    /// [`JwksError::Malformed`] or [`JwksError::NoUsableKeys`].
    pub fn parse(text: &str) -> Result<Jwks, JwksError> {
        let document: Document =
            serde_json::from_str(text).map_err(|e| JwksError::Malformed(e.to_string()))?;

        let found = document.keys.len();
        let mut keys = BTreeMap::new();
        let mut reasons = Vec::new();

        for (index, jwk) in document.keys.into_iter().enumerate() {
            let name = jwk.kid.clone().unwrap_or_else(|| index.to_string());

            // `use: enc` is an encryption key. Using one to verify a signature is a category
            // error the JWKS itself is telling us about, so it is skipped by name.
            if jwk.usage.as_deref().is_some_and(|u| u != "sig") {
                reasons.push(format!("{name}: use is {:?}, not sig", jwk.usage.unwrap()));
                continue;
            }

            let built = match jwk.kty.as_str() {
                "RSA" => match (&jwk.n, &jwk.e) {
                    (Some(n), Some(e)) => DecodingKey::from_rsa_components(n, e)
                        .map(|key| Key {
                            key,
                            family: Family::Rsa,
                        })
                        .map_err(|e| format!("{name}: {e}")),
                    _ => Err(format!("{name}: an RSA key needs both `n` and `e`")),
                },
                "EC" => match (&jwk.x, &jwk.y) {
                    (Some(x), Some(y)) => DecodingKey::from_ec_components(x, y)
                        .map(|key| Key {
                            key,
                            family: Family::Ec,
                        })
                        .map_err(|e| format!("{name}: {e}")),
                    _ => Err(format!("{name}: an EC key needs both `x` and `y`")),
                },
                // `oct` is a symmetric key. It is skipped rather than loaded, and that is one
                // of the two independent reasons an `HS256` token cannot be verified here:
                // there is no secret in this map to verify it with.
                other => Err(format!("{name}: kty {other:?} is not a verification key")),
            };

            match built {
                Ok(key) => {
                    keys.insert(name, key);
                }
                Err(why) => reasons.push(why),
            }
        }

        if keys.is_empty() {
            return Err(JwksError::NoUsableKeys { found, reasons });
        }
        let only = (keys.len() == 1).then(|| keys.keys().next().cloned().unwrap());
        Ok(Jwks { keys, only })
    }

    /// How many keys are usable.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether there are none. Cannot happen for a `Jwks` that exists — see
    /// [`JwksError::NoUsableKeys`] — and present because clippy asks for it beside `len`.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The key a token header names, and what family it is.
    ///
    /// A token with no `kid` resolves only when the JWKS holds exactly one key. With two or
    /// more, trying each in turn would work and is refused anyway: a verifier that tries keys
    /// until one fits is a verifier whose behaviour during a key rotation depends on map
    /// order, and "which key signed this" stops being answerable from the token.
    pub(crate) fn find(&self, kid: Option<&str>) -> Option<(&DecodingKey, Family)> {
        let name = match kid {
            Some(kid) => kid,
            None => self.only.as_deref()?,
        };
        self.keys.get(name).map(|k| (&k.key, k.family))
    }
}
