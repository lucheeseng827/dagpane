//! Two apps on one port, over real sockets. The end of the arrow `crates/host` starts.
//!
//! Everything in `crates/host`'s own suite is a library asserting about a map. These are the
//! tests that need a listener: that the `Host` header actually routes, that a redeploy of one
//! app is invisible to a viewer of the other, and that the `Origin` check still means
//! something when there is no single address to derive it from.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use dagpane_host::{AppId, Budget, Host, MemAppSource};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

fn manifest(title: &str, csv: &str) -> String {
    format!(
        r#"
[app]
title = "{title}"

[[source]]
name = "sales"
csv = "{csv}"

[[input]]
name = "min_amount"
label = "Minimum"
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

[[pane]]
cell = "total"
metric = {{ label = "Total" }}
"#
    )
}

fn write_csv(dir: &std::path::Path) -> String {
    let mut text = String::from("region,amount\n");
    for i in 0..60 {
        text.push_str(&format!("r{},{}.0\n", i % 4, (i % 29) * 10));
    }
    std::fs::write(dir.join("sales.csv"), text).unwrap();
    "sales.csv".to_string()
}

/// A running server over a `MemAppSource` the test can republish into.
async fn start(source: Arc<MemAppSource>) -> SocketAddr {
    let host = Arc::new(Host::new(
        source,
        Budget::new(64 << 20, Duration::from_secs(3600)),
    ));

    // Port 0 and hand the bound listener over, for the reason `socket.rs` gives: dropping it
    // to rebind opens a window anything else on the machine can take.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = dagpane_serve::serve_host_on(host, listener).await;
    });

    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return addr;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the server never began accepting on {addr}");
}

/// Open `/ws` at `addr` while claiming to be `name`, optionally with an `Origin`.
async fn connect(
    addr: SocketAddr,
    name: &str,
    origin: Option<&str>,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    tokio_tungstenite::tungstenite::Error,
> {
    // tungstenite sets `Host` from the URL's authority, so asking for the app by name is a
    // matter of putting the name in the URL and connecting to the address anyway. That is
    // exactly what a proxy in front of this does.
    let mut request = format!("ws://{name}/ws").into_client_request().unwrap();
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
    }
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    tokio_tungstenite::client_async(request, stream)
        .await
        .map(|(s, _)| s)
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
            Message::Text(text) => return serde_json::from_str(&text).expect("valid JSON"),
            Message::Close(_) => panic!("the server closed the socket"),
            _ => continue,
        }
    }
}

/// The one metric pane's rendered value, from an `init` frame (`views`) or a `patch`
/// (`panes`). A string, because the server formats a metric before it sends it — that is
/// what makes `dagpane explain` and a browser show a number the same way.
fn total(frame: &serde_json::Value) -> Option<String> {
    for field in ["views", "panes"] {
        if let Some(updates) = frame[field].as_array() {
            if let Some(v) = updates
                .iter()
                .find_map(|u| u["view"]["value"].as_str().map(str::to_string))
            {
                return Some(v);
            }
        }
    }
    None
}

fn fixture() -> (tempfile::TempDir, Arc<MemAppSource>, AppId, AppId) {
    let dir = tempfile::tempdir().unwrap();
    let csv = write_csv(dir.path());
    let source = Arc::new(MemAppSource::new());
    let a = AppId::parse("acme").unwrap();
    let b = AppId::parse("globex").unwrap();
    source.publish("acme.test", &a, manifest("Acme", &csv), dir.path());
    source.publish("globex.test", &b, manifest("Globex", &csv), dir.path());
    (dir, source, a, b)
}

#[tokio::test]
async fn two_apps_answer_on_one_port_and_the_header_is_what_decides() {
    let (_dir, source, _, _) = fixture();
    let addr = start(source).await;

    for (name, title) in [("acme.test", "Acme"), ("globex.test", "Globex")] {
        let mut socket = connect(addr, name, None).await.unwrap();
        let init = next_json(&mut socket).await;
        assert_eq!(init["type"], "init", "{name}: {init}");
        assert_eq!(init["title"], title, "{name} was served the wrong app");
        socket.close(None).await.unwrap();
    }
}

#[tokio::test]
async fn a_name_with_no_app_behind_it_is_refused_at_the_page_and_not_at_the_socket() {
    let (_dir, source, _, _) = fixture();
    let addr = start(source).await;

    // The page first: serving the client shell here would give a viewer a page whose socket
    // then fails with nothing to read.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Hand-written rather than through a client: the whole point is the `Host` header, and a
    // client that derives it from a URL would be asserting about the client.
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: nobody.test\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    assert!(
        response.contains("no app is served at nobody.test"),
        "{response}"
    );

    // And the socket refuses too, rather than upgrading into nothing.
    assert!(connect(addr, "nobody.test", None).await.is_err());
}

#[tokio::test]
async fn an_origin_from_another_app_on_the_same_port_is_refused() {
    let (_dir, source, _, _) = fixture();
    let addr = start(source).await;

    // Its own origin is fine.
    assert!(connect(addr, "acme.test", Some("http://acme.test"))
        .await
        .is_ok());
    // The neighbour's is not — and this is the case a single-app server cannot even have.
    // Without it, a page one party has open could read the other's panes from the same port.
    assert!(connect(addr, "acme.test", Some("http://globex.test"))
        .await
        .is_err());
    assert!(connect(addr, "acme.test", Some("https://evil.example"))
        .await
        .is_err());
}

#[tokio::test]
async fn a_redeploy_of_one_app_is_invisible_to_a_viewer_of_the_other() {
    let (dir, source, a, _) = fixture();
    let csv = "sales.csv";
    let addr = start(source.clone()).await;

    // A viewer of globex, mid-session, with a value on screen.
    let mut globex = connect(addr, "globex.test", None).await.unwrap();
    let init = next_json(&mut globex).await;
    assert_eq!(init["title"], "Globex");
    let before = total(&init).expect("the metric pane has a value");

    // acme is redeployed under it. New bytes, new digest, new key, new compile.
    source.publish("acme.test", &a, manifest("Acme v2", csv), dir.path());
    let mut acme = connect(addr, "acme.test", None).await.unwrap();
    assert_eq!(next_json(&mut acme).await["title"], "Acme v2");

    // The globex session is untouched: it still answers, and it answers with its own numbers.
    globex
        .send(Message::text(
            serde_json::json!({"type": "set", "seq": 1,
                "values": {"min_amount": {"kind": "float", "v": 1000.0}}})
            .to_string(),
        ))
        .await
        .unwrap();
    let patch = next_json(&mut globex).await;
    assert_eq!(patch["type"], "patch", "{patch}");
    let after = total(&patch).expect("the metric repainted");
    assert_ne!(
        before, after,
        "the interaction did nothing, so this proved nothing"
    );
    // Not "0". Every row is filtered out, so the aggregate has nothing to sum and the
    // metric renders the em dash it uses for a value that is absent rather than zero —
    // which is the distinction a dashboard has to keep and most do not.
    assert_eq!(after, "\u{2014}", "an empty total rendered as a number");

    // And a refresh still names globex's app rather than the one that was redeployed.
    globex
        .send(Message::text(
            serde_json::json!({"type": "refresh", "seq": 2}).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(next_json(&mut globex).await["type"], "refreshed");
}

#[tokio::test]
async fn two_viewers_of_two_apps_do_not_see_each_others_interactions() {
    let (_dir, source, _, _) = fixture();
    let addr = start(source).await;

    let mut acme = connect(addr, "acme.test", None).await.unwrap();
    let mut globex = connect(addr, "globex.test", None).await.unwrap();
    let acme_before = total(&next_json(&mut acme).await).unwrap();
    let globex_before = total(&next_json(&mut globex).await).unwrap();
    assert_eq!(acme_before, globex_before, "the two apps start the same");

    acme.send(Message::text(
        serde_json::json!({"type": "set", "seq": 1,
            "values": {"min_amount": {"kind": "float", "v": 1000.0}}})
        .to_string(),
    ))
    .await
    .unwrap();
    assert_eq!(total(&next_json(&mut acme).await).unwrap(), "\u{2014}");

    globex
        .send(Message::text(
            serde_json::json!({"type": "refresh", "seq": 1}).to_string(),
        ))
        .await
        .unwrap();
    let refreshed = next_json(&mut globex).await;
    assert_eq!(
        total(&refreshed).unwrap(),
        globex_before,
        "acme's slider moved globex's number"
    );
}
