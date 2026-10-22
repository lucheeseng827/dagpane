//! What `--domain` takes, and the ways an operator writes something else instead.
//!
//! `dagpane host` answers `<app>.localhost` and, for each `--domain`, `<app>.<domain>` — and
//! nothing else, because any other name is one somebody else's DNS answers for (see
//! `dagpane_host::AppSource::resolve`). So a domain written slightly wrong is a domain no
//! request ever matches, and the symptom is every app answering 404 behind a proxy that is
//! otherwise fine. That is why the flag is checked at start-up, where a refusal can name the
//! string and the fix, rather than discovered by whoever is sent the first link.

use dagpane_host::Domain;

/// Check one `--domain`, forgive the spellings that mean the same thing, and refuse the rest
/// with the fix named.
///
/// Forgiven: case, one trailing dot, and a leading `*.` — `*.example.com` is how a certificate
/// writes "exactly one label under example.com", which is exactly what this means.
///
/// # Errors
///
/// A value that is not a domain to publish apps under, with what to write instead.
pub fn parse(raw: &str) -> Result<Domain, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(
            "`--domain` was given no value: name the domain the apps are published under, \
             e.g. `--domain example.com`"
                .to_string(),
        );
    }

    // The commonest one: the address bar's spelling, copied whole — or nearly (`https:/x`).
    if let Some((_, rest)) = raw
        .split_once(':')
        .filter(|(_, rest)| rest.starts_with('/'))
    {
        let rest = rest.trim_start_matches('/');
        let host = rest.split(['/', ':', '?', '#']).next().unwrap_or(rest);
        return Err(write_instead(
            format!("`{raw}` is a URL and --domain takes a domain"),
            host,
        ));
    }
    if raw.contains(',') {
        return Err(format!(
            "`{raw}` holds a comma, and --domain takes one domain: repeat the flag, \
             `--domain a.example --domain b.example`"
        ));
    }
    if let Some((host, _)) = raw.split_once('/') {
        return Err(write_instead(
            format!("`{raw}` carries a path and a domain has none"),
            host,
        ));
    }
    let before_port = raw.split(':').next().unwrap_or(raw);
    if raw.starts_with('[')
        || raw.parse::<std::net::IpAddr>().is_ok()
        || before_port.parse::<std::net::IpAddr>().is_ok()
    {
        return Err(format!(
            "`{raw}` is an IP address, and an app is reached by name — an address has no \
             label to put the app in. Use a name that resolves to it"
        ));
    }
    if raw.contains(':') {
        return Err(write_instead(
            format!(
                "`{raw}` carries a port and a domain has none — the port is whichever one the \
                 request arrived on"
            ),
            before_port,
        ));
    }

    let lowered = raw.to_ascii_lowercase();
    let mut domain = lowered.as_str();
    domain = domain.strip_suffix('.').unwrap_or(domain);
    domain = domain.strip_prefix("*.").unwrap_or(domain);

    if domain.starts_with('.') {
        return Err(write_instead(
            format!("`{raw}` starts with a dot"),
            domain.trim_start_matches('.'),
        ));
    }
    if domain.contains('*') {
        return Err(format!(
            "`{raw}`: only a leading `*.` is understood, and it already means exactly one label"
        ));
    }
    if !domain.is_ascii() {
        return Err(format!(
            "`{raw}` is not ASCII: write its punycode form (`xn--…`), which is what a browser \
             sends"
        ));
    }
    Domain::parse(domain).map_err(|e| format!("--domain {raw}: {e}"))
}

/// A refusal that names what to write instead — but only if what it would name is itself a
/// domain to declare. Suggesting `com` for `.com`, and then refusing `com`, would send the
/// operator round in a circle; then the reason the suggestion is no good is the useful part.
fn write_instead(problem: String, suggestion: &str) -> String {
    let suggestion = suggestion.to_ascii_lowercase();
    let suggestion = suggestion.strip_suffix('.').unwrap_or(&suggestion);
    match Domain::parse(suggestion) {
        Ok(_) => format!("{problem}: write `{suggestion}`"),
        Err(e) => format!("{problem}, and what is left is not a domain either: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(raw: &str) -> String {
        parse(raw).expect_err(raw)
    }

    #[test]
    fn the_spellings_that_mean_a_domain_are_accepted_as_it() {
        for (raw, domain) in [
            ("example.com", "example.com"),
            ("Example.COM.", "example.com"),
            ("*.example.com", "example.com"),
            ("  dash.example.com ", "dash.example.com"),
            ("internal", "internal"),
            ("localhost", "localhost"),
        ] {
            assert_eq!(parse(raw).unwrap().as_str(), domain, "{raw}");
        }
    }

    #[test]
    fn each_near_miss_is_refused_with_its_fix() {
        let url = refused("https://dash.example.com/");
        assert!(url.contains("write `dash.example.com`"), "{url}");
        assert!(refused("Https://Dash.Example.com:8443/x").contains("write `dash.example.com`"));

        assert!(refused("https:/dash.example.com").contains("write `dash.example.com`"));

        let port = refused("dash.example.com:8443");
        assert!(port.contains("carries a port"), "{port}");
        assert!(port.contains("write `dash.example.com`"), "{port}");

        // A fix the parser would refuse is never offered: the reason is, instead.
        for circular in [".com", "com/x", "https://com/", "com:80"] {
            let message = refused(circular);
            assert!(!message.contains("write `com`"), "{circular}: {message}");
            assert!(
                message.contains("anyone can register"),
                "{circular}: {message}"
            );
        }

        assert!(refused("a.com,b.com").contains("repeat the flag"));
        assert!(refused("example.com/x").contains("write `example.com`"));

        for address in ["127.0.0.1", "127.0.0.1:80", "[::1]", "::1"] {
            let message = refused(address);
            assert!(message.contains("IP address"), "{address}: {message}");
        }

        assert!(refused(".example.com").contains("write `example.com`"));
        assert!(refused("a*.example.com").contains("only a leading `*.`"));
        assert!(refused("bücher.example").contains("punycode"));
        assert!(refused("").contains("no value"));

        // What the library refuses arrives with the flag and the library's reason.
        let tld = refused("com");
        assert!(tld.starts_with("--domain com:"), "{tld}");
        assert!(tld.contains("anyone can register"), "{tld}");
        assert!(refused("example..com").contains("--domain example..com:"));
    }
}
