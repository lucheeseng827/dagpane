//! The wire, for real: a bound socket, a WebSocket client, and the frames a browser sees.
//!
//! `dagpane-app` already tests the patch as a value. This tests it as bytes, because the
//! claim a reader can check is the one in their network tab.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

async fn start() -> SocketAddr {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/sales.toml");
    let app = Arc::new(dagpane_app::load(&manifest).expect("the bundled example compiles"));

    // Port 0: the OS picks a free one, so tests do not collide with each other or with a
    // developer's own `dagpane run`. The listener is handed to the server rather than dropped
    // and rebound — dropping it opens a window in which anything else on the machine can take
    // the port, and these tests run concurrently.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let _ = dagpane_serve::serve_on(app, listener).await;
    });

    // The server starts accepting inside the task; the port is already bound, so this waits
    // for the accept loop rather than for the bind.
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return addr;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the server never began accepting on {addr}");
}

async fn next_json<S>(socket: &mut S) -> serde_json::Value
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match socket
            .next()
            .await
            .expect("the socket closed early")
            .unwrap()
        {
            Message::Text(t) => return serde_json::from_str(&t).expect("valid JSON"),
            Message::Close(_) => panic!("the server closed the connection"),
            _ => continue,
        }
    }
}

#[tokio::test]
async fn a_browser_gets_the_app_then_only_the_panes_that_moved() {
    let addr = start().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("the upgrade succeeds");

    // ── init ──────────────────────────────────────────────────────────────────────────
    let init = next_json(&mut socket).await;
    assert_eq!(init["type"], "init");
    assert_eq!(init["title"], "Sales explorer");
    assert_eq!(init["widgets"].as_array().unwrap().len(), 2);
    assert_eq!(init["panes"].as_array().unwrap().len(), 7);
    assert_eq!(init["views"].as_array().unwrap().len(), 7);
    assert_eq!(init["stats"]["total_cells"], 11);
    assert!(
        init["stats"]["micros"].is_number(),
        "the server has a clock and reports it"
    );

    // ── one interaction ───────────────────────────────────────────────────────────────
    socket
        .send(Message::Text(
            r#"{"type":"set","seq":7,"values":{"min_amount":{"kind":"float","v":400.0}}}"#.into(),
        ))
        .await
        .unwrap();

    let patch = next_json(&mut socket).await;
    assert_eq!(patch["type"], "patch");
    assert_eq!(patch["seq"], 7, "the client's own counter comes back");

    let panes = patch["panes"].as_array().unwrap();
    let ids: Vec<&str> = panes.iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        vec!["revenue", "order_count", "region_totals"],
        "three panes of seven"
    );

    let stats = &patch["stats"];
    assert_eq!(stats["visited"], 8);
    assert_eq!(stats["evaluated"], 6);
    assert_eq!(stats["untouched"], 3);

    // ── the same value again is free ──────────────────────────────────────────────────
    socket
        .send(Message::Text(
            r#"{"type":"set","seq":8,"values":{"min_amount":{"kind":"float","v":400.0}}}"#.into(),
        ))
        .await
        .unwrap();
    let idle = next_json(&mut socket).await;
    assert_eq!(idle["panes"].as_array().unwrap().len(), 0);
    assert_eq!(idle["stats"]["visited"], 0, "no cell was even looked at");
}

#[tokio::test]
async fn a_value_the_widget_could_not_produce_is_rejected_and_the_session_survives() {
    let addr = start().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    let _ = next_json(&mut socket).await;

    socket
        .send(Message::Text(
            r#"{"type":"set","seq":1,"values":{"min_amount":{"kind":"float","v":1000000.0}}}"#
                .into(),
        ))
        .await
        .unwrap();
    let rejected = next_json(&mut socket).await;
    assert_eq!(rejected["type"], "rejected");
    assert!(rejected["message"]
        .as_str()
        .unwrap()
        .contains("outside its range"));

    // The connection is still usable, and nothing was applied.
    socket
        .send(Message::Text(
            r#"{"type":"set","seq":2,"values":{"region":{"kind":"text","v":"north"}}}"#.into(),
        ))
        .await
        .unwrap();
    let patch = next_json(&mut socket).await;
    assert_eq!(patch["type"], "patch");
    assert!(!patch["panes"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn malformed_json_is_answered_rather_than_dropping_the_connection() {
    let addr = start().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    let _ = next_json(&mut socket).await;

    socket
        .send(Message::Text("{ not json".into()))
        .await
        .unwrap();
    let reply = next_json(&mut socket).await;
    assert_eq!(reply["type"], "rejected");

    socket
        .send(Message::Text(r#"{"type":"refresh","seq":3}"#.into()))
        .await
        .unwrap();
    let refreshed = next_json(&mut socket).await;
    assert_eq!(
        refreshed["type"], "refreshed",
        "a refresh runs no pass, so it is not a patch"
    );
    assert_eq!(refreshed["panes"].as_array().unwrap().len(), 7);
    assert_eq!(
        refreshed["stats"]["visited"], 0,
        "and it says so: nothing was visited"
    );
}

#[tokio::test]
async fn the_socket_refuses_an_upgrade_from_another_origin() {
    // A WebSocket upgrade is not subject to the same-origin policy and gets no preflight, so
    // without this any page open in the viewer's browser could connect to the loopback port
    // and read every pane. This is the test that keeps "binds loopback" meaning what a reader
    // assumes it means.
    let addr = start().await;

    let mut evil = format!("ws://{addr}/ws").into_client_request().unwrap();
    evil.headers_mut()
        .insert("origin", "http://evil.example".parse().unwrap());
    assert!(
        tokio_tungstenite::connect_async(evil).await.is_err(),
        "an upgrade from a foreign origin must be refused"
    );

    // The page's own origin is fine...
    let mut good = format!("ws://{addr}/ws").into_client_request().unwrap();
    good.headers_mut()
        .insert("origin", format!("http://{addr}").parse().unwrap());
    let (mut ok, _) = tokio_tungstenite::connect_async(good)
        .await
        .expect("the app's own origin is allowed");
    assert_eq!(next_json(&mut ok).await["type"], "init");

    // ...and so is a client that sends no Origin at all, which is every non-browser caller.
    // A page cannot suppress the header, so this is not a hole in the check.
    let (mut bare, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("a non-browser client has no Origin and is not the attack this stops");
    assert_eq!(next_json(&mut bare).await["type"], "init");
}

#[tokio::test]
async fn two_browsers_do_not_share_a_session() {
    let addr = start().await;
    let (mut a, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    let (mut b, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    let _ = next_json(&mut a).await;
    let b_init = next_json(&mut b).await;

    a.send(Message::Text(
        r#"{"type":"set","seq":1,"values":{"region":{"kind":"text","v":"north"}}}"#.into(),
    ))
    .await
    .unwrap();
    let _ = next_json(&mut a).await;

    b.send(Message::Text(r#"{"type":"refresh","seq":1}"#.into()))
        .await
        .unwrap();
    let b_after = next_json(&mut b).await;

    let pane_of = |m: &serde_json::Value, key: &str, id: &str| -> String {
        m[key]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .unwrap()["view"]["value"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(
        pane_of(&b_init, "views", "order_count"),
        pane_of(&b_after, "panes", "order_count"),
        "the second browser's page did not move when the first one filtered"
    );
}
