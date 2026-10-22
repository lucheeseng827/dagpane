# ADR-0012: a name is served only where it was declared

**Status:** accepted.

## Context

`dagpane host` serves every manifest in a directory from one process on one port, and picks the
app by the `Host` header. It picked by the first label of whatever name arrived, and its own
`--help` said so as a convenience: *"reachable at any name whose first label is `sales` —
`sales.example.com`, `sales.localhost:8787`, or a bare `Host: sales`"*. Any `sales.<anything>`
reached the same file.

The socket beside it had one rule about which page may open it: `Origin` must name the same
host as `Host`. That is the right check against the attack it was written for — a page at some
other name using the viewer's browser to open this app's socket — and no check at all against
DNS rebinding, which is the attack in which the page's name **is** the name the request is for:

1. A page is served from `http://sales.attacker.example:8787`. Its owner controls that name's
   DNS.
2. Once it has loaded, the owner changes the name's answer to `127.0.0.1` — or to any address
   this process listens on — with a short lifetime, and the browser's next lookup gets it.
3. The page opens a socket to its own name. The browser sends
   `Host: sales.attacker.example:8787` and `Origin: http://sales.attacker.example:8787`. They
   agree, the first label is `sales`, and the page is sent `init` and every pane.

`GET /` for the same name compiled `sales.toml` and could evict other apps to fit it, so the
same page could spend the process's budget as well. No refinement of the `Origin` comparison
can close this: both headers are the name the attacker chose, and they agree because they are
one choice. `run` and `serve` were never exposed, because their allowlist is the bound address
or `serve`'s declared `--origin`s, and a rebound page's `Origin` is its own name. No tagged
release contains `dagpane host` — it is under *Unreleased* in `CHANGELOG.md` — but `main` has
been public with it. *(As of this decision. `host` first shipped in 0.2.0, with this rule.)*

The same check had a second, smaller hole, found while closing the first. A `.localhost` name
cannot be rebound — Chrome and Firefox resolve `*.localhost` to loopback themselves (RFC 6761
§6.3) — but it can be *served* by anything on the machine: a dev server on `:3000` or
`python -m http.server` serves pages at `sales.localhost:3000` without anybody configuring a
name. The comparison ignored ports, so such a page opened `sales.localhost:8787`'s socket and
read every pane. That was reproduced against the built binary before this decision.

## Decision

### 1. The source is the boundary, and its contract says so

`AppSource::resolve` turns a host header into an `AppKey`, and everything after it — the fetch,
the digest check, the compile, the budget — trusts that answer. So the rule goes there, as an
obligation on every implementation: **a source answers only names its operator declared.** An
exact table, as `MemAppSource` keeps, is bounded by construction — it answers what was published
and nothing else. A pattern — a label, a suffix, a fallback app — must be bounded to declared
names, and must refuse **before any I/O**, because a source that reads a file or a registry to
decide has already done work for a name nobody declared.

A refusal is an error of its own, `HostError::OutsideDomains`, beside `UnknownHost`, because
the fixes differ: an unknown name under a declared domain is a missing manifest, and a name
outside every declared domain is either an attack or a domain the operator forgot to declare,
which only the operator can tell apart. `crates/serve` answers both with `404` and words them
differently, so a `curl` can tell which it hit: the first is `no app is served at <name>`, and
the second adds `: this server does not answer to that name`. `Host::open` and
`Host::open_session` resolve before anything else, so nothing is compiled or evicted for a
refused name.

Two other places for the rule were considered and rejected:

* **An allowlist in `crates/serve`'s router.** It would be a second list of names beside the one
  the source routes by, and two lists can disagree: the router admits a name the source then
  routes somewhere nobody expected, or refuses one the source was told to serve. For an
  exact-name source it would only restate the table. The source is the one piece of code that
  knows what a name means, so it is the one place a bound on names is a single fact.
* **A name policy required at `Host::new`.** It moves the same second list one crate down, makes
  every caller — every test, every embedding over a `MemAppSource` — supply a policy its source
  already enforces, and still leaves the pattern in the source, which is where an unbounded
  answer is produced. The obligation belongs with the code that writes the pattern.

### 2. A directory answers one label in front of a declared domain

`DirAppSource` answers `<label>.localhost` always, and `<label>.<d>` for each domain added with
`DirAppSource::with_domains` — on the command line, each `--domain`. One label, because whoever
runs it owns a domain and should not restate it in every filename. The check is the first thing
`resolve` does: strip one trailing dot (`sales.localhost.` is the same name, and is answered),
split at the first dot, and refuse unless what follows is **exactly equal** to a declared domain.
Equality and not a suffix test: one on the whole name admits `salesexample.com` under
`example.com`, and one on the part after the dot admits `sales.evillocalhost` under `localhost`.
A bare `sales`, `localhost` itself, `x.sales.localhost`, `sales.localhost.attacker.example` and
every address are refused by the same rule, because none of them is one label followed by a
declared domain.

What may be declared is a `dagpane_host::Domain`, whose only constructors are `Domain::parse`
and `Domain::localhost`, so the library cannot be handed an unchecked domain any more than the
command line can. It is strict and does not normalise, for `AppId`'s reason: labels of ASCII
lowercase letters, digits and `-`, none empty, none over 63 bytes, at most 253 bytes in all; not
a numeric last label, which the WHATWG URL parser reads as an IPv4 address (`127.1`,
`a.0x7f`); not a name ending in `.local`, which is multicast DNS (RFC 6762) — no registry, and
any machine on the network may answer for any `.local` name, so a zone under it is no more the
operator's than `local` itself, and it is what `hostname` prints on a Mac; and not a single
label other than `localhost`, `internal` or `test`, because anyone can register a name directly
under any other top-level domain. The command line forgives what
means the same — case, one trailing dot, a leading `*.` — and refuses the rest at start-up with
the fix named, before anything is bound: a URL, a comma list, a path, an address, a port, a
leading dot, any other `*`, a non-ASCII name.

**Declaring a domain trusts every name directly under it**, which is safe only for a zone whose
DNS the operator controls. What the parser can refuse — a top-level domain, an address — it
does. What it cannot see is a multi-label public suffix (`co.uk`, `github.io`, `duckdns.org`) or
a shared wildcard service (`localtest.me`, `lvh.me`, `nip.io`, `sslip.io`), and those are
documented — in `dagpane host --help` and `SECURITY.md` — rather than checked.

### 3. `localhost` is always one of them

Every `DirAppSource` answers under `localhost`, and `with_domains` adds to that set and never
replaces it: a process published under a domain is still one an operator reaches on the machine
itself. It is safe for a reason that lives in the client. Chrome and Firefox resolve
`*.localhost` to loopback themselves (RFC 6761 §6.3), so no DNS answer can point a `.localhost`
name anywhere else and a rebinding page cannot arrive under one. A client that forwards
`.localhost` names to network DNS loses that, and a front door (`--auth-jwks`) is what removes
the dependence on it. It is a guarantee about where the name leads, not about who serves pages
there — which is the next decision.

### 4. Under `localhost`, the `Origin` must agree on the port

The host socket's `Origin` check stays what it was — scheme `http` or `https`, host equal to the
request's `Host` name — and is now stated for what it is: CSRF defence, not the rebinding
defence. Under `localhost` — the name is `localhost` or ends in `.localhost` — it compares the
port as well, whenever the `Host` header carries one: the `Origin`'s explicit port, or 80 or 443
by its scheme, must equal it. Nothing rewrites either header for a `.localhost` name, so the two
agree for every honest request, and a page another local server serves on another port is
refused with `403`.

Two cases stay port-blind, both deliberately. When `Host` carries no port there is nothing to
agree with, and only the name is compared: a proxy forwarding nginx's `$host` drops the port,
and a browser leaves it out at its scheme's default — so the rule protects
`<app>.localhost:<port>`, the URL `dagpane host` prints on any port but 80, and not a
`.localhost` name served on 80 or 443 or through such a proxy. And a name under a `--domain` is
never compared by port, because behind a proxy the `Origin` carries the public port and `Host`
this process's — eggrd sends `Host: sales.example.com:8787` beside
`Origin: https://sales.example.com` — and which machines answer under a declared zone is the
operator's choice.

### 5. A mistake is said where the operator is looking

A domain written slightly wrong is a domain no request matches, and the symptom — every app
`404` behind a proxy that is otherwise healthy — is far from the cause. So:

* **`--domain` is checked at start-up**, and a refusal names the string and what to write
  instead: `https://x/` is told to write `x`, a comma list to repeat the flag, a non-ASCII name
  to write its punycode form.
* **`dagpane host` prints the names it answers before it binds**, with one to try. It has no
  scheme in it, so a failed bind still prints no `http://`:

  ```
  dagpane: names     <app>.localhost, <app>.example.com — e.g. ops.example.com; any other name, a bare <app> or an address gets 404
  ```

* **Two warnings name the likely mistakes.** A non-loopback bind that declared nothing beyond
  `localhost` is told, after binding, that each app answers only as `<app>.localhost` — the
  commonest way to have a proxy in front do nothing. A `--domain` that begins with an app's own
  name (`--domain sales.example.com` beside `sales.toml`) is told, before binding, that the app
  answers at `sales.sales.example.com`, and to pass `--domain example.com` if that is what the
  apps sit under — but only when that suggestion is itself declarable, so an app named after
  its own apex (`acme.toml` with `--domain acme.com`) is not told to declare `com`. Every fix a
  start-up refusal suggests passes the parser the same way, or the reason it would not is given
  instead.
* **A refused name is logged on stderr once per domain** — the text after its first dot — with
  one bucket for addresses, one for bare names and one for names longer than any DNS name, at
  most 32 distinct keys per process, then nothing, and no name printed past 253 bytes, so the
  sender of a name chooses neither how many lines this writes nor how long one is. A name one
  label too deep under `localhost` (`x.sales.localhost`) is told so, rather than offered
  `--domain sales.localhost`. One line is enough to diagnose a forgotten `--domain`, and a
  prober chooses the names but must not choose how much this process writes. The line offers
  `pass --domain <d>` only when `<d>` could be a domain, and always as *if `<d>` is a domain you
  control*: the name came from whoever sent the request, and a log line telling an operator to
  declare an attacker's domain would be the attack finishing itself.

## Consequences

* **A name that used to work may now be a `404`.** A bare `Host: sales`, an address, and any
  real hostname not declared with `--domain` are refused. Nobody moving between tagged releases
  meets it, since none contained `dagpane host`; a deployment built from `main` and reached
  through a proxy needs `--domain`, and stderr names the domain it is missing.
* **Behind eggrd, the upstream URL's hostname must be under a declared domain.**
  `upstream = "http://sales.example.com:8787"` needs `--domain example.com`, because the name
  `dagpane host` sees is the one in that URL.
* **The library has new public surface:** `Domain` and `InvalidDomain`,
  `DirAppSource::with_domains` and `DirAppSource::domains`, and `HostError::OutsideDomains` — so
  a `match` on `HostError` with no catch-all arm needs one more.
* **`AppSource::resolve` carries a written obligation**, and it binds a source written outside
  this repository as much as the two `dagpane-host` ships: a registry is held to it as a
  directory is.
* **A page another local server serves can no longer open a `.localhost` app's socket** when
  the browser sends the app's port in `Host`, which it does for every URL `dagpane host` prints
  unless the port is 80, the default it leaves out. Honest pages are unaffected.

### How it is checked

* `crates/host/src/source.rs` — the table is
  `a_directory_answers_one_label_under_a_declared_domain_and_nothing_else`: four spellings
  answered, eighteen names refused as outside every declared domain, four inside one with no
  app behind them. `with_domains_adds_to_localhost_and_never_replaces_it` holds the set.
  Dropping the domain check fails both; replacing exact membership with `ends_with` fails the
  table, and so does skipping the trailing-dot strip.
* `crates/host/src/key.rs` — `a_domain_is_dns_labels_and_the_near_misses_are_not`: seven
  domains accepted and twenty-five refused, among them `com`, `uk`, `local`, `x.local`,
  `127.0.0.1`, `a.0x7f` and a name over each length limit.
* `crates/serve/src/lib.rs` — `a_refused_name_is_keyed_by_its_domain_and_the_log_stops_at_the_cap`
  holds the log's keys, its once-per-key rule and its cap of 32.
* `crates/cli/src/domain.rs` holds every spelling the flag forgives and every near miss with its
  fix; unit tests in `crates/cli/src/main.rs` hold the names line, the wide-bind warning and the
  collision warning.
* `crates/serve/tests/multi_app.rs`, over real sockets:
  * `a_rebound_name_is_refused_before_anything_is_compiled_or_evicted` is the attack itself —
    `ops.attacker.example` as both `Host` and `Origin`, against a budget that holds one app, so
    an admission would have to evict. `404` at `/` and at `/ws`, nothing compiled, nothing
    evicted; then `ops.localhost`, as the control, is admitted and does evict, so the
    assertions before it were not vacuous. Dropping the domain check fails it.
  * `a_declared_domain_answers_behind_a_proxy_that_moves_the_port` — the eggrd shape upgrades,
    and two near misses of a declared name, a bare label and both address families are `404`.
  * `under_localhost_the_origin_must_name_the_port_too` — an `Origin` on `:3000`, on `:1` or on
    the scheme's default port is `403`, the same port is `101`, a `Host` with no port is
    compared by name, and a trailing dot keeps the rule on. A port-blind comparison fails it.
* `crates/cli/tests/e2e.rs` — `host_answers_only_the_names_it_was_given` runs the built binary
  with `--domain example.com`: both declared names `200`, `sales.attacker.example`, `sales` and
  `127.0.0.1` `404`, and five refusal lines for six refused requests — the second name under
  `attacker.example` says nothing, and `sales.com`, under a domain nobody can declare, is not
  offered `--domain com`. A last refusal under a key of its own is waited for, so a line that
  should not have been written would already be read.
  `a_domain_that_begins_with_an_app_is_warned_about_at_start_up` holds the collision warning end
  to end.
  `a_domain_that_is_a_url_is_refused_at_start_up` — the process exits non-zero, names the fix,
  and prints no `http://`.

### What is not built

* **A `Host` check on the probe listener.** `/healthz` and `/readyz` answer any name, and
  `dagpane host` binds them on every interface by default (`--admin-bind 0.0.0.0`). A rebinding
  page aimed at that port can read `/healthz` — app names, manifest digests, byte and session
  counts, no pane data. It is a port not to publish, and on a machine where a browser runs, no
  bind keeps it from that browser.
* **A token before `GET /` compiles.** The page is where signing in starts, so it is served
  without one, and under `host` serving it opens the app — compiling it if it is not resident,
  evicting others if the budget needs the room. This decision confines that to the directory's
  apps under declared names; anyone who can reach the port and send one can still make the
  process compile and evict. A token is checked only at `/ws`. `/auth` answers any name too, and
  holds no app data.
* **A token checked against the routed app.** It is checked against the `Host`'s first label.
  Under `DirAppSource` that is the routed app by construction; a source that routed a name to a
  differently named app would make the two differ.
* **A public-suffix check.** Recognising `co.uk` or `github.io` would mean carrying the Public
  Suffix List, which changes, in a binary that would carry it stale — and it would still not
  name every zone whose DNS somebody else answers. It is documentation instead.
