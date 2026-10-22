//! The front door, over a real socket.
//!
//! The exit criterion: **an unauthenticated socket upgrade is refused.** Everything else here
//! is the shape of that refusal — which status, what it says, and what it does not say.
//!
//! Keys are generated per test. This repository does not commit them.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use dagpane_auth::{Access, Algorithm, Jwks, Policy};
use dagpane_serve::{Auth, Login, Options};
use futures_util::StreamExt;
use jsonwebtoken::{encode, EncodingKey, Header};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

const MANIFEST: &str = r#"
[app]
title = "Door"

[[input]]
name = "n"
number = { min = 0.0, max = 10.0, default = 1.0 }

[[cell]]
name = "doubled"
from = "n"

[[pane]]
cell = "doubled"
metric = { label = "N" }
"#;

struct Idp {
    pkcs8: Vec<u8>,
    x: String,
    y: String,
}

impl Idp {
    fn new() -> Idp {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        let point = pair.public_key().as_ref().to_vec();
        Idp {
            pkcs8: pkcs8.as_ref().to_vec(),
            x: B64.encode(&point[1..33]),
            y: B64.encode(&point[33..65]),
        }
    }

    fn jwks(&self) -> Jwks {
        Jwks::parse(&format!(
            r#"{{"keys":[{{"kty":"EC","crv":"P-256","use":"sig","kid":"k1","x":"{}","y":"{}"}}]}}"#,
            self.x, self.y
        ))
        .unwrap()
    }

    fn token(&self, apps: &[&str]) -> String {
        self.token_until(apps, unix_now() + 300)
    }

    fn token_until(&self, apps: &[&str], exp: u64) -> String {
        let mut header = Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some("k1".into());
        encode(
            &header,
            &serde_json::json!({
                "sub": "alice", "iss": "https://idp.test", "aud": "dagpane",
                "exp": exp, "dagpane_apps": apps,
            }),
            &EncodingKey::from_ec_der(&self.pkcs8),
        )
        .unwrap()
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn policy(access: Access) -> Policy {
    Policy {
        issuer: "https://idp.test".into(),
        audience: "dagpane".into(),
        algorithms: BTreeSet::from([Algorithm::ES256]),
        leeway_seconds: 5,
        access,
    }
}

/// A server for one app called `door`, with whatever front door the test wants.
async fn start(auth: Option<Arc<Auth>>) -> (tempfile::TempDir, SocketAddr) {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("door.toml");
    std::fs::write(&manifest, MANIFEST).unwrap();
    let app = Arc::new(dagpane_app::load(&manifest).unwrap());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = dagpane_serve::serve_on_with(
            app,
            listener,
            Options {
                app_name: "door".into(),
                auth,
                // Loopback, so the allowlist derived from the bound address is the one these
                // tests want. `tests/replica.rs` covers the declared-origin path.
                origins: Vec::new(),
                // No timer: these tests are about who gets through the door, and a ping on a
                // socket they hold open is a frame they would have to skip past.
                // `tests/heartbeat.rs` is where the heartbeat is checked.
                heartbeat: None,
                ..Default::default()
            },
        )
        .await;
    });
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return (dir, addr);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the server never began accepting");
}

/// Try the upgrade. `Ok` means it completed and the opening frame arrived.
async fn upgrade(
    addr: SocketAddr,
    authorization: Option<&str>,
    protocols: Option<&str>,
) -> Result<serde_json::Value, tokio_tungstenite::tungstenite::Error> {
    let mut request = format!("ws://{addr}/ws").into_client_request().unwrap();
    if let Some(value) = authorization {
        request
            .headers_mut()
            .insert("authorization", value.parse().unwrap());
    }
    if let Some(value) = protocols {
        request
            .headers_mut()
            .insert("sec-websocket-protocol", value.parse().unwrap());
    }
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut socket, _) = tokio_tungstenite::client_async(request, stream).await?;
    loop {
        match socket.next().await.unwrap().unwrap() {
            Message::Text(text) => return Ok(serde_json::from_str(&text).unwrap()),
            Message::Close(_) => panic!("closed before the opening frame"),
            _ => continue,
        }
    }
}

/// An ordinary GET, so a test can read the status and the body of a refusal.
async fn get(addr: SocketAddr, path: &str, authorization: Option<&str>) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let auth = authorization
        .map(|v| format!("authorization: {v}\r\n"))
        .unwrap_or_default();
    // Enough of an upgrade request to reach the handler, so `/ws` is exercised as `/ws`.
    let upgrade = if path == "/ws" {
        "connection: Upgrade\r\nupgrade: websocket\r\nsec-websocket-version: 13\r\n\
         sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
    } else {
        ""
    };
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nhost: 127.0.0.1\r\n{auth}{upgrade}connection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response).await;
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, response)
}

// ── the exit criterion ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_unauthenticated_socket_upgrade_is_refused() {
    let idp = Idp::new();
    let auth = Arc::new(Auth {
        jwks: idp.jwks(),
        policy: policy(Access::Claim("dagpane_apps".into())),
        login: None,
    });
    let (_dir, addr) = start(Some(auth)).await;

    assert!(
        upgrade(addr, None, None).await.is_err(),
        "a viewer with no token got a session"
    );

    let (status, body) = get(addr, "/ws", None).await;
    assert_eq!(status, 401);
    assert!(body.contains("requires a valid token"), "{body}");

    // And the same door opens for a token that satisfies it.
    let token = idp.token(&["door"]);
    let init = upgrade(addr, Some(&format!("Bearer {token}")), None)
        .await
        .expect("a good token should get in");
    assert_eq!(init["type"], "init");
    assert_eq!(init["title"], "Door");
}

#[tokio::test]
async fn with_no_front_door_configured_nothing_changes() {
    // The default is still an open socket, and the CLI is what warns about it. A test,
    // because "auth is optional" is exactly the kind of thing a refactor breaks in the
    // direction of refusing everybody.
    let (_dir, addr) = start(None).await;
    let init = upgrade(addr, None, None).await.unwrap();
    assert_eq!(init["type"], "init");
}

// ── the shape of the refusal ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_valid_token_for_another_app_is_403_and_not_401() {
    let idp = Idp::new();
    let auth = Arc::new(Auth {
        jwks: idp.jwks(),
        policy: policy(Access::Claim("dagpane_apps".into())),
        login: None,
    });
    let (_dir, addr) = start(Some(auth)).await;

    let token = idp.token(&["payroll"]);
    let (status, body) = get(addr, "/ws", Some(&format!("Bearer {token}"))).await;
    // 401 would send this person back to a login they have already completed, round a loop
    // that cannot succeed.
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("do not have access"), "{body}");
}

#[tokio::test]
async fn a_refusal_tells_the_caller_nothing_it_could_iterate_on() {
    let idp = Idp::new();
    let stranger = Idp::new();
    let auth = Arc::new(Auth {
        jwks: idp.jwks(),
        policy: policy(Access::Claim("dagpane_apps".into())),
        login: None,
    });
    let (_dir, addr) = start(Some(auth)).await;

    // Four different wrongnesses; the body must not distinguish them. A caller who can tell
    // "wrong audience" from "expired" from "unknown key" can iterate towards a token that
    // works. The detail goes to the log, where the operator is.
    let cases = [
        format!("Bearer {}", stranger.token(&["door"])),
        "Bearer not.a.jwt".to_string(),
        "Bearer ".to_string(),
        "Basic aGk6dGhlcmU=".to_string(),
    ];
    let mut bodies = Vec::new();
    for value in &cases {
        let (status, response) = get(addr, "/ws", Some(value)).await;
        assert_eq!(status, 401, "{value}: {response}");
        bodies.push(response.rsplit("\r\n\r\n").next().unwrap_or("").to_string());
    }
    assert!(
        bodies.windows(2).all(|w| w[0] == w[1]),
        "the refusals differ and so tell a prober which one to fix: {bodies:?}"
    );
}

// ── how a browser gets its token in ───────────────────────────────────────────────────────

#[tokio::test]
async fn a_browser_may_carry_its_token_in_the_subprotocol_because_it_cannot_set_headers() {
    let idp = Idp::new();
    let auth = Arc::new(Auth {
        jwks: idp.jwks(),
        policy: policy(Access::AnyApp),
        login: None,
    });
    let (_dir, addr) = start(Some(auth)).await;
    let token = idp.token(&["door"]);

    // Exactly what the bundled client sends: a constant entry the server can select, and the
    // credential beside it.
    let offered = format!("dagpane, dagpane.auth.{token}");
    let init = upgrade(addr, None, Some(&offered)).await.unwrap();
    assert_eq!(init["type"], "init");

    // A token is never accepted from the query string: it would be logged by every proxy in
    // the path, and a credential in an access log is a credential.
    let mut request = format!("ws://{addr}/ws?t={token}")
        .into_client_request()
        .unwrap();
    request.headers_mut().remove("authorization");
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    assert!(
        tokio_tungstenite::client_async(request, stream)
            .await
            .is_err(),
        "a token in the query string was accepted"
    );
}

// ── what the page is told before it has a token ───────────────────────────────────────────

#[tokio::test]
async fn the_auth_endpoint_is_reachable_without_a_token_and_carries_only_public_things() {
    let idp = Idp::new();
    let auth = Arc::new(Auth {
        jwks: idp.jwks(),
        policy: policy(Access::AnyApp),
        login: Some(Login {
            authorize_endpoint: "https://idp.test/authorize".into(),
            token_endpoint: "https://idp.test/token".into(),
            client_id: "dagpane-web".into(),
            scope: "openid profile".into(),
        }),
    });
    let (_dir, addr) = start(Some(auth)).await;

    // Unauthenticated on purpose: the page has to be able to find out where to sign in.
    let (status, body) = get(addr, "/auth", None).await;
    assert_eq!(status, 200, "{body}");
    let json: serde_json::Value =
        serde_json::from_str(body.rsplit("\r\n\r\n").next().unwrap()).unwrap();
    assert_eq!(json["required"], true);
    assert_eq!(json["login"]["client_id"], "dagpane-web");

    // Nothing secret is in there, and nothing about the keys or the policy either: a client
    // needs the authorize URL, not the audience it will be checked against.
    let text = body.to_lowercase();
    for leak in [
        "idp.test/token\",\"secret",
        "\"n\":",
        "\"x\":",
        "\"y\":",
        "audience",
        "kid",
    ] {
        assert!(!text.contains(leak), "/auth carried {leak}: {body}");
    }

    // With no door, it says so rather than 404ing — the page needs an answer either way.
    let (_dir2, open) = start(None).await;
    let (status, body) = get(open, "/auth", None).await;
    assert_eq!(status, 200);
    assert!(body.contains("\"required\":false"), "{body}");
}

// ── a connection that outlives its token ──────────────────────────────────────────────────
//
// A token is checked when a connection opens. Before scheduled refreshes, a connection that
// outlived its token could only go on seeing what it already had: new data meant a restart, and
// a restart meant coming back through the door. A refresh brings new data to an open socket, so
// the door's promise has to be kept there as well — new rows go to a connection whose token
// still holds, and one whose token has run out is closed with a code its page can act on.

/// `door`, over data that is re-read every second.
const LIVE: &str = r#"
[app]
title = "Door"

[[source]]
name = "sales"
csv = "sales.csv"
refresh_secs = 1

[[cell]]
name = "total"
from = "sales"
[[cell.step]]
group_by = { agg = [{ column = "amount", agg = "sum", as = "t" }] }
[[cell.step]]
scalar = { column = "t" }

[[pane]]
id = "total"
cell = "total"
metric = { label = "Total" }
"#;

fn sales(rows: usize) -> String {
    let mut text = String::from("region,amount\n");
    for i in 0..rows {
        text.push_str(&format!("r{},{}.0\n", i % 4, i * 10));
    }
    text
}

type Socket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

/// What a socket said next: a frame of the app's, or a close and its code.
#[derive(Debug)]
enum Said {
    Frame(serde_json::Value),
    Closed(Option<(u16, String)>),
}

async fn said(socket: &mut Socket) -> Said {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => {
                return Said::Frame(serde_json::from_str(&text).unwrap())
            }
            Some(Ok(Message::Close(frame))) => {
                return Said::Closed(frame.map(|f| (u16::from(f.code), f.reason.to_string())))
            }
            Some(Ok(_)) => continue,
            Some(Err(_)) | None => return Said::Closed(None),
        }
    }
}

/// Through the door on `host` with `token`, past the opening frame.
async fn open_as(addr: SocketAddr, host: &str, token: &str) -> Socket {
    let mut request = format!("ws://{host}/ws").into_client_request().unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut socket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .expect("a token that has not expired is admitted");
    match said(&mut socket).await {
        Said::Frame(init) if init["type"] == "init" => socket,
        other => panic!("expected the opening frame, got {other:?}"),
    }
}

/// A door that allows no leeway, so a token is refused the second after its `exp`.
fn strict(idp: &Idp) -> Arc<Auth> {
    let mut policy = policy(Access::AnyApp);
    policy.leeway_seconds = 0;
    Arc::new(Auth {
        jwks: idp.jwks(),
        policy,
        login: None,
    })
}

/// The same check whichever front door the app is behind: two viewers, one whose token outlives
/// the test and one whose token expires before the data moves.
async fn only_the_viewer_whose_token_holds_is_sent_new_rows(
    addr: SocketAddr,
    host: &str,
    dir: &std::path::Path,
    idp: &Idp,
) {
    let now = unix_now();
    let lasting = idp.token(&["door"]);
    let ending = idp.token_until(&["door"], now + 1);
    let mut kept = open_as(addr, host, &lasting).await;
    let mut closed = open_as(addr, host, &ending).await;

    // Two whole seconds past `exp`, and only then is there anything new to send.
    let target = std::time::UNIX_EPOCH + Duration::from_secs(now + 3);
    if let Ok(left) = target.duration_since(std::time::SystemTime::now()) {
        tokio::time::sleep(left + Duration::from_millis(50)).await;
    }
    std::fs::write(dir.join("sales.csv"), sales(40)).unwrap();

    let within = Duration::from_secs(5);
    match tokio::time::timeout(within, said(&mut kept)).await {
        Ok(Said::Frame(frame)) => assert_eq!(frame["type"], "reloaded", "{frame}"),
        other => panic!("the viewer whose token holds was not sent the new rows: {other:?}"),
    }
    match tokio::time::timeout(within, said(&mut closed)).await {
        Ok(Said::Closed(Some((code, reason)))) => {
            assert_eq!(code, dagpane_serve::TOKEN_EXPIRED);
            assert_eq!(reason, "token expired");
        }
        other => panic!("a connection that outlived its token was not closed: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn new_rows_are_not_sent_to_a_connection_that_has_outlived_its_token() {
    let idp = Idp::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("door.toml"), LIVE).unwrap();
    std::fs::write(dir.path().join("sales.csv"), sales(10)).unwrap();
    let app = Arc::new(dagpane_app::load(&dir.path().join("door.toml")).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let auth = strict(&idp);
    tokio::spawn(async move {
        let _ = dagpane_serve::serve_on_with(
            app,
            listener,
            Options {
                app_name: "door".into(),
                auth: Some(auth),
                origins: Vec::new(),
                heartbeat: None,
                ..Default::default()
            },
        )
        .await;
    });
    let host = addr.to_string();
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    only_the_viewer_whose_token_holds_is_sent_new_rows(addr, &host, dir.path(), &idp).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn under_host_too_a_connection_that_has_outlived_its_token_is_closed_not_refreshed() {
    use dagpane_host::{AppId, Budget, Host, MemAppSource};
    let idp = Idp::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sales.csv"), sales(10)).unwrap();
    let source = Arc::new(MemAppSource::new());
    source.publish(
        "door.test",
        &AppId::parse("door").unwrap(),
        LIVE.to_string(),
        dir.path(),
    );
    let host = Arc::new(Host::new(
        source,
        Budget::new(64 << 20, Duration::from_secs(3600)),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let auth = strict(&idp);
    tokio::spawn(async move {
        let _ = dagpane_serve::serve_host_on_with(host, listener, Some(auth)).await;
    });
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    only_the_viewer_whose_token_holds_is_sent_new_rows(addr, "door.test", dir.path(), &idp).await;
}
