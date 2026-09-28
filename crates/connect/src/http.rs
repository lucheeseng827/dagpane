//! A source that is a URL returning CSV. **Behind the `http` feature; off by default.**
//!
//! This is the first network client in the tree, and the invariant it replaces was worth
//! something: *a data-app runtime that can phone home is not one anybody self-hosts.* What
//! makes it acceptable to weaken is the shape, not the intention — the client is compiled in
//! only when somebody asks for it by feature, a default `cargo build` of this workspace
//! links no HTTP stack at all, and CI names this crate's manifest as the only place the
//! dependency may appear.

use std::sync::Arc;
use std::time::Duration;

use dagpane_core::frame::{Frame, FrameBuilder};
use dagpane_core::ColumnType;

use crate::error::SourceError;
use crate::version::{Version, VersionPart};
use crate::{csv, Source};

/// How long to wait before calling a URL unreachable.
///
/// A default rather than a constant a caller cannot change: the number that matters is that
/// there **is** one. A source with no timeout turns a scheduler into a thread pool holding
/// sockets open against a server that stopped answering, and the symptom is a control plane
/// that appears to be doing nothing.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// How much body to accept.
///
/// The row limit this runtime is honest about is roughly a hundred thousand rows; a response
/// far past that is a mistake somebody wants to hear about rather than an out-of-memory kill
/// with no message. Configurable, because "roughly" is doing work in that sentence.
pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// A URL that returns CSV.
#[derive(Clone, Debug)]
pub struct HttpSource {
    url: String,
    timeout: Duration,
    max_bytes: u64,
}

impl HttpSource {
    /// A source at this URL, with the default timeout and size cap.
    ///
    /// # Errors
    ///
    /// [`SourceError::Misconfigured`] for a scheme this source will not fetch. Only `http`
    /// and `https`: `file://` would be [`crate::FileSource`] wearing a disguise and would
    /// let a manifest read a path the manifest's own `file` field would have resolved
    /// differently, and everything else is a surprise.
    pub fn new(url: impl Into<String>) -> Result<HttpSource, SourceError> {
        let url = url.into();
        let lower = url.to_ascii_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://")) {
            return Err(SourceError::Misconfigured {
                source: redact(&url),
                reason: "a URL source is http:// or https:// — a local path is a `file` source"
                    .to_string(),
            });
        }
        Ok(HttpSource {
            url,
            timeout: DEFAULT_TIMEOUT,
            max_bytes: DEFAULT_MAX_BYTES,
        })
    }

    /// Wait no longer than this for the whole exchange.
    pub fn with_timeout(mut self, timeout: Duration) -> HttpSource {
        self.timeout = timeout;
        self
    }

    /// Accept no more than this many bytes of body.
    pub fn with_max_bytes(mut self, max_bytes: u64) -> HttpSource {
        self.max_bytes = max_bytes;
        self
    }

    /// The URL, as given.
    pub fn url(&self) -> &str {
        &self.url
    }

    fn agent(&self) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(self.timeout))
            // Redirects are followed, with a bound. A source that refused them would break on
            // every host that serves a canonical URL; one that followed them without a bound
            // is a loop somebody else controls.
            .max_redirects(5)
            // A status is a RESPONSE here, not a transport failure. ureq's default folds a
            // 404 into the same `Err` as a refused connection, and the two must not be one
            // thing: `SourceError::is_retryable` is what a scheduler backs off on, and a
            // scheduler that retries a 404 every minute forever is the consequence of
            // letting the client make that judgement. `classify` below makes it here, once.
            .http_status_as_error(false)
            .build()
            .into()
    }

    fn body(&self) -> Result<String, SourceError> {
        let response = self
            .agent()
            .get(&self.url)
            .call()
            .map_err(|e| self.unreachable(&e))?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(self.status_error(status));
        }

        response
            .into_body()
            .with_config()
            .limit(self.max_bytes)
            .read_to_string()
            .map_err(|e| SourceError::Unreadable {
                source: self.describe(),
                reason: format!(
                    "could not read the body within {} bytes: {e}",
                    self.max_bytes
                ),
            })
    }

    /// A non-success status, split by whether waiting could plausibly help.
    ///
    /// The split is the whole reason this is a function. **5xx and 429 are `Unreachable`** —
    /// the server is saying "not now", and a refresh schedule should back off and come back.
    /// **Every other 4xx is `Unreadable`** — a 404, a 401, a 403 is the configuration being
    /// wrong, and retrying it every minute until somebody notices is how a source that was
    /// deleted last quarter is still generating traffic today.
    fn status_error(&self, status: u16) -> SourceError {
        let reason = format!("HTTP {status}");
        if status >= 500 || status == 429 {
            SourceError::Unreachable {
                source: self.describe(),
                reason,
            }
        } else {
            SourceError::Unreadable {
                source: self.describe(),
                reason,
            }
        }
    }

    fn unreachable(&self, e: &ureq::Error) -> SourceError {
        SourceError::Unreachable {
            source: self.describe(),
            // `e.to_string()` and never the debug form: ureq's `Debug` can carry the request,
            // and a request carries whatever is in the URL.
            reason: e.to_string(),
        }
    }
}

impl Source for HttpSource {
    /// The URL with its query string and any userinfo removed — see [`redact`].
    fn describe(&self) -> String {
        format!("url {}", redact(&self.url))
    }

    fn schema(&self) -> Result<Vec<(String, ColumnType)>, SourceError> {
        // One request, and the types come from the body, so this costs exactly a load. Said
        // out loud because over a network the difference between "cheap" and "the same as
        // loading" is somebody's rate limit.
        let columns = csv::parse_columns(&self.body()?).map_err(|e| SourceError::Unreadable {
            source: self.describe(),
            reason: e.to_string(),
        })?;
        Ok(columns
            .iter()
            .map(|c| (c.name.clone(), c.data.column_type()))
            .collect())
    }

    fn load(&self, into: Box<dyn FrameBuilder>) -> Result<Arc<dyn Frame>, SourceError> {
        csv::parse_into(&self.body()?, into).map_err(|e| SourceError::Unreadable {
            source: self.describe(),
            reason: e.to_string(),
        })
    }

    /// A `HEAD` request, and the validators the server chose to send.
    ///
    /// `ETag` first, `Last-Modified` second, and **[`Version::unknowable`] when there is
    /// neither** — which makes every refresh reload. That is the deliberate choice: a server
    /// with no validator has told us nothing, and reporting an equal version because two
    /// responses happened to have the same length is how a dashboard shows yesterday's
    /// number with today's timestamp on it. `Content-Length` is included when a validator is
    /// present and never on its own.
    fn version(&self) -> Result<Version, SourceError> {
        let response = match self.agent().head(&self.url).call() {
            Ok(r) => r,
            Err(e) => return Err(self.unreachable(&e)),
        };

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            // A server that refuses `HEAD` — some answer 405, some 501 — has told us nothing
            // about staleness, and failing the refresh over it would strand the source
            // entirely. Reload instead. A 5xx or a 404 is a different matter: that is the
            // source being unreachable or gone, and it is reported as such so a schedule can
            // back off and a person can be told.
            if status == 405 || status == 501 {
                return Ok(Version::unknowable());
            }
            return Err(self.status_error(status));
        }

        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let etag = header("etag");
        let modified = header("last-modified");

        if etag.is_none() && modified.is_none() {
            return Ok(Version::unknowable());
        }

        let length = header("content-length");
        Ok(Version::of(
            b'h',
            &[
                etag.as_deref()
                    .map_or(VersionPart::Absent, VersionPart::Text),
                modified
                    .as_deref()
                    .map_or(VersionPart::Absent, VersionPart::Text),
                length
                    .as_deref()
                    .map_or(VersionPart::Absent, VersionPart::Text),
            ],
        ))
    }
}

/// A URL with everything that could be a secret removed: userinfo, and the whole query.
///
/// Signed URLs put a token in the query string and plenty of APIs put a key there, so the
/// query goes whole rather than by a list of parameter names somebody has to keep current.
/// What is left — scheme, host, path — is what a person needs to recognise which source
/// failed.
pub fn redact(url: &str) -> String {
    let (before_query, has_query) = match url.split_once('?') {
        Some((head, _)) => (head, true),
        None => (url, false),
    };

    // `scheme://userinfo@host/path` — the userinfo is between `//` and the FIRST `@` that
    // precedes the first `/` of the path.
    let redacted = match before_query.split_once("://") {
        Some((scheme, rest)) => {
            let authority_end = rest.find('/').unwrap_or(rest.len());
            match rest[..authority_end].rsplit_once('@') {
                Some((_, host)) => format!("{scheme}://***@{host}{}", &rest[authority_end..]),
                None => before_query.to_string(),
            }
        }
        None => before_query.to_string(),
    };

    if has_query {
        format!("{redacted}?***")
    } else {
        redacted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_string_and_any_userinfo_are_removed() {
        assert_eq!(
            redact("https://example.com/data.csv?token=hunter2"),
            "https://example.com/data.csv?***"
        );
        assert_eq!(
            redact("https://user:pass@example.com/data.csv"),
            "https://***@example.com/data.csv"
        );
        assert_eq!(
            redact("https://user:pass@example.com/d.csv?sig=abc"),
            "https://***@example.com/d.csv?***"
        );
        assert_eq!(
            redact("https://example.com/data.csv"),
            "https://example.com/data.csv"
        );
        // An `@` in the PATH is not userinfo, and must not be mistaken for it.
        assert_eq!(
            redact("https://example.com/a@b/data.csv"),
            "https://example.com/a@b/data.csv"
        );
    }

    #[test]
    fn a_scheme_this_source_will_not_fetch_is_refused_before_anything_is_opened() {
        for bad in [
            "file:///etc/passwd",
            "ftp://example.com/x.csv",
            "/tmp/x.csv",
        ] {
            let err = HttpSource::new(bad).unwrap_err();
            assert!(
                matches!(err, SourceError::Misconfigured { .. }),
                "{bad}: {err:?}"
            );
            assert!(!err.is_retryable(), "{bad} is not fixed by retrying");
        }
        assert!(HttpSource::new("HTTPS://Example.com/x.csv").is_ok());
    }
}
