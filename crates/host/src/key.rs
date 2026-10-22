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

/// A domain apps are published under: with `example.com`, the app `sales` answers at
/// `sales.example.com`.
///
/// **Declaring one trusts every name directly beneath it**, because a source that routes
/// `<label>.<domain>` serves whatever label a request puts there. That is safe only for a zone
/// whose DNS the operator controls: anyone who controls a name's DNS can point it at this
/// machine, and a page served from that name then reaches the server with its own name in
/// both `Host` and `Origin`. So what is refused here is refused because it is a zone somebody
/// else's DNS answers for — a top-level domain, or an address.
///
/// The only constructors are [`Domain::parse`] and [`Domain::localhost`], so a `Domain` that
/// exists passed the one or is the other, and the library cannot be handed an unchecked one
/// any more than the command line can.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Domain(String);

/// Why a string is not a [`Domain`]. Carries the string, for the same reason
/// [`InvalidAppId`] does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidDomain {
    /// The string that was refused, verbatim.
    pub input: String,
    /// What is wrong with it, as a phrase that completes "a domain ...".
    pub reason: &'static str,
}

impl fmt::Display for InvalidDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} is not a domain to answer under: a domain {}",
            self.input, self.reason
        )
    }
}

impl std::error::Error for InvalidDomain {}

impl Domain {
    /// The longest a DNS name may be, from RFC 1035 §2.3.4.
    pub const MAX_LEN: usize = 253;

    /// The single-label domains that may be declared, because nobody can register a name
    /// under them: `localhost` (RFC 6761 §6.3), `test` (RFC 6761 §6.2) and `internal`
    /// (reserved by ICANN for private use, 2024). Any other single label is a top-level
    /// domain, or one that can become one.
    pub const SINGLE_LABELS: [&'static str; 3] = ["localhost", "internal", "test"];

    /// Check a string and keep it, or say what is wrong with it.
    ///
    /// Strict, and **not normalised**, for [`AppId`]'s reason: `Example.com` and
    /// `example.com.` are refused rather than rewritten. Normalising what an operator typed
    /// is the command line's job, where the rewrite can be shown to them.
    ///
    /// # Errors
    ///
    /// [`InvalidDomain`] when the string is empty or longer than a DNS name; when a label is
    /// empty, too long, carries anything but ASCII lowercase letters, digits and `-`, or
    /// starts or ends with `-`; when the last label is numeric, which a browser reads as an
    /// IPv4 address; when it ends in `.local`, which multicast DNS lets anyone on the network
    /// answer for; and when it is a single label other than [`Domain::SINGLE_LABELS`].
    pub fn parse(s: &str) -> Result<Domain, InvalidDomain> {
        let bad = |reason| InvalidDomain {
            input: s.to_string(),
            reason,
        };

        if s.is_empty() {
            return Err(bad("is not empty"));
        }
        if s.len() > Self::MAX_LEN {
            return Err(bad("is at most 253 bytes, the DNS name limit"));
        }
        // Every label is held to the app id's rule, because that rule IS the DNS label rule;
        // an empty label is how a leading dot, a trailing dot and `..` all show up.
        let labels: Vec<&str> = s.split('.').collect();
        if labels.iter().any(|label| AppId::parse(label).is_err()) {
            return Err(bad(
                "is labels of ASCII lowercase letters, digits and `-` joined by single dots — \
                 none empty, none longer than 63 bytes, none starting or ending with `-`",
            ));
        }
        // The WHATWG URL parser reads a host whose last label is a number as an IPv4 address
        // (`127.1`, `a.0x7f`), so such a "domain" is an address a browser will never send as a
        // name. Decimal, and `0x` followed by hex — octal is all digits, so it is covered.
        let last = labels[labels.len() - 1];
        let numeric = last.bytes().all(|b| b.is_ascii_digit())
            || last
                .strip_prefix("0x")
                .is_some_and(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()));
        if numeric {
            return Err(bad(
                "does not end in a numeric label, which a browser reads as an IPv4 address",
            ));
        }
        // Multicast DNS (RFC 6762): no registry, and any machine on the network may answer for
        // any `.local` name, so a zone under it is no more the operator's than `local` itself.
        // It is also what macOS prints for `hostname`, which is how one gets here.
        if last == "local" {
            return Err(bad(
                "does not end in `.local`, which multicast DNS lets any machine on the network \
                 answer for",
            ));
        }
        if labels.len() == 1 && !Self::SINGLE_LABELS.contains(&s) {
            return Err(bad(
                "has two labels or more, unless it is `localhost`, `internal` or `test` — \
                 anyone can register a name directly under a top-level domain",
            ));
        }
        Ok(Domain(s.to_string()))
    }

    /// `localhost`, which every [`crate::DirAppSource`] answers under.
    pub fn localhost() -> Domain {
        Domain("localhost".to_string())
    }

    /// The domain itself.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Domain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
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
    fn a_domain_is_dns_labels_and_the_near_misses_are_not() {
        for good in [
            "example.com",
            "dash.example.com",
            "home.arpa",
            "xn--bcher-kva.example",
            "localhost",
            "internal",
            "test",
        ] {
            assert_eq!(Domain::parse(good).unwrap().as_str(), good);
        }

        let long_label = format!("{}.com", "a".repeat(64));
        let long_name = format!("{}.com", vec!["a".repeat(62); 4].join("."));
        assert!(long_name.len() > 253);
        for bad in [
            "",
            "com",
            "uk",
            "local",
            "Example.com",
            "example.com.",
            ".example.com",
            "*.example.com",
            "example..com",
            "-a.com",
            "a-.com",
            "example.com:443",
            "https://example.com",
            "example.com/x",
            "127.0.0.1",
            "0.0.1",
            "1.2.3",
            "a.0x7f",
            "a.0x",
            "x.local",
            "nicks-macbook-pro.local",
            "[::1]",
            "bücher.example",
            long_label.as_str(),
            long_name.as_str(),
        ] {
            assert!(Domain::parse(bad).is_err(), "{bad:?} should be refused");
        }

        // The two refusals an operator is most likely to meet name their reason.
        let tld = Domain::parse("com").unwrap_err().to_string();
        assert!(tld.contains("anyone can register"), "{tld}");
        let address = Domain::parse("127.0.0.1").unwrap_err().to_string();
        assert!(address.contains("IPv4 address"), "{address}");
        for mdns in ["local", "x.local"] {
            let refused = Domain::parse(mdns).unwrap_err().to_string();
            assert!(refused.contains("multicast DNS"), "{mdns}: {refused}");
        }
        // A label that merely starts with digits is a name, not an address.
        assert!(Domain::parse("a.1x").is_ok());
        assert!(Domain::parse("a.0xg").is_ok());
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
