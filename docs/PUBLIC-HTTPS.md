# Serving dagpane on the public internet, over HTTPS

`dagpane` speaks plain HTTP, binds loopback by default, and says so on startup when you tell
it otherwise. Putting it on the internet means putting something in front of it that
terminates TLS, absorbs abuse, and decides who gets through. This is how to do that with
[eggrd](https://github.com/lucheeseng827/eggrd), the sibling edge proxy, and what is genuinely
required rather than merely advisable.

**Every command and configuration line below was run.** The results quoted are from a real
pair — eggrd 0.4.0 terminating TLS in front of `dagpane host` — not from reading either
manual.

[`OPERATIONS.md`](../OPERATIONS.md) has the general rule and an nginx recipe. This page is
what changes when the proxy is eggrd, and the difference is not cosmetic: **eggrd cannot
forward `Host` unchanged**, which is the one thing that nginx recipe relies on.

---

## Read this first: three things that will otherwise cost you an afternoon

**1. Use `dagpane host`, not `dagpane run`.** They check the browser's `Origin` differently,
and only one of them can work behind a proxy.

| | how it decides which origins may open a socket |
|---|---|
| `dagpane run` | the address the process bound. Behind a proxy that is `0.0.0.0:8787`, which no browser will ever send — **every dashboard shows "offline"** |
| `dagpane host` | the `Host` of the request it is already answering. A page at `https://sales.example.com` may open that app's socket; anything else may not |

**2. Turn on `websocket_passthrough`.** It is off by default, and the default silently breaks
every dashboard. Measured, both ways, on the same pair:

```
websocket_passthrough = false     page 200      websocket 400
websocket_passthrough = true      page 200      websocket 101
```

The page loads either way. Without it the socket never opens and the dashboard sits at
"offline" behind an otherwise healthy proxy — with a green health check and nothing in the
logs that looks wrong.

**3. eggrd replaces the client's `Host` with the host of its own upstream URL.** It drops
the client's and lets its HTTP client set one — deliberate, and the reason
[`OPERATIONS.md`](../OPERATIONS.md)'s `proxy_set_header Host $host` has no eggrd equivalent. `dagpane host` routes on that header, so the two must agree. The fix
is one line and it is the whole trick: **make the upstream URL's hostname the public
hostname**, resolved on your internal network to the dagpane process.

```toml
# not this — dagpane then sees Host: 127.0.0.1 and answers 404 for every app
# upstream = "http://127.0.0.1:8787"

# this — dagpane sees Host: sales.example.com, routes to sales.toml, and accepts the
# browser's Origin because it matches
upstream = "http://sales.example.com:8787"
```

Give that name an internal answer: a `/etc/hosts` entry, a private DNS record, a container
alias. It never resolves publicly to the dagpane port — the only public listener is eggrd's.

**The consequence, stated plainly:** one eggrd instance in front of `dagpane host` serves
**one** app, because the upstream host is fixed. Multi-app routing by `Host` survives only
where the proxy preserves the client's `Host`; through eggrd it does not. For several apps,
run one eggrd per app, or run them on separate ports with one upstream each.

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

## Running the pair

```console
# dagpane binds loopback; only eggrd is reachable from outside the host
$ dagpane host /srv/apps --port 8787 --bind 127.0.0.1

$ eggrd --config /etc/eggrd/edgeguard.toml
EdgeGuard listening (HTTPS)  listen=0.0.0.0:443  upstream=http://sales.example.com:8787
                             auth=none rate_limit=true waf=off tls=true
```

Verified against exactly this pair:

```
page,     Host: sales.localtest.me                     200
websocket, Origin: https://sales.localtest.me          101   the upgrade survives the proxy
websocket, Origin: https://evil.example                403   the origin check still refuses
```

And the socket carries data, not just a handshake — the first frame off it:

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
$ dagpane host /srv/apps --port 8787 --bind 127.0.0.1 \
    --auth-jwks https://issuer.example.com/.well-known/jwks.json \
    --auth-issuer https://issuer.example.com \
    --auth-audience dagpane \
    --auth-authorize-url https://issuer.example.com/authorize \
    --auth-token-url https://issuer.example.com/oauth/token \
    --auth-client-id <public client id>
```

Algorithms are pinned to `RS256,ES256` by default, and `--auth-apps-claim` names the claim
that lists which apps a holder may open. The browser does the PKCE exchange; the server never
talks to the identity provider.

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

**Health checks go to the admin port.** Set `admin_port` so `/__edgeguard/health`, `/ready`
and `/metrics` leave the public listener. It has no authentication of its own, so keep it on a
trusted interface.

**Let the edge own abuse.** `[ratelimit]` is per-IP and on by default in the configuration
above; `[waf]` is off and worth turning on. `trust_forwarded_for` stays `false` unless
something trusted is in front of eggrd, or a client can spoof its own address and the limiter
counts nothing.

**Data still comes from wherever the manifest says.** A source is read by dagpane, not by the
viewer, so a database credential belongs in the dagpane process's environment and never in a
manifest. See [CUSTOMISING.md](CUSTOMISING.md) for the source forms, and
[SECURITY.md](../SECURITY.md) for what dagpane does and does not protect.

**Restarting is a deploy.** `dagpane host` recompiles an app when its file changes, so
editing a manifest on disk is the deploy; sources are read at start-up, so changing *data*
means a restart or a scheduled refresh. [RUNBOOK.md](RUNBOOK.md) is symptom-first when it is
already broken.

---

## If the dashboard says "offline"

In the order they actually happen:

| what you see | almost certainly |
|---|---|
| page loads, socket never opens | `websocket_passthrough` is still `false` |
| socket returns 403 | the browser's `Origin` does not match the `Host` dagpane received — the upstream URL's hostname is not the public one |
| every app 404s | same cause, seen from the page instead of the socket |
| 502 from the edge | dagpane is not running, or not on the port the upstream names |
| works over plain HTTP, not HTTPS | the certificate. A self-signed one is refused by strict clients; check the browser's interstitial before blaming the proxy |

`dagpane host` logs the apps it loaded and the address each is reachable at, and eggrd logs
every request with its status and outcome. The pair of logs answers all five rows above.
