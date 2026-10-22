//! The binary, run as a user runs it.
//!
//! These assert on the exact text the commands print, because the README quotes that text
//! and a README whose transcripts drift is a README nobody trusts twice.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn example() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/sales.toml")
}

fn dagpane(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dagpane"))
        .args(args)
        .output()
        .expect("the binary was built by the test harness")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn check_accepts_the_bundled_example() {
    let out = dagpane(&["check", example().to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    assert!(text.contains("Sales explorer — 11 cells"), "{text}");
    assert!(text.ends_with("  ok\n"), "{text}");
}

#[test]
fn check_reports_a_broken_app_and_exits_non_zero() {
    // Target-scoped AND process-unique: `CARGO_TARGET_TMPDIR` alone is shared by every
    // process in a run, so two concurrent invocations would race on the same broken.toml.
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("broken-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("broken.toml");
    std::fs::write(
        &path,
        "[app]\ntitle = \"t\"\n[[cell]]\nname = \"a\"\nfrom = \"ghost\"\n",
    )
    .unwrap();

    let out = dagpane(&["check", path.to_str().unwrap()]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("`ghost`"), "{err}");
}

#[test]
fn explain_prints_the_interaction_and_names_what_was_never_looked_at() {
    let out = dagpane(&[
        "explain",
        example().to_str().unwrap(),
        "--set",
        "min_amount=400",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);

    assert!(text.contains("looked at 8 of 11 cells"), "{text}");
    assert!(text.contains("reused   channel_count"), "{text}");
    assert!(text.contains("same value — nothing below it ran"), "{text}");
    assert!(
        text.contains("3 cell(s) never looked at: sales, region, all_time_revenue"),
        "{text}"
    );
    assert!(
        text.contains("patch: 3 of 7 panes — revenue, order_count, region_totals"),
        "{text}"
    );
    assert!(
        !text.lines().any(|l| l.ends_with(' ')),
        "no line may end in whitespace; a README quoting this would carry it"
    );
}

#[test]
fn explain_with_nothing_set_says_so_rather_than_printing_an_empty_pass() {
    let out = dagpane(&["explain", example().to_str().unwrap()]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("nothing was set"), "{}", stdout(&out));
}

#[test]
fn explain_reports_a_no_op_interaction_as_a_no_op() {
    // The dropdown already reads "all". Setting it there again must not report a pass.
    let out = dagpane(&[
        "explain",
        example().to_str().unwrap(),
        "--set",
        "region=all",
    ]);
    let text = stdout(&out);
    assert!(text.contains("nothing changed"), "{text}");
}

#[test]
fn explain_sets_several_inputs_in_one_pass() {
    let out = dagpane(&[
        "explain",
        example().to_str().unwrap(),
        "--set",
        "min_amount=100",
        "--set",
        "region=north",
    ]);
    let text = stdout(&out);
    assert!(
        text.contains("set min_amount = 100, region = north"),
        "{text}"
    );
    assert!(text.contains("epoch 2"), "one pass, not two:\n{text}");
}

#[test]
fn explain_rejects_a_value_a_widget_could_not_produce() {
    let out = dagpane(&[
        "explain",
        example().to_str().unwrap(),
        "--set",
        "min_amount=99999",
    ]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("outside its range"), "{err}");
}

#[test]
fn explain_json_carries_the_same_numbers_as_the_text() {
    let out = dagpane(&[
        "explain",
        example().to_str().unwrap(),
        "--set",
        "min_amount=400",
        "--json",
    ]);
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    let trace = &json["interaction"]["trace"];
    assert_eq!(trace["total_cells"], 11);
    assert_eq!(trace["steps"].as_array().unwrap().len(), 8);
    assert_eq!(
        json["interaction"]["panes_sent"].as_array().unwrap().len(),
        3
    );
}

#[test]
fn graph_prints_every_cell_in_height_order() {
    let out = dagpane(&["graph", example().to_str().unwrap()]);
    let text = stdout(&out);
    let heights: Vec<u32> = text
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter_map(|h| h.parse().ok())
        .collect();
    assert_eq!(heights.len(), 11);
    assert!(
        heights.windows(2).all(|w| w[0] <= w[1]),
        "heights are not ascending: {heights:?}"
    );
}

#[test]
fn graph_mermaid_is_a_flowchart_with_one_edge_per_declared_input() {
    let out = dagpane(&["graph", example().to_str().unwrap(), "--format", "mermaid"]);
    let text = stdout(&out);
    assert!(text.starts_with("flowchart TD\n"), "{text}");
    // Node ids are synthetic and positional; the author's name only ever appears inside a
    // quoted label, so a cell called `a"]; b[[evil` cannot reshape the diagram.
    assert!(text.contains("c0[(\"sales\")]"), "{text}");
    assert!(text.contains("c1([\"min_amount\"])"), "{text}");
    assert!(text.contains("c3[\"filtered\"]"), "{text}");
    assert!(
        text.contains("c0 --> c3") && text.contains("c1 --> c3"),
        "{text}"
    );
    assert_eq!(
        text.matches(" --> ").count(),
        10,
        "one arrow per declared edge:\n{text}"
    );
}

#[test]
fn graph_json_describes_the_structure_a_tool_would_want() {
    let out = dagpane(&["graph", example().to_str().unwrap(), "--format", "json"]);
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    let cells = json["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 11);
    let filtered = cells.iter().find(|c| c["name"] == "filtered").unwrap();
    assert_eq!(filtered["kind"], "cell");
    assert_eq!(filtered["inputs"].as_array().unwrap().len(), 3);
}

/// The binary, run until it exits on its own, for a command expected to stop at start-up.
///
/// Killed after `within` and failed rather than waited on: a server that was meant to stop and
/// is still running is listening on something, and `output()` would wait on it for ever.
fn dagpane_exiting(args: &[&str], within: Duration) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dagpane"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary was built by the test harness");
    let until = Instant::now() + within;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= until {
            let _ = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!(
                "dagpane {args:?} was still running after {within:?}:\n{}{}",
                stdout(&out),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

/// Both listeners on loopback, at these ports.
fn on<'a>(port: &'a str, admin_port: &'a str) -> [&'a str; 8] {
    [
        "--bind",
        "127.0.0.1",
        "--port",
        port,
        "--admin-bind",
        "127.0.0.1",
        "--admin-port",
        admin_port,
    ]
}

#[test]
fn a_server_whose_port_is_taken_prints_no_url_and_names_the_address() {
    // Each serving command prints its address once the port is bound, so a URL on screen is
    // something listening on it. A port that is taken stops the command before that line, with
    // an error naming the address it could not bind: the app's or the probes'.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port().to_string();
    let app = example();
    let app = app.to_str().unwrap();
    let apps = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let apps = apps.to_str().unwrap();
    let commands: Vec<Vec<&str>> = vec![
        vec!["run", app, "--port", &port],
        ["serve", app].into_iter().chain(on(&port, "0")).collect(),
        ["serve", app].into_iter().chain(on("0", &port)).collect(),
        ["host", apps].into_iter().chain(on(&port, "0")).collect(),
        ["host", apps].into_iter().chain(on("0", &port)).collect(),
    ];
    for args in commands {
        let out = dagpane_exiting(&args, Duration::from_secs(30));
        let printed = stdout(&out);
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?} succeeded");
        assert!(
            !printed.contains("http://"),
            "{args:?} printed a URL it was not listening on:\n{printed}"
        );
        assert!(
            said.contains(&format!("dagpane: 127.0.0.1:{port}: ")),
            "{args:?} did not name the address it could not bind: {said}"
        );
    }
    drop(taken);
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

/// A serving command, killed when the test lets go of it, and what it has printed.
struct Serving {
    child: Child,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
}

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Serving {
    fn start(args: &[&str]) -> Serving {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dagpane"))
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the binary was built by the test harness");
        Serving {
            stdout: collect(child.stdout.take().unwrap()),
            stderr: collect(child.stderr.take().unwrap()),
            child,
        }
    }

    /// The first line it printed containing `needle`, waiting up to 30 s for it.
    fn printed(&self, needle: &str) -> String {
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            let found = self
                .stdout
                .lock()
                .unwrap()
                .lines()
                .find(|line| line.contains(needle))
                .map(str::to_owned);
            if let Some(line) = found {
                return line;
            }
            assert!(
                Instant::now() < until,
                "it never printed {needle:?}:\n{}{}",
                self.stdout.lock().unwrap(),
                self.stderr.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The port in the first URL on `line`: `http://127.0.0.1:41234`,
/// `http://sales.localhost:41234/` or `http://127.0.0.1:41234/healthz, /readyz`.
fn port_in(line: &str) -> u16 {
    line.split("http://")
        .nth(1)
        .and_then(|rest| rest.split(['/', ' ']).next())
        .and_then(|authority| authority.rsplit_once(':'))
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or_else(|| panic!("no port in a URL on {line:?}"))
}

/// The status of `GET path` from what is listening on loopback at `port`, addressed to `host`.
fn get(port: u16, host: &str, path: &str) -> u16 {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))
        .unwrap_or_else(|e| panic!("nothing is listening on the port printed, {port}: {e}"));
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut status = String::new();
    BufReader::new(stream).read_line(&mut status).unwrap();
    status
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("not an HTTP status line: {status:?}"))
}

#[test]
fn every_address_a_server_prints_is_listening() {
    // The other half of the test above. An address is printed once its listener is bound, and
    // read from the listener: asked for port 0, each command prints the port the kernel picked,
    // and the app answers on it, and the probes on theirs.
    let app = example();
    let app = app.to_str().unwrap();
    let apps = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let apps = apps.to_str().unwrap();

    let run = Serving::start(&["run", app, "--port", "0"]);
    let port = port_in(&run.printed("dagpane: http://"));
    assert_eq!(get(port, &format!("127.0.0.1:{port}"), "/"), 200, "run");

    let serve = Serving::start(&[["serve", app].as_slice(), &on("0", "0")].concat());
    let port = port_in(&serve.printed("dagpane: app "));
    let probes = port_in(&serve.printed("dagpane: probes "));
    assert_eq!(get(port, &format!("127.0.0.1:{port}"), "/"), 200, "serve");
    let health = get(probes, &format!("127.0.0.1:{probes}"), "/healthz");
    assert_eq!(health, 200, "serve's probes");

    let host = Serving::start(&[["host", apps].as_slice(), &on("0", "0")].concat());
    let port = port_in(&host.printed("http://sales.localhost:"));
    let probes = port_in(&host.printed("dagpane: probes "));
    assert_eq!(
        get(port, &format!("sales.localhost:{port}"), "/"),
        200,
        "host"
    );
    let health = get(probes, &format!("127.0.0.1:{probes}"), "/healthz");
    assert_eq!(health, 200, "host's probes");
}

impl Serving {
    /// Everything it has written to stderr, once `needle` is in it, waiting up to 30 s.
    fn said(&self, needle: &str) -> String {
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            let said = self.stderr.lock().unwrap().clone();
            if said.contains(needle) {
                return said;
            }
            assert!(Instant::now() < until, "it never said {needle:?}:\n{said}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn host_answers_only_the_names_it_was_given() {
    // `dagpane host` routes `<app>.localhost` and `<app>.<--domain>`, and nothing else: a name
    // anyone else's DNS answers for is the name a rebinding page sends, as Host and as Origin.
    let apps = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let apps = apps.to_str().unwrap();
    let host = Serving::start(
        &[
            ["host", apps, "--domain", "example.com"].as_slice(),
            &on("0", "0"),
        ]
        .concat(),
    );
    let names = host.printed("dagpane: names ");
    assert!(
        names.contains("<app>.localhost, <app>.example.com"),
        "{names}"
    );
    let port = port_in(&host.printed("http://sales.localhost:"));

    for name in ["sales.localhost", "sales.example.com"] {
        assert_eq!(get(port, &format!("{name}:{port}"), "/"), 200, "{name}");
    }
    for name in ["sales.attacker.example", "sales", "127.0.0.1"] {
        assert_eq!(get(port, &format!("{name}:{port}"), "/"), 404, "{name}");
    }

    // Said once per domain, however many names under it are tried: a forgotten --domain is one
    // line to read, and a scanner choosing names cannot choose how much this writes. A name
    // under something that could never be declared (`com`) is said without the hint.
    assert_eq!(get(port, &format!("ops.attacker.example:{port}"), "/"), 404);
    assert_eq!(get(port, &format!("sales.com:{port}"), "/"), 404);
    // One label too deep under the domain that IS declared: said as that, never as a name under
    // no answered domain, and the hint to declare it is conditional on its being a zone.
    assert_eq!(get(port, &format!("x.sales.example.com:{port}"), "/"), 404);
    // Last, under a key of its own: the collector reads stderr in order, so once this line is
    // in, any line the requests before it caused is in too — including one that should not be.
    assert_eq!(get(port, &format!("x.sentinel.example:{port}"), "/"), 404);
    let said = host.said("refused Host \"x.sentinel.example\"");
    assert_eq!(
        said.matches("refused Host").count(),
        6,
        "attacker.example, the bare name, the address, com, sales.example.com and the sentinel, \
         once each:\n{said}"
    );
    assert!(
        said.contains("refused Host \"sales.attacker.example\""),
        "{said}"
    );
    assert!(
        !said.contains("refused Host \"ops.attacker.example\""),
        "{said}"
    );
    assert!(
        said.contains("If attacker.example is a domain you control"),
        "{said}"
    );
    let com = said
        .lines()
        .find(|line| line.contains("refused Host \"sales.com\""))
        .expect("sales.com was refused and said");
    assert!(!com.contains("pass --domain"), "{com}");
    let deep = said
        .lines()
        .find(|line| line.contains("refused Host \"x.sales.example.com\""))
        .expect("x.sales.example.com was refused and said");
    assert!(deep.contains("one label in front of example.com"), "{deep}");
    assert!(!deep.contains("no domain this host answers"), "{deep}");
    assert!(
        deep.contains("If sales.example.com is a zone of its own"),
        "{deep}"
    );
}

#[test]
fn a_domain_that_begins_with_an_app_is_warned_about_at_start_up() {
    let apps = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let apps = apps.to_str().unwrap();
    let host = Serving::start(
        &[
            ["host", apps, "--domain", "sales.example.com"].as_slice(),
            &on("0", "0"),
        ]
        .concat(),
    );
    let said = host.said("begins with sales");
    assert!(
        said.contains("--domain sales.example.com begins with sales"),
        "{said}"
    );
    assert!(said.contains("pass --domain example.com"), "{said}");
}

#[test]
fn a_domain_that_is_a_url_is_refused_at_start_up() {
    let apps = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let apps = apps.to_str().unwrap();
    let out = dagpane_exiting(
        &[
            ["host", apps, "--domain", "https://example.com/"].as_slice(),
            &on("0", "0"),
        ]
        .concat(),
        Duration::from_secs(30),
    );
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(said.contains("write `example.com`"), "{said}");
    assert!(!stdout(&out).contains("http://"), "{}", stdout(&out));
}

/// The status a WebSocket upgrade at `port` gets with this `Origin`, or with none: `101` when it
/// upgrades. Written by hand, as `get` is, so the headers are the test's.
fn upgrade(port: u16, host: &str, origin: Option<&str>) -> u16 {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))
        .unwrap_or_else(|e| panic!("nothing is listening on the port printed, {port}: {e}"));
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let origin = origin
        .map(|o| format!("Origin: {o}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "GET /ws HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{origin}\r\n"
    )
    .unwrap();
    let mut status = String::new();
    BufReader::new(stream).read_line(&mut status).unwrap();
    status
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("not an HTTP status line: {status:?}"))
}

#[test]
fn serve_takes_the_origins_it_is_given_in_place_of_the_bound_address() {
    // Through the binary rather than the library: `--origin` reaches the server, and it
    // REPLACES the allowlist derived from the bound address rather than adding to it — so
    // behind a proxy, the address the process happens to bind is not an origin a page may use
    // either. A wildcard bind left alone allows only `http(s)://0.0.0.0:<port>`, which no
    // browser sends; that is how the documented container served a page with dead controls.
    let app = example();
    let app = app.to_str().unwrap();
    let serve = Serving::start(
        &[
            // Repeatable, and each one written as an operator might: the second is the form
            // a browser never sends, which the server receives as the form it does.
            [
                "serve",
                app,
                "--origin",
                "https://dash.example",
                "--origin",
                "HTTPS://Second.Example:443",
            ]
            .as_slice(),
            &on("0", "0"),
        ]
        .concat(),
    );
    let port = port_in(&serve.printed("dagpane: app "));
    let host = format!("127.0.0.1:{port}");

    assert_eq!(upgrade(port, &host, Some("https://dash.example")), 101);
    assert_eq!(upgrade(port, &host, Some("https://second.example")), 101);
    assert_eq!(upgrade(port, &host, Some("https://evil.example")), 403);
    // Exact, not a prefix: a name that merely begins with a declared one is somebody else's.
    assert_eq!(
        upgrade(port, &host, Some("https://dash.example.evil.example")),
        403
    );
    assert_eq!(
        upgrade(port, &host, Some(&format!("http://127.0.0.1:{port}"))),
        403,
        "the bound address is still allowed beside --origin"
    );
    // No Origin at all is a non-browser client, which the check is not about.
    assert_eq!(upgrade(port, &host, None), 101);
}
