//! What identifies an app, and what identifies the *version* of it that is running.

use std::fmt;
use std::str::FromStr;

use dagpane_core::digest::Hasher;
use dagpane_core::Digest;

/// An app's stable name. A DNS label, because it ends up in a hostname.
///
/// The only constructors are [`AppId::try_from`] and [`AppId::from_str`], so an `AppId` that
/// exists is one that passed [`AppId::parse`]. Anything that wants to route on it — a host
/// header, a subdomain, a path segment — gets that guarantee for free.
///
/// **Not normalised.** `Sales` is refused rather than lowered to `sales`. An identifier that
/// changes when you write it differently is two identifiers wearing one name, and the place
/// that difference surfaces is a registry lookup that misses and deploys a second copy.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AppId(String);

/// Why a string is not an [`AppId`]. Carries the offending string: an error that says
/// "invalid app id" and not which one is an error somebody has to reproduce to read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidAppId {
    /// The string that was refused, verbatim.
    pub input: String,
    /// What is wrong with it, as a phrase that completes "an app id ...".
    pub reason: &'static str,
}

impl fmt::Display for InvalidAppId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} is not an app id: an app id {}",
            self.input, self.reason
        )
    }
}

impl std::error::Error for InvalidAppId {}

impl AppId {
    /// The longest a DNS label may be, from RFC 1035 §2.3.4. Not a policy this crate chose.
    pub const MAX_LEN: usize = 63;

    /// Check a string and keep it, or say what is wrong with it.
    ///
    /// # Errors
    ///
    /// [`InvalidAppId`] when the string is empty, too long, carries anything but ASCII
    /// lowercase letters, digits and `-`, or starts or ends with `-`.
    pub fn parse(s: &str) -> Result<AppId, InvalidAppId> {
        let bad = |reason| InvalidAppId {
            input: s.to_string(),
            reason,
        };

        if s.is_empty() {
            return Err(bad("is not empty"));
        }
        if s.len() > Self::MAX_LEN {
            return Err(bad("is at most 63 bytes, the DNS label limit"));
        }
        if !s
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(bad("is ASCII lowercase letters, digits and `-` only"));
        }
        if s.starts_with('-') || s.ends_with('-') {
            return Err(bad("does not start or end with `-`"));
        }
        Ok(AppId(s.to_string()))
    }

    /// The label itself.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AppId {
    type Error = InvalidAppId;

    fn try_from(s: String) -> Result<AppId, InvalidAppId> {
        AppId::parse(&s)
    }
}

impl TryFrom<&str> for AppId {
    type Error = InvalidAppId;

    fn try_from(s: &str) -> Result<AppId, InvalidAppId> {
        AppId::parse(s)
    }
}

impl FromStr for AppId {
    type Err = InvalidAppId;

    fn from_str(s: &str) -> Result<AppId, InvalidAppId> {
        AppId::parse(s)
    }
}

impl fmt::Display for AppId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for AppId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AppId({})", self.0)
    }
}

/// The digest of a manifest's **bytes**.
///
/// Bytes, not the parse tree, and the trade is deliberate in both directions. Reformatting a
/// manifest or editing a comment produces a new digest and therefore a new [`AppKey`], so a
/// cosmetic edit costs one recompile. Digesting the parse tree instead would need a
/// canonical form of it, which is a second definition of "the same app" that has to be kept
/// in step with the first — and the failure mode of that one is serving a graph nobody
/// deployed, which is the failure this key exists to prevent.
///
/// Domain-separated with a tag, so the digest of a manifest can never collide with the
/// digest of a value that happens to hold the same bytes.
pub fn manifest_digest(text: &str) -> Digest {
    let mut h = Hasher::new();
    h.tag(TAG_MANIFEST);
    h.bytes(text.as_bytes());
    h.finish()
}

/// Distinct from every tag `dagpane_core` writes. The value tags are 0..=9, an outcome's are
/// `0xa0`/`0xa1`; this is `0xd0` so that a reader comparing the two files can see at a glance
/// that the ranges do not meet.
const TAG_MANIFEST: u8 = 0xd0;

/// The identity of a *compiled* app: which app, and which manifest it was compiled from.
///
/// This is the type the whole crate turns on. A manifest edit is a different key, so:
///
/// * a redeploy cannot serve a graph compiled from the manifest before it — the new key
///   misses the map and compiles, and the old key is evicted rather than left to be found;
/// * a rollback is a key change and not a rebuild — the control plane points a route at the
///   digest it wants and the host loads it, which is why rollback has no deploy step;
/// * two processes holding this key hold the same graph, because the key names the bytes.
///
/// `Ord` so a key can index a `BTreeMap`: anything filing deployments under one wants
/// iteration that is the same twice running, and a hash seed is not that. The order is
/// (app, digest) and is meaningless as a *version* order — a digest is not chronological, so
/// anything wanting "which came first" has to carry a timestamp of its own.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AppKey {
    /// Which app.
    pub app_id: AppId,
    /// Which manifest, by [`manifest_digest`].
    pub manifest_digest: Digest,
}

impl AppKey {
    /// Pair an app with a manifest digest.
    pub fn new(app_id: AppId, manifest_digest: Digest) -> AppKey {
        AppKey {
            app_id,
            manifest_digest,
        }
    }

    /// The key a manifest's text implies for this app.
    pub fn of_manifest(app_id: AppId, text: &str) -> AppKey {
        AppKey::new(app_id, manifest_digest(text))
    }
}

impl fmt::Display for AppKey {
    /// `sales@1f4a…` — the app, then enough of the digest to tell two deployments apart in
    /// a log line. The short form is the low 64 bits; [`AppKey::manifest_digest`] is what
    /// anything comparing keys must use.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{:016x}", self.app_id, self.manifest_digest.short())
    }
}

impl fmt::Debug for AppKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AppKey({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dns_label_is_an_app_id_and_the_near_misses_are_not() {
        assert!(AppId::parse("sales").is_ok());
        assert!(AppId::parse("sales-eu-2").is_ok());
        assert!(AppId::parse("9").is_ok());

        for bad in [
            "", "Sales", "sales_eu", "-sales", "sales-", "sales.eu", "sÅ‚es",
        ] {
            assert!(AppId::parse(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(AppId::parse(&"a".repeat(63)).is_ok());
        assert!(AppId::parse(&"a".repeat(64)).is_err());
    }

    #[test]
    fn the_key_changes_when_the_manifest_bytes_change() {
        let id = AppId::parse("sales").unwrap();
        let a = AppKey::of_manifest(id.clone(), "[app]\ntitle = \"x\"\n");
        let b = AppKey::of_manifest(id.clone(), "[app]\ntitle = \"y\"\n");
        let c = AppKey::of_manifest(id, "[app]\ntitle = \"x\"\n");
        assert_ne!(a, b, "different manifests are different keys");
        assert_eq!(a, c, "the same bytes are the same key");
    }

    #[test]
    fn a_manifest_digest_is_not_the_digest_of_the_same_bytes_as_a_value() {
        use dagpane_core::Digestible;
        let text = "hello";
        assert_ne!(
            manifest_digest(text),
            dagpane_core::Value::text(text).digest(),
            "the manifest tag is what keeps these two apart"
        );
    }
}
