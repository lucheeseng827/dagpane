//! Scheduled refresh over real sockets: the exit criterion the plan set for it.
//!
//! > With two sessions open, a refresh that changes the source repaints both; a refresh that
//! > does not changes neither.
//!
//! Stated here as frames on the wire, because that is what a viewer sees: a `reloaded` frame
//! carrying exactly the panes that moved in *their* session, at *their* filter — and, when the
//! rows came back the same, no frame at all. `refresh_secs = 1` is the finest a manifest can
//! say, so each of these waits on a real clock for a second or two.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use dagpane_host::{AppId, Budget, Host, MemAppSource};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

type Socket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

/// `total` reads the rows at the viewer's floor; `floor` reads only the control. No refresh of
/// `sales` may ever repaint the second.
fn manifest(refresh: bool) -> String {
    let every = if refresh { "refresh_secs = 1" } else { "" };
    format!(
        r#"
[app]
title = "Live"

[[source]]
name = "sales"
csv = "sales.csv"
{every}

[[input]]
name = "min_amount"
slider = {{ min = 0.0, max = 1000.0, step = 10.0, default = 0.0 }}

[[cell]]
name = "big"
from = "sales"
[[cell.step]]
filter = {{ column = "amount", op = "ge", param = "min_amount" }}

[[cell]]
name = "total"
from = "big"
[[cell.step]]
group_by = {{ agg = [{{ column = "amount", agg = "sum", as = "t" }}] }}
[[cell.step]]
scalar = {{ column = "t" }}

[[cell]]
name = "floor"
from = "min_amount"

[[pane]]
id = "total"
cell = "total"
metric = {{ label = "Total" }}

[[pane]]
id = "floor"
cell = "floor"
metric = {{ label = "Floor" }}
"#
    )
}

/// `rows` rows whose amounts step by ten and never reach 1 000, so a viewer at the top of the
/// slider sees none of them whatever a refresh does.
fn csv(rows: usize) -> String {
    let mut text = String::from("region,amount\n");
    for i in 0..rows {
        text.push_str(&format!("r{},{}.0\n", i % 4, (i % 50) * 10));
    }
    text
}

fn write(dir: &std::path::Path, rows: usize) {
    // A rewrite has to move the file's version for the schedule to read it at all, and the
    // version is the length and the modification time. The pause keeps the time distinct even
    // on a filesystem that stamps coarsely.
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(dir.join("sales.csv"), csv(rows)).unwrap();
}

async fn wait_for(addr: SocketAddr) {
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the server never began accepting on {addr}");
}

/// `dagpane run`'s shape: one app, one process.
async fn serve_one(dir: &std::path::Path) -> SocketAddr {
    std::fs::write(dir.join("app.toml"), manifest(true)).unwrap();
    let app = Arc::new(dagpane_app::load(&dir.join("app.toml")).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let options = dagpane_serve::Options {
        // No pings: these tests read every frame and a ping is not one of the app's.
        heartbeat: None,
        ..dagpane_serve::Options::default()
    };
    tokio::spawn(async move {
        let _ = dagpane_serve::serve_on_with(app, listener, options).await;
    });
    wait_for(addr).await;
    addr
}

async fn open(addr: SocketAddr, name: &str) -> (Socket, serde_json::Value) {
    let request = format!("ws://{name}/ws").into_client_request().unwrap();
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut socket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .expect("the upgrade succeeds");
    let init = next(&mut socket).await;
    assert_eq!(init["type"], "init", "{init}");
    (socket, init)
}

async fn next(socket: &mut Socket) -> serde_json::Value {
    loop {
        match socket
            .next()
            .await
            .expect("the socket closed early")
            .unwrap()
        {
            Message::Text(text) => return serde_json::from_str(&text).expect("valid JSON"),
            Message::Close(_) => panic!("the server closed the socket"),
            _ => continue,
        }
    }
}

/// The next frame within `within`, or `None` if the server sent nothing.
async fn next_within(socket: &mut Socket, within: Duration) -> Option<serde_json::Value> {
    tokio::time::timeout(within, next(socket)).await.ok()
}

async fn set_floor(socket: &mut Socket, seq: u64, floor: f64) -> serde_json::Value {
    let message = serde_json::json!({
        "type": "set",
        "seq": seq,
        "values": { "min_amount": { "kind": "float", "v": floor } },
    });
    socket
        .send(Message::Text(message.to_string().into()))
        .await
        .unwrap();
    let reply = next(socket).await;
    assert_eq!(reply["type"], "patch", "{reply}");
    reply
}

/// A pane's rendered value from an `init` (`views`), `patch` or `reloaded` (`panes`) frame.
fn pane(frame: &serde_json::Value, id: &str) -> Option<String> {
    ["views", "panes"]
        .iter()
        .filter_map(|field| frame[*field].as_array())
        .flatten()
        .find(|u| u["id"] == id)
        .and_then(|u| u["view"]["value"].as_str().map(str::to_string))
}

/// What `total` should say for `rows` at `floor`, formatted as the server formats it.
fn expected_total(rows: usize, floor: f64) -> f64 {
    (0..rows)
        .map(|i| ((i % 50) * 10) as f64)
        .filter(|a| *a >= floor)
        .sum()
}

fn number(text: &str) -> f64 {
    text.replace([',', ' '], "")
        .parse()
        .unwrap_or_else(|_| panic!("{text:?} is not a number"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_change_repaints_every_viewer_at_their_own_filter_and_no_change_repaints_none() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), 100);
    let addr = serve_one(dir.path()).await;

    let (mut all, init) = open(addr, &addr.to_string()).await;
    assert_eq!(
        number(&pane(&init, "total").unwrap()),
        expected_total(100, 0.0)
    );
    let (mut big, _) = open(addr, &addr.to_string()).await;
    set_floor(&mut big, 1, 300.0).await;
    // A viewer at the top of the slider sees no rows, before the refresh or after it.
    let (mut none, _) = open(addr, &addr.to_string()).await;
    set_floor(&mut none, 1, 1000.0).await;

    // Let the first check — made the moment the first viewer arrived — find the file as it is,
    // so the change below is the one the next tick reads.
    tokio::time::sleep(Duration::from_millis(300)).await;
    write(dir.path(), 160);

    for (socket, floor) in [(&mut all, 0.0), (&mut big, 300.0)] {
        let frame = next_within(socket, Duration::from_secs(5))
            .await
            .expect("a viewer was not told the data moved");
        assert_eq!(frame["type"], "reloaded", "{frame}");
        assert!(
            frame.get("seq").is_none(),
            "an unasked frame carries a seq: {frame}"
        );
        assert_eq!(frame["sources"], serde_json::json!(["sales"]));
        assert_eq!(
            number(&pane(&frame, "total").expect("the total moved and was not sent")),
            expected_total(160, floor),
            "at floor {floor}"
        );
        assert!(
            pane(&frame, "floor").is_none(),
            "a pane that reads only the control was repainted by a refresh: {frame}"
        );
        let stats = &frame["stats"];
        assert!(
            stats["visited"].as_u64().unwrap() < stats["total_cells"].as_u64().unwrap(),
            "the refresh visited every cell: {stats}"
        );
    }
    // Nothing this viewer can see moved, so nothing was sent to them.
    assert_eq!(
        next_within(&mut none, Duration::from_millis(1500)).await,
        None,
        "a viewer whose panes did not move was sent a frame"
    );

    // The same rows again: the file is read — its version moved — and nobody hears about it.
    write(dir.path(), 160);
    for socket in [&mut all, &mut big] {
        assert_eq!(
            next_within(socket, Duration::from_millis(2500)).await,
            None,
            "rows identical to the last ones repainted a page"
        );
    }

    // And a viewer's own interaction after all that is answered as it always was.
    let patch = set_floor(&mut all, 1, 100.0).await;
    assert_eq!(
        number(&pane(&patch, "total").unwrap()),
        expected_total(160, 100.0)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_viewer_who_arrives_after_a_refresh_opens_on_the_new_rows() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), 100);
    let addr = serve_one(dir.path()).await;

    // The schedule runs while somebody is looking, so somebody looks.
    let (mut first, _) = open(addr, &addr.to_string()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    write(dir.path(), 140);
    let frame = next_within(&mut first, Duration::from_secs(5))
        .await
        .expect("the refresh never arrived");
    assert_eq!(frame["type"], "reloaded");

    let (_later, init) = open(addr, &addr.to_string()).await;
    assert_eq!(
        number(&pane(&init, "total").unwrap()),
        expected_total(140, 0.0),
        "a viewer arriving after the refresh was shown the rows before it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rows_with_new_columns_are_refused_and_nobody_is_repainted() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), 100);
    let addr = serve_one(dir.path()).await;
    let (mut viewer, _) = open(addr, &addr.to_string()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(
        dir.path().join("sales.csv"),
        csv(160).replacen("region,amount", "region,revenue", 1),
    )
    .unwrap();
    assert_eq!(
        next_within(&mut viewer, Duration::from_millis(2500)).await,
        None,
        "rows the app was never checked against reached a viewer"
    );

    // Put right, the next tick reads it.
    write(dir.path(), 160);
    let frame = next_within(&mut viewer, Duration::from_secs(5))
        .await
        .expect("a corrected file was never read");
    assert_eq!(
        number(&pane(&frame, "total").unwrap()),
        expected_total(160, 0.0)
    );
}

// ── many apps on one port ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn under_host_a_refresh_reaches_that_apps_viewers_and_nobody_elses() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), 100);
    let source = Arc::new(MemAppSource::new());
    source.publish(
        "live.test",
        &AppId::parse("live").unwrap(),
        manifest(true),
        dir.path(),
    );
    source.publish(
        "still.test",
        &AppId::parse("still").unwrap(),
        manifest(false),
        dir.path(),
    );
    let host = Arc::new(Host::new(
        source,
        Budget::new(64 << 20, Duration::from_secs(3600)),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = Arc::clone(&host);
    tokio::spawn(async move {
        let _ = dagpane_serve::serve_host_on(served, listener).await;
    });
    wait_for(addr).await;

    let (mut live, _) = open(addr, "live.test").await;
    let (mut still, _) = open(addr, "still.test").await;
    let before = host
        .resident()
        .iter()
        .find(|s| s.key.app_id.as_str() == "live")
        .unwrap()
        .footprint
        .source_bytes;
    tokio::time::sleep(Duration::from_millis(300)).await;
    write(dir.path(), 400);

    let frame = next_within(&mut live, Duration::from_secs(5))
        .await
        .expect("the refreshing app's viewer was not told");
    assert_eq!(frame["type"], "reloaded");
    assert_eq!(
        number(&pane(&frame, "total").unwrap()),
        expected_total(400, 0.0)
    );
    // The same CSV behind an app that declared no schedule: it is not re-read, and its viewer
    // keeps the rows the app was compiled with until a redeploy says otherwise.
    assert_eq!(
        next_within(&mut still, Duration::from_millis(1500)).await,
        None
    );

    // The resident slot was replaced, so the budget is spent against the new rows and the next
    // viewer opens on them. Replaced just after the viewers are told — see the schedule for why
    // that order — so a test that reads the frame first can arrive a moment before it.
    let mut slot = None;
    for _ in 0..100 {
        let resident = host
            .resident()
            .into_iter()
            .find(|s| s.key.app_id.as_str() == "live")
            .unwrap();
        if resident.footprint.source_bytes > before {
            slot = Some(resident);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let slot = slot.expect("the budget still counts the old rows");
    let (_next, init) = open(addr, "live.test").await;
    assert_eq!(
        number(&pane(&init, "total").unwrap()),
        expected_total(400, 0.0)
    );
    // And every live viewer's lease moved with them: the sources their admission is reported
    // as holding are the refreshed ones, not the ones the app was compiled with.
    let sessions = host.sessions();
    let mine = sessions
        .iter()
        .filter(|s| s.key.app_id.as_str() == "live")
        .collect::<Vec<_>>();
    assert_eq!(mine.len(), 2);
    assert!(mine
        .iter()
        .all(|s| s.source_bytes == slot.footprint.source_bytes));
}
