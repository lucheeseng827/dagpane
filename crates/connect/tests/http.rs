//! `HttpSource`, against a server this test starts.
//!
//! A hand-written responder rather than a mock library, and a real socket rather than a
//! stubbed client. The thing being tested is what happens to *bytes off a network* — a
//! header that is absent, a status that is not 200, a body larger than the cap — and a mock
//! of the client would be asserting about this author's model of the client.

#![cfg(feature = "http")]

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use dagpane_connect::{HttpSource, Source, SourceError};
use dagpane_core::frame::{FrameBuilder, TableBuilder};
use dagpane_core::ColumnType;

fn builder() -> Box<dyn FrameBuilder> {
    Box::new(TableBuilder::new())
}

const SALES: &str = "region,amount\nnorth,10\nsouth,20\n";

/// What one request should be answered with.
#[derive(Clone)]
struct Reply {
    status: &'static str,
    headers: Vec<(&'static str, String)>,
    body: String,
}

impl Reply {
    fn ok(body: &str) -> Reply {
        Reply {
            status: "200 OK",
            headers: Vec::new(),
            body: body.to_string(),
        }
    }

    fn with(mut self, name: &'static str, value: &str) -> Reply {
        self.headers.push((name, value.to_string()));
        self
    }
}

/// A server that answers every request the same way, and counts them.
struct Server {
    addr: SocketAddr,
    requests: Arc<AtomicUsize>,
}

impl Server {
    fn start(reply: Reply) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                counter.fetch_add(1, Ordering::Relaxed);

                // Read the request line and the headers, so the client's write completes
                // before this end starts writing — a server that replies without draining
                // can give the client a broken pipe on a large request.
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) if line == "\r\n" || line == "\n" => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }

                // A HEAD gets the headers and no body, which is the whole point of asking.
                let head = request_line.starts_with("HEAD");
                let mut out = format!("HTTP/1.1 {}\r\n", reply.status);
                out.push_str(&format!("content-length: {}\r\n", reply.body.len()));
                out.push_str("content-type: text/csv\r\n");
                for (name, value) in &reply.headers {
                    out.push_str(&format!("{name}: {value}\r\n"));
                }
                out.push_str("connection: close\r\n\r\n");
                if !head {
                    out.push_str(&reply.body);
                }
                let _ = stream.write_all(out.as_bytes());
                let _ = stream.flush();
            }
        });

        Server { addr, requests }
    }

    fn url(&self) -> String {
        format!("http://{}/data.csv", self.addr)
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }
}

#[test]
fn a_url_returning_csv_loads_like_a_file_does() {
    let server = Server::start(Reply::ok(SALES).with("etag", "\"v1\""));
    let source = HttpSource::new(server.url()).unwrap();

    assert_eq!(
        source.schema().unwrap(),
        vec![
            ("region".to_string(), ColumnType::Text),
            ("amount".to_string(), ColumnType::Int),
        ]
    );

    let frame = source.load(builder()).unwrap();
    assert_eq!(frame.rows(), 2);
    assert_eq!(frame.value_at(0, 0).as_text(), Some("north"));
    assert_eq!(frame.value_at(1, 1).as_int(), Some(20));
}

#[test]
fn an_etag_is_the_version_and_an_unchanged_one_costs_a_head_and_no_body() {
    let server = Server::start(Reply::ok(SALES).with("etag", "\"v1\""));
    let source = HttpSource::new(server.url()).unwrap();

    let first = source.version().unwrap();
    assert_eq!(
        first,
        source.version().unwrap(),
        "the same ETag moved the version"
    );
    assert_eq!(
        server.requests(),
        2,
        "two versions should be two HEADs and nothing else"
    );

    // A different ETag on an identical body still moves the version — the cheap check is
    // allowed to over-report, and the engine's digest is what stops a repaint.
    let other = Server::start(Reply::ok(SALES).with("etag", "\"v2\""));
    let moved = HttpSource::new(other.url()).unwrap().version().unwrap();
    assert_ne!(first, moved);
}

#[test]
fn a_last_modified_alone_is_enough_to_version_by() {
    let server =
        Server::start(Reply::ok(SALES).with("last-modified", "Wed, 17 Sep 2026 00:00:00 GMT"));
    let source = HttpSource::new(server.url()).unwrap();
    assert_eq!(source.version().unwrap(), source.version().unwrap());
}

#[test]
fn a_server_with_no_validator_is_reloaded_every_time_rather_than_assumed_unchanged() {
    // The dangerous case. Two identical responses with no `ETag` and no `Last-Modified` tell
    // us nothing, and reporting "unchanged" because the lengths matched is how a dashboard
    // shows yesterday's number with today's timestamp on it.
    let server = Server::start(Reply::ok(SALES));
    let source = HttpSource::new(server.url()).unwrap();
    assert_ne!(
        source.version().unwrap(),
        source.version().unwrap(),
        "a source with no validator claimed it was unchanged"
    );
}

#[test]
fn a_status_is_never_rows_and_is_split_by_whether_waiting_could_help() {
    // The split a refresh schedule backs off on. A 503 is "not now"; a 404 is the
    // configuration being wrong, and retrying it every minute until somebody notices is how
    // a source deleted last quarter is still generating traffic today.
    let cases = [
        ("404 Not Found", false),
        ("401 Unauthorized", false),
        ("403 Forbidden", false),
        ("429 Too Many Requests", true),
        ("500 Internal Server Error", true),
        ("503 Service Unavailable", true),
    ];

    for (status, retryable) in cases {
        let server = Server::start(Reply {
            status,
            headers: Vec::new(),
            body: "<html>nope</html>".to_string(),
        });
        let source = HttpSource::new(server.url()).unwrap();
        let err = source.load(builder()).unwrap_err();

        assert_eq!(
            err.is_retryable(),
            retryable,
            "{status} classified wrong: {err:?}"
        );
        assert!(
            err.to_string().contains(status.split(' ').next().unwrap()),
            "the error should name the status: {err}"
        );
        assert!(
            !err.to_string().contains("<html>"),
            "an error body is not rows and must not be quoted at a viewer: {err}"
        );
    }
}

#[test]
fn a_server_that_refuses_head_is_reloaded_rather_than_failed() {
    // Some servers answer 405 or 501 to a HEAD. That tells us nothing about staleness, and
    // failing the refresh over it would strand the source entirely.
    for status in ["405 Method Not Allowed", "501 Not Implemented"] {
        let server = Server::start(Reply {
            status,
            headers: Vec::new(),
            body: String::new(),
        });
        let source = HttpSource::new(server.url()).unwrap();
        let first = source.version().expect(status);
        assert_ne!(first, source.version().unwrap(), "{status}");
    }
}

#[test]
fn a_body_past_the_cap_is_refused_rather_than_read_into_memory() {
    let big = format!("region,amount\n{}", "north,1\n".repeat(20_000));
    let server = Server::start(Reply::ok(&big));
    let source = HttpSource::new(server.url()).unwrap().with_max_bytes(1024);

    let err = source.load(builder()).unwrap_err();
    assert!(matches!(err, SourceError::Unreadable { .. }), "{err:?}");
    assert!(err.to_string().contains("1024"), "{err}");
}

#[test]
fn nothing_listening_is_unreachable_and_retryable() {
    // Bind and drop, so the port is almost certainly free and nothing is behind it.
    let addr = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let source = HttpSource::new(format!("http://{addr}/x.csv")).unwrap();
    let err = source.load(builder()).unwrap_err();
    assert!(matches!(err, SourceError::Unreachable { .. }), "{err:?}");
    assert!(err.is_retryable());
}

#[test]
fn an_error_never_carries_the_query_string() {
    let addr = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let source = HttpSource::new(format!("http://{addr}/x.csv?token=hunter2")).unwrap();
    let err = source.load(builder()).unwrap_err();
    assert!(!err.to_string().contains("hunter2"), "{err}");
    assert!(err.to_string().contains("?***"), "{err}");
}
