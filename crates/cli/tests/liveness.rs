//! The liveness flags, end to end: `--heartbeat-seconds`, `--write-timeout-seconds` and
//! `--ping-timeout-seconds` given to the real binary under `run`, `serve` and `host`, and each
//! seen doing its own job over real sockets.
//!
//! What each setting does once the server has it is `dagpane-serve`'s to prove, and its tests do
//! (`tests/heartbeat.rs`, `tests/deadline.rs`, `tests/unanswered.rs`). These prove the step
//! before that: that the number typed is the number the server runs with — not dropped on the
//! way, not left at its default, not read into one of the other two settings. So the three flags
//! get three different numbers, and each is checked by something only its own number produces:
//!
//! * the heartbeat, by how often a client that reads is pinged: about once a second;
//! * the ping deadline, by the allowance the server reports when it drops a client that stopped
//!   reading on a quiet connection: two seconds and a fraction;
//! * the write deadline, by the allowance it reports when it drops a client that asked for far
//!   more than it reads: five seconds and a fraction.
//!
//! The fraction is the time [`SLOWEST_READER`] needs for the bytes a frame carries, and every
//! frame here is well under a second of that. So a reported allowance names the flag it came
//! from, and a swapped flag or a default (30) shows up as a number in the wrong place.

use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use dagpane_serve::SLOWEST_READER;
use futures_util::{SinkExt, StreamExt};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

type Socket = WebSocketStream<tokio::net::TcpStream>;

/// The three flags, at three different numbers.
const HEARTBEAT_SECONDS: u64 = 1;
const PING_TIMEOUT_SECONDS: u64 = 2;
const WRITE_TIMEOUT_SECONDS: u64 = 5;

/// Rows in the one table pane: a frame of a few kilobytes, a fraction of a second at
/// [`SLOWEST_READER`]'s rate.
const ROWS: usize = 50;

/// Asked of the client that stops reading: about seven megabytes of answers, more than the
/// largest send buffer the kernel gives a socket (`tcp_wmem`'s 4 MiB) and the client's few
/// kilobytes hold between them, so the server's write backs up behind it and stays backed up.
const ASKED: u64 = 1_000;

/// How long to wait for anything the server is expected to do or say.
const PATIENCE: Duration = Duration::from_secs(20);

fn manifest() -> String {
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
table = {{ max_rows = {ROWS} }}
"#
    )
}

fn csv() -> String {
    let mut text = String::from("region,amount,note\n");
    for i in 0..ROWS {
        text.push_str(&format!(
            "r{},{}.0,a note long enough that every row costs the wire some bytes {i}\n",
            i % 4,
            i * 10
        ));
    }
    text
}

/// A directory holding `wide.toml` and the CSV it reads, of its own: tests here run at once.
fn app_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("wide.toml"), manifest()).unwrap();
    std::fs::write(dir.path().join("sales.csv"), csv()).unwrap();
    dir
}

/// The flags under test, the same for every command.
fn liveness_flags() -> Vec<String> {
    [
        ("--heartbeat-seconds", HEARTBEAT_SECONDS),
        ("--write-timeout-seconds", WRITE_TIMEOUT_SECONDS),
        ("--ping-timeout-seconds", PING_TIMEOUT_SECONDS),
    ]
    .into_iter()
    .flat_map(|(flag, seconds)| [flag.to_string(), seconds.to_string()])
    .collect()
}

/// A command line: these words, then the flags under test.
fn command(words: &[&str]) -> Vec<String> {
    words
        .iter()
        .map(|word| word.to_string())
        .chain(liveness_flags())
        .collect()
}

fn arg(path: &Path) -> String {
    path.to_str().unwrap().to_string()
}

/// Everything `pipe` says, a line at a time as it says it.
fn collect(pipe: impl Read + Send + 'static) -> Arc<Mutex<String>> {
    let text = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&text);
    std::thread::spawn(move || {
        for line in BufReader::new(pipe).lines().map_while(Result::ok) {
            let mut text = sink.lock().unwrap();
            text.push_str(&line);
            text.push('\n');
        }
    });
    text
}

/// The first line of `text` that contains `needle`.
fn find(text: &Mutex<String>, needle: &str) -> Option<String> {
    let text = text.lock().unwrap();
    text.lines()
        .find(|line| line.contains(needle))
        .map(str::to_owned)
}

/// The port in the first URL on `line`: `http://127.0.0.1:41234`, or
/// `http://wide.localhost:41234/`.
fn port_in(line: &str) -> Option<u16> {
    let url = line.split("http://").nth(1)?.split_whitespace().next()?;
    url.trim_end_matches('/').rsplit_once(':')?.1.parse().ok()
}

/// A `dagpane` process, killed when the test lets go of it, and what it has printed so far.
struct Server {
    /// Locked so that a failure can ask whether it is still running: a server that died says
    /// nothing more, which reads like one that never did what was waited for.
    child: Mutex<Child>,
    addr: SocketAddr,
    /// The name its app is reached at, which `host` routes by.
    host: String,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        // A lock poisoned by a panic in `output` still holds a process to kill.
        let child = self.child.get_mut().unwrap_or_else(PoisonError::into_inner);
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Server {
    /// Whether it is still running, and what it has printed so far, for a failure to show.
    ///
    /// A server on its way out can fail a test before it has finished going, because its
    /// connections close first: it has a second to finish before it is called running. Once it
    /// has gone, what it said on its way out may still be in the pipe, and that gets a moment.
    fn output(&self) -> String {
        let mut child = self.child.lock().unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(1);
        let exited = loop {
            match child.try_wait() {
                Ok(None) if std::time::Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                exited => break exited,
            }
        };
        drop(child);
        let running = match exited {
            Ok(None) => "is still running".to_string(),
            Ok(Some(status)) => {
                std::thread::sleep(Duration::from_millis(200));
                format!("exited with {status}")
            }
            Err(e) => format!("is in a state that could not be read: {e}"),
        };
        format!(
            "--- dagpane {running}\n--- stdout\n{}--- stderr\n{}",
            self.stdout.lock().unwrap(),
            self.stderr.lock().unwrap()
        )
    }

    /// Through the upgrade, past the opening frame — and how many bytes that frame carried.
    /// `receive_buffer` fixes the client's TCP receive buffer.
    ///
    /// Within [`PATIENCE`], failing with what the server printed rather than waiting for ever.
    async fn open(&self, receive_buffer: Option<u32>) -> (Socket, usize) {
        match tokio::time::timeout(PATIENCE, upgrade(self.addr, &self.host, receive_buffer)).await {
            Ok(Ok(opened)) => opened,
            Ok(Err(why)) => panic!(
                "no connection to {} opened: {why}\n{}",
                self.addr,
                self.output()
            ),
            Err(_) => panic!(
                "no opening frame from {} within {PATIENCE:?}\n{}",
                self.addr,
                self.output()
            ),
        }
    }

    /// The first line it printed to stdout containing `needle`, waiting for it.
    async fn printed(&self, needle: &str) -> Option<String> {
        Self::wait_for(&self.stdout, needle).await
    }

    /// The first line it logged to stderr containing `needle`, waiting for it.
    async fn logged(&self, needle: &str) -> Option<String> {
        Self::wait_for(&self.stderr, needle).await
    }

    async fn wait_for(text: &Mutex<String>, needle: &str) -> Option<String> {
        let until = Instant::now() + PATIENCE;
        loop {
            if let Some(line) = find(text, needle) {
                return Some(line);
            }
            if Instant::now() >= until {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// `dagpane` with `args`, once it is listening.
///
/// Every port here is 0, and each command prints its address only once its port is bound, with
/// the port the kernel picked: `start` waits for the line that contains `listening` and reads
/// the port from it. Nothing is chosen in advance, so nothing can take a port first. `host` gives
/// the name the app is reached at on that port.
async fn start(args: Vec<String>, listening: &str, host: impl Fn(u16) -> String) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dagpane"))
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary was built by the test harness");
    let mut server = Server {
        stdout: collect(child.stdout.take().unwrap()),
        stderr: collect(child.stderr.take().unwrap()),
        child: Mutex::new(child),
        // Known once the server says which port it got.
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        host: String::new(),
    };
    let until = Instant::now() + PATIENCE;
    let line = loop {
        if let Some(line) = find(&server.stdout, listening) {
            break line;
        }
        let exited = server.child.get_mut().unwrap().try_wait().unwrap();
        if exited.is_some() {
            panic!("dagpane exited before listening\n{}", server.output());
        }
        assert!(
            Instant::now() < until,
            "dagpane printed no {listening:?} line within {PATIENCE:?}\n{}",
            server.output()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let port = port_in(&line).unwrap_or_else(|| panic!("no port in {line:?}"));
    server.addr.set_port(port);
    server.host = host(port);
    server
}

/// [`Server::open`]'s work, with what went wrong for it to report.
async fn upgrade(
    addr: SocketAddr,
    host: &str,
    receive_buffer: Option<u32>,
) -> Result<(Socket, usize), String> {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    if let Some(bytes) = receive_buffer {
        socket.set_recv_buffer_size(bytes).unwrap();
    }
    let stream = socket.connect(addr).await.map_err(|e| e.to_string())?;
    let request = format!("ws://{host}/ws").into_client_request().unwrap();
    let (mut ws, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .map_err(|e| format!("the upgrade failed: {e}"))?;
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) if text.contains("\"type\":\"init\"") => {
                return Ok((ws, text.len()))
            }
            Some(Ok(Message::Text(text))) => {
                let start: String = text.chars().take(80).collect();
                return Err(format!(
                    "the first message is not an opening frame: {start}"
                ));
            }
            Some(Ok(_)) => continue,
            other => return Err(format!("no opening frame: {other:?}")),
        }
    }
}

/// How many pings a client that reads is sent in `window`, and whether its connection ended.
/// Reading is also what answers them: the client's transport echoes each ping it reads.
async fn pinged(ws: &mut Socket, window: Duration) -> (usize, bool) {
    let until = Instant::now() + window;
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

/// The seconds in a log line, between `before` and `after`.
fn seconds(line: &str, before: &str, after: &str) -> f64 {
    line.split_once(before)
        .and_then(|(_, rest)| rest.split_once(after))
        .and_then(|(number, _)| number.parse().ok())
        .unwrap_or_else(|| panic!("no seconds between {before:?} and {after:?} in {line:?}"))
}

/// Whether a reported allowance is `seconds` and the fraction a frame's bytes add: whether it
/// came from a flag set to `seconds`.
fn came_from(seconds: u64, allowed: f64) -> bool {
    let seconds = seconds as f64;
    (seconds..seconds + 1.0).contains(&allowed)
}

/// Each flag, seen doing its own job at its own number, on `server`.
async fn assert_each_flag_reaches_the_server(server: &Server) {
    // What the server says it was given…
    for said in [
        format!("dagpane: heartbeat {HEARTBEAT_SECONDS}s "),
        format!("dagpane: writes    {WRITE_TIMEOUT_SECONDS}s per frame"),
        format!("dagpane: answers   {PING_TIMEOUT_SECONDS}s per ping"),
    ] {
        if server.printed(&said).await.is_none() {
            panic!("dagpane never printed {said:?}\n{}", server.output());
        }
    }

    // …and what it does with it. Three clients at once, each showing one setting.
    //
    // A client that stops reading on a quiet connection: it reads its opening frame and nothing
    // after, so the first ping it is sent goes unanswered.
    let (quiet, opening) = server.open(None).await;
    assert!(
        opening < SLOWEST_READER as usize / 2,
        "the opening frame carries {opening} bytes, half a second or more at the slowest reader's \
         rate, which brings an allowance too near the next flag's number to name its own: make \
         ROWS smaller"
    );

    // A client that asks for far more than it reads, through a receive buffer of a few
    // kilobytes, so the server's write backs up behind it within a few hundred answers.
    let (mut stuck, _) = server.open(Some(4_096)).await;
    let asking = async {
        for seq in 1..=ASKED {
            let ask = format!(r#"{{"type":"refresh","seq":{seq}}}"#);
            stuck.send(Message::Text(ask.into())).await?;
        }
        Ok::<_, tokio_tungstenite::tungstenite::Error>(())
    };
    match tokio::time::timeout(PATIENCE, asking).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("the requests did not go out: {e}\n{}", server.output()),
        Err(_) => panic!(
            "the requests were not taken within {PATIENCE:?}\n{}",
            server.output()
        ),
    }

    // A client that reads, and so answers every ping: pinged about once a second, and kept.
    // Closed once counted, so the pings it would stop answering then cannot drop it too.
    let (mut reading, _) = server.open(None).await;
    let window = Duration::from_millis(4_500);
    let (pings, ended) = pinged(&mut reading, window).await;
    assert!(
        !ended,
        "a client that read everything and answered every ping was dropped\n{}",
        server.output()
    );
    assert!(
        (3..=6).contains(&pings),
        "{pings} pings in {window:?}, where --heartbeat-seconds {HEARTBEAT_SECONDS} sends one a \
         second\n{}",
        server.output()
    );
    drop(reading);

    let Some(unanswered) = server.logged("a ping went ").await else {
        panic!(
            "the client that stopped reading on a quiet connection was never dropped for an \
             unanswered ping\n{}",
            server.output()
        );
    };
    let allowed = seconds(&unanswered, "a ping went ", "s unanswered");
    assert!(
        came_from(PING_TIMEOUT_SECONDS, allowed),
        "the quiet client's ping was allowed {allowed}s, where --ping-timeout-seconds \
         {PING_TIMEOUT_SECONDS} allows {PING_TIMEOUT_SECONDS}s and its opening frame's bytes\n{}",
        server.output()
    );

    let Some(stalled) = server.logged("a frame waited ").await else {
        panic!(
            "the client that asked for more than it read was never dropped for a frame that \
             could not be written\n{}",
            server.output()
        );
    };
    let allowed = seconds(&stalled, "a frame waited ", "s to be written");
    assert!(
        came_from(WRITE_TIMEOUT_SECONDS, allowed),
        "the stuck client's frame was allowed {allowed}s, where --write-timeout-seconds \
         {WRITE_TIMEOUT_SECONDS} allows {WRITE_TIMEOUT_SECONDS}s and the frame's bytes\n{}",
        server.output()
    );

    drop((quiet, stuck));
}

#[tokio::test(flavor = "multi_thread")]
async fn under_run_each_liveness_flag_reaches_the_server() {
    let dir = app_dir();
    let manifest = arg(&dir.path().join("wide.toml"));
    let server = start(
        command(&["run", &manifest, "--port", "0"]),
        "dagpane: http://",
        |port| format!("127.0.0.1:{port}"),
    )
    .await;
    assert_each_flag_reaches_the_server(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn under_serve_each_liveness_flag_reaches_the_server() {
    let dir = app_dir();
    let manifest = arg(&dir.path().join("wide.toml"));
    let server = start(
        command(&[
            "serve",
            &manifest,
            "--bind",
            "127.0.0.1",
            "--port",
            "0",
            "--admin-bind",
            "127.0.0.1",
            "--admin-port",
            "0",
        ]),
        "dagpane: app ",
        |port| format!("127.0.0.1:{port}"),
    )
    .await;
    assert_each_flag_reaches_the_server(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn under_host_each_liveness_flag_reaches_the_server() {
    let dir = app_dir();
    let apps = arg(dir.path());
    let server = start(
        command(&[
            "host",
            &apps,
            "--bind",
            "127.0.0.1",
            "--port",
            "0",
            "--admin-bind",
            "127.0.0.1",
            "--admin-port",
            "0",
        ]),
        // Routed by the first label of the name it is reached at: `wide.toml` is `wide`.
        "http://wide.localhost:",
        |port| format!("wide.localhost:{port}"),
    )
    .await;
    assert_each_flag_reaches_the_server(&server).await;
}

/// Stop `server` the way every supervisor does.
fn terminate(server: &Server) {
    let pid = server.child.lock().unwrap().id().to_string();
    let sent = Command::new("kill").args(["-TERM", &pid]).status();
    assert!(
        matches!(sent, Ok(status) if status.success()),
        "could not signal {pid}: {sent:?}"
    );
}

/// A `SIGTERM`, a viewer told the server is going away once `drain` is over, and an exit of
/// `0` — the whole stop, through the real binary and a real signal.
async fn assert_a_signal_closes_every_viewer(server: &Server, drain: Duration) {
    let (mut ws, _) = server.open(None).await;
    // Before the signal: the drain cannot start before it is sent, so a close at least `drain`
    // after this is a close after the drain.
    let asked = Instant::now();
    terminate(server);

    let patience = drain + dagpane_serve::CLOSE_GRACE + PATIENCE;
    let closed = tokio::time::timeout(patience, async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(frame))) => break frame.map(|f| u16::from(f.code)),
                Some(Ok(_)) => continue,
                None | Some(Err(_)) => break None,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no close within {patience:?}\n{}", server.output()));
    assert_eq!(
        closed,
        Some(dagpane_serve::GOING_AWAY),
        "a stop closes its viewers with going-away, not by dropping them\n{}",
        server.output()
    );
    assert!(
        asked.elapsed() >= drain,
        "closed inside the {drain:?} drain\n{}",
        server.output()
    );
    // Answered, as a browser answers it.
    let _ = ws.next().await;

    let until = std::time::Instant::now() + PATIENCE;
    let status = loop {
        if let Some(status) = server.child.lock().unwrap().try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < until,
            "still running {PATIENCE:?} after its viewer closed\n{}",
            server.output()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(status.success(), "{status}\n{}", server.output());
}

#[tokio::test(flavor = "multi_thread")]
async fn under_run_a_signal_closes_every_viewer_with_going_away() {
    let dir = app_dir();
    let manifest = arg(&dir.path().join("wide.toml"));
    let server = start(
        command(&["run", &manifest, "--port", "0"]),
        "dagpane: http://",
        |port| format!("127.0.0.1:{port}"),
    )
    .await;
    // `run` stops at once: no balancer, so no drain.
    assert_a_signal_closes_every_viewer(&server, Duration::ZERO).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn under_serve_a_signal_closes_every_viewer_with_going_away_after_the_drain() {
    let dir = app_dir();
    let manifest = arg(&dir.path().join("wide.toml"));
    let server = start(
        command(&[
            "serve",
            &manifest,
            "--bind",
            "127.0.0.1",
            "--port",
            "0",
            "--admin-bind",
            "127.0.0.1",
            "--admin-port",
            "0",
            "--drain-seconds",
            "1",
        ]),
        "dagpane: app ",
        |port| format!("127.0.0.1:{port}"),
    )
    .await;
    assert_a_signal_closes_every_viewer(&server, Duration::from_secs(1)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn under_host_a_signal_closes_every_viewer_with_going_away_after_the_drain() {
    let dir = app_dir();
    let apps = arg(dir.path());
    let server = start(
        command(&[
            "host",
            &apps,
            "--bind",
            "127.0.0.1",
            "--port",
            "0",
            "--admin-bind",
            "127.0.0.1",
            "--admin-port",
            "0",
            "--drain-seconds",
            "1",
        ]),
        "http://wide.localhost:",
        |port| format!("wide.localhost:{port}"),
    )
    .await;
    assert_a_signal_closes_every_viewer(&server, Duration::from_secs(1)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn under_run_a_viewer_stuck_in_a_write_cannot_hold_the_stop() {
    // `run`'s own wait for its sockets, which no library test can reach: it stops on a signal
    // to the process, and only a process of its own can be sent one. A viewer stuck in a write
    // is the one a stop cannot close — its loop is waiting on the write — so the process is
    // held to the grace and says what it left, rather than waiting the write deadline out.
    let dir = app_dir();
    let manifest = arg(&dir.path().join("wide.toml"));
    let server = start(
        command(&["run", &manifest, "--port", "0"]),
        "dagpane: http://",
        |port| format!("127.0.0.1:{port}"),
    )
    .await;

    let (mut stuck, _) = server.open(Some(4_096)).await;
    let asking = async {
        for seq in 1..=ASKED {
            let ask = format!(r#"{{"type":"refresh","seq":{seq}}}"#);
            stuck.send(Message::Text(ask.into())).await?;
        }
        Ok::<_, tokio_tungstenite::tungstenite::Error>(())
    };
    if !matches!(tokio::time::timeout(PATIENCE, asking).await, Ok(Ok(()))) {
        panic!("the requests did not go out\n{}", server.output());
    }
    // Long enough for the answers to back up behind it, and well inside the write deadline.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let (mut reading, _) = server.open(None).await;

    let asked = Instant::now();
    terminate(&server);
    let closed = tokio::time::timeout(PATIENCE, async {
        loop {
            match reading.next().await {
                Some(Ok(Message::Close(frame))) => break frame.map(|f| u16::from(f.code)),
                Some(Ok(_)) => continue,
                None | Some(Err(_)) => break None,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the reading viewer was never closed\n{}", server.output()));
    assert_eq!(
        closed,
        Some(dagpane_serve::GOING_AWAY),
        "{}",
        server.output()
    );

    let until = std::time::Instant::now() + PATIENCE;
    let status = loop {
        if let Some(status) = server.child.lock().unwrap().try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < until,
            "still running {PATIENCE:?} after the signal\n{}",
            server.output()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let took = asked.elapsed();
    assert!(status.success(), "{status}\n{}", server.output());
    assert!(
        took >= dagpane_serve::CLOSE_GRACE,
        "exited {took:?} after the signal, before the grace was up\n{}",
        server.output()
    );
    assert!(
        server.logged("still open").await.is_some(),
        "the stuck viewer was not what the grace ended\n{}",
        server.output()
    );
    drop(stuck);
}
