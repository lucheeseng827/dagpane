//! A connection that goes quiet, and the thing that keeps it.
//!
//! **A dashboard nobody is clicking is the ordinary case, not an idle one.** A load balancer
//! does not know that: an AWS ALB, an nginx `proxy_read_timeout` and most ingress controllers
//! reclaim a connection they have seen no bytes on for sixty seconds. So a viewer who is
//! *reading* gets disconnected, and the page left behind renders perfectly and answers
//! nothing, with no error anywhere to say why.
//!
//! The fix is a WebSocket Ping on a timer, and it is a **server** fix with no client-side
//! counterpart, for a reason worth stating: a browser cannot send a Ping from script — the
//! WebSocket API has no method for it — and it answers one in the transport with no
//! JavaScript involved. The server is the only end that can start this, and the page needs no
//! code to finish it.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dagpane_serve::Options;
use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::Message;

async fn start(heartbeat: Option<Duration>) -> SocketAddr {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/sales.toml");
    let app = Arc::new(dagpane_app::load(&manifest).expect("the bundled example compiles"));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = dagpane_serve::serve_on_with(
            app,
            listener,
            Options {
                app_name: "beat".into(),
                heartbeat,
                ..Default::default()
            },
        )
        .await;
    });
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return addr;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the server never began accepting on {addr}");
}

#[tokio::test]
async fn a_connection_that_says_nothing_is_pinged_anyway() {
    // The property, at a hundredth of the shipped interval so the test costs a fraction of a
    // second rather than a minute. What is being asserted is that the pings arrive *while the
    // client sends nothing at all* — which is the case a proxy's idle timeout is counting.
    //
    // Mutation check: `heartbeat: None` on the server fails this by timing out.
    let addr = start(Some(Duration::from_millis(120))).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("the upgrade succeeds");

    let mut pings = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while pings < 3 && Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(2), socket.next()).await {
            // The client sends NOTHING in this loop. `tokio-tungstenite` answers each Ping
            // with a Pong itself, which is the same thing a browser's transport does.
            Ok(Some(Ok(Message::Ping(_)))) => pings += 1,
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => panic!("the socket failed: {e}"),
            Ok(None) => panic!("the server closed a connection it should have been pinging"),
            Err(_) => break,
        }
    }
    assert!(
        pings >= 3,
        "a silent connection got {pings} ping(s); a proxy would have reaped it"
    );
}

#[tokio::test]
async fn the_first_ping_waits_a_whole_interval() {
    // A connection that has just sent its opening frame is the least idle it will ever be,
    // and `tokio::time::interval` fires its first tick immediately unless told otherwise. A
    // ping riding out behind every `init` would be a ping per connection that proves nothing.
    //
    // Mutation check: `interval` in place of `interval_at` fails this.
    let addr = start(Some(Duration::from_millis(400))).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("the upgrade succeeds");

    let opened = Instant::now();
    loop {
        match tokio::time::timeout(Duration::from_secs(3), socket.next())
            .await
            .expect("a ping arrives eventually")
        {
            Some(Ok(Message::Ping(_))) => break,
            Some(Ok(_)) => continue,
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    assert!(
        opened.elapsed() >= Duration::from_millis(300),
        "the first ping arrived after {:?}, before a whole interval had passed",
        opened.elapsed()
    );
}

#[tokio::test]
async fn the_heartbeat_does_not_disturb_the_conversation() {
    // A ping is a transport frame and a patch is a message; a client reading one must not be
    // handed the other. With the interval far shorter than the exchange, several pings land
    // in the middle of this round trip — and the `init` and the `patch` must still arrive
    // whole, in order, and parseable.
    //
    // Mutation check: sending the ping as a text frame instead fails this at `from_str`.
    use futures_util::SinkExt;
    let addr = start(Some(Duration::from_millis(50))).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("the upgrade succeeds");

    async fn next_message<S>(socket: &mut S) -> serde_json::Value
    where
        S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .expect("the server answers")
            {
                Some(Ok(Message::Text(t))) => {
                    return serde_json::from_str(&t).expect("every text frame is a message")
                }
                Some(Ok(_)) => continue,
                other => panic!("unexpected frame: {other:?}"),
            }
        }
    }

    let init = next_message(&mut socket).await;
    assert_eq!(init["type"], "init");

    // Long enough that pings are certainly interleaved with what follows.
    tokio::time::sleep(Duration::from_millis(220)).await;

    socket
        .send(Message::Text(
            r#"{"type":"set","seq":1,"values":{"min_amount":{"kind":"float","v":400.0}}}"#.into(),
        ))
        .await
        .unwrap();

    let patch = next_message(&mut socket).await;
    assert_eq!(patch["type"], "patch", "{patch}");
    assert_eq!(patch["seq"], 1);
    // The product claim, unchanged by anything above it.
    assert_eq!(patch["stats"]["visited"], 8, "{patch}");
    assert_eq!(
        patch["panes"].as_array().map(|p| p.len()),
        Some(3),
        "{patch}"
    );
}

#[tokio::test]
async fn a_server_told_not_to_ping_does_not() {
    // `--heartbeat-seconds 0`. Nothing in front to idle the connection out, nothing to send.
    let addr = start(None).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("the upgrade succeeds");

    // Drain the opening frame, then require silence.
    match tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("the opening frame arrives")
    {
        Some(Ok(Message::Text(_))) => {}
        Some(Ok(other)) => panic!("a server with no heartbeat sent {other:?}"),
        other => panic!("unexpected frame: {other:?}"),
    }
    match tokio::time::timeout(Duration::from_millis(600), socket.next()).await {
        Err(_) => {}
        Ok(other) => panic!("expected silence, got {other:?}"),
    }
}
