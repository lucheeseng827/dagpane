# dagpane-app — the app model

`dagpane-core` knows about cells and values. This crate knows what a slider is and what goes
on the wire — and it knows nothing about sockets, runtimes or async. `cargo test -p
dagpane-app` runs the whole patch protocol with no server in the process, which is the
property that makes the protocol testable at all.

## The invariant this crate carries alone

**A pane is not a cell.** Two panes can show one cell, a cell can drive no pane, and a cell
whose *value* changed can leave its pane's *view* identical — a table pane sending its first
fifty rows does not change when row nine thousand does.

So a patch is computed from rendered views, not from the trace: the trace says which cells to
re-render, and the rendered view's own digest decides whether it goes on the wire. Skipping
that second step would put the engine's honesty at the mercy of the renderer.

## Architecture

```mermaid
flowchart TD
    toml["app.toml<br/>sources · inputs · cells · panes"]
    manifest["manifest — the compiler<br/>every edge is one somebody typed"]
    app["App — immutable, shared<br/>Arc&lt;Graph&gt; + widgets + panes"]
    appsession["AppSession — one viewer<br/>engine Session + what it has been shown"]
    view["view — one cell's outcome<br/>becomes one View"]
    wire["wire — ClientMessage / ServerMessage<br/>+ the PassStats that carry the claim"]
    csv["csv — reader with<br/>per-column type inference"]

    toml --> manifest
    csv --> manifest
    manifest -->|"compile(): unknown cell,<br/>bad step or cycle rejected here"| app
    app --> appsession
    appsession --> view
    view -->|"only if the rendered view's<br/>digest changed"| wire
```

## What each module owns

| module | what it owns |
|---|---|
| `manifest` | TOML in, a checked `App` out |
| `view` | what a pane shows, and how one cell's outcome becomes it |
| `widget` | the controls, and `accepts` — a browser is not trusted, so a bad value is refused at the edge rather than inside somebody's compute |
| `wire` | the two messages that cross the socket, and the `stats` block |
| `csv` | a reader with per-column type inference, because the inference is the work |
| `session` | one viewer: the engine session plus what that viewer has been shown |

## Event flow

```mermaid
sequenceDiagram
    participant W as Client
    participant A as AppSession
    participant G as Widget
    participant S as core::Session
    participant R as view::render

    W->>A: Set { seq, values }
    A->>G: accepts(value) for every value first
    alt any value is one this control could not produce
        A-->>W: Rejected — nothing was staged, the page is still correct
    else all acceptable
        A->>S: set(...) then commit()
        S-->>A: Trace + the cells whose value changed
        loop each pane showing a changed cell
            A->>R: render(pane, outcome)
            R-->>A: a View
            A->>A: digest it against what this viewer already has
        end
        A-->>W: Patch { panes that actually moved, stats }
    end
```

Validating every value *before* staging any of them is deliberate: a half-applied batch would
leave the viewer's controls and the session disagreeing, with no way to tell which is right.

## Quickstart

```rust
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use dagpane_app::{load, AppSession};
use dagpane_core::Value;

let app = Arc::new(load(Path::new("examples/sales.toml"))?);
let (mut session, first) = AppSession::open(app);
session.full_views();                       // what a client gets on connect
assert_eq!(first.evaluated(), 8);           // the first render computes every cell

let mut values = BTreeMap::new();
values.insert("min_amount".to_string(), Value::float(400.0));
session.set(&values)?;
let (trace, patch) = session.commit();

assert_eq!(trace.evaluated(), 6);           // six of eleven cells ran
assert_eq!(patch.len(), 3);                 // three of seven panes went on the wire
```

```sh
cargo test -p dagpane-app
```

## Deliberately absent

No SQL and no expression language. The manifest's job is to produce **edges**, and a
dependency inferred wrongly from SQL text by a regular expression is a wrong app — a cell
that recomputes when it should not is a cost, but a cell that does not recompute when it
should is a stale number on a page that looks correct.
