//! Being one replica of however many, against real listeners.
//!
//! What a supervisor does to a process is not a thing a unit test can assert, so these bind
//! two real sockets, run the real [`dagpane_serve::serve_replica`], and ask it the questions a
//! kubelet and a load balancer ask. The one substitution is the trigger: `Lifecycle::
//! request_stop` stands in for `SIGTERM`, and it is the same code path from there — `kill(2)`
//! is not the property worth asserting, and what `/readyz` says **while the listener is still
//! open** is.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use dagpane_serve::lifecycle::State;
use dagpane_serve::{Identity, Lifecycle, Options};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MANIFEST: &str = "../../examples/sales.toml";

fn manifest_text() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(MANIFEST);
    std::fs::read_to_string(path).expect("the bundled example is in the tree")
}

fn app() -> Arc<dagpane_app::App> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(MANIFEST);
    Arc::new(dagpane_app::load(&path).expect("the bundled example compiles"))
}

/// One HTTP GET, written by hand.
///
/// A whole HTTP client as a dev-dependency to send four GETs would be a poor trade, and
/// `Connection: close` makes the response's end unambiguous without one.
async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .unwrap_or_else(|e| panic!("connecting to {addr}{path}: {e}"));
    let request = format!("GET {path} HTTP/1.1\r\nHost: probe\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {text:?}"));
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_string())
}

/// A replica on two ephemeral ports, running the real server.
struct Replica {
    app: SocketAddr,
    admin: SocketAddr,
    lifecycle: Arc<Lifecycle>,
    joined: tokio::task::JoinHandle<std::io::Result<()>>,
}

async fn start(drain: Duration, origins: Vec<String>) -> Replica {
    start_with(drain, origins, manifest_text()).await
}

async fn start_with(drain: Duration, origins: Vec<String>, manifest: String) -> Replica {
    let compiled = app();
    let identity = Identity::of("sales", &manifest, &compiled);
    let lifecycle = Arc::new(Lifecycle::new(identity, drain));

    // Port 0 for the same reason `tests/socket.rs` gives: these run concurrently, and handing
    // the bound listener over closes the window a rebind would open.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app_addr = listener.local_addr().unwrap();
    let admin_addr = admin_listener.local_addr().unwrap();

    let joined = tokio::spawn(dagpane_serve::serve_replica(
        compiled,
        listener,
        admin_listener,
        Options {
            app_name: "sales".to_string(),
            auth: None,
            origins,
            // No timer. These tests count frames on a socket they hold open across a drain,
            // and a ping landing in the middle is one more thing to skip past for no gain —
            // `tests/heartbeat.rs` is where the heartbeat is the subject.
            heartbeat: None,
        },
        Arc::clone(&lifecycle),
    ));

    for _ in 0..200 {
        if tokio::net::TcpStream::connect(admin_addr).await.is_ok()
            && tokio::net::TcpStream::connect(app_addr).await.is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    Replica {
        app: app_addr,
        admin: admin_addr,
        lifecycle,
        joined,
    }
}

#[tokio::test]
async fn a_replica_says_who_it_is_and_that_it_is_ready() {
    let r = start(Duration::from_millis(50), Vec::new()).await;

    let (status, body) = get(r.admin, "/readyz").await;
    assert_eq!(status, 200, "{body}");

    let (status, body) = get(r.admin, "/healthz").await;
    assert_eq!(status, 200);
    let health: serde_json::Value = serde_json::from_str(&body).expect("healthz is JSON");
    assert_eq!(health["state"], "serving");
    assert_eq!(health["ready"], true);
    assert_eq!(health["app"], "sales");
    // The counts the product claim is about, from the graph this replica actually compiled —
    // so a replica serving a different app is visible as a different pair of numbers even
    // before anyone compares digests.
    assert_eq!(health["cells"], 11);
    assert_eq!(health["panes"], 7);
    assert!(
        health["fingerprint"]
            .as_str()
            .is_some_and(|f| !f.is_empty()),
        "{body}"
    );

    r.lifecycle.request_stop();
    r.joined.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_draining_replica_is_unready_while_it_is_still_accepting() {
    // THE ordering property, and the reason this module exists. A replica that closed its
    // listener the moment it was signalled would still be in the balancer's pool — the
    // balancer finds out at its own pace, and every connection routed in the meantime is a
    // reset. Going unready FIRST and waiting is what makes a rolling update invisible.
    //
    // Mutation check: swapping the two halves of `Lifecycle::drain` — sleeping before setting
    // `Draining` — fails the 503 below. Dropping the sleep entirely fails the 200 after it,
    // because the listener is gone by the time this asks.
    let r = start(Duration::from_secs(2), Vec::new()).await;

    r.lifecycle.request_stop();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (status, body) = get(r.admin, "/readyz").await;
    assert_eq!(
        status, 503,
        "a draining replica must ask for no more work: {body}"
    );

    let (status, _) = get(r.app, "/").await;
    assert_eq!(
        status, 200,
        "a draining replica must still serve what is already being routed to it"
    );

    r.joined.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_draining_replica_is_still_alive_to_its_liveness_probe() {
    // Conflating the two probes is the mistake that turns a graceful stop into a `SIGKILL`: a
    // liveness probe that went red during a drain would have the supervisor conclude the
    // process had hung and kill it, which is exactly the ungraceful stop the drain exists to
    // avoid. Liveness answers *is this alive*; readiness answers *should it be sent work*.
    //
    // Mutation check: making `/healthz` return the same status as `/readyz` fails this.
    let r = start(Duration::from_secs(2), Vec::new()).await;

    r.lifecycle.request_stop();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (status, body) = get(r.admin, "/healthz").await;
    assert_eq!(status, 200, "liveness must not fail during a drain");
    let health: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(health["state"], "draining");
    assert_eq!(health["ready"], false);

    r.joined.await.unwrap().unwrap();
}

#[tokio::test]
async fn the_probes_outlive_the_drain_they_report() {
    // A supervisor has to be able to SEE the drain it asked for. Probes that died with the
    // signal would make every drain indistinguishable from a crash — the balancer would get a
    // connection refused rather than a 503, which is the same signal an unhealthy replica
    // gives, and the graceful stop would be graceful only on this side of the wire.
    //
    // Mutation check: giving the probe listener the same shutdown future as the app listener
    // fails this — the probe port is refused before the drain window is half over.
    let r = start(Duration::from_secs(2), Vec::new()).await;
    r.lifecycle.request_stop();

    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (status, _) = get(r.admin, "/readyz").await;
        assert_eq!(
            status, 503,
            "the probe listener answered for the whole drain"
        );
    }

    r.joined.await.unwrap().unwrap();
}

#[tokio::test]
async fn both_listeners_are_closed_once_the_replica_has_stopped() {
    let r = start(Duration::from_millis(100), Vec::new()).await;
    r.lifecycle.request_stop();
    r.joined.await.unwrap().unwrap();

    assert_eq!(r.lifecycle.state(), State::Stopped);
    // A probe port left listening after the app port closed is a replica a supervisor still
    // believes in.
    assert!(
        tokio::net::TcpStream::connect(r.admin).await.is_err(),
        "the probe listener outlived the process it reports on"
    );
    assert!(tokio::net::TcpStream::connect(r.app).await.is_err());
}

#[tokio::test]
async fn a_replica_that_has_stopped_does_not_report_itself_draining_again() {
    // The state only ever moves forward. `drain` is public, so "something asked to stop" can
    // arrive after the listeners have closed — a supervisor sending a second `SIGTERM`
    // because the first appeared to do nothing, which is what a drain looks like from
    // outside. A replica that went back to `draining` would have that supervisor waiting on a
    // drain that was over.
    //
    // Mutation check: `send_replace(State::Draining)` in place of the guard fails this.
    let r = start(Duration::from_millis(100), Vec::new()).await;
    r.lifecycle.request_stop();
    r.joined.await.unwrap().unwrap();
    assert_eq!(r.lifecycle.state(), State::Stopped);

    r.lifecycle.drain().await;
    assert_eq!(
        r.lifecycle.state(),
        State::Stopped,
        "a stopped replica went back to draining"
    );
}

#[tokio::test]
async fn the_app_port_serves_four_endpoints_and_the_probes_are_not_among_them() {
    // The probes are on a listener of their own so that this stays true: the app's port is
    // what an ingress publishes, and a health surface on it is a fifth endpoint on the public
    // side and a collision with the renderer route's namespace on ours.
    let r = start(Duration::from_millis(50), Vec::new()).await;

    for path in ["/healthz", "/readyz"] {
        let (status, _) = get(r.app, path).await;
        assert_eq!(status, 404, "{path} answered on the app's own port");
    }
    // And the four that are there. `/ws` without an upgrade is not a 404, which is all this
    // needs to show: the route exists.
    assert_eq!(get(r.app, "/").await.0, 200);
    assert_eq!(get(r.app, "/auth").await.0, 200);
    assert_ne!(get(r.app, "/ws").await.0, 404);

    r.lifecycle.request_stop();
    r.joined.await.unwrap().unwrap();
}

#[tokio::test]
async fn replicas_of_one_manifest_report_one_fingerprint() {
    // The check a rollout runs: collect `/healthz` from every replica and count the distinct
    // digests. One means the rollout finished. Two means two apps are being served under one
    // name — the failure `OPERATIONS.md` names and which previously had no symptom but
    // viewers disagreeing with each other.
    let text = manifest_text();
    let a = start_with(Duration::from_millis(50), Vec::new(), text.clone()).await;
    let b = start_with(Duration::from_millis(50), Vec::new(), text.clone()).await;

    // A comment-only edit: the same app, compiled identically, deployed from different bytes.
    // The digest is of the bytes, so this is a different deployment — and that is the answer
    // a rollout wants, because it cannot know the edit was cosmetic.
    let edited = format!("# a one-line note nobody reads\n{text}");
    let c = start_with(Duration::from_millis(50), Vec::new(), edited).await;

    let mut seen = Vec::new();
    for r in [&a, &b, &c] {
        let (_, body) = get(r.admin, "/healthz").await;
        let health: serde_json::Value = serde_json::from_str(&body).unwrap();
        seen.push(health["fingerprint"].as_str().unwrap().to_string());
    }
    assert_eq!(seen[0], seen[1], "two replicas of one manifest disagreed");
    assert_ne!(
        seen[0], seen[2],
        "a replica holding different bytes reported the same identity"
    );

    for r in [a, b, c] {
        r.lifecycle.request_stop();
        r.joined.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn a_declared_origin_replaces_the_one_derived_from_the_bound_address() {
    // The trap `OPERATIONS.md` spends a section on: the allowlist is otherwise the address
    // bound, so a container's wildcard bind allows an origin no browser sends, the page loads
    // with 200, the upgrade is refused with 403, and the controls do nothing.
    //
    // Replacing rather than adding is the safe direction, and the last assertion is the one
    // that says so: once an operator has named the origins, the bound address is not one of
    // them.
    let r = start(
        Duration::from_millis(50),
        vec!["https://panels.example.com".to_string()],
    )
    .await;

    async fn upgrade(addr: SocketAddr, origin: &str) -> u16 {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let request = format!(
            "GET /ws HTTP/1.1\r\nHost: probe\r\nOrigin: {origin}\r\nConnection: Upgrade\r\n\
             Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buf = [0u8; 64];
        let n = stream.read(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf[..n])
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap()
    }

    assert_eq!(upgrade(r.app, "https://panels.example.com").await, 101);
    assert_eq!(upgrade(r.app, "https://evil.example").await, 403);
    assert_eq!(
        upgrade(r.app, &format!("http://{}", r.app)).await,
        403,
        "declaring origins must replace the derived allowlist, not extend it"
    );

    r.lifecycle.request_stop();
    r.joined.await.unwrap().unwrap();
}
