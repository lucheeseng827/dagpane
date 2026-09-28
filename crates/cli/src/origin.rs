//! What a browser actually sends, and the four ways an operator writes it instead.
//!
//! An `Origin` is compared as a string, so a value that is nearly right is exactly as wrong as
//! one that is nothing like it — and the symptom is identical either way: a page that loads
//! with `200` and a socket refused with `403`, so the controls do nothing and nothing in the
//! log says why. `OPERATIONS.md` spends a section on that trap.
//!
//! So the flag is checked at start-up rather than at the first upgrade. A refusal here names
//! the string and the fix, in a terminal an operator is looking at; a mismatch at upgrade time
//! is a silent dead page found by whoever was sent the link.

/// Check one `--origin` and return it in the form a browser sends.
///
/// # Errors
///
/// A string a browser would never send, with the fix named.
pub fn parse(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    let (scheme, rest) = match raw.split_once("://") {
        Some((s, rest)) => (s.to_ascii_lowercase(), rest),
        // The commonest one. A bare hostname is what people mean, and it is not what a
        // browser sends: the scheme is part of an origin, and `http` and `https` are two
        // different ones.
        None => {
            return Err(format!(
                "`{raw}` is not an origin: an origin carries its scheme, and `http://` and \
                 `https://` are two different origins. Write `https://{raw}`"
            ))
        }
    };
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "`{raw}` is not an origin a browser sends: the scheme is `{scheme}`, and only \
             `http` and `https` reach this server"
        ));
    }

    // The second commonest, and the one that looks right: an address bar shows a trailing
    // slash and an `Origin` header never has one.
    if let Some(bare) = rest.strip_suffix('/') {
        if bare.is_empty() || !bare.contains('/') {
            return Err(format!(
                "`{raw}` has a trailing slash and an `Origin` header never does — a browser \
                 sends `{scheme}://{bare}`. Drop the slash"
            ));
        }
    }
    if let Some((host, tail)) = rest.split_once('/') {
        return Err(format!(
            "`{raw}` carries a path (`/{tail}`) and an origin has none: a browser sends \
             `{scheme}://{host}` whatever page it is on. Drop everything after the host"
        ));
    }
    if rest.contains('?') || rest.contains('#') {
        return Err(format!(
            "`{raw}` carries a query or a fragment and an origin has neither. Drop everything \
             after the host"
        ));
    }
    if rest.is_empty() {
        return Err(format!("`{raw}` names no host"));
    }
    if rest.contains('@') {
        return Err(format!(
            "`{raw}` carries credentials and an origin has none. Drop everything before the `@`"
        ));
    }

    // Lowercased: a browser lowercases the scheme and the host before it sends them, and the
    // comparison is exact.
    let host = rest.to_ascii_lowercase();

    // And a browser omits the scheme's default port, so `https://x.example:443` is sent as
    // `https://x.example` and an operator who wrote the explicit form declared an origin that
    // can never match. Verified against a real URL parser: `:443` on https and `:80` on http
    // are dropped, every other port is kept.
    //
    // Stripping a suffix is safe for an IPv6 literal too, because a host that contains colons
    // is bracketed — `[::1]:443` ends with the port and `[::1]` does not.
    let default_port = match scheme.as_str() {
        "https" => ":443",
        _ => ":80",
    };
    let host = host.strip_suffix(default_port).unwrap_or(&host);

    Ok(format!("{scheme}://{host}"))
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn a_published_name_passes_through_unchanged() {
        assert_eq!(
            parse("https://panels.example.com").unwrap(),
            "https://panels.example.com"
        );
        assert_eq!(
            parse("http://localhost:8787").unwrap(),
            "http://localhost:8787"
        );
    }

    #[test]
    fn the_case_a_browser_normalises_is_normalised_here_too() {
        // A browser lowercases both before it sends them, and the comparison is exact, so an
        // operator who typed the name the way their documentation capitalises it would
        // otherwise have declared an origin that never matches.
        assert_eq!(
            parse("HTTPS://Panels.Example.COM").unwrap(),
            "https://panels.example.com"
        );
    }

    #[test]
    fn a_default_port_is_dropped_because_a_browser_never_sends_one() {
        // A browser's `Origin` omits the scheme's default port, and the comparison is exact —
        // so the explicit spelling, which is the one somebody copies out of a config file,
        // would otherwise declare an origin that can never match and refuse every upgrade.
        //
        // Mutation check: returning the host unstripped fails all four of these.
        assert_eq!(
            parse("https://panels.example.com:443").unwrap(),
            "https://panels.example.com"
        );
        assert_eq!(
            parse("http://panels.example.com:80").unwrap(),
            "http://panels.example.com"
        );
        // Both spellings have to land on the same string, since that is the whole point.
        assert_eq!(
            parse("https://panels.example.com:443").unwrap(),
            parse("https://panels.example.com").unwrap()
        );
        // A default port for the OTHER scheme is not a default port. `https://x:80` is a
        // browser origin with a port in it, and dropping it would break a real deployment.
        assert_eq!(
            parse("https://panels.example.com:80").unwrap(),
            "https://panels.example.com:80"
        );
    }

    #[test]
    fn every_other_port_is_kept() {
        assert_eq!(
            parse("http://localhost:3000").unwrap(),
            "http://localhost:3000"
        );
        assert_eq!(
            parse("https://panels.example.com:8443").unwrap(),
            "https://panels.example.com:8443"
        );
        // An IPv6 literal keeps its brackets and loses only a real default port.
        assert_eq!(parse("http://[::1]:80").unwrap(), "http://[::1]");
        assert_eq!(parse("http://[::1]:8787").unwrap(), "http://[::1]:8787");
    }

    #[test]
    fn the_address_bars_trailing_slash_is_refused_by_name() {
        let e = parse("https://panels.example.com/").unwrap_err();
        assert!(e.contains("trailing slash"), "{e}");
        // The fix has to be in the message: the value looks right, so a reader who is not
        // told what to change will retype the same thing.
        assert!(e.contains("https://panels.example.com`"), "{e}");
    }

    #[test]
    fn a_bare_hostname_is_refused_with_the_scheme_spelled_out() {
        let e = parse("panels.example.com").unwrap_err();
        assert!(e.contains("https://panels.example.com"), "{e}");
    }

    #[test]
    fn a_pasted_page_url_is_refused_and_says_which_part_to_drop() {
        let e = parse("https://panels.example.com/sales?tab=1").unwrap_err();
        assert!(e.contains("path"), "{e}");
        assert!(e.contains("https://panels.example.com"), "{e}");
    }

    #[test]
    fn a_scheme_this_server_cannot_be_reached_over_is_refused() {
        // `ws://` is the one worth catching: it is the scheme of the *connection*, and the
        // `Origin` of the page that opens it is still `http(s)`.
        let e = parse("ws://panels.example.com").unwrap_err();
        assert!(e.contains("ws"), "{e}");
    }

    #[test]
    fn an_empty_host_is_refused() {
        assert!(parse("https://").is_err());
    }
}
