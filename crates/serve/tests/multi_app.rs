//! Two apps on one port, over real sockets. The end of the arrow `crates/host` starts.
//!
//! Everything in `crates/host`'s own suite is a library asserting about a map. These are the
//! tests that need a listener: that the `Host` header actually routes, that a redeploy of one
//! app is invisible to a viewer of the other, that the `Origin` check still means
//! something when there is no single address to derive it from, and that a name nobody
//! declared — the name a DNS-rebinding page sends — is refused before anything is compiled.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use dagpane_host::{AppId, AppSource, Budget, DirAppSource, Domain, Host, MemAppSource};
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
    start_host(source, Budget::new(64 << 20, Duration::from_secs(3600)))
        .await
        .0
}

/// A running server over any source, and the host behind it so a test can look at what it
/// holds.
async fn start_host(source: Arc<dyn AppSource>, budget: Budget) -> (SocketAddr, Arc<Host>) {
    let host = Arc::new(Host::new(source, budget));
    let served = Arc::clone(&host);

    // Port 0 and hand the bound listener over, for the reason `socket.rs` gives: dropping it
    // to rebind opens a window anything else on the machine can take.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = dagpane_serve::serve_host_on(served, listener).await;
    });

    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return (addr, host);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the server never began accepting on {addr}");
}

/// `GET /` with exactly this `Host` header, written by hand so the header is the test's and
/// not a client's idea of it. The status and the body.
async fn get_page(addr: SocketAddr, host_header: &str) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!("GET / HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("not an HTTP response: {response:?}"));
    (status, response)
}

/// The status an upgrade at `addr` gets while claiming to be `name` (`101` when it upgrades).
async fn upgrade_status(addr: SocketAddr, name: &str, origin: Option<&str>) -> u16 {
    match connect(addr, name, origin).await {
        Ok(_) => 101,
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => response.status().as_u16(),
        Err(e) => panic!("{name} with {origin:?}: not an HTTP refusal: {e}"),
    }
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

/// Two apps in a directory, as `dagpane host` serves them: `sales.toml` and `ops.toml` over one
/// CSV, so they cost the same.
fn directory() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let csv = write_csv(dir.path());
    std::fs::write(dir.path().join("sales.toml"), manifest("Sales", &csv)).unwrap();
    std::fs::write(dir.path().join("ops.toml"), manifest("Ops", &csv)).unwrap();
    dir
}

#[tokio::test]
async fn a_rebound_name_is_refused_before_anything_is_compiled_or_evicted() {
    let dir = directory();

    // A budget that holds one of the two apps and not both, measured rather than guessed: an
    // app admitted for a refused name would have to evict `sales` to fit, and that shows.
    let one_app = {
        let probe = Host::new(
            Arc::new(DirAppSource::new(dir.path())),
            Budget::new(u64::MAX, Duration::from_secs(3600)),
        );
        probe.open("sales.localhost").unwrap();
        probe.resident_bytes()
    };
    assert!(
        one_app > 0,
        "the app holds no source bytes, so the budget proves nothing"
    );
    let (addr, host) = start_host(
        Arc::new(DirAppSource::new(dir.path())),
        Budget::new(one_app, Duration::from_secs(3600)),
    )
    .await;
    let port = addr.port();

    let (status, _) = get_page(addr, &format!("sales.localhost:{port}")).await;
    assert_eq!(status, 200);
    let resident = || -> Vec<String> {
        host.resident()
            .iter()
            .map(|slot| slot.key.app_id.to_string())
            .collect()
    };
    assert_eq!(resident(), vec!["sales"]);

    // The rebinding shape: a page at ops.attacker.example, whose DNS now answers 127.0.0.1,
    // sends its own name as Host AND Origin — which is why Origin == Host cannot be the
    // defence. Under first-label routing both of these reached ops.toml.
    let rebound = format!("ops.attacker.example:{port}");
    let (status, body) = get_page(addr, &rebound).await;
    assert_eq!(status, 404, "{body}");
    assert!(
        body.contains("this server does not answer to that name"),
        "{body}"
    );
    assert_eq!(
        upgrade_status(
            addr,
            &rebound,
            Some(&format!("http://ops.attacker.example:{port}"))
        )
        .await,
        404
    );
    assert_eq!(
        resident(),
        vec!["sales"],
        "a refused name was compiled, or evicted what was resident"
    );
    assert!(host.evictions().is_empty(), "{:?}", host.evictions());

    // Control, last: the same app under its declared name IS admitted, and evicts — so the
    // budget above bites, and the two assertions before this one were not vacuous.
    let (status, _) = get_page(addr, &format!("ops.localhost:{port}")).await;
    assert_eq!(status, 200);
    assert_eq!(resident(), vec!["ops"]);
    assert_eq!(host.evictions().len(), 1);
}

#[tokio::test]
async fn a_declared_domain_answers_behind_a_proxy_that_moves_the_port() {
    let dir = directory();
    let source =
        DirAppSource::new(dir.path()).with_domains([Domain::parse("example.com").unwrap()]);
    let (addr, _host) = start_host(
        Arc::new(source),
        Budget::new(64 << 20, Duration::from_secs(3600)),
    )
    .await;
    let port = addr.port();

    // What eggrd and an nginx `proxy_set_header Host $http_host` send: the public name with
    // this process's port, and the browser's Origin with the public scheme and no port. Port-
    // blind for a declared domain, because the port in the Origin is the proxy's.
    assert_eq!(
        upgrade_status(
            addr,
            &format!("sales.example.com:{port}"),
            Some("https://sales.example.com")
        )
        .await,
        101
    );
    assert_eq!(
        get_page(addr, &format!("sales.example.com:{port}")).await.0,
        200
    );

    for refused in [
        format!("sales.example.com.attacker.example:{port}"),
        format!("salesexample.com:{port}"),
        format!("sales:{port}"),
        format!("127.0.0.1:{port}"),
        format!("[::1]:{port}"),
    ] {
        assert_eq!(get_page(addr, &refused).await.0, 404, "{refused}");
    }
}

#[tokio::test]
async fn under_localhost_the_origin_must_name_the_port_too() {
    let dir = directory();
    let (addr, _host) = start_host(
        Arc::new(DirAppSource::new(dir.path())),
        Budget::new(64 << 20, Duration::from_secs(3600)),
    )
    .await;
    let port = addr.port();
    let name = format!("sales.localhost:{port}");

    assert_eq!(
        upgrade_status(addr, &name, Some(&format!("http://sales.localhost:{port}"))).await,
        101
    );
    // A browser resolves sales.localhost to loopback whoever serves it, so these are pages
    // any other local server — a dev server on 3000 — can serve. Port-blind, both upgraded
    // and read every pane.
    for origin in ["http://sales.localhost:3000", "https://sales.localhost:1"] {
        assert_eq!(
            upgrade_status(addr, &name, Some(origin)).await,
            403,
            "{origin}"
        );
    }
    // A default port is the port the Origin names when it names none.
    assert_eq!(
        upgrade_status(addr, &name, Some("http://sales.localhost")).await,
        403
    );

    // A proxy that forwards `$host` drops the port from Host, and then there is no port to
    // agree with: only the name is compared, so an honest TLS front on 8443 still works.
    assert_eq!(
        upgrade_status(
            addr,
            "sales.localhost",
            Some("https://sales.localhost:8443")
        )
        .await,
        101
    );

    // The same name with its trailing dot is still under localhost, and still port-strict.
    let dotted = format!("sales.localhost.:{port}");
    assert_eq!(
        upgrade_status(
            addr,
            &dotted,
            Some(&format!("http://sales.localhost.:{port}"))
        )
        .await,
        101
    );
    assert_eq!(
        upgrade_status(addr, &dotted, Some("http://sales.localhost.:3000")).await,
        403
    );
}
