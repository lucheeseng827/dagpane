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
        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 300;
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
