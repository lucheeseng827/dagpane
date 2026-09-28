//! Durable sessions, over a real socket.
//!
//! The exit criterion: **a reconnect after a restart restores state in one pass.** Both
//! halves matter. "Restores state" is the feature; "in one pass" is the difference between
//! seeding a session and rendering it at the defaults and then correcting it — which is two
//! passes, a visible flicker, and twice the work on every reconnect in a fleet.
//!
//! The session lives with the viewer, not in the server, so "a restart" here is literal: the
//! server is dropped and a new one bound, sharing nothing. See `dagpane_app::resume` for why
//! that is the design rather than a store.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const MANIFEST: &str = r#"
[app]
title = "Resume"

[[source]]
name = "sales"
csv = "sales.csv"

[[input]]
name = "min_amount"
label = "Minimum"
number = { min = 0.0, max = 1000.0, default = 0.0 }

[[input]]
name = "region"
label = "Region"
select = { options = ["all", "north", "south"], default = "all" }

[[cell]]
name = "filtered"
from = "sales"
[[cell.step]]
filter = { column = "amount", op = "ge", param = "min_amount" }
[[cell.step]]
filter = { column = "region", op = "eq", param = "region", skip_when = "all" }

[[cell]]
name = "total"
from = "filtered"
[[cell.step]]
count = true

[[pane]]
cell = "total"
metric = { label = "Rows" }
"#;

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let mut csv = String::from("region,amount\n");
    for i in 0..100 {
        let region = ["north", "south"][i % 2];
        csv.push_str(&format!("{region},{}.0\n", (i % 20) * 50));
    }
    std::fs::write(dir.path().join("sales.csv"), csv).unwrap();
    let manifest = dir.path().join("app.toml");
    std::fs::write(&manifest, MANIFEST).unwrap();
    (dir, manifest)
}

/// Bind a fresh server on a fresh port. Called twice per test: **this is the restart.**
async fn start(manifest: &std::path::Path) -> SocketAddr {
    let app = Arc::new(dagpane_app::load(manifest).expect("the app compiles"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = dagpane_serve::serve_on(app, listener).await;
    });
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return addr;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the server never began accepting on {addr}");
}

async fn open(
    addr: SocketAddr,
    saved: Option<&str>,
) -> tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> {
    let query = saved
        .map(|s| format!("?s={}", urlencode(s)))
        .unwrap_or_default();
    let request = format!("ws://{addr}/ws{query}")
        .into_client_request()
        .unwrap();
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    tokio_tungstenite::client_async(request, stream)
        .await
        .unwrap()
        .0
}

/// Enough of one to carry JSON through a query string.
fn urlencode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
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

/// The one metric pane's rendered value.
fn rows(frame: &serde_json::Value) -> String {
    for field in ["views", "panes"] {
        if let Some(updates) = frame[field].as_array() {
            if let Some(v) = updates
                .iter()
                .find_map(|u| u["view"]["value"].as_str().map(str::to_string))
            {
                return v;
            }
        }
    }
    panic!("no metric in {frame}");
}

// ── the exit criterion ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_reconnect_after_a_restart_restores_state_in_one_pass() {
    let (_dir, manifest) = fixture();

    // A viewer, setting two controls.
    let first = start(&manifest).await;
    let mut socket = open(first, None).await;
    let init = next_json(&mut socket).await;
    let at_defaults = rows(&init);
    assert_eq!(at_defaults, "100", "every row before any filter");

    socket
        .send(Message::text(
            serde_json::json!({"type": "set", "seq": 1, "values": {
                "min_amount": {"kind": "float", "v": 500.0},
                "region": {"kind": "text", "v": "north"}
            }})
            .to_string(),
        ))
        .await
        .unwrap();
    let patch = next_json(&mut socket).await;
    let filtered = rows(&patch);
    assert_ne!(filtered, at_defaults, "the interaction did nothing");

    // The restart. The old server is dropped and a NEW one bound, sharing no state — which
    // is what makes this a test of durability rather than of a cache.
    drop(socket);
    let saved = r#"{"min_amount":{"kind":"float","v":500.0},"region":{"kind":"text","v":"north"}}"#;
    let second = start(&manifest).await;
    assert_ne!(first, second, "the same server answered twice");

    let mut socket = open(second, Some(saved)).await;
    let resumed = next_json(&mut socket).await;

    assert_eq!(resumed["type"], "init");
    assert_eq!(
        rows(&resumed),
        filtered,
        "the viewer came back to a different number than they left"
    );
    assert!(
        resumed["dropped"].is_null(),
        "nothing should have been dropped: {}",
        resumed["dropped"]
    );

    // **One pass.** `epoch` is the pass counter, and a session that rendered at the defaults
    // and then applied the saved values would be at 2. Every cell was evaluated exactly once,
    // from the state the viewer left.
    assert_eq!(
        resumed["stats"]["epoch"].as_u64(),
        Some(1),
        "restoring took more than one pass: {}",
        resumed["stats"]
    );
    // And it **computed** no more than a cold open: the same two cells ran, once each. That
    // is the claim — restoring is not re-rendering — and it is stated against the first
    // connection's own opening frame rather than against `total_cells`, because a first
    // render visits the cells that need computing and not every cell in the graph.
    assert_eq!(
        resumed["stats"]["evaluated"], init["stats"]["evaluated"],
        "restoring a session evaluated more cells than opening a fresh one"
    );

    // `visited` differs by exactly the two inputs that were restored, and that is not slack:
    // a trace counts an input the pass SET, so `visited + untouched` adds up to the whole
    // app. Two restored inputs, two more visited cells, no more work.
    let cold = init["stats"]["visited"].as_u64().unwrap();
    assert_eq!(
        resumed["stats"]["visited"].as_u64(),
        Some(cold + 2),
        "a restored input should appear in the trace exactly once: {} vs {}",
        resumed["stats"],
        init["stats"]
    );
}

#[tokio::test]
async fn the_opening_frame_of_a_plain_connection_is_unchanged() {
    // The resume field is omitted when empty, so a client written before any of this existed
    // reads the same bytes. Asserted rather than assumed: `skip_serializing_if` is one
    // attribute away from being a wire break for every existing viewer.
    let (_dir, manifest) = fixture();
    let addr = start(&manifest).await;
    let mut socket = open(addr, None).await;
    let init = next_json(&mut socket).await;

    assert!(init.get("dropped").is_none(), "{init}");
    for key in ["type", "title", "widgets", "panes", "views", "stats"] {
        assert!(init.get(key).is_some(), "the opening frame lost {key}");
    }
}

// ── what a saved state is not allowed to do ───────────────────────────────────────────────

#[tokio::test]
async fn a_saved_state_cannot_reach_a_cell_that_is_not_an_input() {
    // A resume goes through the same predicates as a `Set`, which is the whole reason it is
    // not a privileged path. `total` is computed; `nonesuch` does not exist.
    let (_dir, manifest) = fixture();
    let addr = start(&manifest).await;
    let saved = r#"{"total":{"kind":"int","v":7},"nonesuch":{"kind":"int","v":7}}"#;
    let mut socket = open(addr, Some(saved)).await;
    let init = next_json(&mut socket).await;

    let dropped = init["dropped"].as_array().expect("both should be dropped");
    assert_eq!(dropped.len(), 2, "{dropped:?}");
    let names: Vec<&str> = dropped.iter().filter_map(|d| d["input"].as_str()).collect();
    assert!(names.contains(&"total"), "{names:?}");
    assert!(names.contains(&"nonesuch"), "{names:?}");
    assert_eq!(
        rows(&init),
        "100",
        "a refused resume still started at the defaults"
    );
}

#[tokio::test]
async fn a_saved_value_outside_a_control_s_bounds_is_dropped_and_the_rest_survives() {
    // The case a redeploy creates: a link saved when the slider went to 1000, reopened after
    // the manifest narrowed it. The good half of the state must still apply — otherwise every
    // bookmark in the company breaks on one manifest edit.
    let (_dir, manifest) = fixture();
    let addr = start(&manifest).await;
    let saved =
        r#"{"min_amount":{"kind":"float","v":9999.0},"region":{"kind":"text","v":"south"}}"#;
    let mut socket = open(addr, Some(saved)).await;
    let init = next_json(&mut socket).await;

    let dropped = init["dropped"].as_array().unwrap();
    assert_eq!(dropped.len(), 1, "{dropped:?}");
    assert_eq!(dropped[0]["input"], "min_amount");
    assert!(
        dropped[0]["reason"].as_str().unwrap().contains("bounds"),
        "the reason should say what is wrong: {}",
        dropped[0]["reason"]
    );

    // `region = south` did apply: half the rows, unfiltered by amount.
    assert_eq!(rows(&init), "50");
    assert_eq!(init["stats"]["epoch"].as_u64(), Some(1), "still one pass");
}

#[tokio::test]
async fn an_unreadable_state_starts_fresh_and_says_so() {
    let (_dir, manifest) = fixture();
    let addr = start(&manifest).await;

    for saved in ["not json", "[1,2,3]", "{\"min_amount\":"] {
        let mut socket = open(addr, Some(saved)).await;
        let init = next_json(&mut socket).await;
        let dropped = init["dropped"].as_array().unwrap_or_else(|| {
            panic!("{saved:?} should have been reported, got {init}");
        });
        assert_eq!(dropped.len(), 1, "{saved:?}: {dropped:?}");
        assert_eq!(
            dropped[0]["input"], "",
            "a whole-state failure names no input"
        );
        assert_eq!(rows(&init), "100");
    }
}

#[tokio::test]
async fn a_state_past_the_size_limit_is_refused_whole() {
    let (_dir, manifest) = fixture();
    let addr = start(&manifest).await;

    // Valid JSON, valid inputs, far too much of it. Truncating would apply half a remembered
    // state with no way for the viewer to see which half.
    let mut saved = String::from(r#"{"min_amount":{"kind":"float","v":500.0}"#);
    while saved.len() <= dagpane_app::resume::MAX_ENCODED_BYTES {
        saved.push_str(&format!(r#","pad{}":{{"kind":"int","v":1}}"#, saved.len()));
    }
    saved.push('}');

    let mut socket = open(addr, Some(&saved)).await;
    let init = next_json(&mut socket).await;
    let dropped = init["dropped"].as_array().unwrap();
    assert_eq!(dropped.len(), 1);
    assert!(
        dropped[0]["reason"].as_str().unwrap().contains("limit"),
        "{}",
        dropped[0]["reason"]
    );
    assert_eq!(rows(&init), "100", "no part of an oversized state applied");
}

#[tokio::test]
async fn two_viewers_resume_independently_over_one_app() {
    // The property a server-side store has to work for and this design gets for free: two
    // viewers of one app, each with their own saved state, sharing one `Arc<App>`.
    let (_dir, manifest) = fixture();
    let addr = start(&manifest).await;

    let mut north = open(addr, Some(r#"{"region":{"kind":"text","v":"north"}}"#)).await;
    let mut south = open(
        addr,
        Some(r#"{"region":{"kind":"text","v":"south"},"min_amount":{"kind":"float","v":500.0}}"#),
    )
    .await;

    let a = next_json(&mut north).await;
    let b = next_json(&mut south).await;
    assert_eq!(rows(&a), "50");
    assert_ne!(
        rows(&b),
        rows(&a),
        "one viewer's saved state reached the other"
    );
    assert_eq!(a["stats"]["epoch"].as_u64(), Some(1));
    assert_eq!(b["stats"]["epoch"].as_u64(), Some(1));
}
