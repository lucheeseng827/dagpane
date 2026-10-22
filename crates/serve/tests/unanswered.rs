//! The ping deadline, over real sockets: a client that stops reading on a quiet connection is
//! dropped once a ping goes unanswered, and a client that reads — even slowly, a large frame at
//! a time — is not.
//!
//! This is the case the write deadline (`tests/deadline.rs`) cannot see. Nothing is being sent
//! but pings, and a ping is a few bytes the client's kernel takes and acknowledges whether its
//! process reads them or not, so no write ever waits; the pings themselves keep a proxy's idle
//! timeout from firing; and TCP sees a healthy peer. Only an answer shows the client is reading:
//! a Pong echoing the ping, which it can send only once it has read everything before it.
//!
//! The pings here are a tenth of a second apart and the deadline a second, so a stall shows in
//! about a second rather than a minute. The clients are `tokio-tungstenite`, which answers a
//! ping when it reads one — the same thing a browser's transport does — and stops answering the
//! moment it stops reading.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use std::time::Duration;

use dagpane_host::{AppId, Budget, Host, MemAppSource};
use dagpane_serve::{Fleet, Lifecycle, Liveness, Options};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// One table pane over every row: `rows` sets how large the opening frame is.
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

/// One request whose pass takes a while, a filter and a sort over [`HEAVY`] rows, and whose
/// answer is a few bytes: a count and the top three rows.
fn heavy_manifest() -> String {
    r#"
[app]
title = "Heavy"

[[source]]
name = "sales"
csv = "sales.csv"

[[input]]
name = "min_amount"
label = "Min"
slider = { min = 0.0, max = 100000000.0, step = 1.0, default = 0.0 }

[[cell]]
name = "filtered"
from = "sales"
[[cell.step]]
filter = { column = "amount", op = "ge", param = "min_amount" }

[[cell]]
name = "top"
from = "filtered"
[[cell.step]]
sort = { column = "amount", descending = true }
[[cell.step]]
limit = 3

[[cell]]
name = "n"
from = "filtered"
[[cell.step]]
count = true

[[pane]]
cell = "n"
metric = { label = "Rows" }

[[pane]]
cell = "top"
title = "Top"
table = { max_rows = 3 }
"#
    .to_string()
}

/// `rows` amounts, scrambled so the sort has work to do.
fn heavy_csv(rows: usize) -> String {
    let mut text = String::from("region,amount\n");
    for i in 0..rows {
        let amount = (i as u64).wrapping_mul(2_654_435_761) % 10_000_000;
        text.push_str(&format!("r{},{amount}.0\n", i % 4));
    }
    text
}

/// Rows for a quiet connection: an opening frame of a couple of kilobytes, so the first ping's
/// allowance is the deadline and barely more.
const QUIET: usize = 10;

/// Rows in the slow reader's opening frame: about eight and a half megabytes, more than the
/// kernel's buffers hold, so the pings written after it wait behind megabytes the client has
/// not read yet.
const LARGE: usize = 60_000;

/// How often the server pings.
const BEAT: Duration = Duration::from_millis(100);

/// How long a ping may go unanswered, beyond the time the bytes before it need.
const ANSWER_WITHIN: Duration = Duration::from_secs(1);

/// Rows the long pass works through: a couple of seconds of filtering and sorting in the debug
/// build the tests run in, and still a few tenths of one in a release build.
const HEAVY: usize = 400_000;

/// WebSocket opcodes, for the tests that read and write frames themselves.
const TEXT: u8 = 0x1;
const BINARY: u8 = 0x2;
const CLOSE: u8 = 0x8;
const PING: u8 = 0x9;
const PONG: u8 = 0xA;

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
/// rendering and serialising a frame of megabytes, which a debug build takes its time over —
/// holds a worker the clients need, and on a runner with two cores it can hold both. A client
/// that answered in time is then not polled to send its answer, and is dropped for a deadline
/// it met. A browser is another process, which the operating system runs whatever the server is
/// doing; on threads of their own, so are these clients.
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

/// `dagpane run`'s shape: one app over `rows` rows, pinging every [`BEAT`], with no write
/// deadline — so whatever drops a connection here is the ping deadline.
async fn serve_one(
    dir: &std::path::Path,
    rows: usize,
    ping_timeout: Option<Duration>,
) -> SocketAddr {
    serve_app(dir, manifest(rows), csv(rows), BEAT, ping_timeout).await
}

/// [`serve_one`] for any app, pinging every `beat`.
async fn serve_app(
    dir: &std::path::Path,
    app_toml: String,
    sales_csv: String,
    beat: Duration,
    ping_timeout: Option<Duration>,
) -> SocketAddr {
    std::fs::write(dir.join("wide.toml"), app_toml).unwrap();
    std::fs::write(dir.join("sales.csv"), sales_csv).unwrap();
    let app = Arc::new(dagpane_app::load(&dir.join("wide.toml")).unwrap());
    // A receive buffer of a quarter of a megabyte, which each connection takes from the
    // listener. Left to size itself over loopback it stays small: a client flooding the server
    // can never queue a whole read's worth, every read comes back short, and a short read has
    // the server wait for the next one — which puts its timers right, and hides the lag the
    // flood tests are for. A stock kernel grants 208 KiB of it, and even then the window a
    // connection keeps can hold less than a read's worth for a while: the cut-off test floods
    // five connections for that reason.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(256 * 1024).unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = socket.listen(1024).unwrap().into_std().unwrap();
    let addr = listener.local_addr().unwrap();
    let options = Options {
        app_name: "wide".into(),
        heartbeat: Some(beat),
        write_timeout: None,
        ping_timeout,
        ..Default::default()
    };
    serve_apart(async move {
        let _ = dagpane_serve::serve_on_with(app, listening(listener), options).await;
    });
    wait_for(addr).await;
    addr
}

/// Through the upgrade on `host` over `stream`, past the opening frame.
async fn open<S>(stream: S, host: &str) -> WebSocketStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = format!("ws://{host}/ws").into_client_request().unwrap();
    let (mut ws, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .expect("the upgrade succeeds");
    loop {
        match ws.next().await.expect("the opening frame") {
            Ok(Message::Text(text)) => {
                assert!(
                    text.contains("\"type\":\"init\""),
                    "{}",
                    &text[..80.min(text.len())]
                );
                return ws;
            }
            Ok(_) => continue,
            Err(e) => panic!("the server dropped a client before its opening frame arrived: {e}"),
        }
    }
}

async fn connect(addr: SocketAddr) -> tokio::net::TcpStream {
    tokio::net::TcpStream::connect(addr).await.unwrap()
}

/// One frame as a client must send it: FIN and `opcode`, the mask bit and the length, the mask,
/// and the payload under it.
fn masked(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mask = [1u8, 2, 3, 4];
    let mut frame = vec![0x80 | opcode];
    match payload.len() {
        n if n < 126 => frame.push(0x80 | n as u8),
        n if n <= usize::from(u16::MAX) => {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().zip(mask.iter().cycle()).map(|(b, m)| b ^ m));
    frame
}

/// What a flooding client writes, again and again: ten thousand empty binary frames, built once
/// and written straight to the socket. A client framing each through its library barely
/// outpaces a server reading them, and a server that catches up waits, which puts its timers
/// right. Each of these costs the server a frame's work for six bytes, so they queue faster than
/// it reads them, and it never does.
fn flood_frames() -> Vec<u8> {
    masked(BINARY, &[]).repeat(10_000)
}

/// One frame from the server, which sends them unmasked: its opcode and its payload.
async fn read_frame<R: AsyncRead + Unpin>(from: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    from.read_exact(&mut head).await?;
    let len = match head[1] & 0x7f {
        126 => u64::from(from.read_u16().await?),
        127 => from.read_u64().await?,
        n => u64::from(n),
    };
    let mut payload = vec![0u8; len as usize];
    from.read_exact(&mut payload).await?;
    Ok((head[0] & 0x0f, payload))
}

/// Make one request and wait for its answer, as a client that reads does: an empty `set`,
/// whose `patch` is a few bytes however large the table — a refresh would send the whole of it
/// again, which a slow reader takes seconds over.
async fn ask_and_read<S>(ws: &mut WebSocketStream<S>, seq: u64) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let ask = Message::Text(format!(r#"{{"type":"set","seq":{seq},"values":{{}}}}"#).into());
    if ws.send(ask).await.is_err() {
        return false;
    }
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) if text.contains("\"patch\"") => return true,
            Ok(Some(Ok(_))) => continue,
            _ => return false,
        }
    }
}

/// Read for `window`: how many pings arrived, and whether the server ended the connection.
async fn read_for<S>(ws: &mut WebSocketStream<S>, window: Duration) -> (usize, bool)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let until = tokio::time::Instant::now() + window;
    let mut pings = 0;
    loop {
        match tokio::time::timeout_at(until, ws.next()).await {
            Err(_) => return (pings, false),
            Ok(None | Some(Err(_)) | Some(Ok(Message::Close(_)))) => return (pings, true),
            Ok(Some(Ok(Message::Ping(_)))) => pings += 1,
            Ok(Some(Ok(_))) => {}
        }
    }
}

/// A client on a slow link, or one that reads slowly: at most `CHUNK` bytes, then a pause. It
/// still reads, so it still answers every ping — once it has reached it.
struct Throttled {
    inner: tokio::net::TcpStream,
    pause: Option<Pin<Box<tokio::time::Sleep>>>,
    /// Slow only while this holds, so a client can read at full speed and then slow down.
    slow: Arc<AtomicBool>,
}

impl Throttled {
    async fn connect(addr: SocketAddr, slow: bool) -> (Throttled, Arc<AtomicBool>) {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        // A small window, so what the client has not read waits in the server's buffers
        // rather than its own.
        socket.set_recv_buffer_size(65_536).unwrap();
        let slow = Arc::new(AtomicBool::new(slow));
        let stream = Throttled {
            inner: socket.connect(addr).await.unwrap(),
            pause: None,
            slow: Arc::clone(&slow),
        };
        (stream, slow)
    }
}

impl Throttled {
    /// About one and a half megabytes a second: a hundred times the slowest reader the server
    /// allows for, and still several seconds for the large opening frame.
    const CHUNK: usize = 64 * 1024;
    const PAUSE: Duration = Duration::from_millis(40);
}

impl AsyncRead for Throttled {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.slow.load(Ordering::Relaxed) {
            return Pin::new(&mut self.inner).poll_read(cx, buf);
        }
        if let Some(pause) = self.pause.as_mut() {
            ready!(pause.as_mut().poll(cx));
            self.pause = None;
        }
        let mut chunk = vec![0u8; buf.remaining().min(Self::CHUNK)];
        let mut part = ReadBuf::new(&mut chunk);
        ready!(Pin::new(&mut self.inner).poll_read(cx, &mut part))?;
        let n = part.filled().len();
        buf.put_slice(&chunk[..n]);
        if n > 0 {
            self.pause = Some(Box::pin(tokio::time::sleep(Self::PAUSE)));
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Throttled {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_client_that_stops_reading_is_dropped_and_one_that_reads_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let addr = serve_one(dir.path(), QUIET, Some(ANSWER_WITHIN)).await;
    let host = addr.to_string();
    let mut stalled = open(connect(addr).await, &host).await;
    let mut reading = open(connect(addr).await, &host).await;

    // Nothing is asked of either. The stalled client is simply never read from again: its
    // kernel takes the pings, so no write on the server waits, and nothing but an answer is
    // missing. The other reads, and so answers, throughout.
    let (heard, ended) = read_for(&mut reading, Duration::from_secs(4)).await;
    assert!(!ended, "a client that reads was dropped");
    assert!(
        heard >= 20,
        "only {heard} pings in four seconds: the heartbeat is not running"
    );
    assert!(
        ask_and_read(&mut reading, 1).await,
        "a client that reads is no longer answered"
    );

    // Now read what the stalled connection was sent. Dropped, it ends after the pings written
    // before its deadline — a second and a little after the first; kept, it carries on for
    // as long as anyone reads it.
    let (buffered, ended) = read_for(&mut stalled, Duration::from_secs(3)).await;
    assert!(
        ended,
        "a client that stopped reading kept its connection: {buffered} pings and still open"
    );
    assert!(
        buffered < 30,
        "the connection ended, but only after {buffered} pings — not at the ping deadline"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_stops_reading_cannot_keep_its_connection_with_pongs_of_its_own() {
    // RFC 6455 lets an endpoint send a Pong nobody asked for, as a one-way heartbeat. One
    // proves the client can write, not that it reads — so it must not count as an answer, or
    // a client wedged in its reading loop but not its writing one would be kept for good. Nor
    // must a Pong a client could produce by counting: the pings go out once a beat, numbered
    // in order, so their numbers alone are no secret.
    let dir = tempfile::tempdir().unwrap();
    let addr = serve_one(dir.path(), QUIET, Some(ANSWER_WITHIN)).await;
    let mut stalled = open(connect(addr).await, &addr.to_string()).await;
    let opened = tokio::time::Instant::now();

    // Sending never reads, so these go out and nothing comes in.
    'sending: for _ in 0..30 {
        let counted = opened.elapsed().as_millis() / BEAT.as_millis();
        let guesses = counted.saturating_sub(3)..=counted + 1;
        let pongs = std::iter::once(b"still here".to_vec())
            .chain(guesses.map(|k| (k as u64).to_be_bytes().to_vec()));
        for payload in pongs {
            if stalled.send(Message::Pong(payload.into())).await.is_err() {
                break 'sending;
            }
        }
        tokio::time::sleep(BEAT).await;
    }
    let (buffered, ended) = read_for(&mut stalled, Duration::from_secs(3)).await;
    assert!(
        ended,
        "unsolicited pongs kept a client that never read: {buffered} pings and still open"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_stops_reading_cannot_put_the_deadline_off_by_sending() {
    // Whatever has already arrived is read before an overdue ping drops its client, in case the
    // answer is in it — so a client that never reads might hope to stay by never letting the
    // server run out of frames to read. It must go while it is still sending, and soon after
    // its first ping falls due: a beat and the deadline in, a little over a second. Two things
    // see to that: the clock, asked before every frame, because a loop a flood never lets wait
    // leaves the runtime's timers behind — by another second on an idle machine and four on a
    // busy one, measured — and the cap on how many frames are read for an overdue answer.
    //
    // Five connections, one after another, and each must go within a second and a half of its
    // flood starting, its first ping having fallen due at about 1.2 s. Only a flood that keeps a
    // whole read's worth queued tells the clock from the timer, and how much a connection keeps
    // queued is the kernel's to decide: at a stock kernel's buffer size, measured, as many as one
    // connection in three kept less for stretches of its flood, the server waited between reads,
    // and each wait put its timers right. A server that waits on its timer still drops the
    // client only at its first wait after the deadline, which on those connections came
    // anywhere after about 1.22 s and often well past 2 s, against 1.21 s for the clock. So a
    // server that waited on its timer would pass only if all five connections fell short, and
    // each early enough.
    let dir = tempfile::tempdir().unwrap();
    let addr = serve_one(dir.path(), QUIET, Some(ANSWER_WITHIN)).await;
    let flood = flood_frames();
    for connection in 1..=5 {
        let mut stalled = open(connect(addr).await, &addr.to_string()).await;
        let tcp = stalled.get_mut();
        let started = tokio::time::Instant::now();
        let mut cut_off = false;
        while let Ok(written) =
            tokio::time::timeout_at(started + ANSWER_WITHIN * 3 / 2, tcp.write_all(&flood)).await
        {
            if written.is_err() {
                cut_off = true;
                break;
            }
        }
        let after = started.elapsed();
        assert!(
            cut_off,
            "connection {connection}: a client that never read was kept for {after:?} while it \
             kept sending, though its first ping fell due a beat and a deadline in"
        );
        // And it was the deadline that went, not anything sooner: the first ping is a beat in,
        // and due a deadline after that.
        assert!(
            after >= ANSWER_WITHIN,
            "connection {connection}: a client that never read was cut off after {after:?}, \
             before its first ping could fall due: something other than the deadline dropped it"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_flood_does_not_hold_the_heartbeat_back() {
    // The heartbeat is asked of the clock for the reason the deadline is: a client that keeps
    // the loop from waiting would leave its timer behind, and pings would go out late or not at
    // all, the first of them being the one a deadline counts from. This client floods and reads,
    // and with no deadline nothing drops it, so it sees every ping it is sent.
    let dir = tempfile::tempdir().unwrap();
    let addr = serve_one(dir.path(), QUIET, None).await;
    let tcp = open(connect(addr).await, &addr.to_string())
        .await
        .into_inner();
    let (mut from_server, mut to_server) = tokio::io::split(tcp);
    let window = Duration::from_secs(2);
    let started = tokio::time::Instant::now();
    let flooding = tokio::spawn(async move {
        let flood = flood_frames();
        while tokio::time::timeout_at(started + window, to_server.write_all(&flood))
            .await
            .is_ok_and(|written| written.is_ok())
        {}
    });
    let mut pings = Vec::new();
    while let Ok(Ok((opcode, _))) =
        tokio::time::timeout_at(started + window, read_frame(&mut from_server)).await
    {
        if opcode == PING {
            pings.push(started.elapsed());
        }
    }
    flooding.await.unwrap();
    let longest = pings
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .max()
        .unwrap_or(window);
    assert!(
        pings.len() >= 12 && longest < Duration::from_millis(500),
        "{} pings in {window:?} under a flood, at a {BEAT:?} beat, the longest gap {longest:?}",
        pings.len()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_reading_a_large_frame_slowly_answers_late_and_is_kept() {
    // The case a bare deadline per ping gets wrong. The pings written after a large opening
    // frame queue behind megabytes the client has not read yet, so it cannot answer them until
    // it has — seconds after they were sent, on this link, which is still a hundred times the
    // slowest reader the server allows for. It must not be taken for a client that stopped:
    // that would reconnect it into the same frame, and the page would never render.
    let dir = tempfile::tempdir().unwrap();
    let addr = serve_one(dir.path(), LARGE, Some(ANSWER_WITHIN)).await;
    let (stream, _) = Throttled::connect(addr, true).await;
    let mut ws = open(stream, &addr.to_string()).await;

    // The pings that queued behind the frame arrive together, the moment it ends; after them,
    // one a beat. So the number in hand at once says how long the first of them waited — and
    // unless that was longer than the bare deadline, this test proves nothing about the time
    // the frame's bytes are given. It holds where the kernel's send buffers are the usual
    // megabytes; a host that caps them small fails here, rather than passing without the
    // allowance ever being needed.
    let (queued, ended) = read_for(&mut ws, BEAT * 2).await;
    assert!(
        !ended,
        "the server dropped a client the moment it caught up"
    );
    let needed = (ANSWER_WITHIN.as_millis() / BEAT.as_millis()) as usize + 4;
    assert!(
        queued >= needed,
        "only {queued} pings had queued behind the frame, so the first waited less than the \
         bare deadline and the bytes before it were never needed; {needed} would show it"
    );

    // Caught up, it answers the pings that waited behind the frame and every one after.
    let (heard, ended) = read_for(&mut ws, Duration::from_secs(2)).await;
    assert!(
        !ended,
        "the server dropped a client that was reading, after {heard} pings"
    );
    assert!(
        ask_and_read(&mut ws, 1).await,
        "a slow reader is no longer answered"
    );
}

/// One client of the race below. It reads the opening frame and answers pings at full speed,
/// then reads one more ping and asks for every pane in the same breath — its library writes
/// the request ahead of the Pong — and reads the answer, megabytes, slowly. Whether the server
/// dropped it.
async fn answer_then_ask_for_a_large_frame(addr: SocketAddr) -> bool {
    let (stream, slow) = Throttled::connect(addr, false).await;
    let mut ws = open(stream, &addr.to_string()).await;
    // Caught up, and answering, so everything before these pings is shown read.
    if read_for(&mut ws, BEAT * 5).await.1 {
        return true;
    }
    loop {
        match ws.next().await {
            Some(Ok(Message::Ping(_))) => break,
            Some(Ok(_)) => continue,
            _ => return true,
        }
    }
    let ask = Message::Text(r#"{"type":"refresh","seq":1}"#.into());
    if ws.send(ask).await.is_err() {
        return true;
    }
    // Read slowly, the answer keeps the server writing for seconds — past that ping's
    // deadline — while the Pong it is owed waits, unread, on the server's side.
    slow.store(true, Ordering::Relaxed);
    loop {
        match tokio::time::timeout(Duration::from_secs(30), ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) if text.contains("\"refreshed\"") => break,
            Ok(Some(Ok(_))) => continue,
            _ => return true,
        }
    }
    read_for(&mut ws, Duration::from_secs(2)).await.1
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_that_arrives_while_the_server_is_busy_writing_still_counts() {
    // The race a deadline beside `recv` in a `select!` loses. A client answers a ping at once,
    // but the server is busy writing to it when the answer lands — and every server is busy
    // like that when it is short of workers. Once the write is done the answer and the overdue
    // deadline are both ready, and left to `select!` the deadline won most of the time: the
    // client was dropped for an answer it gave in milliseconds. Four clients at once, so a
    // race lost even a third of the time fails this nearly always.
    let dir = tempfile::tempdir().unwrap();
    let addr = serve_one(dir.path(), LARGE, Some(ANSWER_WITHIN)).await;
    let clients: Vec<_> = (0..4)
        .map(|_| tokio::spawn(answer_then_ask_for_a_large_frame(addr)))
        .collect();
    let mut dropped = 0;
    for client in clients {
        if client.await.unwrap() {
            dropped += 1;
        }
    }
    assert_eq!(
        dropped, 0,
        "{dropped} of 4 clients that answered in time were dropped"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_that_lands_while_the_server_is_busy_computing_still_counts() {
    // The same race with the server computing rather than writing, and the hard case of it. A
    // pass holds the server's one thread from end to end, so nothing reads the operating
    // system's word that the answer has landed: when the pass ends, the clock says the ping is
    // overdue and the runtime has seen nothing arrive. The server must let the runtime look
    // before it believes the clock, and look past a frame that is not the answer. Twice on one
    // connection, because the look is owed to the first overdue ping after every answer, not
    // only to the first the connection ever has.
    let dir = tempfile::tempdir().unwrap();
    let beat = Duration::from_millis(300);
    let deadline = Duration::from_millis(100);
    let addr = serve_app(
        dir.path(),
        heavy_manifest(),
        heavy_csv(HEAVY),
        beat,
        Some(deadline),
    )
    .await;
    let mut tcp = open(connect(addr).await, &addr.to_string())
        .await
        .into_inner();
    for round in 1..=2 {
        // Answer pings for a while, so that everything written before the one this round leaves
        // to the pass is shown read, and its allowance is the deadline alone.
        let settled = tokio::time::Instant::now() + beat * 2;
        let ping = loop {
            let frame = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut tcp))
                .await
                .unwrap_or_else(|_| panic!("round {round}: no ping in ten seconds"));
            match frame {
                Ok((PING, payload)) if tokio::time::Instant::now() >= settled => break payload,
                Ok((PING, payload)) => tcp.write_all(&masked(PONG, &payload)).await.unwrap(),
                Ok(_) => {}
                Err(e) => panic!("round {round}: dropped while answering every ping: {e}"),
            }
        };
        // A request whose pass runs for seconds; then, once the server is in it, a Pong that
        // answers nothing, as RFC 6455 allows, and the answer behind it. Each round's minimum
        // leaves out another tenth of the rows: one that left the same rows in would find the
        // sort's input unchanged, and the pass would be over in a moment.
        let asked = tokio::time::Instant::now();
        let set = format!(
            r#"{{"type":"set","seq":{round},"values":{{"min_amount":{{"kind":"float","v":{}.0}}}}}}"#,
            round * 1_000_000
        );
        tcp.write_all(&masked(TEXT, set.as_bytes())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        tcp.write_all(&masked(PONG, b"not an answer"))
            .await
            .unwrap();
        tcp.write_all(&masked(PONG, &ping)).await.unwrap();

        // The patch, and a little past it: a client dropped when the pass ends is dropped just
        // after its patch goes out. The pings that come meanwhile are answered.
        let mut patched = None;
        let mut pings_after = 0;
        loop {
            let until = patched.map_or(asked + Duration::from_secs(30), |at| {
                at + Duration::from_millis(150)
            });
            let Ok(frame) = tokio::time::timeout_at(until, read_frame(&mut tcp)).await else {
                break;
            };
            match frame {
                Ok((PING, payload)) => {
                    pings_after += usize::from(patched.is_some());
                    // Into a connection that may be gone: the next read says so.
                    let _ = tcp.write_all(&masked(PONG, &payload)).await;
                }
                Ok((TEXT, text)) if String::from_utf8_lossy(&text).contains("\"patch\"") => {
                    patched.get_or_insert_with(tokio::time::Instant::now);
                }
                Ok((CLOSE, _)) | Err(_) => panic!(
                    "round {round}: a client whose answer landed during the pass was dropped \
                     when it ended"
                ),
                Ok(_) => {}
            }
        }
        let took = patched.expect("the pass is answered within 30 s") - asked;
        // Shorter than the deadline, and the answer is read before anything is overdue.
        assert!(
            took > deadline * 2,
            "round {round}: the pass took {took:?}, too short to outlast a {deadline:?} deadline, \
             so this proves nothing: make HEAVY larger"
        );
        // The beat that fell due during the pass goes out once it ends, and only that: not one
        // for every beat the pass held up, to prove a liveness its patch has just proved.
        assert!(
            pings_after <= 2,
            "round {round}: {pings_after} pings came together after a pass of {took:?}, at a \
             {beat:?} beat"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_ping_deadline_a_quiet_client_that_stops_reading_keeps_its_connection() {
    // The control for the first test: the same stall with the deadline off is waited out, and
    // the client is answered once it reads again. Without it, that test could pass because
    // something else dropped the connection.
    let dir = tempfile::tempdir().unwrap();
    let addr = serve_one(dir.path(), QUIET, None).await;
    let mut stalled = open(connect(addr).await, &addr.to_string()).await;

    tokio::time::sleep(Duration::from_secs(4)).await;
    let (buffered, ended) = read_for(&mut stalled, Duration::from_secs(1)).await;
    assert!(!ended, "the connection ended with the ping deadline off");
    assert!(buffered >= 20, "only {buffered} pings were waiting");
    assert!(
        ask_and_read(&mut stalled, 1).await,
        "the client was not answered"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn under_host_a_quiet_viewer_that_stops_reading_lets_go_of_its_session() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sales.csv"), csv(QUIET)).unwrap();
    let source = Arc::new(MemAppSource::new());
    source.publish(
        "wide.test",
        &AppId::parse("wide").unwrap(),
        manifest(QUIET),
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
                heartbeat: Some(BEAT),
                write_timeout: None,
                ping_timeout: Some(ANSWER_WITHIN),
            },
            lifecycle,
        )
        .await;
    });
    wait_for(addr).await;

    let stalled = open(connect(addr).await, "wide.test").await;
    assert_eq!(
        host.sessions().len(),
        1,
        "the viewer's session is not on the books"
    );

    // Never read again. The session lives exactly as long as the connection does, so the books
    // are where a dropped connection shows.
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
        "a quiet viewer that stopped reading still holds its session ten seconds later"
    );
    drop(stalled);
}
