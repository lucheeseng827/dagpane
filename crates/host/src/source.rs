//! Where a manifest comes from, and how a request finds one.
//!
//! The host does not know what a deployment is. It knows how to turn a host header into an
//! [`AppKey`] and a key into manifest bytes, and it asks something else both questions. That
//! something else is a directory of manifests in the open-source case and a registry in a
//! managed one, and neither of them is this crate's business.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use crate::error::HostError;
use crate::key::{manifest_digest, AppId, AppKey};

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
    /// # Errors
    ///
    /// [`HostError::UnknownHost`] when nothing is routed there.
    fn resolve(&self, host: &str) -> Result<AppKey, HostError>;

    /// The manifest this key names.
    ///
    /// # Errors
    ///
    /// [`HostError::NotFound`] when the key names nothing this source has.
    fn fetch(&self, key: &AppKey) -> Result<ManifestBytes, HostError>;
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
/// the directory is an app named by its file stem, and a request routes to it by the **first
/// label of its host header** — `sales.example.com`, `sales.localhost:8787` and a bare
/// `sales` all reach `sales.toml`. One label rather than the whole name, because the person
/// running this owns a domain and does not want to restate it in every filename.
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
}

impl DirAppSource {
    /// Serve every well-named `*.toml` in this directory.
    pub fn new(dir: impl Into<PathBuf>) -> DirAppSource {
        DirAppSource { dir: dir.into() }
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
        let label = host.split('.').next().unwrap_or(host);
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

    #[test]
    fn a_directory_routes_by_the_first_label_and_skips_what_is_not_a_label() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sales.toml"), "[app]\ntitle = \"S\"\n").unwrap();
        std::fs::write(dir.path().join("ops.toml"), "[app]\ntitle = \"O\"\n").unwrap();
        std::fs::write(dir.path().join("My App.toml"), "[app]\ntitle = \"X\"\n").unwrap();
        std::fs::write(dir.path().join("notes.md"), "not an app").unwrap();

        let src = DirAppSource::new(dir.path());
        assert_eq!(
            src.apps().iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            vec!["ops", "sales"]
        );
        assert_eq!(src.skipped(), vec!["My App".to_string()]);

        for header in ["sales", "sales.example.com", "sales.localhost"] {
            assert_eq!(
                src.resolve(header).unwrap().app_id.as_str(),
                "sales",
                "{header}"
            );
        }
        assert!(matches!(
            src.resolve("nobody.example.com"),
            Err(HostError::UnknownHost(_))
        ));
        assert!(
            matches!(src.resolve("My App"), Err(HostError::UnknownHost(_))),
            "a name that is not a label must not resolve"
        );
    }

    #[test]
    fn editing_a_manifest_on_disk_moves_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sales.toml");
        std::fs::write(&path, "[app]\ntitle = \"before\"\n").unwrap();

        let src = DirAppSource::new(dir.path());
        let before = src.resolve("sales").unwrap();
        std::fs::write(&path, "[app]\ntitle = \"after\"\n").unwrap();
        let after = src.resolve("sales").unwrap();

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
