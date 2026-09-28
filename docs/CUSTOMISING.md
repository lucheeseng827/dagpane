# Customising a dashboard

What a viewer can change, what an author can change, and what nothing yet can. Everything
here was read out of the manifest compiler, the CLI and the embedded client in this
repository, and every command shown was run against the binary built from it.

**The short version.** A dashboard is one TOML file. A viewer moves the controls its author
declared and can send the result to somebody as a link; an author changes the file and
restarts. Layout, colours and number formatting beyond a metric's own options are not
configurable in this version.

---

## The viewer

A viewer can move any control the author declared — sliders, dropdowns, number boxes,
checkboxes and text boxes — and read the result.

**Their filters are in the address bar.** The client keeps the viewer's own input values in
the URL fragment and rewrites it as they go, so a filtered dashboard is a link they can send
to somebody. Two consequences worth knowing:

* It uses `replaceState`, so dragging a slider does not fill the back button with every value
  it passed through.
* It is the fragment, never a query string, so the values are not sent to the server on the
  request line and do not land in a proxy log.

Reloading the page restores whatever is in the URL, and a bare URL restores the manifest's
defaults.

What a viewer still **cannot** do:

* rearrange, resize or hide panes;
* add a chart, or change a chart's type;
* choose a colour scheme — the page follows the operating system's light or dark setting and
  offers no toggle;
* save a named view. The link is the only thing they can keep.

---

## The author

The author edits the manifest and restarts. `dagpane run` reads sources once at start-up and
shares them immutably across every session, which is what makes a hundred viewers a hundred
vectors of values over one allocation per source — the *source*, measured; the frames a cell
computes are per viewer and are not free. There is no hot reload.

```console
$ dagpane check apps/13-weekly-review.toml     # compile it; errors name the cell and the step
$ dagpane graph apps/13-weekly-review.toml     # the shape, before anything runs
$ dagpane explain apps/13-weekly-review.toml --set region=north   # what one change costs
$ dagpane run   apps/13-weekly-review.toml     # serve it
$ dagpane export apps/13-weekly-review.toml --out dist   # …or ship it with no server at all
```

Unknown keys are rejected rather than ignored, so a misspelt option is an error at `check`
time and never a setting that silently does nothing.

`dagpane host <dir>` serves every manifest in a directory from one process, routed by the
`Host` header, and recompiles an app when its file changes — so in that mode editing a
manifest is the deploy.

`dagpane export` writes the same page with the engine compiled into it and the rows inlined:
a folder for any static host, with the same controls and the same counts. It has to be
**served over `http://`** — a browser refuses to load modules and WebAssembly from a
`file://` origin, and the page tells you so if you try. `python3 -m http.server` in the folder
is enough. What it does not have is what a server was doing — no sign-in, no scheduled
refresh, no HTTP or SQL sources — and it is as big as its data, which the command prints.

---

## 1. The heading

```toml
[app]
title = "Weekly business review"
subtitle = "26 weeks, four regions, three segments"   # optional
```

## 2. The controls

Five kinds. Each sets a cell of the same name; `label` is what the viewer reads.

| kind | options | notes |
|---|---|---|
| `slider` | `min`, `max`, `step`, `default` | all four required |
| `number` | `default`, optional `min`, `max` | a typed box rather than a track |
| `select` | `options`, `default` | `default` must be one of `options` |
| `checkbox` | `default` | sets a boolean cell |
| `text` | `default`, `placeholder` | free text; pairs with the `contains` filter |

```toml
[[input]]
name = "region"
label = "Region"
select = { options = ["all", "north", "south", "east", "west"], default = "all" }

[[input]]
name = "min_amount"
label = "Minimum order value"
slider = { min = 0.0, max = 800.0, step = 25.0, default = 0.0 }
```

## 3. Where the data comes from

```toml
[[source]]
name = "sales"
csv = "sales.csv"              # short form of file = { path = "sales.csv", format = "csv" }
```

Paths are relative to the manifest's own directory, never to the directory you started the
process from, so an app behaves the same wherever it is launched. Four forms:

| form | needs |
|---|---|
| `csv = "…"` or `file = { path, format }` | nothing; `format` defaults to the extension |
| `http = { url = "…" }` | a binary built with the `http` feature |
| `sql = { dsn, query, watch }` | a binary built with the `sql` feature |
| `refresh_secs = N` on any source | something that owns a clock; `dagpane refresh` re-reads regardless |

The published container image is built without those two features, and says so rather than
failing at run time:

```console
$ dagpane check http.toml
dagpane: http.toml: source `s`: this build cannot read https://example.com/a.csv:
         it was compiled without the `http` feature (`cargo build --features http`)
```

## 4. What the data does

A cell starts `from` a source or another cell and applies steps **in the order written** —
`limit` before `sort` and `sort` before `limit` mean different things, as they should.

| step | what it does |
|---|---|
| `filter` | keep rows where `column op (value \| param)` holds |
| `derive` | add a column computed per row: `{ name, expr }` |
| `join` | join another cell's table: `{ with, on \| left_on + right_on, how, suffix }` |
| `select` | keep these columns, in this order |
| `sort` | `{ column, descending }` |
| `limit` | keep the first n rows |
| `group_by` | `{ by = [...], agg = [...] }` — `count`, `sum`, `mean`, `min`, `max` |
| `scalar` | pull one value out: `{ column, row }`. Ends the pipeline |
| `count` | the row count as a number. Also ends the pipeline |

Filter operators are `eq`, `ne`, `lt`, `le`, `gt`, `ge` and `contains` (substring, text
columns only, case-sensitive). A null is equal to nothing, itself included, so no comparison
keeps a null row.

**Three of these write edges, and only these three** ([ADR-0001](adr/0001-declared-edges.md)). A filter's `param`, an expression's
`$control` reference and a join's `with` are where a dependency is declared, which is why a
misspelling in them is a compile error rather than a silently empty table.

```toml
[[cell]]
name = "filtered"
from = "sales"

[[cell.step]]
filter = { column = "amount", op = "ge", param = "min_amount" }

# `skip_when` is how a dropdown gets an "everything" option without a conditional: when the
# control holds this value the step does nothing, so the cell produces the value it already
# had — and the panes below it are not repainted.
[[cell.step]]
filter = { column = "region", op = "eq", param = "region", skip_when = "all" }
```

Derived columns are ordinary arithmetic over the row, and they stack:

```toml
[[cell.step]]
derive = { name = "margin", expr = "monthly_revenue - support_cost - infra_cost" }
[[cell.step]]
derive = { name = "margin_pct", expr = "margin / monthly_revenue * 100" }
```

An expression reads a control by writing `$name`, and that `$` is the edge:

```toml
derive = { name = "verdict", expr = "if(margin_pct >= $target_margin, 'clears', 'below')" }
```

A `derive` with no `$` in it depends on no control, so no control can make it recompute.
Expressions are not SQL and do not become SQL: [ADR-0005](adr/0005-sql-lowers-to-the-verbs.md)
has the argument for lowering a query to these verbs rather than inferring edges from text.

A join names the cell it joins with, and `how` is required with no default — `inner` is
SQL's default and choosing it silently is how a page loses rows nobody asked it to lose.
`inner`, `left`, `semi` and `anti` are the four.

## 5. What the page shows

Panes appear **in the order they are written**. Every pane names a `cell` and may take an
`id` and a `title`; two panes may show the same cell.

| pane | options |
|---|---|
| `metric` | `label`, `decimals`, `prefix`, `suffix` |
| `table` | `max_rows` (50 by default — a wire cap; a truncated table says so) |
| `bar` | `label_column`, `value_column` |
| `line` | `x_column`, `y_column` — the `sort` step decides the line's direction |
| `text` | none |
| `custom` | `renderer`, `columns`, `max_rows` (500), `options` — see below |

```toml
[[pane]]
cell = "revenue"
metric = { label = "Revenue in view", prefix = "$", decimals = 0 }

[[pane]]
cell = "per_week"
title = "Revenue by week"
line = { x_column = "week", y_column = "revenue" }
```

### 5b. A drawing the runtime does not have

Those five are the ones the runtime knows about. Anything else — a heatmap, a treemap, a
sankey, a map — is a `custom` pane and a JavaScript file, and **no Rust and no rebuilt binary**.

```toml
[app]
title = "Fleet"
renderers = ["renderers/heatmap.js"]      # relative to this manifest

[[pane]]
cell = "latency_by_hour"
title = "p99 by hour"
custom = { renderer = "heatmap", columns = ["day", "hour", "p99"],
           options = { low = "#f7f4ea", high = "#2f6f4f" } }
```

```js
// renderers/heatmap.js
window.dagpane.renderer("heatmap", (view, options, el) => {
  // view.data === "table"  -> view.head [{name, type}], view.rows [[value, …]], view.total_rows
  // view.data === "scalar" -> view.value {kind, v}
  // options                -> this pane's `options` table, verbatim
  // el                     -> an empty <div> to fill
});
```

`examples/renderers/heatmap.js` is a complete working one, written to be read.

Four things to know, because they are the difference between an extension point and a hole:

* **`columns` is a wire economy, not a compute one.** It decides how much of the answer
  crosses the socket — worth setting on anything wide — but the cell still computes every
  column it was written to compute. To narrow the *computation*, put a `select` in the cell;
  that is what sub-node invalidation reads.
* **`options` is opaque.** Write whatever TOML your renderer wants; it arrives as JSON with
  nothing in between giving it a meaning. It rides the pane, which the client is sent once,
  so it never appears in a patch.
* **A column you name that is gone is an error, not a missing series.** The pane says so. A
  chart quietly drawing three of its four series is the failure this whole project is
  arranged not to have.
* **The engine still decides what goes on the wire.** A renderer is handed the answer and an
  element; it is never on the path that produces one. So a custom pane whose view did not
  change is still absent from the patch, and a renderer that throws takes out its own pane
  and nothing else.

Scripts are read when the app is compiled, so `dagpane check` fails on one that is missing
rather than a page loading with a blank card — and a restart picks up an edit, the same way a
manifest edit needs one. A renderer path must be relative, must not climb out of the app's
directory, and must not be a URL: a renderer is served with the app, never fetched from
somewhere else.

---

## A worked change

Add a pane to the weekly review showing the worst week in the current slice.

```toml
# 1. a cell: the smallest weekly revenue in view
[[cell]]
name = "worst_week"
from = "per_week"
[[cell.step]]
sort = { column = "revenue", descending = false }
[[cell.step]]
scalar = { column = "revenue", row = 0 }

# 2. a pane for it, placed where you want it read
[[pane]]
cell = "worst_week"
metric = { label = "Worst week in view", prefix = "$", decimals = 0 }
```

```console
$ dagpane check apps/13-weekly-review.toml
dagpane: Weekly business review — 15 cells (3 inputs, 11 computed), 10 panes
  1 data source(s) loaded at start-up and shared by every session
  ok

$ dagpane explain apps/13-weekly-review.toml --set region=north
  epoch 2 — looked at 10 of 15 cells
    ran      worst_week           changed
  5 cell(s) never looked at: weekly, segment, from_week, company_revenue, company_new_accounts
  patch: 8 of 10 panes — ... , worst_week
```

`worst_week` reads `per_week`, which is downstream of the region control, so it recomputes
and its pane is sent. Had it read the source directly it would never have been looked at —
which is how the two company totals on that page stay still while everything around them
moves. **Where you attach a cell is the whole decision**, and `explain` is how you check it
before anyone else sees the page.

---

## What is not customisable in this version

Each of these is a question somebody asks on the first day.

**Layout.** The controls column is a fixed 260px and panes flow into an automatic grid. A
table or line pane always spans the full width; a metric or bar pane takes one column. Pane
width, height, position and grouping are not settable.

**Colours, fonts and branding.** The whole front end is one HTML file compiled into the
binary. There is no stylesheet to override, no CSS injection and no theme setting. Changing
the look means editing `crates/serve/src/client.html` and rebuilding — with one exception:
a `custom` pane's renderer owns its own appearance and can inject a stylesheet for itself,
which is what `examples/renderers/heatmap.js` does.

**Number formatting beyond a metric's own options.** A `metric` takes `decimals`, `prefix`
and `suffix`; nothing else does. Tables and bar charts print non-integers to two decimal
places, and there are no thousands separators anywhere — a large metric renders as
`$15418974`, not `$15,418,974`.

**Charts, out of the box.** The five built-in panes take no series colours, axis titles,
stacked or multi-series options, and there is no pie chart. What there *is* instead of a
growing list of options is §5b: a `custom` pane hands the drawing to a script you write, so a
stacked bar with your own palette is a file rather than a feature request. That is a real
answer and not a deflection — but it is JavaScript, and if what you wanted was one more TOML
key then this version does not have it.

**Per-viewer state beyond the link.** No saved views and no per-user defaults. Sign-in, where
an operator has configured it, decides *whether* someone may open an app — not what they see
inside it. [SECURITY.md](../SECURITY.md) has what is and is not protected.

**Where a cell runs — written, checked, not yet served.** A `[[cell]]` or `[[input]]` takes
`place = "client"`, and `dagpane check` reports the cut it makes:

```
  placement: 4 cell(s) in the page, 2 on the server
  frontier: 1 cell(s) cross the wire — sales
  1 of 1 control(s) would be answered without the network — min_amount
```

The rule is that placement is **monotone**: a cell on the server may not read a cell in the
page, and a manifest that breaks it is refused by edge name. `examples/apps/20-placed.toml`
is the worked example.

What the conditional mood is hiding: **`dagpane run` and `dagpane export` do not serve a split
yet.** They compile the cut, check it, report it, and then evaluate every cell on their own
side — so today `place` is a manifest you can validate rather than a deployment you can make.
`ROADMAP.md` §4 lists what is left.

---

## Generating the manifest instead of writing it

Both of these produce the same TOML, so everything above still applies.

* **From a notebook.** `examples/notebook/` shapes data in Python with pandas, polars or
  plain rows, declares a `Panel`, and writes one CSV per source plus an `app.toml`. It proves
  an interaction is cheap with `.explain()` before anyone opens a browser, and after that the
  kernel is not in the loop.
* **From object storage.** `examples/bucket/` follows a bucket: sync, compile, and swap the
  snapshot only if it changed *and* it compiles. A half-finished upload never reaches a
  viewer, and an unchanged bucket causes no restart.

Nineteen worked apps live in [`examples/`](../examples/README.md), grouped by who asks the
question, and `examples/tools/verify.py` runs one real interaction on every one of them.
