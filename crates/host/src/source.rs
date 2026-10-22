//! Where a manifest comes from, and how a request finds one.
//!
//! The host does not know what a deployment is. It knows how to turn a host header into an
//! [`AppKey`] and a key into manifest bytes, and it asks something else both questions. That
//! something else is a directory of manifests in the open-source case and a registry in a
//! managed one, and neither of them is this crate's business.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use crate::error::HostError;
use crate::key::{manifest_digest, AppId, AppKey, Domain};

/// A manifest as bytes, plus the directory its relative paths resolve against.
///
/// The base directory travels with the text for the reason `dagpane_app::compile` states: a
/// `[[source]]`'s `csv = "…"` resolves against the manifest's own directory and never against
/// the process's working directory, so an app behaves the same whichever directory it was
/// started from. A host serving four hundred apps has four hundred such directories and one
/// working directory, so this is the only place the answer can come from.
#[derive(Clone, Debug)]
pub struct ManifestBytes {
    /// The manifest, verbatim. [`manifest_digest`] of this is what the key must name.
    pub text: String,
    /// What a relative path inside `text` resolves against.
    pub base_dir: PathBuf,
}

impl ManifestBytes {
    /// Bundle text with the directory its paths resolve against.
    pub fn new(text: impl Into<String>, base_dir: impl Into<PathBuf>) -> ManifestBytes {
        ManifestBytes {
            text: text.into(),
            base_dir: base_dir.into(),
        }
    }

    /// The digest these bytes imply.
    pub fn digest(&self) -> dagpane_core::Digest {
        manifest_digest(&self.text)
    }
}

/// Where the host gets its apps.
///
/// Two questions, deliberately separate. `resolve` is asked on **every** request and must be
/// cheap; `fetch` is asked only on a miss and may read a disk or a network. Folding them into
/// one call would put the expensive half on the hot path, and splitting them is also what
/// makes the digest check in [`crate::Host`] possible: the key is decided before the bytes
/// arrive, so the bytes can be checked against it.
pub trait AppSource: Send + Sync + fmt::Debug {
    /// Which app a request for this host header wants, and which version of it.
    ///
    /// The header arrives already lowered and stripped of its port — see
    /// [`normalise_host`], which [`crate::Host`] applies before calling this.
    ///
    /// **This is the boundary for which names are served at all, and an implementation must
    /// answer only names its operator declared.** A `Host` header is chosen by whoever controls
    /// DNS for the name: a page whose name was pointed at this machine sends that name as
    /// both `Host` and `Origin`, so the socket's check that the two agree passes, and nothing
    /// after this call can tell the request from a real one. An exact table of names, as
    /// [`MemAppSource`] keeps, is bounded by construction; a pattern — a label, a suffix, a
    /// fallback app — must be bounded to declared names, as [`DirAppSource`]'s
    /// `<label>.<declared domain>` is, and must refuse before any I/O.
    ///
    /// # Errors
    ///
    /// [`HostError::OutsideDomains`] when the name is outside every name this source answers
    /// for, and [`HostError::UnknownHost`] when it is inside them and nothing is routed there.
    fn resolve(&self, host: &str) -> Result<AppKey, HostError>;

    /// The manifest this key names.
    ///
    /// # Errors
    ///
    /// [`HostError::NotFound`] when the key names nothing this source has.
    fn fetch(&self, key: &AppKey) -> Result<ManifestBytes, HostError>;

    /// The domains this source answers one label under, if it answers by pattern at all.
    ///
    /// For saying why a name was refused — `x.sales.example.com` is one label too many under
    /// a declared `example.com`, not a domain somebody forgot to declare — and never for
    /// deciding whether to serve one, which is [`AppSource::resolve`]'s alone. The default,
    /// right for a source that answers an exact table of names, is none.
    fn domains(&self) -> Vec<Domain> {
        Vec::new()
    }
}

/// Strip a host header down to the name a route is keyed by: lowercased, port removed.
///
/// `EXAMPLE.com:8080` and `example.com` are one route, because they are one name — the port
/// is which socket the request arrived on and the case is whatever the client felt like
/// sending. An IPv6 literal keeps its brackets (`[::1]:8080` → `[::1]`), because the colons
/// inside it are not the port separator and a naive `split(':')` would leave `[`.
pub fn normalise_host(header: &str) -> String {
    let trimmed = header.trim();
    let name = if let Some(rest) = trimmed.strip_prefix('[') {
        // `[::1]:8080` — the port, if any, is after the closing bracket.
        // `end` indexes `]` within `rest`, so it sits one later in `trimmed` and the slice
        // that keeps it runs to `end + 2`.
        match rest.find(']') {
            Some(end) => &trimmed[..end + 2],
            None => trimmed,
        }
    } else {
        trimmed.split(':').next().unwrap_or(trimmed)
    };
    name.to_ascii_lowercase()
}

/// An [`AppSource`] holding its manifests in memory.
///
/// The open-source default and the thing every test drives. It is also the shape a managed
/// registry has to fit: publish replaces the bytes for an app, which changes its digest,
/// which changes the [`AppKey`] — so nothing here has a concept of "deploy" separate from
/// "these are the bytes now".
#[derive(Debug, Default)]
pub struct MemAppSource {
    inner: RwLock<Inner>,
    /// Counts [`AppSource::fetch`] calls. A host that compiles once for a thousand viewers
    /// is the claim this crate exists to make; this is how a test sees it.
    fetches: AtomicU64,
}

#[derive(Debug, Default)]
struct Inner {
    /// host header → app.
    routes: HashMap<String, AppId>,
    /// app → the bytes currently published for it.
    published: HashMap<AppId, ManifestBytes>,
}

impl MemAppSource {
    /// An empty source. Nothing routes anywhere until [`MemAppSource::publish`].
    pub fn new() -> MemAppSource {
        MemAppSource::default()
    }

    /// Publish `text` as `app_id`'s current manifest and route `host` at it.
    ///
    /// Publishing different bytes for an app is a redeploy; publishing the same bytes is a
    /// no-op that a running host cannot even observe, because the key does not move.
    pub fn publish(
        &self,
        host: &str,
        app_id: &AppId,
        text: impl Into<String>,
        base_dir: impl AsRef<Path>,
    ) {
        let mut inner = self.inner.write().expect("MemAppSource lock");
        inner.routes.insert(normalise_host(host), app_id.clone());
        inner.published.insert(
            app_id.clone(),
            ManifestBytes::new(text, base_dir.as_ref().to_path_buf()),
        );
    }

    /// How many times the host has asked for manifest bytes.
    pub fn fetches(&self) -> u64 {
        self.fetches.load(Ordering::Relaxed)
    }
}

impl AppSource for MemAppSource {
    fn resolve(&self, host: &str) -> Result<AppKey, HostError> {
        let inner = self.inner.read().expect("MemAppSource lock");
        let app_id = inner
            .routes
            .get(host)
            .ok_or_else(|| HostError::UnknownHost(host.to_string()))?;
        let bytes = inner
            .published
            .get(app_id)
            .ok_or_else(|| HostError::UnknownHost(host.to_string()))?;
        Ok(AppKey::new(app_id.clone(), bytes.digest()))
    }

    fn fetch(&self, key: &AppKey) -> Result<ManifestBytes, HostError> {
        self.fetches.fetch_add(1, Ordering::Relaxed);
        let inner = self.inner.read().expect("MemAppSource lock");
        let bytes = inner
            .published
            .get(&key.app_id)
            .ok_or_else(|| HostError::NotFound(key.clone()))?;
        // Deliberately NOT checked against `key.manifest_digest` here. A source that answers
        // with the wrong bytes is the case `Host` checks for, and a source that checks itself
        // would hide it — the check has to live on the side that does not trust the answer.
        Ok(bytes.clone())
    }
}

/// An [`AppSource`] over a directory of manifests: `sales.toml` is the app `sales`.
///
/// The open-source way to run more than one app without a control plane. Every `*.toml` in
/// the directory is an app named by its file stem, and a request reaches it as **exactly one
/// label in front of a domain this source answers under**: `sales.localhost` always, and
/// `sales.example.com` once [`DirAppSource::with_domains`] declares `example.com`. One label
/// rather than the whole name, because the person running this owns a domain and does not
/// want to restate it in every filename. A trailing dot (`sales.localhost.`) is the same name
/// and is answered.
///
/// **Every other name is refused before anything is read**: another domain, a bare `sales`,
/// `x.sales.localhost`, an address. Routing on the first label of *any* name was the original
/// rule, and it served `sales.attacker.example` — a name whose owner can point its DNS at this
/// machine, so that a page served from it sends that name as `Host` and `Origin` and passes
/// the socket's check that the two agree. See [`AppSource::resolve`].
///
/// `localhost` is always answered, and it is safe for a reason that lives in the client:
/// Chrome and Firefox resolve `*.localhost` to loopback themselves (RFC 6761 §6.3), so no DNS
/// answer can point one at anything. A client that sends `.localhost` names to network DNS
/// loses that, and a front door (`--auth-jwks`) is what removes the dependence on it.
///
/// **Nothing is cached here and that is on purpose.** [`AppSource::resolve`] reads the file
/// to digest it, so editing a manifest on disk changes its key and the host recompiles it on
/// the next open — a redeploy with no deploy step, which is the behaviour anybody running
/// this from a directory expects. The read is the cost; the compile is what the host's own
/// map avoids.
///
/// A stem that is not a DNS label (`My App.toml`, `sales_v2.toml`) is **skipped**, not
/// refused: a directory is a place people put files, and one stray name must not take every
/// other app down with it. [`DirAppSource::skipped`] lists them so a process can say so at
/// start-up rather than leaving somebody wondering where their app went.
#[derive(Debug)]
pub struct DirAppSource {
    dir: PathBuf,
    /// What an app's label may stand in front of. Always holds `localhost`.
    domains: BTreeSet<Domain>,
}

impl DirAppSource {
    /// Serve every well-named `*.toml` in this directory, at `<app>.localhost`.
    pub fn new(dir: impl Into<PathBuf>) -> DirAppSource {
        DirAppSource {
            dir: dir.into(),
            domains: BTreeSet::from([Domain::localhost()]),
        }
    }

    /// Also answer at `<app>.<domain>` for each of these.
    ///
    /// Adds to the set and never removes `localhost`: a process published under a domain is
    /// still one an operator reaches on the machine itself. Declaring a domain trusts every
    /// name directly under it — see [`Domain`].
    #[must_use]
    pub fn with_domains(mut self, domains: impl IntoIterator<Item = Domain>) -> DirAppSource {
        self.domains.extend(domains);
        self
    }

    /// Every domain this source answers under, sorted, `localhost` included.
    pub fn domains(&self) -> Vec<Domain> {
        self.domains.iter().cloned().collect()
    }

    /// The apps this directory currently offers, sorted.
    ///
    /// Read from the filesystem on every call rather than remembered: a file added after
    /// start-up is an app, and a listing that disagrees with what `resolve` will do is worse
    /// than no listing.
    pub fn apps(&self) -> Vec<AppId> {
        let mut found: Vec<AppId> = self
            .entries()
            .into_iter()
            .filter_map(|(stem, _)| AppId::parse(&stem).ok())
            .collect();
        found.sort();
        found
    }

    /// Filenames in the directory that are `*.toml` but not usable app names.
    pub fn skipped(&self) -> Vec<String> {
        let mut bad: Vec<String> = self
            .entries()
            .into_iter()
            .filter(|(stem, _)| AppId::parse(stem).is_err())
            .map(|(stem, _)| stem)
            .collect();
        bad.sort();
        bad
    }

    fn entries(&self) -> Vec<(String, PathBuf)> {
        let Ok(dir) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        dir.flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "toml"))
            .filter_map(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| (s.to_string(), p.clone()))
            })
            .collect()
    }

    /// The manifest path for an app, if the directory has one.
    fn path_of(&self, app: &AppId) -> Option<PathBuf> {
        let path = self.dir.join(format!("{app}.toml"));
        path.is_file().then_some(path)
    }

    fn read(&self, app: &AppId) -> Option<ManifestBytes> {
        let path = self.path_of(app)?;
        let text = std::fs::read_to_string(&path).ok()?;
        // The manifest's own directory, exactly as `dagpane_app::compile` requires: a
        // `csv = "sales.csv"` beside it resolves here and not against the process's cwd.
        Some(ManifestBytes::new(text, self.dir.clone()))
    }
}

impl AppSource for DirAppSource {
    fn resolve(&self, host: &str) -> Result<AppKey, HostError> {
        // FIRST, before any I/O: is this one label in front of a declared domain? Membership
        // is exact string equality on everything after the first dot — never `ends_with`,
        // which would let `salesexample.com` stand in for `sales` under `example.com`.
        let outside = || HostError::OutsideDomains(host.to_string());
        let name = host.strip_suffix('.').unwrap_or(host);
        let (label, domain) = name.split_once('.').ok_or_else(outside)?;
        if !self.domains.iter().any(|d| d.as_str() == domain) {
            return Err(outside());
        }

        let app = AppId::parse(label).map_err(|_| HostError::UnknownHost(host.to_string()))?;
        let bytes = self
            .read(&app)
            .ok_or_else(|| HostError::UnknownHost(host.to_string()))?;
        Ok(AppKey::new(app, bytes.digest()))
    }

    fn fetch(&self, key: &AppKey) -> Result<ManifestBytes, HostError> {
        // Deliberately not checked against `key.manifest_digest`: a file edited between
        // resolve and fetch must surface as `HostError::DigestMismatch` from the host, which
        // is the side that does not trust the answer. Hiding it here would mean a manifest
        // saved mid-request is served under the previous version's key.
        self.read(&key.app_id)
            .ok_or_else(|| HostError::NotFound(key.clone()))
    }

    fn domains(&self) -> Vec<Domain> {
        DirAppSource::domains(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_header_loses_its_port_and_its_case() {
        assert_eq!(normalise_host("EXAMPLE.com:8080"), "example.com");
        assert_eq!(normalise_host(" example.com "), "example.com");
        assert_eq!(normalise_host("sales.example.com"), "sales.example.com");
    }

    #[test]
    fn an_ipv6_literal_keeps_its_brackets() {
        assert_eq!(normalise_host("[::1]:8080"), "[::1]");
        assert_eq!(normalise_host("[::1]"), "[::1]");
        // Malformed, but it must not become `[` — a route key that is one bracket would
        // collide with every other malformed header.
        assert_eq!(normalise_host("[::1"), "[::1");
    }

    fn example_com() -> Domain {
        Domain::parse("example.com").unwrap()
    }

    #[test]
    fn a_directory_answers_one_label_under_a_declared_domain_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sales.toml"), "[app]\ntitle = \"S\"\n").unwrap();
        std::fs::write(dir.path().join("ops.toml"), "[app]\ntitle = \"O\"\n").unwrap();
        std::fs::write(dir.path().join("My App.toml"), "[app]\ntitle = \"X\"\n").unwrap();
        std::fs::write(dir.path().join("notes.md"), "not an app").unwrap();

        let src = DirAppSource::new(dir.path()).with_domains([example_com()]);
        assert_eq!(
            src.apps().iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            vec!["ops", "sales"]
        );
        assert_eq!(src.skipped(), vec!["My App".to_string()]);

        for header in [
            "sales.localhost",
            "sales.localhost.",
            "sales.example.com",
            "sales.example.com.",
        ] {
            assert_eq!(
                src.resolve(header).unwrap().app_id.as_str(),
                "sales",
                "{header}"
            );
        }

        // Each of these would have reached sales.toml (or another app) under first-label
        // routing, and each is a name somebody other than the operator can own, or no name
        // at all. Exact membership is what refuses the near misses: `ends_with` would admit
        // `salesexample.com` and `sales.evillocalhost` is not `localhost`.
        for header in [
            "sales",
            "localhost",
            "example.com",
            "saleslocalhost",
            "sales.notlocalhost",
            "sales.evillocalhost",
            "x.sales.localhost",
            "x.sales.example.com",
            "sales.localhost.attacker.example",
            "sales.example.com.attacker.example",
            "salesexample.com",
            "sales-example.com",
            "sales.com",
            "sales.attacker.example",
            "127.0.0.1",
            "[::1]",
            "",
            "sales.localhost..",
        ] {
            assert!(
                matches!(src.resolve(header), Err(HostError::OutsideDomains(_))),
                "{header:?} must be outside every declared domain, got {:?}",
                src.resolve(header)
            );
        }

        // Inside a declared domain, and no app by that label: the ordinary miss.
        for header in [
            "nobody.localhost",
            "nobody.example.com",
            "my_app.localhost",
            ".localhost",
        ] {
            assert!(
                matches!(src.resolve(header), Err(HostError::UnknownHost(_))),
                "{header:?} is inside a declared domain and names no app, got {:?}",
                src.resolve(header)
            );
        }
    }

    #[test]
    fn with_domains_adds_to_localhost_and_never_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sales.toml"), "[app]\ntitle = \"S\"\n").unwrap();

        let bare = DirAppSource::new(dir.path());
        assert_eq!(bare.domains(), vec![Domain::localhost()]);
        assert!(matches!(
            bare.resolve("sales.example.com"),
            Err(HostError::OutsideDomains(_))
        ));

        let src = DirAppSource::new(dir.path()).with_domains([
            example_com(),
            Domain::localhost(),
            example_com(),
        ]);
        assert_eq!(src.domains(), vec![example_com(), Domain::localhost()]);
        assert!(src.resolve("sales.localhost").is_ok());
        assert!(src.resolve("sales.example.com").is_ok());
    }

    #[test]
    fn editing_a_manifest_on_disk_moves_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sales.toml");
        std::fs::write(&path, "[app]\ntitle = \"before\"\n").unwrap();

        let src = DirAppSource::new(dir.path());
        let before = src.resolve("sales.localhost").unwrap();
        std::fs::write(&path, "[app]\ntitle = \"after\"\n").unwrap();
        let after = src.resolve("sales.localhost").unwrap();

        assert_eq!(before.app_id, after.app_id);
        assert_ne!(
            before.manifest_digest, after.manifest_digest,
            "an edited manifest kept its key, so the host would keep serving the old graph"
        );
    }

    #[test]
    fn republishing_different_bytes_moves_the_key() {
        let id = AppId::parse("sales").unwrap();
        let src = MemAppSource::new();
        src.publish("sales.example.com", &id, "[app]\ntitle = \"a\"\n", ".");
        let first = src.resolve("sales.example.com").unwrap();
        src.publish("sales.example.com", &id, "[app]\ntitle = \"b\"\n", ".");
        let second = src.resolve("sales.example.com").unwrap();
        assert_eq!(first.app_id, second.app_id);
        assert_ne!(first.manifest_digest, second.manifest_digest);
    }
}
