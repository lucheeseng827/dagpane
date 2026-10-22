//! The write deadline, over real sockets: a connection stuck sending to a client that has
//! stopped reading is dropped, and a client that reads — even slowly, a large frame at a time —
//! is not.
//!
//! A connection is a sequential loop, so a frame that cannot be written stops everything behind
//! it — and without a deadline the kernel lets it wait for as long as the client advertises a
//! zero window, holding the session and, for a scheduled source, the rows it had. These tests
//! make a client stop reading for real rather than pretend: a receive buffer of a few
//! kilobytes, and a stream of `refresh` requests whose answers it never reads, so the server's
//! send backs up once the kernel's buffers are full — about a hundred answers, here, near
//! three megabytes — and stays backed up.
//!
//! A frame's allowance is the deadline **plus** the time the slowest reader the server allows
//! for (`SLOWEST_READER`) needs for its bytes, so the stalls here use small frames — a couple of
//! seconds' allowance — and the slow reader a large one, which a bare deadline would cut off.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use dagpane_host::{AppId, Budget, Host, MemAppSource};
use dagpane_serve::{Fleet, Lifecycle, Liveness, Options};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

type Socket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

/// One table pane over every row: `rows` sets how large every answer is.
fn manifest(rows: usize) -> String {
    format!(
        r#"
[app]
title = "Wide"

[[source]]
name = "sales"
csv = "sales.csv"

[[cell]]
name = "rows"
from = "sales"

[[pane]]
id = "rows"
cell = "rows"
table = {{ max_rows = {rows} }}
"#
    )
}

/// Rows per answer for the stalls: an allowance of about two seconds at the deadline below.
const SMALL: usize = 200;

/// Rows in the slow reader's opening frame: about eight and a half megabytes, twice what the
/// largest send buffer the kernel will give a socket holds (`tcp_wmem`'s 4 MiB), so the
/// server's write cannot finish until the client has read most of it.
const LARGE: usize = 60_000;

/// Asked of a stalled client: far more answer bytes than any socket buffer holds.
const ASKED: u64 = 300;

/// The deadline every test here runs under.
const DEADLINE: Duration = Duration::from_secs(1);

fn csv(rows: usize) -> String {
    let mut text = String::from("region,amount,note\n");
    for i in 0..rows {
        text.push_str(&format!(
            "r{},{}.0,a note long enough that every row costs the wire some bytes {i}\n",
            i % 4,
            i * 10
        ));
    }
    text
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

/// Run `server` on a runtime and a thread of its own, for the rest of the process.
///
/// Not on the test's runtime, where its clients run. There, the server's synchronous work —
/// rendering and serialising frames, which a debug build takes its time over — holds a worker
/// the clients need, and on a runner with two cores it can hold both. A client that is reading
/// is then not polled to read, and is dropped for a deadline it met, or not answered within the
/// time it waits. A browser is another process, which the operating system runs whatever the
/// server is doing; on threads of their own, so are these clients.
///
/// One thread, the hardest case for the server's own deadlines: with no other worker to keep
/// the runtime's timers, and its word on the socket, up to date while a connection is busy,
/// that connection has only the clock and the turns of the runtime it takes itself.
fn serve_apart(server: impl Future<Output = ()> + Send + 'static) {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the server")
            .block_on(server)
    });
}

/// `listener`, registered with the runtime this is called on: called inside the server's, so the
/// test's runtime has no part in serving it.
fn listening(listener: std::net::TcpListener) -> tokio::net::TcpListener {
    listener.set_nonblocking(true).unwrap();
    tokio::net::TcpListener::from_std(listener).unwrap()
}

/// `dagpane run`'s shape: one app over `rows` rows, one process, the deadline as given.
async fn serve_one(
    dir: &std::path::Path,
    rows: usize,
    write_timeout: Option<Duration>,
) -> SocketAddr {
    std::fs::write(dir.join("wide.toml"), manifest(rows)).unwrap();
    std::fs::write(dir.join("sales.csv"), csv(rows)).unwrap();
    let app = Arc::new(dagpane_app::load(&dir.join("wide.toml")).unwrap());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let options = Options {
        app_name: "wide".into(),
        // No pings: the only frames here are the ones the tests ask for.
        heartbeat: None,
        write_timeout,
        ..Default::default()
    };
    serve_apart(async move {
        let _ = dagpane_serve::serve_on_with(app, listening(listener), options).await;
    });
    wait_for(addr).await;
    addr
}

/// A TCP connection with a fixed receive buffer of `bytes`, so that once its client stops
/// reading — or reads slowly — the server's sends back up rather than the kernel absorbing them.
async fn connect_with_buffer(addr: SocketAddr, bytes: u32) -> tokio::net::TcpStream {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(bytes).unwrap();
    socket.connect(addr).await.unwrap()
}

/// Through the upgrade on `host`, past the opening frame.
async fn open(addr: SocketAddr, host: &str, small: bool) -> Socket {
    let stream = if small {
        connect_with_buffer(addr, 4_096).await
    } else {
        tokio::net::TcpStream::connect(addr).await.unwrap()
    };
    let request = format!("ws://{host}/ws").into_client_request().unwrap();
    let (mut ws, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .expect("the upgrade succeeds");
    loop {
        match ws.next().await.expect("the opening frame").unwrap() {
            Message::Text(text) => {
                assert!(
                    text.contains("\"type\":\"init\""),
                    "{}",
                    &text[..80.min(text.len())]
                );
                return ws;
            }
            _ => continue,
        }
    }
}

/// Ask for every pane `n` times without reading a single answer.
async fn ask_without_reading(ws: &mut Socket, n: u64) {
    for seq in 1..=n {
        ws.send(Message::Text(
            format!(r#"{{"type":"refresh","seq":{seq}}}"#).into(),
        ))
        .await
        .unwrap();
    }
}

/// Ask once and wait for the answer, as a client that reads does.
async fn ask_and_read(ws: &mut Socket, seq: u64) -> bool {
    if ws
        .send(Message::Text(
            format!(r#"{{"type":"refresh","seq":{seq}}}"#).into(),
        ))
        .await
        .is_err()
    {
        return false;
    }
    loop {
        match tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) if text.contains("\"refreshed\"") => return true,
            Ok(Some(Ok(_))) => continue,
            _ => return false,
        }
    }
}

/// Read until the server ends the connection, or until it has been quiet for `quiet` with the
/// connection still open. How many answers arrived, and whether it ended.
async fn drain(ws: &mut Socket, quiet: Duration) -> (u64, bool) {
    let mut answers = 0;
    loop {
        match tokio::time::timeout(quiet, ws.next()).await {
            Err(_) => return (answers, false),
            Ok(None | Some(Err(_)) | Some(Ok(Message::Close(_)))) => return (answers, true),
            Ok(Some(Ok(Message::Text(text)))) => {
                if text.contains("\"refreshed\"") {
                    answers += 1;
                }
            }
            Ok(Some(Ok(_))) => {}
        }
    }
}

/// The whole length of the first WebSocket frame in `bytes` — header and payload — once its
/// header has arrived. A server's frames are unmasked.
fn frame_len(bytes: &[u8]) -> Option<usize> {
    let short = usize::from(*bytes.get(1)? & 0x7f);
    match short {
        126 => {
            let len = u16::from_be_bytes([*bytes.get(2)?, *bytes.get(3)?]);
            Some(4 + usize::from(len))
        }
        127 => {
            let len = u64::from_be_bytes(bytes.get(2..10)?.try_into().ok()?);
            Some(10 + usize::try_from(len).ok()?)
        }
        len => Some(2 + len),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_stops_reading_is_dropped_and_one_that_reads_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let addr = serve_one(dir.path(), SMALL, Some(DEADLINE)).await;
    let host = addr.to_string();
    let mut stalled = open(addr, &host, true).await;
    let mut reading = open(addr, &host, false).await;

    ask_without_reading(&mut stalled, ASKED).await;
    // Well past the stalled frame's allowance — and all the while the other viewer asks and is
    // answered, at the same frame size, because a deadline that cost a client who reads would
    // be worse than none.
    for seq in 1..=20 {
        assert!(
            ask_and_read(&mut reading, seq).await,
            "a client that reads was not answered (request {seq})"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Now read what the stalled connection was sent. Dropped, it ends after a few answers;
    // kept, it answers all of them and then waits for more.
    let (answers, ended) = drain(&mut stalled, Duration::from_secs(3)).await;
    assert!(
        ended,
        "a connection whose client stopped reading was kept: {answers} of {ASKED} answered"
    );
    assert!(
        answers < ASKED,
        "every request was answered, so nothing was ever stuck"
    );
    assert!(
        ask_and_read(&mut reading, 99).await,
        "the reading client went too"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_reading_slowly_is_given_the_time_a_large_frame_needs() {
    // The case a deadline on the whole frame gets wrong: a client that is reading, steadily,
    // over a link too slow to take one large frame inside the bare deadline. It must not be
    // dropped as though it had stopped — that would reconnect it into the same frame, and the
    // page would never render.
    let dir = tempfile::tempdir().unwrap();
    let addr = serve_one(dir.path(), LARGE, Some(DEADLINE)).await;

    let mut tcp = connect_with_buffer(addr, 65_536).await;
    tcp.write_all(
        format!(
            "GET /ws HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();

    // About one and a half megabytes a second — a hundred times the slowest reader the server
    // allows for, and still several seconds for this frame: 64 KiB, then a pause.
    let mut got = Vec::new();
    let mut buf = vec![0u8; 65_536];
    let mut frame: Option<Vec<u8>> = None;
    let started = tokio::time::Instant::now();
    loop {
        let n = tokio::time::timeout(Duration::from_secs(10), tcp.read(&mut buf))
            .await
            .expect("the server sent nothing for ten seconds")
            .unwrap_or(0);
        if n == 0 {
            let had = frame.as_ref().map_or(0, Vec::len);
            panic!(
                "the server dropped a client that was reading, {had} bytes into the opening \
                 frame of {:?}",
                frame.as_deref().and_then(frame_len)
            );
        }
        got.extend_from_slice(&buf[..n]);
        match frame.as_mut() {
            // Everything after the response's headers is the WebSocket stream.
            None => {
                if let Some(end) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                    let rest = got.split_off(end + 4);
                    let head = String::from_utf8_lossy(&got).to_string();
                    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
                    got.clear();
                    frame = Some(rest);
                }
            }
            Some(bytes) => bytes.append(&mut got),
        }
        if let Some(bytes) = frame.as_ref() {
            if frame_len(bytes).is_some_and(|whole| bytes.len() >= whole) {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }

    let bytes = frame.unwrap();
    let whole = frame_len(&bytes).unwrap();
    assert_eq!(bytes[0] & 0x0f, 1, "the opening frame is text");
    // The frame really was too large for the kernel's buffers and the bare deadline at this
    // rate, or the test proves nothing about the allowance.
    assert!(
        whole > 4_500_000 && started.elapsed() > DEADLINE * 2,
        "a {whole}-byte frame read in {:?} did not need more than the bare deadline",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_deadline_a_client_that_stops_reading_keeps_its_connection() {
    // The control for the first test: the same stall with the deadline off is waited out, and
    // every request is answered once the client reads. Without it, that test could pass
    // because something else dropped the connection.
    let dir = tempfile::tempdir().unwrap();
    let addr = serve_one(dir.path(), SMALL, None).await;
    let mut stalled = open(addr, &addr.to_string(), true).await;

    ask_without_reading(&mut stalled, ASKED).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let (answers, ended) = drain(&mut stalled, Duration::from_secs(3)).await;
    assert!(!ended, "the connection ended with the deadline off");
    assert_eq!(answers, ASKED);
}

#[tokio::test(flavor = "multi_thread")]
async fn under_host_a_viewer_that_stops_reading_lets_go_of_its_session() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sales.csv"), csv(SMALL)).unwrap();
    let source = Arc::new(MemAppSource::new());
    source.publish(
        "wide.test",
        &AppId::parse("wide").unwrap(),
        manifest(SMALL),
        dir.path(),
    );
    let host = Arc::new(Host::new(
        source,
        Budget::new(256 << 20, Duration::from_secs(3_600)),
    ));
    let lifecycle = Arc::new(Lifecycle::describing(
        Arc::new(Fleet::new(Arc::clone(&host))),
        Duration::from_secs(1),
    ));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let admin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let served = Arc::clone(&host);
    serve_apart(async move {
        let _ = dagpane_serve::serve_host_replica(
            served,
            listening(listener),
            listening(admin),
            None,
            Liveness {
                heartbeat: None,
                write_timeout: Some(DEADLINE),
                ping_timeout: None,
            },
            lifecycle,
        )
        .await;
    });
    wait_for(addr).await;

    let mut stalled = open(addr, "wide.test", true).await;
    assert_eq!(
        host.sessions().len(),
        1,
        "the viewer's session is not on the books"
    );

    ask_without_reading(&mut stalled, ASKED).await;
    // The session — and with it whatever rows it holds — lives exactly as long as the
    // connection does, so the books are where a dropped connection shows.
    let mut released = false;
    for _ in 0..100 {
        if host.sessions().is_empty() {
            released = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        released,
        "a viewer that stopped reading still holds its session ten seconds later"
    );
    drop(stalled);
}

/// A stop with one viewer that reads and one stuck in a write behind a client that stopped
/// reading: the reader is told the server is going away, and the server returns `CLOSE_GRACE`
/// after its listener closed — no sooner, because the stuck viewer was waited for, and no
/// later, because it was not waited for longer.
///
/// `returned` says when the server has returned. The bounds on the waits are hang guards, wide
/// enough for a loaded runner, and not the property: the property is the two assertions on
/// when it returned, and the check that the stuck viewer really was stuck.
async fn assert_a_stuck_viewer_cannot_hold_the_stop(
    addr: SocketAddr,
    host: &str,
    lifecycle: &Lifecycle,
    returned: tokio::sync::oneshot::Receiver<bool>,
) {
    let mut stalled = open(addr, host, true).await;
    let mut reading = open(addr, host, false).await;
    ask_without_reading(&mut stalled, ASKED).await;
    // Long enough for the answers to back up behind the client that is not reading them; the
    // last check below says whether they did.
    tokio::time::sleep(Duration::from_secs(1)).await;

    lifecycle.request_stop();
    let asked = tokio::time::Instant::now();
    let closed = tokio::time::timeout(Duration::from_secs(15), close_code(&mut reading))
        .await
        .expect("the client that reads was never told");
    assert_eq!(closed, Some(dagpane_serve::GOING_AWAY));

    let returned = tokio::time::timeout(Duration::from_secs(20), returned).await;
    let took = asked.elapsed();
    assert!(
        matches!(returned, Ok(Ok(true))),
        "the server was still stopping {took:?} after it was asked: {returned:?}"
    );
    let drain = lifecycle.drain_for();
    assert!(
        took >= drain + dagpane_serve::CLOSE_GRACE,
        "stopped after {took:?}, before the grace was up: the stuck viewer was not waited for"
    );
    assert!(
        took < drain + dagpane_serve::CLOSE_GRACE + Duration::from_secs(5),
        "stopped after {took:?}: the grace did not bound the wait for the stuck viewer"
    );
    // A viewer that was only slow writes its close once it has caught up; one stuck in its
    // write never gets to, and the server lets go of it with nothing said.
    let stuck = tokio::time::timeout(Duration::from_secs(20), close_code(&mut stalled))
        .await
        .expect("the stuck viewer's connection never ended");
    assert_eq!(stuck, None, "that viewer was told, so it was never stuck");
}

/// Read to the close, or to the end: the close's code, or `None` for a connection that ended
/// without one.
async fn close_code(ws: &mut Socket) -> Option<u16> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Close(frame))) => return frame.map(|f| u16::from(f.code)),
            Some(Ok(_)) => continue,
            None | Some(Err(_)) => return None,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_stopped_reading_cannot_hold_a_stop_past_its_grace() {
    // A stop closes every socket and waits for them, and a socket stuck in a write is one that
    // nothing in its loop will close: the loop is waiting on the write. With the write deadline
    // off — `--write-timeout-seconds 0` — it waits for as long as its client likes, so the stop
    // must not. Every other viewer is still told, and the process goes once `CLOSE_GRACE` is
    // up, rather than when a supervisor's patience runs out and its `SIGKILL` takes the closes
    // still in flight with it.
    //
    // Mutation check: waiting for the sockets in `Connections::serve` without the deadline
    // leaves the replica stopping until the hang guard gives up.
    let dir = tempfile::tempdir().unwrap();
    let text = manifest(SMALL);
    std::fs::write(dir.path().join("wide.toml"), &text).unwrap();
    std::fs::write(dir.path().join("sales.csv"), csv(SMALL)).unwrap();
    let app = Arc::new(dagpane_app::load(&dir.path().join("wide.toml")).unwrap());
    let lifecycle = Arc::new(Lifecycle::new(
        dagpane_serve::Identity::of("wide", &text, &app),
        Duration::from_millis(100),
    ));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let admin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (stopped, returned) = tokio::sync::oneshot::channel();
    let replica = Arc::clone(&lifecycle);
    serve_apart(async move {
        let result = dagpane_serve::serve_replica(
            app,
            listening(listener),
            listening(admin),
            Options {
                app_name: "wide".into(),
                heartbeat: None,
                write_timeout: None,
                ..Default::default()
            },
            replica,
        )
        .await;
        let _ = stopped.send(result.is_ok());
    });
    wait_for(addr).await;
    assert_a_stuck_viewer_cannot_hold_the_stop(addr, &addr.to_string(), &lifecycle, returned).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn under_host_a_client_that_stopped_reading_cannot_hold_a_stop_past_its_grace() {
    // The same stop under `serve_host_replica`, which waits for its sockets through the same
    // code and is what `dagpane host` runs.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sales.csv"), csv(SMALL)).unwrap();
    let source = Arc::new(MemAppSource::new());
    source.publish(
        "wide.test",
        &AppId::parse("wide").unwrap(),
        manifest(SMALL),
        dir.path(),
    );
    let host = Arc::new(Host::new(
        source,
        Budget::new(256 << 20, Duration::from_secs(3_600)),
    ));
    let lifecycle = Arc::new(Lifecycle::describing(
        Arc::new(Fleet::new(Arc::clone(&host))),
        Duration::from_millis(100),
    ));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let admin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (stopped, returned) = tokio::sync::oneshot::channel();
    let replica = Arc::clone(&lifecycle);
    serve_apart(async move {
        let result = dagpane_serve::serve_host_replica(
            host,
            listening(listener),
            listening(admin),
            None,
            Liveness {
                heartbeat: None,
                write_timeout: None,
                ping_timeout: None,
            },
            replica,
        )
        .await;
        let _ = stopped.send(result.is_ok());
    });
    wait_for(addr).await;
    assert_a_stuck_viewer_cannot_hold_the_stop(addr, "wide.test", &lifecycle, returned).await;
}
