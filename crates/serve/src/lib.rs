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
//! Authentication is **off unless a front door is configured** ([`Options::auth`]). `run`
//! binds `127.0.0.1` by default, and the CLI prints a warning when told to bind anything else
//! with no front door. See `SECURITY.md`.

#![forbid(unsafe_code)]
#![deny(missing_debug_implementations)]
#![deny(missing_docs)]

pub mod lifecycle;
mod pings;
mod refresh;

use std::collections::{BTreeMap, HashSet};
use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant, SystemTime};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::{header::HOST, header::ORIGIN, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use dagpane_app::resume::{self, Dropped};
use dagpane_app::{App, AppSession, ClientMessage, PassStats, ServerMessage};
use dagpane_auth::{bearer, AuthError, Jwks, Policy};
use dagpane_host::{normalise_host, Domain, Host, HostError, SessionLease};
use pings::Pings;
use refresh::{Feed, Generation, Opening, Schedules, Scope};

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
    /// The app, as its newest generation — held by the schedule rather than here, so that the
    /// rows from before a refresh are not kept alive for the life of the process by the router.
    schedules: Arc<Schedules>,
    scope: Scope,
    /// Browser origins allowed to open the socket: [`Options::origins`] when any were given
    /// (`dagpane serve --origin`), and otherwise derived from the bound address.
    allowed_origins: Vec<String>,
    options: Options,
    /// The sockets this router has upgraded, so that stopping can close them.
    connections: Connections,
}

impl Shared {
    /// The app over the newest rows read.
    fn app(&self) -> Arc<App> {
        let generation = self
            .schedules
            .current(&self.scope)
            .expect("`run` and `serve` keep their app's schedule for the life of the process");
        Arc::clone(&generation.app)
    }
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
///
/// The same pings are what [`PING_TIMEOUT`] asks a client to answer, so with the heartbeat off
/// there is nothing to answer and that deadline never runs.
pub const HEARTBEAT: Duration = Duration::from_secs(25);

/// The longest interval a heartbeat is scheduled at: a hundred years, which is never in
/// practice and still far enough from the end of the clock that no arithmetic on it overflows.
const LONGEST_HEARTBEAT: Duration = Duration::from_secs(100 * 365 * 24 * 60 * 60);

/// The shortest interval a heartbeat is scheduled at: a millisecond, the grain of the runtime's
/// timers. A connection pings before it reads when a beat is due, so a shorter one could have it
/// ping between every two frames it reads, and meet each answer to a ping with another ping.
const SHORTEST_HEARTBEAT: Duration = Duration::from_millis(1);

/// How long one frame may take to be written before its connection is given up on, when
/// nothing says otherwise — on top of the time the slowest reader needs for the frame's bytes
/// (see [`SLOWEST_READER`]).
///
/// A frame a dashboard sends — a patch, a few panes, a ping — is written in milliseconds on any
/// link that is moving. One that has not been written in thirty seconds more than a slow link
/// would need is going to a client that has stopped reading: a wedged process, a proxy that
/// stopped forwarding, or a client holding the connection open on purpose.
///
/// **Without a deadline a connection waits in that send for as long as the client lets it**,
/// and while it waits it holds its session — and, for an app with a scheduled source, the rows
/// it had when it stopped, which every other viewer has moved off. The kernel keeps a
/// connection whose peer advertises a zero window open indefinitely.
///
/// **It bounds a connection that is stuck sending, and only that.** A client that stops
/// reading on a connection with nothing to send — an idle dashboard with no scheduled source —
/// never fills a buffer, so no write ever waits; [`PING_TIMEOUT`] is what catches that one.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a client has to answer a ping before its connection is given up on, when nothing
/// says otherwise — on top of the time the [`SLOWEST_READER`] needs for everything written to
/// it since it last answered one.
///
/// **This is what catches a client that has stopped reading on a quiet connection**, which
/// nothing else does. No write waits on it, so [`WRITE_TIMEOUT`] never runs; the heartbeat's
/// pings stop the idle timeout in front from firing; and TCP gives up on a peer only when it
/// stops acknowledging, after a quarter of an hour of retransmissions — never, for a client
/// whose kernel still acknowledges what its process no longer reads. Measured against a Linux
/// client, that kernel grows its buffer to fit the pings and compacts them when it reaches its
/// ceiling: a month's worth and a compaction went by with none refused.
///
/// **An answer is the one thing a client sends that proves it reads.** A Pong echoes its Ping,
/// and a client can echo one only after reading everything written before it, so each ping
/// carries a sequence number and only an echo of an unanswered one counts. A browser answers
/// in its network layer with no JavaScript involved, so a page in a background tab, or one
/// whose script is busy, keeps answering; one whose client has gone — a laptop shut, a phone
/// off the network, a NAT that forgot the connection — or stopped reading does not.
///
/// A client reading a large frame slowly answers late, because the ping waits behind the
/// frame: so the allowance counts every byte written since the last answer, at the
/// [`SLOWEST_READER`]'s rate, and a client reading at least that fast is never dropped by it.
pub const PING_TIMEOUT: Duration = Duration::from_secs(30);

/// The slowest reader a frame is given time for, in bytes a second, on top of the write
/// deadline.
///
/// A deadline on a whole frame cannot by itself tell a client that has stopped reading from
/// one reading steadily over a link too slow to take a large frame in time — an opening frame
/// with a big table over a poor mobile connection — and would drop the second as though it were
/// the first, into a reconnect that sends the same frame again. So each frame is given the
/// deadline **plus the time this rate needs to carry it**. 16 KiB/s is about 128 kbit/s, under
/// the slowest link a dashboard is usable over: a client reading at least that fast is never
/// dropped for the size of a frame, and one that has stopped reading still is.
///
/// **What it does not cover is a backlog.** A write finishes when the kernel takes the bytes,
/// and once the kernel's buffers hold frames the client has not read, the next one waits until
/// a share of those has been read — time its allowance, which counts only its own bytes, does
/// not include. A client sent more than its link carries — answers to requests it pipelined, or
/// new rows landing faster than it can read them — fills those buffers, and over a slow enough
/// link misses an allowance and is dropped like one that stopped; from here the two look the
/// same. The page reconnects onto the newest state, which serves it better than data minutes
/// old.
pub const SLOWEST_READER: u64 = 16 * 1024;

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
    /// and what a test that does not want a timer asks for. Zero sends none too, as
    /// `--heartbeat-seconds 0` does, and anything under a millisecond is a millisecond.
    pub heartbeat: Option<Duration>,
    /// How long one frame may take to be written, beyond what the slowest reader needs for its
    /// bytes, before the connection is dropped. [`WRITE_TIMEOUT`] by default.
    ///
    /// `None` waits for as long as the client lets it — what every connection did before this
    /// existed, and what lets a connection stuck sending to a client that stopped reading hold
    /// its session, and the rows it had, until the TCP connection itself dies.
    pub write_timeout: Option<Duration>,
    /// How long a client has to answer a ping, beyond what the slowest reader needs for what
    /// was written to it since its last answer, before the connection is dropped.
    /// [`PING_TIMEOUT`] by default. It asks the heartbeat's pings to be answered, so with
    /// [`Options::heartbeat`] off it never runs.
    ///
    /// `None` waits for as long as the client likes — what every connection did before this
    /// existed, and what lets a client that stopped reading on a quiet connection, or went
    /// without a word, keep its session until TCP gives up on it, or for good.
    pub ping_timeout: Option<Duration>,
}

impl Options {
    /// The three settings every connection takes, as one value.
    fn liveness(&self) -> Liveness {
        Liveness {
            heartbeat: self.heartbeat,
            write_timeout: self.write_timeout,
            ping_timeout: self.ping_timeout,
        }
    }
}

impl Default for Options {
    fn default() -> Options {
        // From `Liveness::default`, so that a front door built from `Options` and one built
        // from a `Liveness` cannot drift apart.
        let Liveness {
            heartbeat,
            write_timeout,
            ping_timeout,
        } = Liveness::default();
        Options {
            app_name: String::new(),
            auth: None,
            origins: Vec::new(),
            heartbeat,
            write_timeout,
            ping_timeout,
        }
    }
}

/// What keeps a connection alive and what gives up on it: how often a quiet connection is
/// pinged, how long a frame may take to be written, and how long a ping may go unanswered.
///
/// One value, and a struct rather than three arguments, because all three are
/// `Option<Duration>` and two of those side by side swap without a word from the compiler.
/// [`Options`] carries the same three settings for one app; [`serve_host_replica`] takes this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Liveness {
    /// How often to ping a connection that has gone quiet — see [`Options::heartbeat`].
    pub heartbeat: Option<Duration>,
    /// How long a frame may take to be written — see [`Options::write_timeout`].
    pub write_timeout: Option<Duration>,
    /// How long a ping may go unanswered — see [`Options::ping_timeout`].
    pub ping_timeout: Option<Duration>,
}

impl Default for Liveness {
    fn default() -> Liveness {
        Liveness {
            // On, because the failure it prevents is silent. A proxy that reaps an idle
            // socket does not log anything a viewer can see, and the page it leaves behind
            // renders perfectly and answers nothing.
            heartbeat: Some(HEARTBEAT),
            // On, for the mirror image of that: a connection stuck sending to a client that
            // stopped reading costs the server memory for as long as the client likes, and
            // nothing logs that either.
            write_timeout: Some(WRITE_TIMEOUT),
            // On, for the same failure on a quiet connection, where no write ever waits.
            ping_timeout: Some(PING_TIMEOUT),
        }
    }
}

/// How long a stopping server gives everything still in flight once its listener has closed —
/// the requests `axum` is finishing, and then the sockets it has asked to close.
///
/// A page answers a close in a round trip, and the server waits a second for that answer,
/// so this is room for every healthy viewer with some to spare. It is a bound rather than an
/// estimate because the clients it exists for are the ones that are not healthy: one that has
/// stopped reading can hold a close frame for as long as the write deadline allows — thirty
/// seconds by default, for ever with it off — and one that sent half a request can hold
/// `axum`'s own wait for as long as it likes. A supervisor's grace period does not wait that
/// long — `docker stop` gives ten seconds and the drain spends five of them — so a stop that
/// waited on either would be ended by a `SIGKILL`, and every other viewer's close with it.
///
/// What it cannot bound is a pass already running when the stop is signalled: a pass holds its
/// thread until it ends, and while it does the runtime may not see a signal or a timer at all,
/// so it can delay the stop, the end of the drain or this deadline by what it has left. When
/// the deadline is up, `dagpane` does not wait for anything still running.
pub const CLOSE_GRACE: Duration = Duration::from_secs(2);

/// The close code a stopping server sends: *going away*, RFC 6455 §7.4.1.
///
/// A browser that is sent no close at all reports `1006`, which says only that the
/// connection died; this one says the server meant it, and the page goes back inside half a
/// second without advancing its backoff, rather than backing off as from a failure.
pub const GOING_AWAY: u16 = 1001;

/// The sockets a listener has upgraded, so that stopping can reach them.
///
/// `axum`'s graceful shutdown cannot. A socket stops being an HTTP connection the moment it
/// upgrades, so `axum::serve` returns with every viewer still connected, and the runtime they
/// run on is dropped straight after — every connection dropped with no close sent, which a
/// browser reports as `1006` and backs off from as from a failure. Before this, "let what is in
/// flight finish" was true of the requests and only half true of the sockets.
///
/// So each socket joins this when it is accepted and leaves when its task ends, and stopping
/// is two steps: [`Connections::go_away`] once the listener stops accepting, which every
/// connection answers between passes with a [`GOING_AWAY`] close, and
/// [`Connections::serve`], which waits for them all — up to [`CLOSE_GRACE`] and no longer.
#[derive(Clone, Debug)]
struct Connections(Arc<Tracked>);

#[derive(Debug)]
struct Tracked {
    /// `None` while serving. Once the listener stops accepting, the moment everything still in
    /// flight has to be done by: [`CLOSE_GRACE`] later.
    going: tokio::sync::watch::Sender<Option<tokio::time::Instant>>,
    /// How many sockets are open.
    open: tokio::sync::watch::Sender<usize>,
}

impl Connections {
    fn new() -> Connections {
        Connections(Arc::new(Tracked {
            going: tokio::sync::watch::channel(None).0,
            open: tokio::sync::watch::channel(0).0,
        }))
    }

    /// One more socket, until the returned [`Member`] is dropped.
    ///
    /// Joined in the handler, before the upgrade rather than after it: the handler's response
    /// is part of a request `axum` waits for, so a socket counted here is counted before
    /// `axum::serve` can return and [`Connections::closed`] start looking.
    fn join(&self) -> Member {
        self.0.open.send_modify(|open| *open += 1);
        Member {
            going: self.0.going.subscribe(),
            connections: self.clone(),
        }
    }

    /// Ask every socket to close, and start the clock on everything still in flight. Sticky: a
    /// socket that joins afterwards closes as soon as it can, and a second call moves nothing.
    fn go_away(&self) {
        self.0.going.send_if_modified(|going| {
            let first = going.is_none();
            if first {
                *going = Some(tokio::time::Instant::now() + CLOSE_GRACE);
            }
            first
        });
    }

    /// When everything after the stop has to be done by — once there is a stop.
    async fn deadline(&self) -> tokio::time::Instant {
        let mut going = self.0.going.subscribe();
        let at = match going.wait_for(Option::is_some).await {
            Ok(at) => *at,
            // The sender lives as long as `self` does.
            Err(_) => None,
        };
        match at {
            Some(at) => at,
            None => std::future::pending().await,
        }
    }

    /// Run `serving` — `axum::serve`, with [`Connections::go_away`] at the end of its graceful
    /// shutdown — and then wait for the sockets, holding everything after the stop to one
    /// deadline: [`CLOSE_GRACE`] from the moment the listener stopped accepting.
    ///
    /// One deadline and not one per stage, because `axum` waits for the requests in flight with
    /// no bound of its own: a client that sends half a request and stops can hold that wait as
    /// long as it likes, and the sockets' wait would not even have begun. `listener_closed` runs
    /// once the listener is closed, whichever way that happened.
    async fn serve(
        &self,
        serving: impl Future<Output = std::io::Result<()>>,
        listener_closed: impl FnOnce(),
    ) -> std::io::Result<()> {
        let served = tokio::select! {
            served = serving => Some(served),
            () = async { tokio::time::sleep_until(self.deadline().await).await } => None,
        };
        listener_closed();
        let Some(served) = served else {
            eprintln!(
                "dagpane: requests still in flight {:.1}s after the listener closed; stopping \
                 without them",
                CLOSE_GRACE.as_secs_f64()
            );
            self.report_open();
            return Ok(());
        };
        // A server that returned without a stop — `axum::serve` failing — has no deadline yet,
        // and its sockets get the full grace from now.
        self.go_away();
        let deadline = self
            .0
            .going
            .borrow()
            .expect("set just above, if not before");
        let mut open = self.0.open.subscribe();
        if tokio::time::timeout_at(deadline, open.wait_for(|open| *open == 0))
            .await
            .is_err()
        {
            self.report_open();
        }
        served
    }

    /// Run the probe listener — `axum::serve` on the admin port, stopping once the app listener
    /// has — held to the same deadline as everything else after the stop. Its graceful shutdown
    /// waits for its own requests in flight with no bound, exactly as the app listener's does,
    /// and `try_join!` waits for both: so without this, half a request on the admin port held
    /// the process after every viewer had been told and every socket had closed.
    async fn probe(
        &self,
        probing: impl Future<Output = std::io::Result<()>>,
    ) -> std::io::Result<()> {
        tokio::select! {
            probed = probing => probed,
            () = async { tokio::time::sleep_until(self.deadline().await).await } => {
                eprintln!(
                    "dagpane: probe requests still in flight {:.1}s after the listener closed; \
                     stopping without them",
                    CLOSE_GRACE.as_secs_f64()
                );
                Ok(())
            }
        }
    }

    /// Say how many sockets are being left open, if any are.
    fn report_open(&self) {
        let left = *self.0.open.borrow();
        if left > 0 {
            eprintln!(
                "dagpane: {left} connection(s) still open {:.1}s after the listener closed; \
                 stopping without them",
                CLOSE_GRACE.as_secs_f64()
            );
        }
    }
}

/// One open socket's place in [`Connections`], and its word that the server is stopping.
#[derive(Debug)]
struct Member {
    going: tokio::sync::watch::Receiver<Option<tokio::time::Instant>>,
    connections: Connections,
}

impl Member {
    /// Whether the server has asked this socket to close.
    fn asked_to_go(&self) -> bool {
        self.going.borrow().is_some()
    }

    /// Resolve once the server asks this socket to close — at once if it already has.
    async fn going_away(&mut self) {
        if self.going.wait_for(Option::is_some).await.is_err() {
            // The sender lives as long as any member does, so this is unreachable; waiting
            // for ever is the answer that cannot close a connection by mistake.
            std::future::pending::<()>().await;
        }
    }
}

impl Drop for Member {
    fn drop(&mut self) {
        self.connections
            .0
            .open
            .send_modify(|open| *open = open.saturating_sub(1));
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
/// For a caller that needs the listener before the app is served on it. A test binds port 0:
/// binding it to find a free port and dropping the listener so `serve` can bind the same address
/// again is a race, because between the two binds anything else on the machine — another test
/// in the same run included — can take the port. `dagpane run` binds first so that it prints
/// the address only once something is listening on it, with the port the kernel picked when
/// asked for 0.
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
    let connections = Connections::new();
    let router = app_router(app, addr, options, connections.clone());
    let stopping = connections.clone();
    let serving = axum::serve(listener, router).with_graceful_shutdown(async move {
        lifecycle::stop_signal().await;
        stopping.go_away();
    });
    connections.serve(serving.into_future(), || {}).await
}

/// The app's four endpoints, and no others.
///
/// Factored out because [`serve_replica`] serves the same four — a cluster is a deployment
/// shape, not a different app, and two routers would be two things to keep identical.
fn app_router(
    app: Arc<App>,
    addr: SocketAddr,
    options: Options,
    connections: Connections,
) -> Router {
    let allowed_origins = if options.origins.is_empty() {
        origins_for(addr)
    } else {
        options.origins.clone()
    };
    let scope = Scope::Single(options.app_name.clone());
    let shared = Arc::new(Shared {
        schedules: Schedules::single(app, &options.app_name),
        scope,
        allowed_origins,
        options,
        connections,
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
/// Returns when both listeners have closed and every socket has — or [`CLOSE_GRACE`] after the
/// app listener closed, whichever is first.
pub async fn serve_replica(
    app: Arc<App>,
    listener: tokio::net::TcpListener,
    admin: tokio::net::TcpListener,
    options: Options,
    lifecycle: Arc<Lifecycle>,
) -> std::io::Result<()> {
    let addr = listener.local_addr()?;
    let connections = Connections::new();
    let router = app_router(app, addr, options, connections.clone());
    let probe_deadline = connections.clone();

    let serving = {
        let shutdown = Arc::clone(&lifecycle);
        let done = Arc::clone(&lifecycle);
        async move {
            let stopping = connections.clone();
            let serving = axum::serve(listener, router).with_graceful_shutdown(async move {
                shutdown.on_signal().await;
                // After the drain, not with the signal: through the drain a viewer is still
                // being served, by design. The listener closes now, and so do they.
                stopping.go_away();
            });
            // Whatever happened to the app listener, it is no longer accepting — so say so,
            // which is also what lets the probe listener stop.
            connections
                .serve(serving.into_future(), || done.stopped())
                .await
        }
    };

    let probing = probe_deadline.probe(
        axum::serve(admin, lifecycle::admin_router(Arc::clone(&lifecycle)))
            .with_graceful_shutdown(async move { lifecycle.until_stopped().await })
            .into_future(),
    );

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

/// The close code for a connection whose token expired before new rows arrived for it.
///
/// A connection is admitted once, when it opens. When a scheduled refresh reads new rows and
/// the token that connection opened on has expired, it is closed with this code instead of
/// being sent them — see `Feed` — and with the reason `token expired`. In the range RFC 6455
/// leaves to applications, and 4401 by the convention other WebSocket protocols use for an
/// HTTP 401 that arrives after the upgrade. The bundled page reads it as "sign in again"
/// rather than as a server that went away, so it says so once instead of retrying a door the
/// same token cannot open.
pub const TOKEN_EXPIRED: u16 = 4401;

/// Serve every app a [`Host`] can resolve, on one port, routed by the `Host` header.
///
/// The multi-app front door. Each request's header decides which app it gets; the host
/// compiles on a miss, shares one `Arc<App>` per key, and evicts against its own budget.
/// Nothing about a session changes — it is still per connection, and two viewers of two
/// different apps on this port share nothing but the socket the process is listening on.
///
/// It answers only the names the host's source resolves, and that is the whole defence
/// against a page whose DNS was pointed at this machine — see
/// `dagpane_host::AppSource::resolve`. A name the source refuses is a `404` before anything is
/// read or compiled for it.
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
    /// Keeps each viewed admission's data current — see [`refresh`].
    schedules: Arc<Schedules>,
    auth: Option<Arc<Auth>>,
    /// How often to ping a quiet connection, how long a frame may take to be written, and how
    /// long a ping may go unanswered. See [`Options::heartbeat`], [`Options::write_timeout`]
    /// and [`Options::ping_timeout`] — a fleet behind one port is if anything more likely to
    /// sit behind a proxy than a single app is.
    liveness: Liveness,
    /// What [`host_error`] has already said about a refused name, so it says it once. Keyed by
    /// the domain the name was under — see [`refusal_key`] — and never more than
    /// [`REFUSALS_LOGGED`] of them.
    refused: Mutex<HashSet<String>>,
    /// The sockets this router has upgraded, so that stopping can close them.
    connections: Connections,
}

/// How many distinct refused domains a hosting process logs before it stops logging them.
///
/// A refused name is either a forgotten `--domain`, which one line is enough to diagnose, or
/// somebody probing, who chooses the name and must not be able to choose how much this process
/// writes. Thirty-two lines is room for the first and no amplifier for the second.
const REFUSALS_LOGGED: usize = 32;

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
    let connections = Connections::new();
    let router = host_router(host, auth, Liveness::default(), connections.clone());
    let stopping = connections.clone();
    let serving = axum::serve(listener, router).with_graceful_shutdown(async move {
        lifecycle::stop_signal().await;
        stopping.go_away();
    });
    connections.serve(serving.into_future(), || {}).await
}

/// The hosting front door's three endpoints. Factored for the same reason [`app_router`] is:
/// [`serve_host_replica`] serves exactly these, and two routers would be two things to keep
/// identical.
fn host_router(
    host: Arc<Host>,
    auth: Option<Arc<Auth>>,
    liveness: Liveness,
    connections: Connections,
) -> Router {
    Router::new()
        .route("/", get(host_page))
        .route("/auth", get(host_auth_config))
        .route("/ws", get(host_socket))
        .with_state(Arc::new(HostState {
            schedules: Schedules::hosted(Arc::clone(&host)),
            host,
            auth,
            liveness,
            refused: Mutex::new(HashSet::new()),
            connections,
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
    liveness: Liveness,
    lifecycle: Arc<Lifecycle>,
) -> std::io::Result<()> {
    let connections = Connections::new();
    let router = host_router(host, auth, liveness, connections.clone());
    let probe_deadline = connections.clone();

    let serving = {
        let shutdown = Arc::clone(&lifecycle);
        let done = Arc::clone(&lifecycle);
        async move {
            let stopping = connections.clone();
            let serving = axum::serve(listener, router).with_graceful_shutdown(async move {
                shutdown.on_signal().await;
                stopping.go_away();
            });
            connections
                .serve(serving.into_future(), || done.stopped())
                .await
        }
    };

    let probing = probe_deadline.probe(
        axum::serve(admin, lifecycle::admin_router(Arc::clone(&lifecycle)))
            .with_graceful_shutdown(async move { lifecycle.until_stopped().await })
            .into_future(),
    );

    tokio::try_join!(serving, probing).map(|_| ())
}

/// Drop apps nobody has opened, on a timer, until the process stops.
///
/// [`Host::sweep_idle`] existed from the day `crates/host` did and **nothing in the binary
/// called it** until this did, so `dagpane host --idle-minutes` configured a sweep that never
/// ran: an app left residency on a redeploy or under budget pressure and never for being idle.
/// No release had `host` before 0.2.0, which ships with this. This is the caller.
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
    match shared.app().renderers.iter().find(|r| r.path == path) {
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
        Err(e) => host_error(&state, &name, &e),
    }
}

/// Why a request could not be served, in terms a person reading a browser tab can act on.
///
/// Deliberately terse about the ones that are the operator's problem rather than the
/// viewer's: a manifest's compile error names cells in an app the viewer cannot see, and an
/// app's byte footprint is nobody's business at the front door. Both are logged in full and
/// summarised here.
fn host_error(state: &HostState, name: &str, e: &HostError) -> Response {
    match e {
        HostError::UnknownHost(_) | HostError::NotFound(_) => {
            (StatusCode::NOT_FOUND, format!("no app is served at {name}")).into_response()
        }
        // Says that the NAME is refused rather than that no app exists, which is what tells a
        // `curl` at a forgotten `--domain` apart from a typo in a filename. It does not list
        // the domains that are answered: they are public in DNS anyway, but a refusal has no
        // business being a directory.
        HostError::OutsideDomains(_) => {
            log_refusal(state, name);
            (
                StatusCode::NOT_FOUND,
                format!("no app is served at {name}: this server does not answer to that name"),
            )
                .into_response()
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
/// The `Origin` check generalises rather than changes: in single-app mode the allowed origins
/// are the ones declared with `--origin`, or with none declared the address the process bound,
/// and here it is **the host the request is already asking for**. A page at `https://sales.example.com` may open this app's socket; a page at
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
        Some(origin) if origin_matches(origin, raw_host(&headers), &name) => {}
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
    let viewer = match admit(state.auth.as_deref(), &app_name, &headers) {
        Ok(viewer) => viewer,
        Err(refusal) => return *refusal,
    };

    // A lease with the app: the host hands the session out and cannot see it afterwards, so
    // this is how it learns the session exists and what it holds. It lives exactly as long as
    // the connection does.
    let (app, lease) = match state.host.open_session(&name) {
        Ok(opened) => opened,
        Err(e) => return host_error(&state, &name, &e),
    };
    let scope = Scope::Admission(lease.key().clone(), lease.admission());
    let feed = state.schedules.subscribe(scope.clone(), &app, viewer);
    let (values, unreadable) = parse_resume(query.as_deref());
    let ws = if state.auth.is_some() {
        ws.protocols([SUBPROTOCOL])
    } else {
        ws
    };
    let liveness = state.liveness;
    let member = state.connections.join();
    ws.on_upgrade(move |socket| async move {
        connection(
            socket,
            app,
            (values, unreadable),
            liveness,
            member,
            Some(lease),
            feed,
        )
        .await
        .report(&scope);
    })
}

/// Does this `Origin` name the host the request is for?
///
/// Compared against the `Host` header rather than against a list built from the bound
/// address: with many apps on one port there is no single address to build one from, and the
/// port in an `Origin` is whatever the browser was given — which is the proxy's port, not this
/// process's. `Host` carries the same name, so comparing the two is the check that stays true
/// behind a proxy.
///
/// **This is CSRF defence and not the rebinding defence.** A page whose name was pointed at
/// this machine sends its own name as both headers and passes it; what stops that page is that
/// the source answers only declared names (see `dagpane_host::AppSource::resolve`).
///
/// **Under `localhost` the port is compared too.** A browser resolves `sales.localhost` to
/// loopback whoever serves it, so any other local server — a dev server, `python -m
/// http.server` — serves pages at `sales.localhost:3000`, and with the port ignored those pages
/// could open `sales.localhost:8787`'s socket and read every pane. Nothing rewrites either
/// header for a `.localhost` name, so the two ports agree for every honest request. When the
/// `Host` carries no port — a proxy that forwards `$host` drops it — only the name is compared,
/// because there is no port to agree with. Any other name stays port-blind, for the proxy
/// reason above.
fn origin_matches(origin: &str, raw_host: &str, name: &str) -> bool {
    let Some((scheme, rest)) = ["http", "https"]
        .into_iter()
        .find_map(|scheme| Some((scheme, origin.strip_prefix(scheme)?.strip_prefix("://")?)))
    else {
        return false;
    };
    if normalise_host(rest) != name {
        return false;
    }

    let bare = name.strip_suffix('.').unwrap_or(name);
    let under_localhost = bare == "localhost" || bare.ends_with(".localhost");
    match (under_localhost, port_of(raw_host)) {
        (true, Some(host_port)) => {
            let default = if scheme == "https" { 443 } else { 80 };
            let origin_port = match port_of(rest) {
                Some(port) => port.parse::<u16>().ok(),
                None => Some(default),
            };
            origin_port.is_some() && origin_port == host_port.parse::<u16>().ok()
        }
        _ => true,
    }
}

/// The port an authority (`name:port`, `[v6]:port`) carries, if it carries one.
fn port_of(authority: &str) -> Option<&str> {
    let authority = authority.trim();
    let after_name = match authority.strip_prefix('[') {
        Some(rest) => &rest[rest.find(']')? + 1..],
        None => authority,
    };
    after_name.split_once(':').map(|(_, port)| port)
}

/// The `Host` header as it arrived, port and all.
fn raw_host(headers: &HeaderMap) -> &str {
    headers
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// What a refused name is logged under: the domain it was under, or what kind of not-a-name it
/// was. One line per key, so a scanner sending a thousand names under one domain is one line,
/// and an operator who forgot `--domain` sees the domain they forgot.
fn refusal_key(name: &str) -> String {
    let bare = name.strip_suffix('.').unwrap_or(name);
    if bare.len() > Domain::MAX_LEN {
        // No DNS name is this long, so it is somebody choosing how much a line weighs: one key
        // for all of them, and the line shows only the start.
        "<too long>".to_string()
    } else if bare.starts_with('[') || bare.parse::<std::net::IpAddr>().is_ok() {
        "<ip>".to_string()
    } else {
        match bare.split_once('.') {
            Some((_, domain)) => domain.to_string(),
            None => "<bare>".to_string(),
        }
    }
}

/// Say on stderr why a name was refused — once per [`refusal_key`], and at most
/// [`REFUSALS_LOGGED`] times in all.
///
/// The hint to pass `--domain` is offered only for something that could BE one, and always
/// with "if you control it": the name came from whoever sent the request, and a log line that
/// told an operator to declare an attacker's domain would be the attack finishing itself.
fn log_refusal(state: &HostState, name: &str) {
    let key = refusal_key(name);
    if !first_refusal(&state.refused, &key) {
        return;
    }
    // Bounded, so the sender of a name chooses neither how many lines this writes nor how
    // long one is. A real name is under the cap and printed whole.
    let shown: String = name.chars().take(Domain::MAX_LEN).collect();
    let name = shown.as_str();
    match (key.as_str(), deepest_under(&state.host.domains(), &key)) {
        ("<too long>", _) => eprintln!(
            "dagpane: refused Host {:?}…: longer than any DNS name, so this gets 404 (logged \
             once)",
            name.chars().take(64).collect::<String>()
        ),
        ("<ip>", _) => eprintln!(
            "dagpane: refused Host {name:?}: an app is reached by name — <app>.localhost or \
             <app>.<a --domain> — never by address, so this gets 404 (logged once)"
        ),
        ("<bare>", _) => eprintln!(
            "dagpane: refused Host {name:?}: an app is reached as <app>.localhost or \
             <app>.<a --domain>, never by a bare name, so this gets 404 (logged once)"
        ),
        // `x.sales.localhost`: one label too many under a domain that IS answered, and the hint
        // below would suggest declaring `sales.localhost`, which is never what was meant. Also
        // for a source that does not say what it answers under, which `localhost` always is
        // for the source `dagpane host` builds.
        (domain, under) if is_localhost(under.unwrap_or(domain)) => {
            let under = under.unwrap_or("localhost");
            eprintln!(
                "dagpane: refused Host {name:?}: an app is one label in front of {under} — \
                 <app>.{under} — and nothing below that is answered, so this gets 404 (logged \
                 once per domain)"
            );
        }
        // `x.team.example.com` under a declared `example.com`: one label too many under a domain
        // that IS answered — or a zone of its own nobody declared, which only the operator can
        // tell apart. So the line says the first and offers the second, conditionally; a
        // `--domain` that is one app's whole name is warned about at start-up in any case.
        (domain, Some(under)) => eprintln!(
            "dagpane: refused Host {name:?}: an app is one label in front of {under} — \
             <app>.{under} — and nothing below that is answered, so every name under {domain:?} \
             gets 404 (logged once per domain). If {domain} is a zone of its own that you \
             publish these apps under, pass --domain {domain} as well"
        ),
        (domain, None) => {
            let hint = if Domain::parse(domain).is_ok() {
                format!(
                    " If {domain} is a domain you control and publish these apps under, pass \
                     --domain {domain}"
                )
            } else {
                String::new()
            };
            eprintln!(
                "dagpane: refused Host {name:?}: no domain this host answers under contains \
                 it, so every name under {domain:?} gets 404 (logged once per domain).{hint}"
            );
        }
    }
}

/// `localhost`, or a name under it.
fn is_localhost(domain: &str) -> bool {
    domain == "localhost" || domain.ends_with(".localhost")
}

/// The declared domain a refused name sits more than one label under, the most specific if it
/// sits under several — given the name's [`refusal_key`], which is everything after its first
/// label. `sales.example.com` is under `example.com`; `example.com` itself is not, because
/// `<app>.example.com` was answered or refused for its app, never for its domain.
fn deepest_under<'d>(domains: &'d [Domain], key: &str) -> Option<&'d str> {
    domains
        .iter()
        .map(Domain::as_str)
        .filter(|domain| {
            key.strip_suffix(domain)
                .is_some_and(|rest| rest.ends_with('.'))
        })
        .max_by_key(|domain| domain.len())
}

/// Whether this refusal key is the first of its kind, and still under [`REFUSALS_LOGGED`].
fn first_refusal(refused: &Mutex<HashSet<String>>, key: &str) -> bool {
    // A poisoned set is still a set: nothing that holds this lock can panic while holding it,
    // and if something ever did, losing the log would be worse than reading it.
    let mut refused = refused
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    refused.len() < REFUSALS_LOGGED && refused.insert(key.to_string())
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
    let viewer = match admit(
        shared.options.auth.as_deref(),
        &shared.options.app_name,
        &headers,
    ) {
        Ok(viewer) => viewer,
        Err(refusal) => return *refusal,
    };

    let (values, unreadable) = parse_resume(query.as_deref());
    let ws = if shared.options.auth.is_some() {
        ws.protocols([SUBPROTOCOL])
    } else {
        ws
    };
    let liveness = shared.options.liveness();
    let app = shared.app();
    let scope = shared.scope.clone();
    let feed = shared.schedules.subscribe(scope.clone(), &app, viewer);
    let member = shared.connections.join();
    ws.on_upgrade(move |socket| async move {
        connection(
            socket,
            app,
            (values, unreadable),
            liveness,
            member,
            // One app and no host to report to. The figure would be the same one; there is just
            // nothing in this process that would read it.
            None,
            feed,
        )
        .await
        .report(&scope);
    })
}

/// One viewer, start to finish.
///
/// Deliberately sequential: a message is read, a pass runs, a patch goes out, and only then
/// is the next message read. A data app's interactions are a person moving a control, so
/// concurrency inside one connection would buy nothing and would cost the guarantee that
/// the viewer's controls and their page describe the same epoch.
///
/// Sequential also means a frame that cannot be written stops everything behind it, so every
/// frame is written within its allowance — [`Options::write_timeout`], and the time the
/// [`SLOWEST_READER`] needs for its bytes: a connection stuck sending to a client that has
/// stopped reading, or fallen too far behind, is dropped rather than keeping the task, the
/// session and the rows it holds for as long as the client likes. A quiet connection has no
/// write to wait on, so every ping is answered within its own allowance —
/// [`Options::ping_timeout`], and the time the same reader needs for what came before it — or
/// the connection is dropped for the same reason.
///
/// And it closes when the server stops, between passes like everything else: a pass that is
/// running when the listener closes runs to the end and its answer goes out, and then the
/// close does — [`GOING_AWAY`], so the page knows to come straight back, to whichever replica
/// is still serving. A message that has arrived but not been read is not answered; the page
/// carries the viewer's values to its next connection, which renders them in its first pass.
async fn connection(
    socket: WebSocket,
    app: Arc<App>,
    (values, unreadable): (BTreeMap<String, dagpane_core::Value>, Option<String>),
    liveness: Liveness,
    mut member: Member,
    mut lease: Option<SessionLease>,
    mut feed: Option<Feed>,
) -> Ended {
    let mut wire = Wire {
        socket,
        deadline: liveness.write_timeout,
        written: 0,
    };
    let started = Instant::now();
    // Open on the newest rows the app has — unless they arrived after the token this
    // connection was admitted on ran out. See `Feed::opening`.
    let mut app = app;
    if let Some(feed) = feed.as_mut() {
        match feed.opening(&app, SystemTime::now()) {
            Opening::Admitted => {}
            Opening::Newer(newest) => {
                app = Arc::clone(&newest.app);
                if let (Some(lease), Some(basis)) = (lease.as_mut(), newest.lease.as_ref()) {
                    lease.rebase(Arc::clone(&basis.baseline), basis.source_bytes);
                }
            }
            Opening::Expired(subject) => {
                return close_expired(&mut wire, &subject, &feed.scope)
                    .await
                    .map_or_else(Lost::ended, |()| Ended::Done);
            }
        }
    }
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
    // After the first pass, which is when a session first holds anything of its own.
    if let Some(lease) = &lease {
        lease.record(session.session());
    }
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
    if let Err(lost) = wire.send(&init).await {
        return lost.ended();
    }

    // Capped, because `Duration::MAX` is a natural way to say "never" and adding it to the clock
    // panics: every connection would die the moment its opening frame went out, and its page
    // would reconnect into the same panic for ever. A century is never too. Floored, because a
    // beat due again by the time its ping is written would have this loop ping between every two
    // frames; and zero is none, as the command line's `0` is.
    let every = liveness
        .heartbeat
        .filter(|every| !every.is_zero())
        .map(|every| every.clamp(SHORTEST_HEARTBEAT, LONGEST_HEARTBEAT));
    // The first ping is one interval away rather than immediately: a connection that has just
    // sent its opening frame is the least idle it will ever be.
    let mut next_beat = every.map(|every| tokio::time::Instant::now() + every);
    // Whether the last pass sent a ping. The next then waits on the socket and the timers
    // together, as an idle loop does, rather than pinging again at once: however long a ping
    // takes to write, ping after ping cannot crowd the socket out.
    let mut pinged = false;
    let mut pings = Pings::new(liveness.ping_timeout);
    // Frames read past an overdue ping while looking for its answer, since the last answer.
    let mut looked = 0;

    loop {
        // Asked here as well as waited on below, because the timers above the wait can keep
        // this loop from reaching it, and a socket that joined after the stop must still close.
        if member.asked_to_go() {
            return going_away(&mut wire).await;
        }
        // The clock first, and the timers only to wait on. A client that never reads can keep
        // this loop from ever waiting, by sending, and a task that never waits can leave the
        // runtime's timers behind: measured, an overdue ping went unseen through hundreds of
        // thousands more frames, another second on an idle machine and four on a busy one. The
        // clock is never behind.
        let now = tokio::time::Instant::now();
        let due = pings.due();
        let event = match (due, next_beat) {
            (Some((at, allowed)), _) if at <= now => Event::Unanswered(allowed),
            (_, Some(at)) if at <= now && !pinged => Event::Beat,
            // `recv` is a `Stream::next`, whose state lives in the stream rather than the
            // future, so losing this branch to a timer or to a refresh drops nothing that had
            // been read. A refresh is handled between messages, never inside a pass: the
            // viewer's own pass runs to the end and is answered, and only then does the data
            // move under them.
            _ => tokio::select! {
                incoming = wire.socket.recv() => Event::Received(incoming),
                () = until(next_beat) => Event::Beat,
                Some(generation) = next_generation(&mut feed) => Event::Refreshed(generation),
                allowed = overdue(due) => Event::Unanswered(allowed),
                () = member.going_away() => Event::GoingAway,
            },
        };
        pinged = matches!(event, Event::Beat);
        let incoming = match event {
            Event::Received(incoming) => incoming,
            Event::Beat => {
                // A pass holds this task while it runs, so a ping can be late. The next is an
                // interval after this one, not on the old schedule: catching up would send a
                // handful of pings back to back to prove liveness that the pass itself just
                // proved.
                next_beat = every.map(|every| tokio::time::Instant::now() + every);
                // A WebSocket Ping, not a message of ours. The browser answers it in the
                // transport with no JavaScript involved, which is the whole reason this needs
                // no client-side counterpart — and a browser cannot send a Ping from script
                // anyway, so the server is the only end that can start one.
                let (seq, ping) = pings.ping();
                if let Err(lost) = wire.write(ping).await {
                    return lost.ended();
                }
                pings.sent(seq, tokio::time::Instant::now(), wire.written);
                continue;
            }
            // The deadline is up, but this loop may have been busy when the answer came —
            // writing a large frame, running a pass, waiting for a worker — and an answer that
            // is already here is not a missing one. Left to `select!`, it races the clock, and
            // measured it lost seven or eight times in ten. So whatever has arrived is read
            // first, and the deadline asked again after it: up to `STILL_ARRIVING` frames, so a
            // client that never reads cannot put it off by sending others.
            Event::Unanswered(allowed) => {
                if looked == 0 {
                    // Asked of the clock, the deadline can be ahead of what the runtime has
                    // seen of the socket: while every worker is busy, nothing reads the
                    // operating system's word that an answer has landed. A turn of the runtime
                    // reads it before its timers fire, so one sleep lets it look first.
                    tokio::time::sleep(Duration::ZERO).await;
                }
                match arrived(&mut wire.socket) {
                    Some(incoming) if looked < STILL_ARRIVING => {
                        looked += 1;
                        incoming
                    }
                    // Dropped, not closed, for the write deadline's reason: a client that has
                    // not read a ping will not read a Close behind it.
                    _ => return Ended::Unanswered(allowed),
                }
            }
            Event::Refreshed(generation) => {
                // New rows are new data, and a connection is admitted once: one that has
                // outlived the token it opened on is closed rather than moved onto them. What
                // it already holds was its to see, and it keeps that until it closes.
                let new_rows = !Arc::ptr_eq(&generation.app, session.app());
                if let Some(feed) = feed.as_ref().filter(|_| new_rows) {
                    if let Some(viewer) = feed.expired(SystemTime::now()) {
                        let closed = close_expired(&mut wire, &viewer.subject, &feed.scope);
                        return closed.await.map_or_else(Lost::ended, |()| Ended::Done);
                    }
                }
                if let Some(message) = reloaded(&mut session, &generation, lease.as_mut()) {
                    if let Err(lost) = wire.send(&message).await {
                        return lost.ended();
                    }
                }
                continue;
            }
            Event::GoingAway => return going_away(&mut wire).await,
        };

        let Some(Ok(message)) = incoming else { break };
        let text = match message {
            Message::Text(t) => t,
            Message::Close(_) => break,
            // How far the client has read: see `Pings`.
            Message::Pong(payload) => {
                if pings.answered(&payload) {
                    looked = 0;
                }
                continue;
            }
            // A Ping is answered by the transport; a data app has no use for binary frames, and
            // a client that sends one is not this client.
            _ => continue,
        };

        let incoming: ClientMessage = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                let rejected = ServerMessage::Rejected {
                    seq: 0,
                    message: format!("could not read that message: {e}"),
                };
                if let Err(lost) = wire.send(&rejected).await {
                    return lost.ended();
                }
                continue;
            }
        };

        let reply = match incoming {
            ClientMessage::Set { seq, values } => {
                let reply = apply(&mut session, seq, &values);
                // Only a pass changes what the session holds. A rejected batch staged nothing,
                // so there is nothing new to weigh.
                if let (Some(lease), ServerMessage::Patch { .. }) = (&lease, &reply) {
                    lease.record(session.session());
                }
                reply
            }
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

        if let Err(lost) = wire.send(&reply).await {
            return lost.ended();
        }
    }
    Ended::Done
}

/// How a connection ended, as far as an operator needs telling.
#[must_use]
enum Ended {
    /// The ordinary ways: the page closed, the socket failed, or a token ran out — which says
    /// so where it happens.
    Done,
    /// A frame was not written within its allowance: its client had stopped reading, or fallen
    /// too far behind what it was sent, and the connection was dropped.
    Shed(Duration),
    /// A ping was not answered within its allowance: its client had stopped reading, or gone,
    /// and the connection was dropped.
    Unanswered(Duration),
}

impl Ended {
    /// Say so, when there is something to say. Called by whoever knows which app it was.
    fn report(self, scope: &Scope) {
        match self {
            Ended::Done => {}
            Ended::Shed(after) => eprintln!(
                "dagpane: dropped a connection to {scope}: a frame waited {:.1}s to be \
                 written, so its client had stopped reading or fallen behind",
                after.as_secs_f64()
            ),
            Ended::Unanswered(after) => eprintln!(
                "dagpane: dropped a connection to {scope}: a ping went {:.1}s unanswered, so \
                 its client had stopped reading or gone",
                after.as_secs_f64()
            ),
        }
    }
}

/// Why a frame did not go out.
enum Lost {
    /// The socket failed or closed: the connection is over, and there is nothing to report.
    Socket,
    /// The frame was not written within the deadline.
    Stalled(Duration),
}

impl Lost {
    fn ended(self) -> Ended {
        match self {
            Lost::Socket => Ended::Done,
            Lost::Stalled(after) => Ended::Shed(after),
        }
    }
}

/// One connection's socket, and what writing to it is held to.
///
/// Every frame goes out through [`Wire::write`], which is what lets the write deadline hold for
/// all of them and the ping deadline know how much a client has been sent.
struct Wire {
    socket: WebSocket,
    /// The write deadline — see [`Wire::write`].
    deadline: Option<Duration>,
    /// Bytes of data written so far, which the ping deadline allows a slow reader time for.
    written: u64,
}

impl Wire {
    /// Write one frame, within its [`allowance`] when there is a deadline.
    ///
    /// A frame that misses it is abandoned mid-write and the connection with it: dropped, not
    /// closed, because a Close frame would only queue behind the bytes that are stuck.
    async fn write(&mut self, frame: Message) -> Result<(), Lost> {
        let bytes = data_len(&frame) as u64;
        match self.deadline {
            None => self.socket.send(frame).await.map_err(|_| Lost::Socket)?,
            Some(deadline) => {
                let limit = allowance(deadline, &frame);
                match tokio::time::timeout(limit, self.socket.send(frame)).await {
                    Ok(written) => written.map_err(|_| Lost::Socket)?,
                    Err(_) => return Err(Lost::Stalled(limit)),
                }
            }
        }
        self.written = self.written.saturating_add(bytes);
        Ok(())
    }

    async fn send(&mut self, message: &ServerMessage) -> Result<(), Lost> {
        let json = serde_json::to_string(message).map_err(|_| Lost::Socket)?;
        self.write(Message::Text(json.into())).await
    }

    /// Close with a reason the page can act on, and wait a moment for the page's own Close so
    /// the handshake completes and the browser reports a clean close.
    ///
    /// Bounded: a page that never answers is no reason to keep this task.
    async fn close(&mut self, code: u16, reason: &'static str) -> Result<(), Lost> {
        let frame = CloseFrame {
            code,
            reason: reason.into(),
        };
        self.write(Message::Close(Some(frame))).await?;
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(Ok(message)) = self.socket.recv().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        })
        .await;
        Ok(())
    }
}

/// How long `frame` may take to be written: the deadline, and the time the [`SLOWEST_READER`]
/// needs for its bytes. Saturating, because a deadline of `Duration::MAX` is a natural way to
/// say "never", and an overflow here would take down every connection before its first frame.
fn allowance(deadline: Duration, frame: &Message) -> Duration {
    deadline.saturating_add(Duration::from_secs_f64(
        data_len(frame) as f64 / SLOWEST_READER as f64,
    ))
}

/// The bytes a frame carries that are worth timing: a message's. A ping's or a close's few are
/// not.
fn data_len(frame: &Message) -> usize {
    match frame {
        Message::Text(text) => text.len(),
        Message::Binary(bytes) => bytes.len(),
        _ => 0,
    }
}

/// What woke a connection up.
enum Event {
    /// The socket: a message, a close, or an error.
    Received(Option<Result<Message, axum::Error>>),
    /// The next heartbeat fell due.
    Beat,
    /// The app's schedule read new rows.
    Refreshed(Arc<Generation>),
    /// An unanswered ping ran out of the allowance given here.
    Unanswered(Duration),
    /// The server is stopping.
    GoingAway,
}

/// Close a connection because the server is stopping. An ordinary end, so nothing to report
/// unless the close itself could not be written in time.
async fn going_away(wire: &mut Wire) -> Ended {
    wire.close(GOING_AWAY, "the server is stopping")
        .await
        .map_or_else(Lost::ended, |()| Ended::Done)
}

/// The next heartbeat at `at`, or never for a connection that sends none.
async fn until(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// The moment the first unanswered ping to fall due is overdue, and the allowance it had — or
/// never, while nothing is waiting on an answer.
async fn overdue(due: Option<(tokio::time::Instant, Duration)>) -> Duration {
    match due {
        Some((at, allowed)) => {
            tokio::time::sleep_until(at).await;
            allowed
        }
        None => std::future::pending().await,
    }
}

/// How many frames an overdue ping waits through for its answer: generous for a client that
/// asked for something and answered behind it, and still an end for one that only sends.
const STILL_ARRIVING: usize = 64;

/// A frame the socket already holds, read without waiting — `None` when nothing has arrived.
///
/// Polled once, outside the task's scheduling budget: a task that has spent its budget is told
/// "pending" by a socket that has data, which here would drop a client whose answer had come.
fn arrived(socket: &mut WebSocket) -> Option<Option<Result<Message, axum::Error>>> {
    let recv = std::pin::pin!(tokio::task::unconstrained(socket.recv()));
    match recv.poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(incoming) => Some(incoming),
        Poll::Pending => None,
    }
}

/// The next generation of this connection's app, or never.
///
/// `None` once the schedule has ended, which happens only when nobody is subscribed — never
/// while this connection is — so it is not expected; the connection simply stops waiting for
/// refreshes rather than spinning on a closed channel.
async fn next_generation(feed: &mut Option<Feed>) -> Option<Arc<Generation>> {
    let Some(rx) = feed.as_mut().map(|f| &mut f.generations) else {
        return std::future::pending().await;
    };
    if rx.changed().await.is_err() {
        *feed = None;
        return None;
    }
    Some(Arc::clone(&rx.borrow_and_update()))
}

/// Close a connection whose token ran out before new rows arrived for it, and say whose it was.
///
/// `Err` when even the Close frame could not be written within its allowance: the client had
/// stopped reading as well, and the connection is dropped and reported like any other.
async fn close_expired(wire: &mut Wire, subject: &str, scope: &Scope) -> Result<(), Lost> {
    eprintln!(
        "dagpane: closed {subject}'s connection to {scope}: their token expired before new rows \
         arrived, so they were not sent them"
    );
    wire.close(TOKEN_EXPIRED, "token expired").await
}

/// Move a viewer onto refreshed data, and say what changed for them — or nothing at all.
///
/// One pass, in their session: what reads the new rows recomputes, their controls stay where
/// they put them, and only panes whose rendered view moved are sent. A refresh that moved none
/// of this viewer's panes sends them nothing, which is the case for a viewer filtered to rows
/// the refresh did not touch and for every viewer when the new rows changed nothing they see.
fn reloaded(
    session: &mut AppSession,
    generation: &Generation,
    lease: Option<&mut SessionLease>,
) -> Option<ServerMessage> {
    if Arc::ptr_eq(&generation.app, session.app()) {
        return None;
    }
    let started = Instant::now();
    let epoch = session.session().epoch() + 1;
    // Unreachable for a generation of the schedule this connection subscribed to, which is
    // always this app over new rows; and a viewer left on the rows they have is the safe way
    // for it to be wrong.
    let (trace, panes) = session.rebase(Arc::clone(&generation.app)).ok()?;
    let micros = started.elapsed().as_micros() as u64;
    if let Some(lease) = lease {
        // The baseline first: weighed against the rows it left, the session would be billed
        // for the new ones as though they were its own.
        if let Some(basis) = &generation.lease {
            lease.rebase(Arc::clone(&basis.baseline), basis.source_bytes);
        }
        lease.record(session.session());
    }
    let frontier = dagpane_app::BoundaryValue::of(&session.frontier(epoch));
    if panes.is_empty() && frontier.is_empty() {
        return None;
    }
    let stats = PassStats::from_trace(&trace).with_micros(micros);
    Some(ServerMessage::Reloaded {
        sources: trace.roots,
        panes,
        frontier,
        stats,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_name_is_keyed_by_its_domain_and_the_log_stops_at_the_cap() {
        assert_eq!(refusal_key("sales.attacker.example"), "attacker.example");
        assert_eq!(refusal_key("ops.attacker.example."), "attacker.example");
        assert_eq!(refusal_key("sales"), "<bare>");
        assert_eq!(refusal_key(""), "<bare>");
        assert_eq!(refusal_key("127.0.0.1"), "<ip>");
        assert_eq!(refusal_key("[::1]"), "<ip>");
        assert_eq!(refusal_key("x.sales.localhost"), "sales.localhost");
        // Longer than any DNS name: one key however it is spelled, so a sender cannot spend a
        // line, or a set entry, on each.
        let long = format!("a.{}", "b".repeat(300));
        assert_eq!(refusal_key(&long), "<too long>");
        assert_eq!(refusal_key(&format!("c{long}")), "<too long>");

        let refused = Mutex::new(HashSet::new());
        assert!(first_refusal(&refused, "attacker.example"));
        assert!(
            !first_refusal(&refused, "attacker.example"),
            "a domain is said once"
        );
        let said = (0..40)
            .filter(|i| first_refusal(&refused, &format!("d{i}.example")))
            .count();
        assert_eq!(
            said,
            REFUSALS_LOGGED - 1,
            "one key was already spent, and the cap is the cap"
        );
        assert_eq!(refused.lock().unwrap().len(), REFUSALS_LOGGED);
        assert!(!first_refusal(&refused, "never.seen.example"));
    }

    #[test]
    fn a_name_one_label_too_deep_is_said_to_be_under_the_domain_that_is_answered() {
        let domains: Vec<Domain> = ["localhost", "example.com", "eu.example.com"]
            .into_iter()
            .map(|d| Domain::parse(d).unwrap())
            .collect();
        // `x.sales.example.com` is too deep under `example.com`, not a forgotten
        // `--domain sales.example.com` — and the most specific declared domain is the one named.
        assert_eq!(
            deepest_under(&domains, "sales.example.com"),
            Some("example.com")
        );
        assert_eq!(
            deepest_under(&domains, "ops.eu.example.com"),
            Some("eu.example.com")
        );
        assert_eq!(
            deepest_under(&domains, "sales.localhost"),
            Some("localhost")
        );
        // A label boundary, never a suffix: `badexample.com` is not under `example.com`.
        assert_eq!(deepest_under(&domains, "badexample.com"), None);
        assert_eq!(deepest_under(&domains, "attacker.example"), None);
        assert_eq!(deepest_under(&domains, "example.com"), None);
        assert_eq!(deepest_under(&[], "sales.localhost"), None);
    }

    #[test]
    fn a_frame_is_allowed_the_deadline_and_the_time_a_slow_link_needs_for_it() {
        let deadline = Duration::from_secs(30);
        // Pings and closes carry no payload worth timing: the deadline alone.
        assert_eq!(
            allowance(deadline, &Message::Ping(Vec::new().into())),
            deadline
        );
        // A frame of exactly one second's worth at the slowest reader gets one second more.
        let second = "x".repeat(SLOWEST_READER as usize);
        assert_eq!(
            allowance(deadline, &Message::Text(second.into())),
            deadline + Duration::from_secs(1)
        );
        // "Never", spelled the natural way, stays never rather than overflowing — which would
        // panic in every connection task before its opening frame.
        assert_eq!(
            allowance(Duration::MAX, &Message::Text("x".repeat(1_000).into())),
            Duration::MAX
        );
    }

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
