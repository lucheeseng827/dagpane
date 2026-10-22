# Serving dagpane on the public internet, over HTTPS

`dagpane` speaks plain HTTP. `dagpane run` and `dagpane host` bind loopback by default and say
so on startup when you tell them otherwise; `dagpane serve`, the container shape, binds every
interface, so give it `--bind 127.0.0.1` when the proxy shares its machine. Putting it on the
internet means putting something in front of it that
terminates TLS, absorbs abuse, and decides who gets through. This is how to do that with
[eggrd](https://github.com/lucheeseng827/eggrd), the sibling edge proxy, and what is genuinely
required rather than merely advisable.

**Every command and configuration line below was run** — in September 2026, when this page
was written. The results quoted are from a real pair — eggrd 0.4.0 terminating TLS in front
of `dagpane host` — not from reading either manual. Three things changed since.
`--domain example.com` on the `dagpane host` commands was added, because `dagpane host` did not
take it then; the dagpane half was re-verified with it by sending `dagpane host` the `Host` and
`Origin` eggrd sends ([Running the pair](#running-the-pair) has both records). eggrd itself
was not re-run, and nothing on its side of the recipe changed. And the sign-in command under
[Who gets in](#who-gets-in) was corrected, because `dagpane 0.2.0` refused to start it as this
page gave it before — `--auth-jwks` takes a file, not a URL, and it named neither
`--auth-apps-claim` nor `--auth-any-app`; the corrected command was run against 0.2.0 on
2026-10-22, and that section has the record. And [One app](#one-app-dagpane-serve---origin)
was added for `dagpane serve`, which 0.2.0 introduced, its dagpane half verified the same way.

[`OPERATIONS.md`](../OPERATIONS.md) has the general rule and an nginx recipe. This page is
what changes when the proxy is eggrd, and the difference is not cosmetic: **eggrd cannot
forward `Host` unchanged**, which is the one thing that nginx recipe relies on.

---

## Read this first: three things that will otherwise cost you an afternoon

**1. Use `dagpane host` or `dagpane serve --origin`, not `dagpane run`.** They check the
browser's `Origin` differently, and `run` cannot work behind a proxy.

| | how it decides which origins may open a socket |
|---|---|
| `dagpane run` | the address the process bound. Behind a proxy that is `0.0.0.0:8787`, which no browser will ever send — **every dashboard says "Disconnected" and never comes back** |
| `dagpane host` | the `Host` of the request it is already answering. Under `--domain example.com`, a page at `https://sales.example.com` may open that app's socket; anything else may not |
| `dagpane serve` | the origins given with `--origin`, and nothing else — whatever `Host` the proxy sends. One app, so point 3 does not apply to it; [one app](#one-app-dagpane-serve---origin) below |

**2. Turn on `websocket_passthrough`.** It is off by default, and the default silently breaks
every dashboard. Measured, both ways, on the same pair:

```
websocket_passthrough = false     page 200      websocket 400
websocket_passthrough = true      page 200      websocket 101
```

The page loads either way. Without it the socket never opens and the dashboard sits at
"Disconnected. Reconnecting in …" behind an otherwise healthy proxy — with a green health
check and nothing in the logs that looks wrong.

**3. eggrd replaces the client's `Host` with the host of its own upstream URL.** It drops
the client's and lets its HTTP client set one — deliberate, and the reason
[`OPERATIONS.md`](../OPERATIONS.md)'s `proxy_set_header Host $host` has no eggrd equivalent. `dagpane host` routes on that header, so the two must agree. The fix
is one line on each side and it is the whole trick: **make the upstream URL's hostname the
public hostname**, resolved on your internal network to the dagpane process, **and put that
hostname under a `--domain`**. `dagpane host` answers `<app>.localhost` and, for each
`--domain d`, `<app>.d` — exactly one label, the app, in front of a declared domain — and every
other name gets 404 before anything is read or compiled. So
`upstream = "http://sales.example.com:8787"` needs `dagpane host … --domain example.com`.

```toml
# not this — dagpane then sees Host: 127.0.0.1, an address and not a name, and answers 404
# for every app
# upstream = "http://127.0.0.1:8787"

# this — under `dagpane host --domain example.com`, dagpane sees Host: sales.example.com,
# routes to sales.toml, and accepts the browser's Origin because it names the same host
upstream = "http://sales.example.com:8787"
```

Give that name an internal answer: a `/etc/hosts` entry, a private DNS record, a container
alias. It never resolves publicly to the dagpane port — the only public listener is eggrd's.
The ports will not agree — eggrd sends `Host: sales.example.com:8787` and the browser
`Origin: https://sales.example.com` — and they need not: under a `--domain` only the names are
compared.

**`--domain` must name a zone whose DNS you control**, because declaring one trusts every name
directly under it. Never a shared wildcard service — `localtest.me` and `lvh.me`, which answer
every name with loopback, or `nip.io` and `sslip.io`, which answer with the address the name
spells — however handy for a test, and never a public suffix such as `duckdns.org`,
`github.io` or `co.uk`: somebody else answers DNS for every name under those.
Some point a name at whatever address its user asks for, which is a DNS-rebinding attack ready
made; the loopback ones resolve to every machine's own loopback, so any other local server — a
dev server on `:3000` — serves pages under the same name, and under a `--domain` the port is
not compared. Declaring one trusts names you do not control, which is exactly what `--domain`
is the boundary against. dagpane refuses a bare top-level domain (`--domain com`) at start-up,
but it cannot tell a multi-label public suffix from a domain you own, so that part is yours.
[ADR 0012](adr/0012-a-name-is-served-only-where-it-was-declared.md) has the reasoning.

**The consequence, stated plainly:** one eggrd instance in front of `dagpane host` serves
**one** app, because the upstream host is fixed. Multi-app routing by `Host` survives only
where the proxy preserves the client's `Host`; through eggrd it does not. For several apps,
run one eggrd per app, or run them on separate ports with one upstream each.

### One app: `dagpane serve --origin`

For a single app, `dagpane serve` needs none of point 3. It never routes on `Host`, and it
compares the browser's `Origin` with the origins it was given and nothing else, so the
upstream can stay an address:

```console
$ dagpane serve /srv/apps/sales.toml --bind 127.0.0.1 --port 8787 --origin https://sales.example.com
```

```toml
upstream = "http://127.0.0.1:8787"
```

No internal DNS, no `--domain`. `--origin` replaces the allowlist derived from the bound
address rather than adding to it, so give it every public URL the app is reached at. Its probe
listener binds every interface on `:9787` for a supervisor to reach; on a host where nothing
probes it, add `--admin-bind 127.0.0.1`. Verified the way the `--domain` change was — the
dagpane half on its own, with `curl` sending `dagpane serve` the headers eggrd sends, on
2026-10-22 against 0.2.0; eggrd was not re-run for it:

```
page,      Host: 127.0.0.1:<port>                               200
websocket, Host: 127.0.0.1:<port>, Origin: https://sales.example.com  101
websocket, Host: 127.0.0.1:<port>, Origin: https://evil.example       403
websocket, Host: 127.0.0.1:<port>, Origin: http://127.0.0.1:<port>    403
```

---

## The configuration

```toml
[server]
port = 443
# The hostname here is the PUBLIC one; it must resolve internally to the dagpane process.
upstream = "http://sales.example.com:8787"
# Only if something else is already in front of eggrd. If eggrd IS the edge, leave it false
# or a client can forge its own IP and walk around the rate limiter.
trust_forwarded_for = false

[tls]
enabled = true
cert_path = "/var/lib/eggrd/edge.crt"
key_path  = "/var/lib/eggrd/edge.key"
# A public domain: let ACME issue the certificate. Port 80 must reach this process.
redirect_port = 80          # catch plaintext and upgrade it; 308 keeps method and body
redirect_hosts = ["sales.example.com"]   # the Host header is attacker-controlled; list yours

[tls.acme]
enabled = true
domains = ["sales.example.com"]
email = "ops@example.com"
accept_tos = true
# The default directory is Let's Encrypt STAGING so a first run cannot burn the production
# rate limits. Switch to production deliberately, once a staging run has succeeded.
directory_url = "https://acme-v02.api.letsencrypt.org/directory"

[validation]
websocket_passthrough = true   # THE line. Without it the dashboard never connects.

[headers]
hsts = true

[ratelimit]
enabled = true
```

For a private network or a staging box, replace `[tls.acme]` with a self-signed certificate —
it encrypts the connection, which is what makes HSTS and secure cookies mean anything, but it
proves no identity and browsers will interstitial:

```toml
[tls]
enabled = true
self_signed = true
self_signed_hosts = ["sales.internal"]
```

Generate it up front rather than on first boot if you run more than one replica, because
generation is per-process and the last writer wins:

```console
$ eggrd cert --host sales.internal --days 90 --cert-out edge.crt --key-out edge.key
```

With `sales.internal` as the upstream's hostname, start dagpane with `--domain internal`:
`internal` is reserved for private networks, so it is one of the three single labels
`--domain` accepts.

## Running the pair

```console
# dagpane serves its apps on loopback; from outside the host only eggrd reaches them
$ dagpane host /srv/apps --port 8787 --bind 127.0.0.1 --domain example.com

$ eggrd --config /etc/eggrd/edgeguard.toml
EdgeGuard listening (HTTPS)  listen=0.0.0.0:443  upstream=http://sales.example.com:8787
                             auth=none rate_limit=true waf=off tls=true
```

Verified against this pair when this page was written, in September 2026 — **before
`dagpane host` took `--domain`**, so read it as the record of that run and not of the command
above. That run named the upstream `sales.localtest.me`, a public wildcard name that resolves
to loopback, where this page writes `sales.example.com`:

```
page,     Host: sales.localtest.me                     200
websocket, Origin: https://sales.localtest.me          101   the upgrade survives the proxy
websocket, Origin: https://evil.example                403   the origin check still refuses
```

That name now gets 404, and it should: the only way to answer it is `--domain localtest.me`,
which trusts every name under a zone whose DNS somebody else answers, so it is not one to copy
(point 3 above). After `--domain` arrived the dagpane half was re-verified on its own, with
`curl` sending `dagpane host` the headers eggrd sends — `Host: sales.example.com:<port>` and
`Origin: https://sales.example.com`:

```
--domain example.com      websocket 101
no --domain               page 404
```

And the socket carries data, not just a handshake — the first frame off it, from the
September run:

```
handshake:  HTTP/1.1 101 Switching Protocols
first frame: init
app: Sales explorer · 7 panes · 2 widgets
first pass: looked at 8 of 11 cells, 7 panes sent
```

The edge adds, with no further configuration:

```
strict-transport-security: max-age=63072000; includeSubDomains
content-security-policy: default-src 'self'
x-frame-options: DENY
x-content-type-options: nosniff
referrer-policy: no-referrer
permissions-policy: geolocation=(), microphone=(), camera=()
```

`x-frame-options: DENY` is worth noticing before you need it: it stops the dashboard being
embedded in another page. If embedding it is the point, that header and the CSP are what you
change, and you should know you are doing it.

---

## Who gets in

**TLS is not authentication.** With the configuration above, everyone who can reach the
address can read every pane. Choose one of these before it is public.

### The app checks the token (recommended)

dagpane verifies a JWT itself and the page runs the sign-in. The identity decision stays with
the app that knows which app is being asked for:

```console
$ dagpane host /srv/apps --port 8787 --bind 127.0.0.1 --domain example.com \
    --auth-jwks /etc/dagpane/jwks.json \
    --auth-issuer https://issuer.example.com \
    --auth-audience dagpane \
    --auth-apps-claim dagpane_apps \
    --auth-authorize-url https://issuer.example.com/authorize \
    --auth-token-url https://issuer.example.com/oauth/token \
    --auth-client-id <public client id>
```

`--auth-jwks` is a **file**: a copy of the provider's JWKS document, because dagpane makes no
outbound request — so rotating keys is a copy and a restart. Algorithms are pinned to
`RS256,ES256` by default. `--auth-apps-claim` names the claim that lists which apps a holder
may open (`*` for all of them), and `--auth-any-app` lets any valid token open any app instead;
one of the two is required, and with neither `dagpane host` refuses to start, because both
wrong defaults would be silent ones. The browser does the PKCE exchange; the server never
talks to the identity provider.

Run against `dagpane 0.2.0` on 2026-10-22, with the paths and ports changed for the test: a
directory holding one app, `sales`; a JWKS file holding one P-256 key made for the run; and
`--port 0 --admin-port 0 --admin-bind 127.0.0.1 --drain-seconds 0`, so nothing but loopback
was bound. The first line it printed is the one to check, because it is the policy that
actually loaded:

```
dagpane: authenticated — 1 key(s), issuer https://issuer.example.com, audience dagpane; a token's `dagpane_apps` must list it
```

`/ws`, sent the `Host` and `Origin` eggrd sends, with ES256 tokens signed by that key:

```
no token                                    401
token whose dagpane_apps lists "sales"      101
token whose dagpane_apps lists "other"      403
```

Each refusal was also a line on stderr naming the app and the reason. The provider's half —
the redirect and the code exchange — was not exercised, because there was no provider in that
run: `/auth` was only seen to hand the page the sign-in settings above.

### The edge checks it

eggrd has its own `[auth]` and `[auth.jwt]`, and it refuses unauthenticated WebSocket upgrades
too. Use this when the edge already fronts several services and you want one door for all of
them — but note it cannot know which *app* a token should be allowed to open, so per-app
access still belongs in dagpane.

Whichever you pick, do not leave both off.

---

## The rest of the checklist

**Nothing but eggrd listens publicly.** `--bind 127.0.0.1` when they share a host; a private
network or firewall rule when they do not. The `--bind` warning dagpane prints is the reminder.
That goes for `dagpane host`'s probe listener too, which `--bind` does not move: it binds every
interface by default (`--admin-bind 0.0.0.0`, port 9787), the commands on this page included,
and checks no `Host`, so a page whose name rebinds to the machine can read `/healthz` there —
the resident apps' names, their manifest digests, byte and session counts, though no pane data.
Keep that port where the supervisor reaches it and no browser does; `--admin-bind 127.0.0.1`
keeps it from browsers on other machines, though not from one on this machine.

**Health checks go to the admin port.** Set `admin_port` so `/__edgeguard/health`, `/ready`
and `/metrics` leave the public listener. It has no authentication of its own, so keep it on a
trusted interface.

**Let the edge own abuse.** `[ratelimit]` is per-IP and on by default in the configuration
above; `[waf]` is off and worth turning on. `trust_forwarded_for` stays `false` unless
something trusted is in front of eggrd, or a client can spoof its own address and the limiter
counts nothing.

**Data still comes from wherever the manifest says.** A source is read by dagpane, not by the
viewer, so its credential never reaches a browser — but the binary reads no environment
variables, and a `sql` source's DSN, credential included, is in the manifest. Connect as a role
that can do nothing but `select`, and guard the manifest as you would the credential;
[OPERATIONS.md](../OPERATIONS.md) states it as a limitation. See [CUSTOMISING.md](CUSTOMISING.md) for the source forms, and
[SECURITY.md](../SECURITY.md) for what dagpane does and does not protect.

**Restarting is a deploy.** `dagpane host` recompiles an app when its file changes, so
editing a manifest on disk is the deploy; sources are read at start-up, so changing *data*
means a restart or a scheduled refresh. [RUNBOOK.md](RUNBOOK.md) is symptom-first when it is
already broken.

---

## If the dashboard says "Disconnected"

In the order they actually happen:

| what you see | almost certainly |
|---|---|
| page loads, socket never opens | `websocket_passthrough` is still `false` |
| socket returns 403 | the browser's `Origin` does not match the `Host` dagpane received — the upstream URL's hostname is not the public one. Under `dagpane serve`: the page's URL is not one given with `--origin` |
| every app 404s, and stderr says `refused Host "127.0.0.1"` | the upstream URL uses an address, and dagpane reaches an app only by name. Make its hostname the public one (point 3) |
| every app 404s, and stderr says `refused Host "sales.example.com"` | `dagpane host` was started without `--domain example.com` — or with `--domain sales.example.com`, which puts the app at `sales.sales.example.com` and is warned about at start-up |
| 502 from the edge | dagpane is not running, or not on the port the upstream names |
| works over plain HTTP, not HTTPS | the certificate. A self-signed one is refused by strict clients; check the browser's interstitial before blaming the proxy |

`dagpane host` logs the apps it loaded, the names it answers to and the address each is
reachable at, and says on stderr why it refused a name — once per domain, so look for the first
such line rather than the latest. eggrd logs every request with its status and outcome. The
pair of logs answers all six rows above.
