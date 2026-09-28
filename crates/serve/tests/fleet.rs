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
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
        // No timer: these tests count HTTP answers, not frames.
        None,
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
async fn the_sweep_drops_an_app_nobody_opened() {
    // `Host::sweep_idle` has existed since `crates/host` did and **nothing in the shipped
    // binary had ever called it**, so `--idle-minutes` configured a sweep that never ran and
    // an app left residency only on a redeploy or under budget pressure. `OPERATIONS.md` said
    // so in its own list of gaps. This is that caller, running.
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
