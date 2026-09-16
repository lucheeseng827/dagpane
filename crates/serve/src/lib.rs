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
//! **A session is a connection.** Opening the page creates an [`AppSession`]; closing the
//! tab discards it. There is no session store, no eviction policy, no TTL and no
//! reconnection token, because there is no state to lose that cannot be recomputed from the
//! app's defaults in one pass — and a store that exists before anybody needs it is a store
//! whose eviction bug ships before its feature does. Durable, resumable sessions are on the
//! roadmap with the thing that would require them (authentication), not before.
//!
//! What that model *does* buy, and it is the part that matters: the app — the graph, the
//! compiled pipelines, and every loaded source — is one `Arc<App>` behind every connection.
//! A hundred viewers of a 600-row app are a hundred slot vectors over one table, not a
//! hundred copies of it, and `crates/core/tests/oracle.rs` asserts the pointer identity that
//! makes that true rather than measuring RSS and hoping.
//!
//! # Security posture
//!
//! There is **no authentication in v0.1.0**. The server binds `127.0.0.1` by default and
//! the CLI prints a warning when told to bind anything else. See `SECURITY.md`.

#![forbid(unsafe_code)]
#![deny(missing_debug_implementations)]
#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header::ORIGIN, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use dagpane_app::{App, AppSession, ClientMessage, PassStats, ServerMessage};

/// The bundled client: one file, no build step, no package manager, nothing fetched from a
/// network at run time.
///
/// A framework whose "hello world" starts with `npm install` has a different first five
/// minutes than one whose binary already contains the page, and the difference is most of
/// the adoption. The cost is a small hand-written renderer with no chart library, which is
/// why the chart panes are deliberately two shapes and not twenty.
const CLIENT: &str = include_str!("client.html");

#[derive(Debug)]
struct Shared {
    app: Arc<App>,
    /// Browser origins allowed to open the socket. Derived from the bound address.
    allowed_origins: Vec<String>,
}

/// Serve `app` on `addr` until the process is asked to stop.
pub async fn serve(app: Arc<App>, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_on(app, listener).await
}

/// Serve `app` on a listener the caller already bound.
///
/// This exists for tests. Binding port 0 to find a free port and then dropping the listener
/// so `serve` can bind the same address again is a race: between the two binds, anything else
/// on the machine — including another test in the same run — can take the port. Handing the
/// bound listener straight over closes the window.
pub async fn serve_on(app: Arc<App>, listener: tokio::net::TcpListener) -> std::io::Result<()> {
    let addr = listener.local_addr()?;
    let shared = Arc::new(Shared {
        app,
        allowed_origins: origins_for(addr),
    });
    let router = Router::new()
        .route("/", get(page))
        .route("/ws", get(socket))
        .with_state(shared);

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown())
        .await
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn page() -> impl IntoResponse {
    Html(CLIENT)
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
/// suppress the header. This is an anti-CSRF measure, not authentication; there is still no
/// authentication in this version, and `SECURITY.md` says so.
async fn socket(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
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
    ws.on_upgrade(move |socket| connection(socket, shared))
}

/// One viewer, start to finish.
///
/// Deliberately sequential: a message is read, a pass runs, a patch goes out, and only then
/// is the next message read. A data app's interactions are a person moving a control, so
/// concurrency inside one connection would buy nothing and would cost the guarantee that
/// the viewer's controls and their page describe the same epoch.
async fn connection(mut socket: WebSocket, shared: Arc<Shared>) {
    let started = Instant::now();
    let (mut session, first) = AppSession::open(Arc::clone(&shared.app));
    let init = session.init_message(&first, Some(started.elapsed().as_micros() as u64));
    if send(&mut socket, &init).await.is_err() {
        return;
    }

    while let Some(Ok(message)) = socket.recv().await {
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
    let (trace, panes) = session.commit();
    let micros = started.elapsed().as_micros() as u64;
    ServerMessage::Patch {
        seq,
        panes,
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
        // The claim is "no build step and nothing fetched at run time". A `<script src=`,
        // a stylesheet `href`, an `@import` or a `fetch` would quietly break it, and an app
        // served on an air-gapped host would then render blank.
        //
        // Two strings starting `http` are allowed, and both are spelled out rather than
        // excluded by a looser pattern, so that a third one appearing here fails this test:
        //
        //   * `https:` — comparing `location.protocol`, to pick `ws://` or `wss://`;
        //   * the SVG namespace, which `createElementNS` requires and which no browser has
        //     ever dereferenced.
        assert!(CLIENT.to_ascii_lowercase().contains("<!doctype html>"));
        for forbidden in [
            "script src=",
            "@import",
            "fetch(",
            "//cdn.",
            "//unpkg",
            "importScripts",
        ] {
            assert!(
                !CLIENT.contains(forbidden),
                "the client must not contain `{forbidden}`"
            );
        }
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
            vec!["https:", "http://www.w3.org/2000/svg"],
            "the client gained a URL it did not have"
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
