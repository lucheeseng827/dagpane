//! The command line. Parsing only — nothing here decides anything.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "dagpane",
    version,
    about = "A reactive runtime for data apps: an interaction recomputes the cells that depend on it, and nothing else."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Compile an app and report what is wrong with it. Exits non-zero on any error.
    Check {
        /// The app manifest. Paths inside it resolve against its own directory.
        app: PathBuf,
    },

    /// Print the dependency graph. The structure the runtime will actually use, before it
    /// runs — which is the point of declaring edges rather than discovering them.
    Graph {
        app: PathBuf,
        #[arg(long, value_enum, default_value_t = GraphFormat::Text)]
        format: GraphFormat,
    },

    /// Run one interaction and print exactly what it cost.
    ///
    /// This is the product claim in a terminal. No socket, no browser, no timing noise —
    /// just which cells ran, which served a cached value, which were never looked at, and
    /// which panes would have gone on the wire.
    Explain {
        app: PathBuf,
        /// `name=value`, repeatable. Everything given is applied in one pass, the way a
        /// client that moved two controls in one gesture would send them.
        #[arg(long = "set", value_name = "NAME=VALUE")]
        set: Vec<String>,
        /// `CELL.COLUMN` — rewrite one column of one source frame and report what that
        /// cost, the way `--set` reports what turning a control costs.
        ///
        /// This is the measurement behind sub-node invalidation: a source's data moving in
        /// one column is what a refresh of a wide table usually is, and the number worth
        /// knowing is how many cells had to run because of it. The edit is synthetic — every
        /// value in that column is nudged — and the output says so.
        #[arg(long = "change-column", value_name = "CELL.COLUMN")]
        change_column: Option<String>,
        /// Machine-readable, for a CI gate that asserts on the numbers.
        #[arg(long)]
        json: bool,
    },

    /// Write the app as a directory of static files, with the engine compiled into the page.
    ///
    /// The same client, the same manifest and the same nine verbs, with `dagpane-wasm` in
    /// place of a server. What you get is a folder for any static host — **served over
    /// `http://`, not opened with `file://`**, because a browser refuses to load modules and
    /// WebAssembly from a file URL's opaque origin. `python3 -m http.server` in the folder is
    /// enough.
    ///
    /// An app that has no server also has no front door, no scheduled refresh and no HTTP or
    /// SQL sources, because all four of those are things a server was doing.
    Export {
        app: PathBuf,
        /// Where to write. Created if it is not there; existing files of the same names are
        /// overwritten and nothing else is touched.
        #[arg(long, default_value = "dist")]
        out: PathBuf,
        /// The wasm module. The default is `target/wasm32-unknown-unknown/release/` **as a
        /// relative path**, so it only finds anything when this runs from a dagpane
        /// workspace that has just built it. An installed `dagpane` needs this flag.
        #[arg(long)]
        wasm: Option<PathBuf>,
    },

    /// Serve the app.
    Run {
        app: PathBuf,
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// The address to bind. Defaults to loopback; binding anything else with no
        /// `--auth-jwks` prints a warning saying who can reach it.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[command(flatten)]
        auth: AuthArgs,
    },

    /// Serve the app as one replica of however many.
    ///
    /// The same app `run` serves, deployed the other way. `run` is a person and a laptop: it
    /// binds loopback, prints a URL, and stops when that person stops it. This is one process
    /// of several behind a load balancer, where something else decides when it starts, when it
    /// stops, and whether traffic goes to it — and decides those by asking. So every default
    /// that `run` sets for the laptop, this one sets for the fleet:
    ///
    /// * **binds every interface**, because a container that binds loopback is a container
    ///   nothing can reach;
    /// * **answers probes** on a second listener you do not publish — `/healthz` for liveness,
    ///   `/readyz` for whether to send it work;
    /// * **drains before it stops**: goes unready, waits for the balancer to notice, and only
    ///   then closes the listener;
    /// * **stops on `SIGTERM`**, which is the signal every supervisor actually sends;
    /// * **takes the origins you publish it at**, because the allowlist derived from a
    ///   wildcard bind is one no browser ever sends.
    ///
    /// Replicas share nothing and stick to nothing, so the count is yours to pick. What they
    /// must share is the same manifest bytes, and `/healthz` reports the digest of the ones
    /// this replica holds so a rollout can check that they agree.
    Serve {
        app: PathBuf,
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// The address to bind. Every interface by default — the opposite of `run`, and for
        /// the opposite reason.
        #[arg(long, default_value = "0.0.0.0")]
        bind: String,
        /// The probe listener's port. Not the app's, and **not one to publish**: leave it out
        /// of the Service, the target group or the upstream, and let only the supervisor reach
        /// it. Keeping it off the app's port is also what keeps that port's four endpoints
        /// four.
        #[arg(long, default_value_t = 9787)]
        admin_port: u16,
        /// The address the probe listener binds. Every interface, because a supervisor probes
        /// the pod's own address and not its loopback.
        #[arg(long, default_value = "0.0.0.0")]
        admin_bind: String,
        /// A public origin this app is reached at — scheme, host and port, no trailing slash.
        /// Repeatable.
        ///
        /// **A wildcard bind needs this or the app is inert.** The `Origin` allowlist is
        /// otherwise derived from the address bound, so `0.0.0.0:8787` allows exactly
        /// `http(s)://0.0.0.0:8787` — an origin no browser sends. The page then loads with
        /// `200` and the socket is refused with `403`, so the controls do nothing and nothing
        /// says why.
        #[arg(long = "origin", value_name = "URL")]
        origins: Vec<String>,
        /// Seconds to answer `/readyz` with `503` before the listener closes.
        ///
        /// Make it longer than the balancer's health interval times its unhealthy threshold:
        /// that product is how long the balancer may take to notice, and this is the window it
        /// has to notice in. Too short and the last connections routed here are reset; too long
        /// and every rollout pays it per replica.
        #[arg(long, value_name = "SECONDS", default_value_t = 5)]
        drain_seconds: u64,
        /// Seconds between pings on a connection that has gone quiet. `0` sends none.
        ///
        /// A load balancer reclaims a connection it has seen no bytes on — 60 seconds by
        /// default for an AWS ALB, an nginx `proxy_read_timeout` and most ingress
        /// controllers. **A dashboard nobody is clicking is the ordinary case, not an idle
        /// one**, so without this the page goes quiet after a minute of being read, with no
        /// error anywhere to say why. Raise the balancer's timeout as well, not instead.
        #[arg(long, value_name = "SECONDS", default_value_t = 25)]
        heartbeat_seconds: u64,
        #[command(flatten)]
        auth: AuthArgs,
    },

    /// Re-read the app's sources and print exactly what that cost.
    ///
    /// Two filters decide the answer, and the command shows both. A source's own cheap
    /// staleness check — a file's mtime and length, an `ETag`, a row count — decides whether
    /// it is read at all. The engine's digest of what came back decides whether any cell
    /// recomputes. So a source that was rewritten with identical content is read and then
    /// repaints nothing, and the pass reports `visited: 0` either way.
    Refresh {
        app: PathBuf,
        /// Read every source whatever its staleness check says. For the cases that check
        /// cannot cover — a file rewritten within its filesystem's timestamp granularity at
        /// the same length, a server that sends no validator, a table whose row count did
        /// not move.
        #[arg(long)]
        force: bool,
        /// Keep re-reading every this many seconds, printing a line per pass, until
        /// interrupted.
        ///
        /// Without it the command reads once — and on a freshly compiled app that is
        /// *always* the unchanged case, because compiling it read the sources a moment ago.
        /// That single reading proves the cheap path costs nothing and proves nothing about
        /// the other one. Watching is how a person sees a source actually move: edit the
        /// file, and the next pass names the cells that recomputed.
        #[arg(long, value_name = "SECONDS")]
        watch: Option<u64>,
        /// Machine-readable, for a CI gate that asserts on the numbers. One object per pass
        /// under `--watch`, newline-delimited.
        #[arg(long)]
        json: bool,
    },

    /// Serve every app in a directory from one process, routed by the `Host` header.
    ///
    /// `sales.toml` becomes the app `sales`, reachable at any name whose first label is
    /// `sales` — `sales.example.com`, `sales.localhost:8787`, or a bare `Host: sales`. One
    /// process, one port, one compiled graph per app however many viewers, and an explicit
    /// byte budget deciding which apps stay resident.
    ///
    /// Editing a manifest on disk changes its digest, so the next request compiles the new
    /// one and the old graph is evicted. There is no deploy step.
    ///
    /// Operated like `serve`: probes on a port of their own, a drain that goes unready before
    /// it stops accepting, and `SIGTERM`. **Readiness is whether this process can compile and
    /// serve, not whether every app it holds is healthy** — one broken manifest among four
    /// hundred is that app's error and not this replica's, and failing the probe on it would
    /// pull a working replica out of the pool to report a fault every replica shares.
    Host {
        /// A directory of `*.toml` manifests. A name that is not a DNS label is skipped and
        /// reported at start-up.
        dir: PathBuf,
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// The address to bind. Same warning as `run`, and it matters more here: this one
        /// serves several apps at once.
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        /// How many megabytes of loaded source data to hold across every app. Past it, the
        /// least recently opened app is evicted; an app larger than the whole budget is
        /// refused rather than admitted at every other app's expense.
        #[arg(long, default_value_t = 512)]
        budget_mb: u64,
        /// How many minutes an app may go unopened before the idle sweep drops it.
        ///
        /// The sweep runs at half this, so an app leaves somewhere between this and one and a
        /// half times it. `0` turns the sweep off, and then an app leaves residency only on a
        /// redeploy or under budget pressure — which is what every release before this one
        /// did, whatever this flag said.
        #[arg(long, default_value_t = 60)]
        idle_minutes: u64,
        /// The probe listener's port. Not the app port, and **not one to publish** — see
        /// `dagpane serve --help`.
        #[arg(long, default_value_t = 9787)]
        admin_port: u16,
        /// The address the probe listener binds.
        #[arg(long, default_value = "0.0.0.0")]
        admin_bind: String,
        /// Seconds to answer `/readyz` with `503` before the listener closes.
        #[arg(long, value_name = "SECONDS", default_value_t = 5)]
        drain_seconds: u64,
        /// Seconds between pings on a connection that has gone quiet. `0` sends none.
        #[arg(long, value_name = "SECONDS", default_value_t = 25)]
        heartbeat_seconds: u64,
        #[command(flatten)]
        auth: AuthArgs,
    },
}

/// The front door's flags, shared by `run` and `host`.
///
/// Flat rather than nested under one `--auth` value: OIDC genuinely has this many inputs,
/// and a single opaque string would hide which one an operator got wrong.
#[derive(Clone, Debug, clap::Args)]
pub struct AuthArgs {
    /// The provider's JWKS document, as a FILE.
    ///
    /// Giving this turns authentication on. It is a file and not a URL on purpose: this
    /// runtime does not make outbound requests, so there is no discovery to fail at
    /// start-up, nothing to redirect, and an air-gapped install works. Rotating keys is a
    /// copy and a restart.
    #[arg(long, value_name = "FILE")]
    pub auth_jwks: Option<String>,

    /// The `iss` every token must carry.
    #[arg(long, value_name = "URL")]
    pub auth_issuer: Option<String>,

    /// The `aud` every token must carry. Your provider mints tokens for every service you
    /// run; without this, any of them opens this one.
    #[arg(long, value_name = "AUD")]
    pub auth_audience: Option<String>,

    /// Signature algorithms to accept, comma-separated. Asymmetric only — `HS*` and `none`
    /// are not spellable here.
    #[arg(long, value_name = "LIST", default_value = "RS256,ES256")]
    pub auth_alg: String,

    /// A claim listing the app ids its holder may open; `*` means all of them.
    #[arg(long, value_name = "CLAIM")]
    pub auth_apps_claim: Option<String>,

    /// Let any valid token open anything this process serves. The right answer for a single
    /// app behind a door that already decided who may reach it.
    #[arg(long)]
    pub auth_any_app: bool,

    /// Clock skew allowed on `exp` and `nbf`, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = 5)]
    pub auth_leeway_secs: u64,

    /// Where to send a viewer to sign in. With the two below, the bundled page runs the
    /// PKCE flow itself; without them it says a token must arrive another way.
    #[arg(long, value_name = "URL")]
    pub auth_authorize_url: Option<String>,

    /// Where the page exchanges its code for a token.
    #[arg(long, value_name = "URL")]
    pub auth_token_url: Option<String>,

    /// This deployment's client id at the provider. Public: PKCE exists so a page needs no
    /// secret.
    #[arg(long, value_name = "ID")]
    pub auth_client_id: Option<String>,

    /// Scopes to ask for.
    #[arg(long, value_name = "SCOPES", default_value = "openid profile email")]
    pub auth_scope: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum GraphFormat {
    /// Indented, one cell per line, in evaluation order.
    Text,
    /// A `flowchart` block, for pasting into a document that renders one.
    Mermaid,
    /// Cells, edges and heights, for a tool.
    Json,
}
