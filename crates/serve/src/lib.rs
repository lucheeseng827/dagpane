//! The process: a socket, a clock, and the loop that joins them to the engine.
//!
//! Everything interesting already happened in [`dagpane_app`]. This crate adds the three
//! things that cannot be tested without a machine — an address to bind, a clock to measure
//! a pass with, and a browser to send bytes to — and it adds nothing else. That is why it is
//! the smallest crate here and why the two below it have no `tokio` in their dependency
//! trees.
//!
//! # The session model, stated plainly
//!
//! **A session is a connection, and the viewer holds what makes it theirs.** Opening the
//! page creates an [`AppSession`]; closing the tab discards it. There is still no session
//! store, no eviction policy, no TTL and no reconnection token — and now there is durability
//! anyway, because a session *is* its input values and the viewer carries those: the client
//! keeps them in its URL fragment and hands them back on the socket's query string, and
//! [`AppSession::resume`] stages them before the first render so coming back costs one pass.
//!
//! That is the whole mechanism. What it buys is the part worth noticing: a viewer who lands
//! on a different replica after a deploy sees the state they left, because they brought it;
//! a filtered dashboard is a link you can send; and there is nothing to run, replicate,
//! evict or leak. A store keyed by a session id, shipped before there is any authentication
//! in front of it, would have been a way to read somebody else's session — which is the
//! hazard the next paragraph is about. See `dagpane_app::resume` for the reasoning in full.
//!
//! What that model *does* buy, and it is the part that matters: the app — the graph, the
//! compiled pipelines, and every loaded source — is one `Arc<App>` behind every connection.
//! A hundred viewers of a 600-row app are a hundred slot vectors over one table, not a
//! hundred copies of it, and `crates/core/tests/oracle.rs` asserts the pointer identity that
//! makes that true. It has now also been **weighed**, and the weighing narrows the sentence:
//! the sources are shared, and the **frames each session computes are not** — they cannot be,
//! since two viewers with two filters must have two answers. A session of the bundled example
//! costs about 8.5× the CSV it is sharing, and one whose pipeline keeps rows costs about the
//! whole table again. `benches/sessions/` is the measurement; `BENCHMARKS.md` is the number.
//!
//! # Security posture
//!
//! There is **no authentication in v0.1.0**. The server binds `127.0.0.1` by default and
//! the CLI prints a warning when told to bind anything else. See `SECURITY.md`.

#![forbid(unsafe_code)]
#![deny(missing_debug_implementations)]
#![deny(missing_docs)]

pub mod lifecycle;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::{header::HOST, header::ORIGIN, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use dagpane_app::resume::{self, Dropped};
use dagpane_app::{App, AppSession, ClientMessage, PassStats, ServerMessage};
use dagpane_auth::{bearer, AuthError, Jwks, Policy};
use dagpane_host::{normalise_host, Host, HostError};

pub use lifecycle::{Fleet, Health, Identity, Lifecycle};

/// The bundled client: one file, no build step, no package manager, and nothing third-party
/// fetched at run time.
///
/// Not "nothing fetched": the page makes two same-origin `fetch`es for the front door and
/// loads two kinds of same-origin module — a renderer script an app declared, and the wasm
/// glue in an exported bundle. `the_client_is_one_self_contained_file` enumerates all four
/// and fails on a fifth, which is the shape that rule takes now that it is not absolute.
///
/// A framework whose "hello world" starts with `npm install` has a different first five
/// minutes than one whose binary already contains the page, and the difference is most of
/// the adoption. The cost is a small hand-written renderer with no chart library, which is
/// why the chart panes are deliberately two shapes and not twenty.
/// The single-file client, as the binary carries it.
///
/// Public so `dagpane export` can write the same page a served app gets, with a config block
/// injected in front of it. Two copies of this file — one for the socket and one for a static
/// export — is how they drift, and the whole argument for the wasm build is that they do not.
pub const CLIENT: &str = include_str!("client.html");

#[derive(Debug)]
struct Shared {
    app: Arc<App>,
    /// Browser origins allowed to open the socket. Derived from the bound address.
    allowed_origins: Vec<String>,
    options: Options,
}

/// The front door, when there is one.
///
/// Holds the keys and the rules and nothing else — no client, no discovery, no cached
/// anything. See `dagpane_auth` for why this process never speaks to the identity provider.
#[derive(Debug)]
pub struct Auth {
    /// The keys, read from a file the operator supplies.
    pub jwks: Jwks,
    /// What a token has to satisfy.
    pub policy: Policy,
    /// What the bundled client needs to go and get a token, when the operator wants it to.
    ///
    /// `None` means a token arrives some other way — an OAuth2 proxy in front, a portal that
    /// hands one over — and the page says so instead of offering a sign-in that cannot work.
    pub login: Option<Login>,
}

/// The public half of an OIDC client registration: what a browser needs to run PKCE.
///
/// Every field here is public by definition — an authorize URL, a token URL and a client id
/// are printed in every provider's documentation. There is no client **secret**, because a
/// page cannot keep one, which is the entire reason PKCE exists.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Login {
    /// Where to send the viewer to sign in.
    pub authorize_endpoint: String,
    /// Where to exchange the code for a token.
    pub token_endpoint: String,
    /// This deployment's client id at the provider.
    pub client_id: String,
    /// The scopes to ask for.
    pub scope: String,
}

/// How often a silent connection is pinged, when nothing says otherwise.
///
/// Comfortably under the 60 s a load balancer idles a connection out at by default — an AWS
/// ALB, an nginx `proxy_read_timeout`, and most ingress controllers all land on that number —
/// with room for one tick to be missed and the next still to arrive in time.
///
/// **The case this exists for is a viewer who is doing nothing**, which is the ordinary state
/// of a dashboard. Sixty seconds of no clicks and no new data is not an idle connection to be
/// reclaimed; it is somebody reading. Without a ping the proxy reaps it, and the page goes
/// quiet with no error anywhere.
pub const HEARTBEAT: Duration = Duration::from_secs(25);

/// How to serve, beyond the app itself.
#[derive(Debug)]
pub struct Options {
    /// The app's name, for a token's per-app claim. `dagpane run` passes the manifest's file
    /// stem, so `sales.toml` is the app `sales` — the same rule `dagpane host` routes by, so
    /// one claim works in both modes.
    pub app_name: String,
    /// The front door. `None` leaves the socket open to anyone who can reach it.
    pub auth: Option<Arc<Auth>>,
    /// The public origins a browser may open the socket from.
    ///
    /// Empty — which is what `dagpane run` passes — derives the allowlist from the bound
    /// address, which is right for a laptop and wrong for everything else: a process bound
    /// `0.0.0.0:8787` allows exactly `http(s)://0.0.0.0:8787`, an origin no browser ever
    /// sends, so **every** containerised deployment is refused at the upgrade even over plain
    /// `localhost`. The symptom is a page that loads and then does nothing, with nothing in
    /// the log, and `OPERATIONS.md` spends a section on it.
    ///
    /// Non-empty replaces the derived list outright. These are exact matches against the
    /// string the browser sends — scheme, host and port, no path and no trailing slash — and
    /// they are the names you publish the app at, not the address it bound.
    pub origins: Vec<String>,
    /// How often to ping a connection that has gone quiet. [`HEARTBEAT`] by default.
    ///
    /// `None` sends none, which is what a deployment with nothing in front of it can afford
    /// and what a test that does not want a timer asks for.
    pub heartbeat: Option<Duration>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            app_name: String::new(),
            auth: None,
            origins: Vec::new(),
            // On, because the failure it prevents is silent. A proxy that reaps an idle
            // socket does not log anything a viewer can see, and the page it leaves behind
            // renders perfectly and answers nothing.
            heartbeat: Some(HEARTBEAT),
        }
    }
}

/// Serve `app` on `addr` until the process is asked to stop.
pub async fn serve(app: Arc<App>, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_on(app, listener).await
}

/// [`serve`], with a front door and an app name.
pub async fn serve_with(app: Arc<App>, addr: SocketAddr, options: Options) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_on_with(app, listener, options).await
}

/// Serve `app` on a listener the caller already bound.
///
/// This exists for tests. Binding port 0 to find a free port and then dropping the listener
/// so `serve` can bind the same address again is a race: between the two binds, anything else
/// on the machine — including another test in the same run — can take the port. Handing the
/// bound listener straight over closes the window.
pub async fn serve_on(app: Arc<App>, listener: tokio::net::TcpListener) -> std::io::Result<()> {
    serve_on_with(app, listener, Options::default()).await
}

/// [`serve_on`], with a front door and an app name.
pub async fn serve_on_with(
    app: Arc<App>,
    listener: tokio::net::TcpListener,
    options: Options,
) -> std::io::Result<()> {
    let addr = listener.local_addr()?;
    let router = app_router(app, addr, options);
    axum::serve(listener, router)
        .with_graceful_shutdown(lifecycle::stop_signal())
        .await
}

/// The app's four endpoints, and no others.
///
/// Factored out because [`serve_replica`] serves the same four — a cluster is a deployment
/// shape, not a different app, and two routers would be two things to keep identical.
fn app_router(app: Arc<App>, addr: SocketAddr, options: Options) -> Router {
    let allowed_origins = if options.origins.is_empty() {
        origins_for(addr)
    } else {
        options.origins.clone()
    };
    let shared = Arc::new(Shared {
        app,
        allowed_origins,
        options,
    });
    Router::new()
        .route("/", get(page))
        // Unauthenticated on purpose, and it carries nothing: it says whether a token is
        // needed and where to go and get one, all of which is public. The page has to be
        // reachable without one or there is nowhere for a viewer to sign in FROM.
        .route("/auth", get(auth_config))
        .route("/ws", get(socket))
        // The app's declared renderer scripts, served from memory. See `renderer_script`.
        .route("/{*path}", get(renderer_script))
        .with_state(shared)
}

/// Serve `app` as one replica of however many, with probes on a listener of their own.
///
/// The difference from [`serve_on_with`] is entirely about being one of several: the app
/// listener serves the same four endpoints, and the second listener answers the two questions
/// a supervisor asks — `/healthz` and `/readyz`. See [`lifecycle`] for why the drain is a
/// sequence, why liveness stays green while readiness goes red, and why the probes outlive
/// the app listener rather than dying with the signal that started the drain.
///
/// Returns when both listeners have closed.
pub async fn serve_replica(
    app: Arc<App>,
    listener: tokio::net::TcpListener,
    admin: tokio::net::TcpListener,
    options: Options,
    lifecycle: Arc<Lifecycle>,
) -> std::io::Result<()> {
    let addr = listener.local_addr()?;
    let router = app_router(app, addr, options);

    let serving = {
        let shutdown = Arc::clone(&lifecycle);
        let done = Arc::clone(&lifecycle);
        async move {
            let result = axum::serve(listener, router)
                .with_graceful_shutdown(async move { shutdown.on_signal().await })
                .await;
            // Whatever happened to the app listener, it is no longer accepting — so say so
            // before returning, which is also what lets the probe listener stop.
            done.stopped();
            result
        }
    };

    let probing = axum::serve(admin, lifecycle::admin_router(Arc::clone(&lifecycle)))
        .with_graceful_shutdown(async move { lifecycle.until_stopped().await });

    // `try_join!` and not `join!`: if the probe listener dies, this replica can no longer tell
    // a supervisor anything, and a replica a balancer cannot health-check is one it should
    // stop routing to. Failing out takes the app listener with it, which is the outcome worth
    // having — the alternative is a process serving traffic that nothing can drain.
    tokio::try_join!(serving, probing).map(|_| ())
}

/// What the page needs to know before it has a token.
async fn auth_config(State(shared): State<Arc<Shared>>) -> Response {
    axum::Json(describe_auth(shared.options.auth.as_deref())).into_response()
}

async fn host_auth_config(State(state): State<Arc<HostState>>) -> Response {
    axum::Json(describe_auth(state.auth.as_deref())).into_response()
}

fn describe_auth(auth: Option<&Auth>) -> serde_json::Value {
    match auth {
        None => serde_json::json!({ "required": false }),
        Some(auth) => serde_json::json!({
            "required": true,
            // Absent rather than null when the operator did not configure a browser flow:
            // the page then says a token must arrive another way, instead of offering a
            // sign-in button that cannot work.
            "login": auth.login,
        }),
    }
}

/// Check the front door, or wave the request through when there is none.
///
/// Returns the viewer on success. The refusal is already an HTTP response: `401` or `403`,
/// with [`AuthError::public`]'s deliberately coarse message, and the detail on stderr where
/// the operator is rather than in the body where a prober is.
fn admit(
    auth: Option<&Auth>,
    app_name: &str,
    headers: &HeaderMap,
) -> Result<Option<dagpane_auth::Viewer>, Box<Response>> {
    let Some(auth) = auth else {
        return Ok(None);
    };

    let authorization = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let protocols = headers
        .get(axum::http::header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok());

    // Boxed: an axum `Response` is large, and this is the cold half of a `Result` whose hot
    // half is a `Viewer` on every accepted connection.
    let refuse = |e: AuthError| -> Box<Response> {
        eprintln!("dagpane: refused a connection to {app_name:?}: {e}");
        let status = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::UNAUTHORIZED);
        Box::new((status, e.public()).into_response())
    };

    let token = bearer(authorization, protocols).map_err(refuse)?;
    auth.policy
        .admit(&auth.jwks, token, app_name)
        .map(Some)
        .map_err(refuse)
}

/// The constant subprotocol a browser offers beside its token, and the one the server
/// selects.
///
/// A browser cannot set headers on `new WebSocket()`, so the token rides in
/// `Sec-WebSocket-Protocol`. The client offers two entries — this one and
/// `dagpane.auth.<jwt>` — and the server answers with this one, so the handshake completes
/// without echoing the viewer's own credential back in a response header.
const SUBPROTOCOL: &str = "dagpane";

/// Serve every app a [`Host`] can resolve, on one port, routed by the `Host` header.
///
/// The multi-app front door. Each request's header decides which app it gets; the host
/// compiles on a miss, shares one `Arc<App>` per key, and evicts against its own budget.
/// Nothing about a session changes — it is still per connection, and two viewers of two
/// different apps on this port share nothing but the socket the process is listening on.
pub async fn serve_host(host: Arc<Host>, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_host_on(host, listener).await
}

/// [`serve_host`], with a front door.
///
/// The app name a token is checked against is the one the `Host` header routed to, so one
/// `dagpane_apps` claim governs a whole fleet without the fleet having to be enumerated
/// anywhere in this process.
pub async fn serve_host_with(
    host: Arc<Host>,
    addr: SocketAddr,
    auth: Option<Arc<Auth>>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_host_on_with(host, listener, auth).await
}

/// What a hosting front door holds: the fleet, and the rules for reaching it.
#[derive(Debug)]
struct HostState {
    host: Arc<Host>,
    auth: Option<Arc<Auth>>,
    /// How often to ping a quiet connection. See [`Options::heartbeat`] — a fleet behind one
    /// port is if anything more likely to sit behind a proxy than a single app is.
    heartbeat: Option<Duration>,
}

/// [`serve_host`] on a listener the caller already bound. Exists for the same reason
/// [`serve_on`] does.
pub async fn serve_host_on(
    host: Arc<Host>,
    listener: tokio::net::TcpListener,
) -> std::io::Result<()> {
    serve_host_on_with(host, listener, None).await
}

/// [`serve_host_on`], with a front door.
pub async fn serve_host_on_with(
    host: Arc<Host>,
    listener: tokio::net::TcpListener,
    auth: Option<Arc<Auth>>,
) -> std::io::Result<()> {
    axum::serve(listener, host_router(host, auth, Some(HEARTBEAT)))
        .with_graceful_shutdown(lifecycle::stop_signal())
        .await
}

/// The hosting front door's three endpoints. Factored for the same reason [`app_router`] is:
/// [`serve_host_replica`] serves exactly these, and two routers would be two things to keep
/// identical.
fn host_router(host: Arc<Host>, auth: Option<Arc<Auth>>, heartbeat: Option<Duration>) -> Router {
    Router::new()
        .route("/", get(host_page))
        .route("/auth", get(host_auth_config))
        .route("/ws", get(host_socket))
        .with_state(Arc::new(HostState {
            host,
            auth,
            heartbeat,
        }))
}

/// Serve a fleet as one replica of however many, with probes on a listener of their own.
///
/// [`serve_replica`] for [`serve_host_on_with`], and the drain is identical because the thing
/// being drained is identical: connections, which belong to no app in particular.
///
/// **What readiness means here is the one decision that is not identical**, and it is worth
/// stating because the tempting answer is wrong. A hosting process is ready when it can
/// compile and serve — **not** when every app it might be asked for is healthy. A fleet of
/// four hundred manifests where one is broken is not an unready replica: that app's viewers
/// get its error and the other three hundred and ninety-nine are fine. Failing readiness on it
/// would pull a working replica out of the pool to report a fault that no replica would be
/// without, and every other replica would fail the same probe for the same reason, which takes
/// the whole service down to report one broken file.
///
/// So the states are [`State`] and nothing else, exactly as for one app. `/healthz` reports
/// residency so an operator can *see* the fleet; readiness does not depend on it.
pub async fn serve_host_replica(
    host: Arc<Host>,
    listener: tokio::net::TcpListener,
    admin: tokio::net::TcpListener,
    auth: Option<Arc<Auth>>,
    heartbeat: Option<Duration>,
    lifecycle: Arc<Lifecycle>,
) -> std::io::Result<()> {
    let router = host_router(host, auth, heartbeat);

    let serving = {
        let shutdown = Arc::clone(&lifecycle);
        let done = Arc::clone(&lifecycle);
        async move {
            let result = axum::serve(listener, router)
                .with_graceful_shutdown(async move { shutdown.on_signal().await })
                .await;
            done.stopped();
            result
        }
    };

    let probing = axum::serve(admin, lifecycle::admin_router(Arc::clone(&lifecycle)))
        .with_graceful_shutdown(async move { lifecycle.until_stopped().await });

    tokio::try_join!(serving, probing).map(|_| ())
}

/// Drop apps nobody has opened, on a timer, until the process stops.
///
/// [`Host::sweep_idle`] has existed since `crates/host` did and **nothing in the shipped
/// binary has ever called it**, so `dagpane host --idle-minutes` configured a sweep that never
/// ran: an app left residency on a redeploy or under budget pressure and never for being idle.
/// `OPERATIONS.md` has said so in its own gaps list. This is the caller.
///
/// It lives in this crate rather than in `dagpane-host` because it needs a clock, and that
/// crate is one of the ones that has none — which is what lets it be tested without one.
///
/// Every eviction is **printed**, which closes the other half of the same gap: the eviction
/// log is in memory and bounded, and until now nothing read it, so an app disappearing from
/// residency was invisible from outside except as a slower next request.
pub async fn sweep_idle(host: Arc<Host>, every: Duration, lifecycle: Arc<Lifecycle>) {
    let mut timer = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = timer.tick() => {
                for gone in host.sweep_idle() {
                    // `AppKey`'s Display is `sales@1f4a…`: the app and enough of the
                    // manifest digest to tell two deployments of it apart in a log line.
                    println!(
                        "dagpane: evicted {} — {:?} after {} s idle, {} byte(s) freed",
                        gone.key,
                        gone.reason,
                        gone.idle_ms / 1000,
                        gone.bytes
                    );
                }
            }
            // Nothing should be evicted for idleness while the process is on its way out: the
            // work is pointless and the log line would be noise in the middle of a drain.
            _ = lifecycle.until_stopped() => return,
        }
    }
}

async fn page() -> impl IntoResponse {
    Html(CLIENT)
}

/// One of the app's `[app] renderers` scripts.
///
/// **A lookup, never a file read.** `compile` read these when the app was built, so what this
/// does is match the requested path against the list the manifest declared and hand back the
/// bytes already in memory. A path that is not on that list is a 404 — there is no filesystem
/// call here for a request to steer, which is why this route can exist at all next to a
/// server whose whole job is to be safe to point at a directory.
///
/// Served at the app's root rather than under a prefix, so that the path a manifest writes,
/// the URL a browser fetches and the filename a static export produces are one string. A
/// prefix would mean the same script is imported by two different names depending on how the
/// app is deployed, and a renderer that works on a server and 404s in an export is exactly
/// the kind of difference this crate's transport seam exists to remove.
async fn renderer_script(
    State(shared): State<Arc<Shared>>,
    UrlPath(path): UrlPath<String>,
) -> Response {
    match shared.app.renderers.iter().find(|r| r.path == path) {
        Some(r) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/javascript; charset=utf-8",
            )],
            r.source.clone(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// The name the saved state travels under on the socket's query string.
const RESUME_PARAM: &str = "s";

/// What a viewer brought back with them, and what to tell them if it could not be read.
///
/// The state rides on the **WebSocket's** query string rather than the page's, and the two
/// are not the same place: the page is a static file that every viewer of every app receives
/// identically, and giving it a per-viewer query would make it uncacheable for nothing. The
/// client reads its own URL fragment — which the server never sees — and puts it here.
fn parse_resume(query: Option<&str>) -> (BTreeMap<String, dagpane_core::Value>, Option<String>) {
    let Some(query) = query else {
        return (BTreeMap::new(), None);
    };
    let Some(raw) = query.split('&').find_map(|pair| {
        pair.strip_prefix(RESUME_PARAM)
            .and_then(|rest| rest.strip_prefix('='))
    }) else {
        return (BTreeMap::new(), None);
    };

    let decoded = percent_decode(raw);
    match resume::decode(&decoded) {
        Ok(values) => (values, None),
        // A state that cannot be READ AT ALL is different from one whose inputs no longer
        // exist: nothing can be applied, so the session starts at the defaults and the viewer
        // is told why rather than left to wonder why their filters vanished.
        Err(e) => (BTreeMap::new(), Some(e.to_string())),
    }
}

/// `%XX` and `+`, which is all a query string can carry.
///
/// Hand-written rather than taken from a crate: this decodes one parameter whose contents are
/// then parsed as JSON and validated against the app, so a malformed escape can produce
/// nothing worse than a resume that does not apply. Bytes that do not form UTF-8 are replaced
/// rather than rejected, for the same reason — the JSON parse is the gate.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                }
                Err(_) => {
                    out.push(b'%');
                    i += 1;
                }
            },
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The header a request is routed by, normalised the way the host expects it.
fn requested_host(headers: &HeaderMap) -> String {
    headers
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .map(normalise_host)
        .unwrap_or_default()
}

/// The page, but only for a host header that resolves to an app.
///
/// Serving the client shell to a name with no app behind it would give a viewer a page whose
/// socket then fails with no explanation. The refusal belongs at the first request.
async fn host_page(headers: HeaderMap, State(state): State<Arc<HostState>>) -> Response {
    let name = requested_host(&headers);
    match state.host.open(&name) {
        Ok(_) => Html(CLIENT).into_response(),
        Err(e) => host_error(&name, &e),
    }
}

/// Why a request could not be served, in terms a person reading a browser tab can act on.
///
/// Deliberately terse about the ones that are the operator's problem rather than the
/// viewer's: a manifest's compile error names cells in an app the viewer cannot see, and an
/// app's byte footprint is nobody's business at the front door. Both are logged in full and
/// summarised here.
fn host_error(name: &str, e: &HostError) -> Response {
    match e {
        HostError::UnknownHost(_) | HostError::NotFound(_) => {
            (StatusCode::NOT_FOUND, format!("no app is served at {name}")).into_response()
        }
        HostError::Manifest { key, reason } => {
            eprintln!("dagpane: {key} did not compile: {reason}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("the app at {name} did not compile; the server log says why"),
            )
                .into_response()
        }
        HostError::DigestMismatch { key, found } => {
            eprintln!("dagpane: {key} was asked for, the source returned {found:?}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("the app at {name} changed while it was being loaded; try again"),
            )
                .into_response()
        }
        HostError::OverBudget { key, bytes, budget } => {
            eprintln!("dagpane: {key} holds {bytes} bytes and the budget is {budget}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("the app at {name} is too large for this server"),
            )
                .into_response()
        }
    }
}

/// The upgrade, in multi-app mode.
///
/// The `Origin` check generalises rather than changes: in single-app mode the one allowed
/// origin is the address the process bound, and here it is **the host the request is already
/// asking for**. A page at `https://sales.example.com` may open this app's socket; a page at
/// `https://anything.else` may not, whichever of them the process happens to be behind. That
/// is the same rule — open it at its own address — stated for a server that has more than one.
async fn host_socket(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    State(state): State<Arc<HostState>>,
) -> Response {
    let name = requested_host(&headers);

    match headers.get(ORIGIN).and_then(|v| v.to_str().ok()) {
        // Absent for a non-browser client, which is not the attack this defends against —
        // a page cannot suppress the header. Same reasoning as the single-app path.
        None => {}
        Some(origin) if origin_matches(origin, &name) => {}
        Some(origin) => {
            return (
                StatusCode::FORBIDDEN,
                format!(
                    "this app does not accept WebSocket connections from {origin}; \
                     open it at its own address"
                ),
            )
                .into_response()
        }
    }

    // The front door BEFORE the app is opened. Compiling somebody else's app for a caller
    // who may not see it would hand them a timing signal about which apps exist, and would
    // let an unauthenticated request spend a core on a manifest.
    let app_name = name.split('.').next().unwrap_or(&name).to_string();
    if let Err(refusal) = admit(state.auth.as_deref(), &app_name, &headers) {
        return *refusal;
    }

    let app = match state.host.open(&name) {
        Ok(app) => app,
        Err(e) => return host_error(&name, &e),
    };
    let (values, unreadable) = parse_resume(query.as_deref());
    let ws = if state.auth.is_some() {
        ws.protocols([SUBPROTOCOL])
    } else {
        ws
    };
    let heartbeat = state.heartbeat;
    ws.on_upgrade(move |socket| connection(socket, app, values, unreadable, heartbeat))
}

/// Does this `Origin` name the host the request is for?
///
/// Compared against the raw header rather than against a list built from the bound address:
/// with many apps on one port there is no single address to build one from, and the port in
/// an `Origin` is whatever the browser was given — which is the proxy's port, not this
/// process's. `Host` carries the same value, so comparing the two is the check that stays
/// true behind a proxy.
fn origin_matches(origin: &str, name: &str) -> bool {
    for scheme in ["http://", "https://"] {
        if let Some(rest) = origin.strip_prefix(scheme) {
            if normalise_host(rest) == name {
                return true;
            }
        }
    }
    false
}

/// The origins a browser may open this socket from: exactly the address it is served on.
///
/// `http://` and `https://` both, because the process cannot know whether something in front
/// of it terminates TLS, and `localhost` alongside a loopback IP because a browser sends
/// whichever the user typed.
fn origins_for(addr: SocketAddr) -> Vec<String> {
    let mut hosts = vec![addr.to_string()];
    if addr.ip().is_loopback() {
        hosts.push(format!("localhost:{}", addr.port()));
    }
    hosts
        .iter()
        .flat_map(|h| [format!("http://{h}"), format!("https://{h}")])
        .collect()
}

/// The upgrade, with an `Origin` check.
///
/// **Why this exists even though the server binds loopback.** A WebSocket upgrade is not
/// subject to the same-origin policy and gets no preflight, so without this check any page a
/// viewer happens to have open in the same browser could connect to `ws://127.0.0.1:8787/ws`,
/// receive `init`, and read every pane of their app. "Binds loopback" then does not mean what
/// a reader assumes it means. Ten lines make it mean it.
///
/// A request with **no** `Origin` header is allowed: that is a non-browser client (`curl`, the
/// integration tests, a script), which is not the attack this defends against — a page cannot
/// suppress the header. This is an anti-CSRF measure and **not** authentication: the two
/// answer different questions — *which page asked* and *who is this* — and the second one is
/// [`admit`], which runs next and does nothing unless an operator configured a door.
async fn socket(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    State(shared): State<Arc<Shared>>,
) -> Response {
    match headers.get(ORIGIN).and_then(|v| v.to_str().ok()) {
        None => {}
        Some(origin) if shared.allowed_origins.iter().any(|a| a == origin) => {}
        Some(origin) => {
            return (
                StatusCode::FORBIDDEN,
                format!(
                    "this app does not accept WebSocket connections from {origin}; \
                     open it at its own address"
                ),
            )
                .into_response()
        }
    }
    if let Err(refusal) = admit(
        shared.options.auth.as_deref(),
        &shared.options.app_name,
        &headers,
    ) {
        return *refusal;
    }

    let (values, unreadable) = parse_resume(query.as_deref());
    let ws = if shared.options.auth.is_some() {
        ws.protocols([SUBPROTOCOL])
    } else {
        ws
    };
    let heartbeat = shared.options.heartbeat;
    ws.on_upgrade(move |socket| {
        connection(
            socket,
            Arc::clone(&shared.app),
            values,
            unreadable,
            heartbeat,
        )
    })
}

/// One viewer, start to finish.
///
/// Deliberately sequential: a message is read, a pass runs, a patch goes out, and only then
/// is the next message read. A data app's interactions are a person moving a control, so
/// concurrency inside one connection would buy nothing and would cost the guarantee that
/// the viewer's controls and their page describe the same epoch.
async fn connection(
    mut socket: WebSocket,
    app: Arc<App>,
    values: BTreeMap<String, dagpane_core::Value>,
    unreadable: Option<String>,
    heartbeat: Option<Duration>,
) {
    let started = Instant::now();
    // ONE pass. The restored values are staged before the first render, so a viewer who comes
    // back gets their state computed once — not the app's defaults rendered and then
    // corrected, which is two passes and a visible flicker.
    // THE WHOLE GRAPH, even for a placed app, and deliberately so until the page can hold the
    // other half. `AppSession::resume_side` would give this process the server half in one
    // word — and a browser that cannot consume a frontier would then render an app with its
    // page-side panes simply missing, which is a worse answer than ignoring the cut. So the
    // switch stays unflipped and `dagpane check` keeps saying so. `ROADMAP.md` §4 has what is
    // left before it can move.
    let (mut session, first, mut dropped) = AppSession::resume(app, &values);
    if let Some(reason) = unreadable {
        // The whole state was unreadable rather than one input of it. Reported through the
        // same channel, because from the viewer's side it is the same event: something they
        // had saved is not in effect.
        dropped.push(Dropped {
            input: String::new(),
            reason,
        });
    }
    let init =
        session.init_message_with(&first, Some(started.elapsed().as_micros() as u64), dropped);
    if send(&mut socket, &init).await.is_err() {
        return;
    }

    // The first tick is one interval away rather than immediately: a connection that has just
    // sent its opening frame is the least idle it will ever be.
    let mut beat = heartbeat.map(|every| {
        let mut timer = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        // A pass holds this task while it runs, so a tick can be late. Delay rather than
        // Burst: catching up would send a handful of pings back to back to prove liveness
        // that the pass itself just proved.
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        timer
    });

    loop {
        // `recv` is a `Stream::next`, whose state lives in the stream rather than the future,
        // so losing this branch to the timer drops nothing that had been read.
        let incoming = match beat.as_mut() {
            Some(timer) => tokio::select! {
                incoming = socket.recv() => incoming,
                _ = timer.tick() => {
                    // A WebSocket Ping, not a message of ours. The browser answers it in the
                    // transport with no JavaScript involved, which is the whole reason this
                    // needs no client-side counterpart — and a browser cannot send a Ping
                    // from script anyway, so the server is the only end that can start one.
                    if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                    continue;
                }
            },
            None => socket.recv().await,
        };

        let Some(Ok(message)) = incoming else { break };
        let text = match message {
            Message::Text(t) => t,
            Message::Close(_) => break,
            // Ping and Pong are answered by the transport; a data app has no use for binary
            // frames, and a client that sends one is not this client.
            _ => continue,
        };

        let incoming: ClientMessage = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                let _ = send(
                    &mut socket,
                    &ServerMessage::Rejected {
                        seq: 0,
                        message: format!("could not read that message: {e}"),
                    },
                )
                .await;
                continue;
            }
        };

        let reply = match incoming {
            ClientMessage::Set { seq, values } => apply(&mut session, seq, &values),
            ClientMessage::Refresh { seq } => {
                // A refresh runs no pass: it re-renders what the session already holds. The
                // stats therefore carry no counts, and reporting zeroes beside "panes sent 7"
                // read as though the engine had done nothing to produce seven panes. `visited`
                // and the rest stay at zero because that is true — nothing was visited — and
                // the client is told this is a refresh so it can say so rather than imply it.
                let panes = session.full_views();
                let stats = PassStats {
                    epoch: session.session().epoch(),
                    total_cells: session.app().graph.len(),
                    ..PassStats::default()
                };
                ServerMessage::Refreshed { seq, panes, stats }
            }
        };

        if send(&mut socket, &reply).await.is_err() {
            break;
        }
    }
}

fn apply(
    session: &mut AppSession,
    seq: u64,
    values: &BTreeMap<String, dagpane_core::Value>,
) -> ServerMessage {
    if let Err(message) = session.set(values) {
        // A rejected batch is not an error state for the app: nothing was staged, the page
        // is still correct, and the client is told which control it got wrong.
        return ServerMessage::Rejected { seq, message };
    }
    // The clock lives here and nowhere else. `dagpane-core` cannot read one, which is what
    // makes its traces byte-identical under a test harness.
    let started = Instant::now();
    let epoch = session.session().epoch() + 1;
    let (trace, panes) = session.commit();
    let micros = started.elapsed().as_micros() as u64;
    // Only what moved on the frontier. Empty for an unsplit app — so a patch on an app that
    // declared no placement is byte-identical to the one it sent before cuts existed — and
    // empty today for a placed one too, because the session above is undivided. Written here
    // rather than added later so that flipping that one line is the whole change.
    let frontier = dagpane_app::BoundaryValue::of(&session.frontier(epoch));
    ServerMessage::Patch {
        seq,
        panes,
        frontier,
        stats: PassStats::from_trace(&trace).with_micros(micros),
    }
}

async fn send(socket: &mut WebSocket, message: &ServerMessage) -> Result<(), ()> {
    let json = serde_json::to_string(message).map_err(|_| ())?;
    socket
        .send(Message::Text(json.into()))
        .await
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_is_one_self_contained_file() {
        // The claim is "no build step, and nothing this page needs in order to render comes
        // off a network". A `<script src=`, a stylesheet `href`, an `@import` or an
        // `importScripts` would quietly break it, and an app served on an air-gapped host
        // would then render blank.
        //
        // `fetch` used to be on that list and no longer is, because the front door needs two
        // — so the rule is now "these two and no others" rather than "none", in the same
        // spelled-out style as the URLs below. Neither is loading CODE and neither can blank
        // the page:
        //
        //   * `/auth` is same-origin, to the server that just served this file, and a
        //     failure falls through to connecting exactly as this page did before a door
        //     existed;
        //   * the provider's token endpoint is reached only when an operator has configured
        //     a browser sign-in, which an air-gapped deployment has not.
        //
        // A third `fetch` appearing here fails this test, which is the point of listing them.
        assert!(CLIENT.to_ascii_lowercase().contains("<!doctype html>"));
        for forbidden in [
            "script src=",
            "@import",
            "//cdn.",
            "//unpkg",
            "importScripts",
        ] {
            assert!(
                !CLIENT.contains(forbidden),
                "the client must not contain `{forbidden}`"
            );
        }
        let fetches: Vec<&str> = CLIENT
            .match_indices("fetch(")
            .map(|(i, _)| CLIENT[i..].split([',', ')']).next().unwrap_or(""))
            .collect();
        assert_eq!(
            fetches,
            // Source order: `finishSignIn` is defined above `frontDoor`.
            vec!["fetch(login.token_endpoint", "fetch(\"/auth\""],
            "the client gained a fetch it did not have"
        );
        let urls: Vec<&str> = CLIENT
            .match_indices("http")
            .map(|(i, _)| {
                CLIENT[i..]
                    .split(['"', '\'', ' ', '\n'])
                    .next()
                    .unwrap_or("")
            })
            .collect();
        assert_eq!(
            urls,
            // Source order. The first two are PROSE, not URLs: the `file:` diagnostic names
            // the scheme the page needs and the command that provides it, because a message
            // that says "serve this properly" without saying how is a message that sends the
            // reader to a search engine. Nothing fetches them, and they are enumerated here
            // rather than reworded because this test is the only thing standing between a
            // future edit and a CDN, and a scan loose enough to catch a CDN is loose enough
            // to catch a sentence.
            vec![
                "http://",
                "http.server`,",
                "https:",
                "http://www.w3.org/2000/svg"
            ],
            "the client gained a URL it did not have"
        );

        // The page now loads CODE it did not ship with: an app's `[app] renderers` scripts,
        // and the wasm glue when it is running locally. That is a real weakening of "nothing
        // this page needs comes off a network", so it is spelled out rather than waved at.
        //
        // Both imports are same-origin by construction and neither can come from a CDN:
        //
        //   * a renderer path is resolved against `document.baseURI`, and
        //     `manifest::compile_renderers` has already refused anything absolute, anything
        //     with a `..` in it, and anything containing a `:` — so it cannot name a host;
        //   * the glue is `./dagpane.js` beside an exported page, reached only when a page
        //     carries `DAGPANE_LOCAL`, which only `dagpane export` writes.
        //
        // A renderer that fails to load takes out its own pane and nothing else, so neither
        // import can blank the page. A third one appearing here fails this test.
        let imports: Vec<&str> = CLIENT
            .match_indices("import(")
            .map(|(i, _)| CLIENT[i..].split([')', '\n']).next().unwrap_or(""))
            .collect();
        assert_eq!(
            imports,
            vec![
                "import(spec.glue || \"./dagpane.js\"",
                "import(new URL(path, document.baseURI"
            ],
            "the client gained an import it did not have"
        );
    }

    #[test]
    fn the_client_never_writes_server_text_as_markup() {
        // Every value on the page came from a manifest or from data, and both are things a
        // deployer might not fully control. `textContent` everywhere means a column called
        // `<img onerror=…>` is a column name, not a script.
        //
        // This assertion used to be `!contains(".innerHTML = ") || contains("escapeHtml")`,
        // which the mere presence of the helper satisfied — a newly added unescaped
        // assignment could not fail it. It now checks every call SITE, so adding one does.
        // The client now builds every node with `createElement`/`textContent` and touches
        // `.innerHTML` nowhere at all, so this is an absence check rather than an escaping
        // check — which is the version that can fail when somebody adds one.
        for (n, line) in CLIENT.lines().enumerate() {
            assert!(
                !line.contains(".innerHTML"),
                "line {} touches innerHTML; build nodes instead: {}",
                n + 1,
                line.trim()
            );
        }
        assert!(
            !CLIENT.contains("insertAdjacentHTML") && !CLIENT.contains("document.write"),
            "the client builds nodes, it does not paste markup"
        );
    }
}
