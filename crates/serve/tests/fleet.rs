//! A hosting process, operated: what it says it is, what readiness means when it holds many
//! apps, and the sweep that nothing had ever called.
//!
//! `tests/replica.rs` asks these questions of one app. The answers are mostly the same, and
//! the one that is not is the interesting one — see
//! `an_app_that_cannot_compile_does_not_make_the_replica_unready`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use dagpane_host::{AppId, Budget, Host, MemAppSource};
use dagpane_serve::lifecycle::State;
use dagpane_serve::{Fleet, Lifecycle};
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

fn manifest(title: &str) -> String {
    format!(
        r#"
[app]
title = "{title}"

[[source]]
name = "sales"
csv = "sales.csv"

[[cell]]
name = "total"
from = "sales"
[[cell.step]]
group_by = {{ agg = [{{ column = "amount", agg = "sum", as = "t" }}] }}
[[cell.step]]
scalar = {{ column = "t" }}

[[pane]]
cell = "total"
metric = {{ label = "Total" }}
"#
    )
}

fn write_csv(dir: &std::path::Path) {
    let mut text = String::from("region,amount\n");
    for i in 0..40 {
        text.push_str(&format!("r{},{}.0\n", i % 4, (i % 17) * 10));
    }
    std::fs::write(dir.join("sales.csv"), text).unwrap();
}

async fn get(addr: SocketAddr, path: &str, host_header: &str) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {text:?}"));
    (
        status,
        text.split_once("\r\n\r\n")
            .map(|(_, b)| b)
            .unwrap_or("")
            .to_string(),
    )
}

struct Fixture {
    dir: tempfile::TempDir,
    source: Arc<MemAppSource>,
    apps: SocketAddr,
    admin: SocketAddr,
    host: Arc<Host>,
    lifecycle: Arc<Lifecycle>,
    joined: tokio::task::JoinHandle<std::io::Result<()>>,
}

async fn start(idle_after: Duration, drain: Duration) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    write_csv(dir.path());
    let source = Arc::new(MemAppSource::new());
    source.publish(
        "alpha.test",
        &AppId::parse("alpha").unwrap(),
        manifest("Alpha"),
        dir.path(),
    );
    source.publish(
        "beta.test",
        &AppId::parse("beta").unwrap(),
        manifest("Beta"),
        dir.path(),
    );
    // A manifest that compiles to nothing: it names a cell's input that does not exist, which
    // is a `check` failure and therefore a 500 for that app and nobody else.
    source.publish(
        "broken.test",
        &AppId::parse("broken").unwrap(),
        "[app]\ntitle = \"b\"\n[[cell]]\nname = \"a\"\nfrom = \"ghost\"\n".to_string(),
        dir.path(),
    );

    let host = Arc::new(Host::new(
        Arc::clone(&source) as Arc<dyn dagpane_host::AppSource>,
        Budget::new(64 << 20, idle_after),
    ));
    let lifecycle = Arc::new(Lifecycle::describing(
        Arc::new(Fleet::new(Arc::clone(&host))),
        drain,
    ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let apps = listener.local_addr().unwrap();
    let admin = admin_listener.local_addr().unwrap();

    let joined = tokio::spawn(dagpane_serve::serve_host_replica(
        Arc::clone(&host),
        listener,
        admin_listener,
        None,
        // No timer and no deadline: these tests count HTTP answers, not frames.
        dagpane_serve::Liveness {
            heartbeat: None,
            write_timeout: None,
            ping_timeout: None,
        },
        Arc::clone(&lifecycle),
    ));
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(admin).await.is_ok()
            && tokio::net::TcpStream::connect(apps).await.is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    Fixture {
        dir,
        source,
        apps,
        admin,
        host,
        lifecycle,
        joined,
    }
}

async fn health(admin: SocketAddr) -> serde_json::Value {
    let (status, body) = get(admin, "/healthz", "probe").await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).expect("healthz is JSON")
}

#[tokio::test]
async fn a_fleet_reports_what_it_is_holding_now() {
    // A single app's identity is fixed at start-up; a fleet's is not. Apps arrive on first
    // request and leave under budget pressure or the sweep, so this has to be asked per
    // request — a snapshot taken at start-up would describe a fleet the process has stopped
    // holding.
    //
    // Mutation check: describing once at construction and caching it fails the second half.
    let f = start(Duration::from_secs(3600), Duration::from_millis(50)).await;

    let before = health(f.admin).await;
    assert_eq!(before["kind"], "host");
    assert_eq!(
        before["apps"].as_array().map(|a| a.len()),
        Some(0),
        "nothing has been asked for yet: {before}"
    );
    assert_eq!(before["resident_bytes"], 0);

    assert_eq!(get(f.apps, "/", "alpha.test").await.0, 200);

    let after = health(f.admin).await;
    let apps = after["apps"].as_array().expect("an array");
    assert_eq!(apps.len(), 1, "{after}");
    assert_eq!(apps[0]["app"], "alpha");
    // The identity a rollout compares, per app rather than per process.
    assert!(
        apps[0]["manifest"].as_str().is_some_and(|d| !d.is_empty()),
        "{after}"
    );
    assert!(after["resident_bytes"].as_u64().unwrap_or(0) > 0, "{after}");

    f.lifecycle.request_stop();
    f.joined.await.unwrap().unwrap();
}

#[tokio::test]
async fn an_app_that_cannot_compile_does_not_make_the_replica_unready() {
    // THE decision this file exists for, and the tempting answer is the wrong one.
    //
    // A fleet of four hundred manifests where one is broken is not an unready replica: that
    // app's viewers get its error and the other three hundred and ninety-nine are fine.
    // Failing readiness on it would pull a working replica out of the pool to report a fault
    // that EVERY replica has, for the same reason, at the same moment — so every replica goes
    // unready together and one bad file takes the whole service down.
    //
    // Mutation check: making `/readyz` walk the fleet and fail on any app that will not
    // compile fails this.
    let f = start(Duration::from_secs(3600), Duration::from_millis(50)).await;

    // Two good apps answer.
    assert_eq!(get(f.apps, "/", "alpha.test").await.0, 200);
    assert_eq!(get(f.apps, "/", "beta.test").await.0, 200);

    // The broken one does not, and says so at ITS own address.
    let (status, _) = get(f.apps, "/", "broken.test").await;
    assert_ne!(
        status, 200,
        "a manifest that cannot compile must not answer 200"
    );

    // And the replica is still ready, because it is still able to serve.
    let (ready, body) = get(f.admin, "/readyz", "probe").await;
    assert_eq!(
        ready, 200,
        "one broken manifest took a replica holding two good apps out of the pool: {body}"
    );
    assert_eq!(health(f.admin).await["ready"], true);

    f.lifecycle.request_stop();
    f.joined.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_draining_fleet_is_unready_while_it_is_still_serving() {
    // The same ordering property `tests/replica.rs` asserts for one app, because the thing
    // being drained is the same thing: connections, which belong to no app in particular.
    let f = start(Duration::from_secs(3600), Duration::from_secs(2)).await;
    assert_eq!(get(f.apps, "/", "alpha.test").await.0, 200);

    f.lifecycle.request_stop();
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(get(f.admin, "/readyz", "probe").await.0, 503);
    assert_eq!(get(f.admin, "/healthz", "probe").await.0, 200);
    assert_eq!(
        get(f.apps, "/", "alpha.test").await.0,
        200,
        "a draining fleet must still serve what is already routed to it"
    );

    f.joined.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_draining_fleet_tells_its_viewers_it_is_going_away_once_the_drain_ends() {
    // `tests/replica.rs` has the reasoning; this is the same stop under `serve_host_replica`,
    // whose sockets `axum` stops tracking at the upgrade just the same — and two viewers on
    // two apps, because what is closed is every connection, not every connection to one app.
    let drain = Duration::from_millis(300);
    let f = start(Duration::from_secs(3600), drain).await;
    let mut alpha = viewer(f.apps, "alpha.test").await;
    let mut beta = viewer(f.apps, "beta.test").await;

    f.lifecycle.request_stop();
    let asked = tokio::time::Instant::now();
    for (name, socket) in [("alpha", &mut alpha), ("beta", &mut beta)] {
        let closed = tokio::time::timeout(drain + dagpane_serve::CLOSE_GRACE, async {
            loop {
                match socket.next().await {
                    Some(Ok(Message::Close(frame))) => break frame.map(|f| u16::from(f.code)),
                    Some(Ok(_)) => continue,
                    None | Some(Err(_)) => break None,
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{name}'s viewer was never closed"));
        assert_eq!(closed, Some(dagpane_serve::GOING_AWAY), "{name}");
        assert!(
            asked.elapsed() >= drain,
            "{name} was closed inside the drain"
        );
        // Answered, as a browser answers it.
        assert!(socket.next().await.is_none(), "{name}");
    }

    let done = tokio::time::timeout(dagpane_serve::CLOSE_GRACE, f.joined).await;
    assert!(
        matches!(done, Ok(Ok(Ok(())))),
        "the fleet did not return once its viewers had closed: {done:?}"
    );
}

#[tokio::test]
async fn the_sweep_drops_an_app_nobody_opened() {
    // `Host::sweep_idle` existed from the day `crates/host` did and **nothing in the binary
    // called it** until `dagpane_serve::sweep_idle` did, so `--idle-minutes` configured a sweep
    // that never ran and an app left residency only on a redeploy or under budget pressure. No
    // release had `host` before 0.2.0, which ships with the caller. This is that caller, running.
    //
    // Mutation check: not spawning the sweep leaves the app resident and fails this.
    let f = start(Duration::from_millis(150), Duration::from_millis(50)).await;

    assert_eq!(get(f.apps, "/", "alpha.test").await.0, 200);
    assert_eq!(f.host.resident().len(), 1);

    let sweeping = tokio::spawn(dagpane_serve::sweep_idle(
        Arc::clone(&f.host),
        Duration::from_millis(60),
        Arc::clone(&f.lifecycle),
    ));

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !f.host.resident().is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        f.host.resident().is_empty(),
        "an app nobody opened stayed resident past its idle window"
    );
    // And it is on the record, which is the other half of the same gap: the log was in memory
    // and nothing read it.
    assert!(
        f.host
            .evictions()
            .iter()
            .any(|e| e.key.app_id.as_str() == "alpha"),
        "the eviction was not recorded"
    );

    f.lifecycle.request_stop();
    f.joined.await.unwrap().unwrap();
    sweeping.await.unwrap();
}

#[tokio::test]
async fn the_sweep_stops_when_the_process_does() {
    // Evicting for idleness while the process is on its way out is work nobody wants done and
    // a log line in the middle of a drain. The task must end on its own rather than be left
    // running against a host nothing is serving.
    //
    // Mutation check: dropping the `until_stopped` branch makes this hang.
    let f = start(Duration::from_secs(3600), Duration::from_millis(50)).await;
    let sweeping = tokio::spawn(dagpane_serve::sweep_idle(
        Arc::clone(&f.host),
        Duration::from_millis(50),
        Arc::clone(&f.lifecycle),
    ));

    f.lifecycle.request_stop();
    f.joined.await.unwrap().unwrap();
    assert_eq!(f.lifecycle.state(), State::Stopped);

    tokio::time::timeout(Duration::from_secs(5), sweeping)
        .await
        .expect("the sweep ended with the process")
        .unwrap();
}

#[tokio::test]
async fn two_deployments_of_one_app_are_two_digests_and_then_one() {
    // A redeploy under `host` is an edit on disk. The new bytes are a new key, the old graph
    // is evicted, and `/healthz` is where a rollout can watch that happen rather than infer it.
    let f = start(Duration::from_secs(3600), Duration::from_millis(50)).await;
    assert_eq!(get(f.apps, "/", "alpha.test").await.0, 200);
    let first = health(f.admin).await["apps"][0]["manifest"]
        .as_str()
        .unwrap()
        .to_string();

    // The same app, different bytes — which under `host` is what a deploy *is*.
    // `MemAppSource` is the seam a directory sits behind, so republishing into it is an edit
    // on disk with no filesystem in the way of the test.
    f.source.publish(
        "alpha.test",
        &AppId::parse("alpha").unwrap(),
        manifest("Alpha v2"),
        f.dir.path(),
    );
    assert_eq!(get(f.apps, "/", "alpha.test").await.0, 200);

    let after = health(f.admin).await;
    let apps = after["apps"].as_array().unwrap();
    assert_eq!(apps.len(), 1, "the old deployment was not evicted: {after}");
    assert_ne!(
        apps[0]["manifest"].as_str().unwrap(),
        first,
        "a redeploy reported the digest it replaced"
    );

    f.lifecycle.request_stop();
    f.joined.await.unwrap().unwrap();
}

// ── what the viewers hold ─────────────────────────────────────────────────────────────────

/// A pipeline whose one cell keeps rows, so a viewer holds something of their own. The
/// fixture's other apps aggregate to a scalar, and a viewer of those correctly holds nothing —
/// true, and useless for telling a report that works from one that always says zero.
const ROW_KEEPING: &str = r#"
[app]
title = "Rows"
[[source]]
name = "sales"
csv = "rows.csv"
[[input]]
name = "min_amount"
slider = { min = 0.0, max = 1000.0, step = 10.0, default = 0.0 }
[[cell]]
name = "kept"
from = "sales"
[[cell.step]]
filter = { column = "amount", op = "ge", param = "min_amount" }
[[pane]]
cell = "kept"
table = { max_rows = 10 }
"#;

/// Its own CSV, and two thousand rows of it: at the fixture's forty, the arrays' fixed
/// bookkeeping is most of what a viewer holds, and filtering rows away barely moves the total.
fn publish_rows(f: &Fixture, title: &str) {
    let mut text = String::from("region,amount\n");
    for i in 0..2_000 {
        text.push_str(&format!("r{},{}.0\n", i % 4, (i % 17) * 10));
    }
    std::fs::write(f.dir.path().join("rows.csv"), text).unwrap();
    f.source.publish(
        "rows.test",
        &AppId::parse("rows").unwrap(),
        ROW_KEEPING.replace("\"Rows\"", &format!("\"{title}\"")),
        f.dir.path(),
    );
}

async fn viewer(
    addr: SocketAddr,
    name: &str,
) -> tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> {
    // tungstenite sets `Host` from the URL's authority, so the app is chosen by name while the
    // connection goes to the address — which is what a proxy in front of this does.
    let request = format!("ws://{name}/ws").into_client_request().unwrap();
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut socket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .unwrap();
    // `init` is sent after the first pass is recorded, so once it arrives the host knows.
    loop {
        match socket.next().await.expect("socket closed early").unwrap() {
            Message::Text(text) => {
                let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(frame["type"], "init", "{frame}");
                return socket;
            }
            _ => continue,
        }
    }
}

/// The server learns a socket closed when its next read fails, which is not synchronous with
/// the client dropping it.
async fn until_sessions(admin: SocketAddr, count: u64) -> serde_json::Value {
    for _ in 0..200 {
        let h = health(admin).await;
        if h["sessions"] == count {
            return h;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never reached {count} sessions: {}", health(admin).await);
}

fn entry<'a>(health: &'a serde_json::Value, list: &str, app: &str) -> &'a serde_json::Value {
    health[list]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["app"] == app)
        .unwrap_or_else(|| panic!("{app} is not in {list}: {health}"))
}

#[tokio::test]
async fn a_fleet_reports_what_its_viewers_hold_and_stops_when_they_leave() {
    let f = start(Duration::from_secs(3600), Duration::from_millis(50)).await;
    publish_rows(&f, "Rows");
    assert_eq!(health(f.admin).await["sessions"], 0);

    let socket = viewer(f.apps, "rows.test").await;
    let during = health(f.admin).await;
    assert_eq!(during["sessions"], 1);
    let rows = entry(&during, "apps", "rows");
    assert_eq!(rows["sessions"], 1);
    let held = rows["session_bytes"].as_u64().unwrap();
    assert!(
        held > 0,
        "a viewer holding 2 000 rows holds nothing? {during}"
    );
    assert_eq!(during["session_bytes"], held);
    // Reported beside the budget's figure, never folded into it.
    let sources: u64 = during["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["bytes"].as_u64().unwrap())
        .sum();
    assert_eq!(during["resident_bytes"], sources);

    // Move the slider so most rows fall away. The figure follows the pass, not the connect:
    // a host that recorded only the first render would go on reporting every row.
    let mut socket = socket;
    use futures_util::SinkExt;
    socket
        .send(Message::Text(
            serde_json::json!({"type": "set", "seq": 1,
                "values": {"min_amount": {"kind": "float", "v": 140.0}}})
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    loop {
        if let Message::Text(text) = socket.next().await.expect("open").unwrap() {
            let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(frame["type"], "patch", "{frame}");
            break;
        }
    }
    let filtered = entry(&health(f.admin).await, "apps", "rows")["session_bytes"]
        .as_u64()
        .unwrap();
    assert!(
        filtered < held / 2,
        "after filtering most rows away the viewer still holds {filtered} of {held}"
    );

    drop(socket);
    let after = until_sessions(f.admin, 0).await;
    assert_eq!(after["session_bytes"], 0);
    assert_eq!(entry(&after, "apps", "rows")["sessions"], 0);

    f.lifecycle.request_stop();
    f.joined.await.unwrap().unwrap();
}

/// What the budget cannot see, made visible. A redeploy evicts the old version at once and
/// `resident_bytes` stops counting it, but a viewer still on it holds its graph — and through
/// the graph, its sources.
#[tokio::test]
async fn a_viewer_on_a_redeployed_version_is_reported_until_they_leave() {
    let f = start(Duration::from_secs(3600), Duration::from_millis(50)).await;
    publish_rows(&f, "Rows");
    let socket = viewer(f.apps, "rows.test").await;
    let old = entry(&health(f.admin).await, "apps", "rows")["manifest"]
        .as_str()
        .unwrap()
        .to_string();

    publish_rows(&f, "Rows v2");
    // A fresh request resolves the new digest and evicts the old one.
    assert_eq!(get(f.apps, "/", "rows.test").await.0, 200);

    let h = health(f.admin).await;
    let resident = entry(&h, "apps", "rows");
    assert_ne!(resident["manifest"], old.as_str(), "{h}");
    assert_eq!(resident["sessions"], 0, "nobody is on the new version yet");
    let held = entry(&h, "held_after_eviction", "rows");
    assert_eq!(held["manifest"], old.as_str());
    assert_eq!(held["sessions"], 1);
    assert!(
        held["bytes"].as_u64().unwrap() > 0,
        "its sources are still alive: {h}"
    );

    drop(socket);
    let after = until_sessions(f.admin, 0).await;
    assert!(
        after["held_after_eviction"].as_array().unwrap().is_empty(),
        "{after}"
    );

    f.lifecycle.request_stop();
    f.joined.await.unwrap().unwrap();
}

/// The rollback version of the same gap, and the one a key alone cannot see. Deploy, roll
/// back, and the original manifest is the original key again — compiled again, with a second
/// copy of its sources — while a viewer who never reloaded is still on the first copy. Booked
/// by key, they would be counted as a viewer of the reloaded copy, and the copy they actually
/// hold would be in neither list.
#[tokio::test]
async fn a_viewer_who_sat_through_a_rollback_is_on_the_copy_they_opened() {
    let f = start(Duration::from_secs(3600), Duration::from_millis(50)).await;
    publish_rows(&f, "Rows");
    let stayed = viewer(f.apps, "rows.test").await;
    let original = entry(&health(f.admin).await, "apps", "rows")["manifest"]
        .as_str()
        .unwrap()
        .to_string();

    // Deploy, then roll back to the very same bytes, each opened once.
    publish_rows(&f, "Rows v2");
    assert_eq!(get(f.apps, "/", "rows.test").await.0, 200);
    publish_rows(&f, "Rows");
    assert_eq!(get(f.apps, "/", "rows.test").await.0, 200);

    let h = health(f.admin).await;
    let resident = entry(&h, "apps", "rows");
    assert_eq!(
        resident["manifest"],
        original.as_str(),
        "a rollback is the original key: {h}"
    );
    assert_eq!(
        resident["sessions"], 0,
        "nobody has opened the reloaded copy: {h}"
    );
    let held = entry(&h, "held_after_eviction", "rows");
    assert_eq!(held["manifest"], original.as_str());
    assert_eq!(held["sessions"], 1);
    assert_eq!(
        held["bytes"], resident["bytes"],
        "a second copy of the same sources: {h}"
    );

    drop(stayed);
    let after = until_sessions(f.admin, 0).await;
    assert!(
        after["held_after_eviction"].as_array().unwrap().is_empty(),
        "{after}"
    );

    f.lifecycle.request_stop();
    f.joined.await.unwrap().unwrap();
}
