//! The declarative app: TOML in, a checked [`crate::App`] out.
//!
//! This is the authoring path that does not need a compiler, and for v0.1.0 it is the
//! *only* complete one — the Rust API underneath it is public and usable, but a data
//! scientist is not the person it is for. A manifest wires inputs to transforms to panes;
//! the reactive graph falls out of the wiring, because a step that reads an input names it
//! and that name is an edge.
//!
//! # What it is not
//!
//! There is no expression language and no SQL. A `filter` compares one column against one
//! literal or one input, and that is the whole vocabulary. The reason is narrow and worth
//! stating: the manifest's job is to produce **edges**, and an edge inferred wrongly is a
//! wrong app — a cell that recomputes when it should not is a cost, but a cell that does not
//! recompute when it should is a stale number on a page that looks correct. Extracting
//! dependencies from SQL text without a SQL parser is a regular expression that will
//! eventually be wrong, so this version does not try. `input =` and `param =` are written
//! out, and every edge in a compiled app is one somebody typed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::reads::Provenance;
use dagpane_connect::file::FileFormat;
use dagpane_connect::{FileSource, Source};
use dagpane_core::expr::{Expr, Scope, Ty};
use dagpane_core::frame::Frame;
use dagpane_core::graph::Inputs;
use dagpane_core::transform::{self, Agg, AggSpec, Comparison, Filter, GroupBy, How, Join};
use dagpane_core::{CellError, ColumnType, Compute, Cut, Graph, Placement, Value};
use serde::{Deserialize, Serialize};

use crate::view::{Pane, PaneKind};
use crate::widget::{Widget, WidgetKind};
use crate::{App, BoundSource};

// ── the file ───────────────────────────────────────────────────────────────────────────

/// A parsed manifest file, before any of it has been checked.
///
/// `deny_unknown_fields` throughout: a misspelt key in an authoring format is a silently
/// ignored instruction, and the app that results looks like a bug in dagpane rather than a
/// typo in the file.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The `[app]` table. The only required section.
    pub app: AppMeta,
    /// `[[source]]` entries: data loaded once and shared by every session.
    #[serde(default)]
    pub source: Vec<SourceSpec>,
    /// `[[input]]` entries: the controls, and the source cells they set.
    #[serde(default)]
    pub input: Vec<InputSpec>,
    /// `[[cell]]` entries: the computed cells. A `from` may name a cell declared later in
    /// the file — the graph resolves every name at once — but a filter's `param` may not, so
    /// in practice a manifest reads top to bottom.
    #[serde(default)]
    pub cell: Vec<CellSpec>,
    /// `[[pane]]` entries: what the page shows, in display order.
    #[serde(default)]
    pub pane: Vec<PaneSpec>,
}

/// The `[app]` table.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppMeta {
    /// The app's title.
    pub title: String,
    /// An optional line under it.
    #[serde(default)]
    pub subtitle: Option<String>,
    /// `renderers = ["charts.js"]` — scripts the client loads before its first paint, each
    /// of which registers one or more drawings a `custom` pane can name.
    ///
    /// Paths relative to the manifest, resolved the same way a `[[source]]`'s `csv` is, and
    /// **served by whoever is serving the app**: this crate reads no files here, it only
    /// carries the list. That separation is why the same manifest works in a browser with no
    /// server at all, where the scripts sit beside the page.
    ///
    /// The security posture is the app author's own directory, exactly as it is for a CSV
    /// source — a manifest that can read your data can already read your data. What it is
    /// *not* is a plugin registry: nothing is fetched from a network, and a path that climbs
    /// out of the app's directory is refused.
    #[serde(default)]
    pub renderers: Vec<PathBuf>,
}

/// Data loaded once at start-up and shared by every session.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSpec {
    /// The cell name this data is bound to. What a `[[cell]]`'s `from` refers to.
    pub name: String,
    /// `csv = "sales.csv"` — the short form, and the only one there used to be.
    ///
    /// Kept because it reads well and because every example in this repository uses it; it
    /// is exactly `file = { path = "sales.csv", format = "csv" }`. Relative to the
    /// manifest's own directory, never to the process's working directory — an app must
    /// behave the same whichever directory it is started from.
    #[serde(default)]
    pub csv: Option<PathBuf>,
    /// `file = { path = "…", format = "csv" }`. `format` defaults to the extension's.
    #[serde(default)]
    pub file: Option<FileSpec>,
    /// `http = { url = "https://…" }`. Needs the `http` feature at build time; a manifest
    /// using it against a binary built without one is an error that says so.
    #[serde(default)]
    pub http: Option<HttpSpec>,
    /// `sql = { dsn = "…", query = "select …", watch = "updated_at" }`. Needs the `sql`
    /// feature, same as above.
    #[serde(default)]
    pub sql: Option<SqlSpec>,
    /// How often a scheduled refresh should re-read this source, in seconds.
    ///
    /// Carried here and acted on by whatever owns a clock — nothing in this crate does.
    /// `dagpane refresh` ignores it and re-reads everything, which is what a person running
    /// a command by hand means.
    #[serde(default)]
    pub refresh_secs: Option<u64>,
}

/// `file = { path = "…", format = "csv" }`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileSpec {
    /// Relative to the manifest's own directory.
    pub path: PathBuf,
    /// Which reader. Omitted means the path's extension decides, and an extension this build
    /// does not read is an error rather than a guess.
    #[serde(default)]
    pub format: Option<String>,
}

/// `http = { url = "https://…" }`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpSpec {
    /// The URL. `http` or `https`; a local path is a `file` source.
    pub url: String,
    /// Seconds to wait for the whole exchange. Omitted means the source's own default.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// `sql = { dsn = "…", query = "select …" }`.
///
/// **The DSN is in the manifest, and that is a deliberate limitation rather than a
/// recommendation.** A manifest is a file people commit. Until there is somewhere else to
/// put a credential, the honest advice is an environment reference or a role with `select`
/// and nothing else — and the field is documented that way rather than pretending a
/// read-only query is a boundary.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SqlSpec {
    /// A Postgres connection string. Connect as a role that can only read.
    pub dsn: String,
    /// One `select`. No pushdown and no cursor: a source produces a whole value.
    pub query: String,
    /// A column whose maximum a staleness check should watch, alongside the row count.
    /// Without one, an `UPDATE` that changes no row count looks like no change at all.
    #[serde(default)]
    pub watch: Option<String>,
}

/// A control, and the source cell it sets.
///
/// One optional field per control kind rather than a tagged enum: TOML's inline tables make
/// `slider = { min = 0, max = 500, step = 10, default = 100 }` the obvious thing to write,
/// and serde's internally-tagged enums do not survive a round trip through `toml` reliably
/// enough to build an authoring surface on.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputSpec {
    /// The source cell this control sets, and the name a filter's `param` refers to.
    pub name: String,
    /// `place = "client"` — this control and its value live in the page.
    ///
    /// The interesting one. A control placed here whose whole downstream closure is also in
    /// the page is answered **without touching the network at all** — that is the thing
    /// per-cell placement is for, and `dagpane check` prints which controls have it.
    ///
    /// A client-placed control may not feed a server-side cell; see `CellSpec::place`.
    #[serde(default)]
    pub place: Option<Placement>,
    /// What the client shows beside the control. Defaults to `name`.
    #[serde(default)]
    pub label: Option<String>,
    /// `slider = { min = …, max = …, default = … }`.
    #[serde(default)]
    pub slider: Option<SliderSpec>,
    /// `number = { default = … }`, optionally bounded.
    #[serde(default)]
    pub number: Option<NumberSpec>,
    /// `select = { options = [...], default = "…" }`.
    #[serde(default)]
    pub select: Option<SelectSpec>,
    /// `checkbox = { default = true }`.
    #[serde(default)]
    pub checkbox: Option<CheckboxSpec>,
    /// `text = { default = "…" }`.
    #[serde(default)]
    pub text: Option<TextSpec>,
}

/// A bounded numeric control. Sets a float cell.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SliderSpec {
    /// Lowest value the server will accept.
    pub min: f64,
    /// Highest value the server will accept.
    pub max: f64,
    /// The client's increment. Defaults to 1, and is not enforced on the server — see
    /// [`WidgetKind::Slider`].
    #[serde(default = "one")]
    pub step: f64,
    /// Where the control starts, and therefore the cell's value before any client connects.
    /// Not range-checked at compile time: a default outside `min..=max` is an authoring
    /// mistake the client will show as an out-of-range handle rather than hide.
    pub default: f64,
}

fn one() -> f64 {
    1.0
}

/// A free numeric entry, optionally bounded. Sets a float cell.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NumberSpec {
    /// Lowest accepted value; omit for unbounded below.
    #[serde(default)]
    pub min: Option<f64>,
    /// Highest accepted value; omit for unbounded above.
    #[serde(default)]
    pub max: Option<f64>,
    /// The cell's starting value.
    pub default: f64,
}

/// A choice from a fixed list. Sets a text cell.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SelectSpec {
    /// The permitted values, in the order the client offers them. Anything else is refused at
    /// the socket, so a `skip_when` can rely on one of these being the current value.
    pub options: Vec<String>,
    /// The starting choice. Not checked against `options` at compile time.
    pub default: String,
}

/// A boolean toggle. Sets a bool cell.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckboxSpec {
    /// Whether it starts checked.
    pub default: bool,
}

/// Free text entry. Sets a text cell.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TextSpec {
    /// The starting text. Empty when omitted.
    #[serde(default)]
    pub default: String,
    /// Shown by the client while the field is empty. Never a value.
    #[serde(default)]
    pub placeholder: String,
}

/// A computed cell: a starting value and a list of steps applied to it in order.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CellSpec {
    /// The cell's name. Its identity everywhere: in `from`, in a pane's `cell`, on the wire,
    /// and in a trace.
    pub name: String,
    /// The cell this one reads: a source, or another cell. Exactly one of `from` and `sql`.
    #[serde(default)]
    pub from: Option<String>,
    /// The steps, applied in order to `from`'s table. Empty is legal and makes this cell an
    /// alias of `from` — and one that costs nothing, since a pipeline that rewrites nothing
    /// never copies the table.
    #[serde(default)]
    pub step: Vec<StepSpec>,
    /// A `select` statement instead of a `from` and a list of steps.
    ///
    /// The whole cell: it names the tables it reads, so it needs no `from`, and it lowers to
    /// steps, so it takes no `step`. [`crate::sql`] is the dialect and ADR-0005 is why there
    /// is one.
    ///
    /// Not to be confused with `[[source]]`'s `sql`, which is a database connection. This one
    /// reads cells and runs in-process; that one reads rows over a network.
    #[serde(default)]
    pub sql: Option<String>,
    /// `place = "client"` — evaluate this cell in the page rather than on the server.
    ///
    /// A deployment decision, not a rewrite: the cell's `from`, its steps and its output are
    /// identical either way, and moving it is one word. What changes is where the work
    /// happens and therefore what an interaction costs.
    ///
    /// **Placement is monotone.** Once a value is in the page it stays there, so a
    /// server-side cell may not read a client-side one. `dagpane check` names the offending
    /// edge and both ways to fix it; `dagpane_core::placement` is the argument for why the
    /// rule is a rule rather than a warning.
    #[serde(default)]
    pub place: Option<Placement>,
}

/// One step. Exactly one field may be set; the compiler says so by name when more or fewer
/// are.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StepSpec {
    /// Keep the rows matching a comparison. Carries an edge via its `param`.
    #[serde(default)]
    pub filter: Option<FilterSpec>,
    /// Add a column computed from each row. Carries an edge per `$name` in its expression.
    #[serde(default)]
    pub derive: Option<DeriveSpec>,
    /// Match this table against another cell's on equal keys. Its `with` is an edge.
    #[serde(default)]
    pub join: Option<JoinSpec>,
    /// Keep only these columns, in this order.
    #[serde(default)]
    pub select: Option<Vec<String>>,
    /// Reorder the rows.
    #[serde(default)]
    pub sort: Option<SortSpec>,
    /// Keep the first n rows. Applied where it is written, so `limit` before `sort` and
    /// `sort` before `limit` mean different things — as they should.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Group and aggregate.
    #[serde(default)]
    pub group_by: Option<GroupBySpec>,
    /// Pull one cell out of the table: `scalar = { column = "total", row = 0 }`. Ends the
    /// pipeline — what follows it would have no table to work on.
    #[serde(default)]
    pub scalar: Option<ScalarSpec>,
    /// The row count as a number. Also ends the pipeline.
    #[serde(default)]
    pub count: Option<bool>,
}

/// Keep the rows where `column op (value | param)` holds.
///
/// Exactly one of `value` and `param` must be set. `param` is the only place in the manifest
/// where an edge is written, so it is also the only field here whose misspelling is a compile
/// error rather than an empty table.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FilterSpec {
    /// The column to test. Checked against the data at run time, not here — a column name is
    /// only as real as the table that arrives.
    pub column: String,
    /// The comparison: `eq`, `ne`, `lt`, `le`, `gt`, `ge` or `contains`.
    pub op: Comparison,
    /// A fixed value to compare against.
    #[serde(default)]
    pub value: Option<toml::Value>,
    /// Or the name of an input cell to read the value from — this is what makes the filter
    /// reactive, and it is the edge. Must already be declared at this point in the file.
    #[serde(default)]
    pub param: Option<String>,
    /// When the parameter currently equals this, skip the step entirely. How an "all"
    /// option on a dropdown is written without a conditional.
    #[serde(default)]
    pub skip_when: Option<toml::Value>,
}

/// Add a column computed from each row: `derive = { name = "margin", expr = "revenue - cost" }`.
///
/// The eighth verb, and the only place a manifest computes a value rather than choosing one.
/// [`dagpane_core::expr`] is the language; two things about it belong here, because they are
/// what make it safe to have at all.
///
/// **A bare name in `expr` is a column. An edge is spelled `$`.** `revenue - cost` reads two
/// columns and depends on nothing new; `amount * (1 - $discount)` reads one column and one
/// cell, and `discount` is an edge in the compiled graph. The edges a derived column declares
/// are exactly the `$` tokens in its text — found by the lexer, not inferred from context —
/// so a reader can see them by reading and the compiler never has to guess. That is
/// ADR-0001's rule applied to an expression: an edge that was inferred wrongly is a stale
/// number on a page that looks correct.
///
/// **Everything else is checked before the app runs.** `dagpane check` binds the expression
/// against the schema the pipeline actually has at this step and names a column that is not
/// there, an operator applied to the wrong type, or a `$name` that resolves to a table.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeriveSpec {
    /// The new column's name. It goes on the end, and it may not be a column the table
    /// already has — two columns of one name would leave every later step that names it
    /// finding the original.
    pub name: String,
    /// The expression, evaluated once per row.
    pub expr: String,
}

/// One column name, or several: `on = "id"` and `on = ["day", "region"]` both work.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Names {
    /// A single name.
    One(String),
    /// Several, in order.
    Many(Vec<String>),
}

impl Names {
    /// The names, as a list.
    pub fn list(&self) -> Vec<String> {
        match self {
            Names::One(n) => vec![n.clone()],
            Names::Many(n) => n.clone(),
        }
    }
}

/// Match this table against another cell's on equal keys:
/// `join = { with = "accounts", left_on = "account_id", right_on = "account", how = "left" }`.
///
/// The ninth verb, and the second one that carries an edge. `with` names a cell, exactly the
/// way a filter's `param` does and with the same rule — it must already be declared, and a
/// name that resolves to nothing is a compile error rather than an edge that quietly does not
/// exist. Nothing here is inferred; the graph gets two cells in and one out because both were
/// typed.
///
/// The edge was never the hard part of a join. Everything that *is* hard is about what
/// happens when the data does not fit the join the author had in mind, and each of those has
/// an answer that fails loudly:
///
/// * **a duplicated key on the right** silently multiplies rows and doubles a total. Refused
///   unless `multiple = true` says it was meant.
/// * **a key that is `int` on one side and `text` on the other** would quietly match nothing.
///   Refused, by `dagpane check` where the schemas are known and at run time otherwise.
/// * **a column name on both sides** would leave every later step finding the first one.
///   Refused unless `suffix` distinguishes them.
/// * **a null key** matches nothing, including another null. SQL's rule.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JoinSpec {
    /// The cell holding the right-hand table. **This is the edge.** Must already be declared.
    pub with: String,
    /// The key column, or columns, when both sides call it the same thing. Exactly one of
    /// `on` and the `left_on`/`right_on` pair.
    #[serde(default)]
    pub on: Option<Names>,
    /// The key columns on this table, when the two sides name them differently.
    #[serde(default)]
    pub left_on: Option<Names>,
    /// The key columns on `with`, positionally paired with `left_on`. They never appear in
    /// the output: they equal the left's by construction.
    #[serde(default)]
    pub right_on: Option<Names>,
    /// `inner`, `left`, `semi` or `anti`. **Required, with no default** — `inner` is SQL's
    /// and choosing it silently is how a page loses rows nobody asked it to lose.
    pub how: How,
    /// Appended to every non-key column carried over from `with`. Only for `inner` and
    /// `left`, which are the two that carry columns at all.
    #[serde(default)]
    pub suffix: String,
    /// Whether one row here may match several rows of `with`, making the table longer.
    /// Only for `inner` and `left`.
    #[serde(default)]
    pub multiple: bool,
}

/// Reorder rows by one column. Nulls sort last in both directions.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SortSpec {
    /// The column to sort on.
    pub column: String,
    /// Descending instead of ascending. Ascending when omitted.
    #[serde(default)]
    pub descending: bool,
}

/// Group rows and aggregate them.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupBySpec {
    /// The grouping columns, in order — which is also the output's column and row order.
    /// Omit for a single group covering the whole table, which is how a summary metric is
    /// written.
    #[serde(default)]
    pub by: Vec<String>,
    /// What to compute per group. At least one.
    pub agg: Vec<AggItem>,
}

/// One aggregated output column.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AggItem {
    /// The column to aggregate. Omitted for `count`, which counts rows and reads no column.
    #[serde(default)]
    pub column: Option<String>,
    /// Which aggregate: `count`, `sum`, `mean`, `min` or `max`.
    pub agg: Agg,
    /// The output column's name, written `as = "…"` in TOML.
    #[serde(rename = "as")]
    pub as_name: String,
}

/// Take one cell out of the table, ending the pipeline with a scalar.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScalarSpec {
    /// The column to read.
    pub column: String,
    /// Which row, 0-based. A row past the end yields null rather than an error: an empty
    /// result is a normal state for a filtered app, and a metric reading "—" beats a page of
    /// red.
    #[serde(default)]
    pub row: usize,
}

/// A pane. Exactly one presentation field may be set.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PaneSpec {
    /// The cell to show. Must name a declared cell; this is checked at compile time.
    pub cell: String,
    /// The pane's identity on the wire. Defaults to `cell`, which is why showing one cell in
    /// two panes needs an explicit `id` on at least one of them.
    #[serde(default)]
    pub id: Option<String>,
    /// A heading above the pane.
    #[serde(default)]
    pub title: Option<String>,
    /// `text = true` — the value, formatted. The odd one out: a bool rather than a table,
    /// because it has nothing to configure.
    #[serde(default)]
    pub text: Option<bool>,
    /// A headline number.
    #[serde(default)]
    pub metric: Option<MetricSpec>,
    /// A table.
    #[serde(default)]
    pub table: Option<TableSpec>,
    /// A bar chart.
    #[serde(default)]
    pub bar: Option<BarSpec>,
    /// A line chart.
    #[serde(default)]
    pub line: Option<LineSpec>,
    /// A drawing this crate does not know about, done by a script the client loaded.
    #[serde(default)]
    pub custom: Option<CustomSpec>,
}

/// A headline number with a caption.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricSpec {
    /// The caption under the number.
    pub label: String,
    /// Decimal places for a float. Omit to print the number as it is.
    #[serde(default)]
    pub decimals: Option<usize>,
    /// Written before the number — a currency symbol, typically.
    #[serde(default)]
    pub prefix: String,
    /// Written after it — a unit or a `%`.
    #[serde(default)]
    pub suffix: String,
}

/// A table pane.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TableSpec {
    /// How many rows to send, 50 by default. A cap on the wire only: the true height travels
    /// with the view, so a truncated table says so rather than looking complete.
    #[serde(default = "fifty")]
    pub max_rows: usize,
}

fn fifty() -> usize {
    50
}

fn five_hundred() -> usize {
    500
}

/// A pane drawn by a client-side renderer.
///
/// The extension point, and the whole of it. Adding a treemap to this runtime used to mean
/// editing five places across two crates and shipping a new server binary; it now means
/// writing `renderers = ["treemap.js"]` and one `custom = { renderer = "treemap", … }`.
///
/// ```toml
/// [[pane]]
/// cell = "spend_flows"
/// title = "Where the money goes"
/// custom = { renderer = "sankey", columns = ["from", "to", "amount"], options = { unit = "USD" } }
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CustomSpec {
    /// The name the script registered. Not checked here — this process has no way to know
    /// what a browser has loaded, and refusing to start over it would be checking the wrong
    /// thing in the wrong place. A client that meets a name it does not know says so in that
    /// pane and leaves the rest of the page working.
    pub renderer: String,
    /// Which columns to send, in this order. Omit to send every column.
    ///
    /// Worth setting on anything wide: this is what keeps a two-series chart over a
    /// hundred-column frame from putting a hundred columns on the wire every time it moves.
    #[serde(default)]
    pub columns: Option<Vec<String>>,
    /// How many rows to send, 500 by default — higher than a table's 50 because a chart's
    /// whole point is the shape of many rows. The true height travels with the view either
    /// way, so a truncated chart can say so.
    #[serde(default = "five_hundred")]
    pub max_rows: usize,
    /// Whatever the renderer wants: a colour scale, a unit, a threshold. Written as ordinary
    /// TOML and carried through to the client as ordinary JSON, with nothing in between
    /// giving it a meaning.
    ///
    /// This is the field that makes the extension point real. Typing it would mean each new
    /// option a drawing wants is a change to this crate — and the point of a `custom` pane is
    /// that a new drawing is not a change to this crate.
    #[serde(default)]
    pub options: std::collections::BTreeMap<String, serde_json::Value>,
}

/// A bar chart, one bar per row.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BarSpec {
    /// The column holding each bar's label.
    pub label_column: String,
    /// The column holding each bar's height. Must be numeric at run time.
    pub value_column: String,
}

/// A line chart, one point per row in row order — so the `sort` step is what decides the
/// line's direction.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LineSpec {
    /// The column holding each point's x. Must be numeric at run time.
    pub x_column: String,
    /// The column holding each point's y. Must be numeric at run time.
    pub y_column: String,
}

// ── errors ─────────────────────────────────────────────────────────────────────────────

/// Everything that can be wrong with a manifest. All of it is found by [`compile`], before
/// any session exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    /// The file is not readable, or is not valid TOML for this schema. Carries the message
    /// from `toml`, which points at a line.
    Parse(String),
    /// A `[[source]]` could not be loaded — a missing file, or a CSV that will not parse.
    Source {
        /// The source's name.
        name: String,
        /// What went wrong, from [`crate::csv::CsvError`] or the filesystem.
        reason: String,
    },
    /// An `[app] renderers` entry that could not be a path inside the app's own directory.
    Renderer {
        /// The path as written.
        path: String,
        /// Why it was refused.
        reason: String,
    },
    /// A `[[input]]`, `[[cell.step]]` or `[[pane]]` that set no presentation, or more than
    /// one. Reported with the names of what was set, because "exactly one" is useless advice
    /// without knowing which ones the reader used.
    NotExactlyOne {
        /// Where in the manifest, in a form a reader can find — the input's or cell's name.
        at: String,
        /// The field names that were set. Empty when none was.
        found: Vec<String>,
        /// The fields that were permitted there, as a written-out list.
        allowed: &'static str,
    },
    /// A placement that would need a value to come back across the cut.
    ///
    /// Separate from [`ManifestError::Graph`] because it is not a structural error in the
    /// app — the graph is fine, and the same manifest with no `place` anywhere compiles and
    /// runs. What is wrong is the deployment, and the message says so in those terms.
    Placement(String),
    /// A filter that gave neither a `value` nor a `param`, or both.
    FilterOperand {
        /// The cell the filter is in.
        cell: String,
        /// The column it filters on.
        column: String,
    },
    /// A filter's `param` names nothing declared *above it* — sources and inputs, and cells
    /// earlier in the file. This is the error that makes a misspelt edge impossible: an edge
    /// that cannot be resolved is not quietly dropped, which would leave a cell that never
    /// recomputes.
    UnknownParam {
        /// The cell holding the filter.
        cell: String,
        /// The name it asked for.
        param: String,
    },
    /// A step that cannot work on the table it will be handed: a column that is not there,
    /// an aggregate over a type it has no meaning for, or an expression that does not
    /// type-check.
    ///
    /// Raised only where the schema at that point in the pipeline is knowable before the app
    /// runs — which, because sources are loaded by [`compile`], is every step of every cell
    /// whose `from` chain reaches a source. Where it is not knowable the step is left to the
    /// run time rather than guessed at, so this error never rejects an app that would have
    /// worked.
    Step {
        /// The cell the step is in.
        cell: String,
        /// Which verb: `filter`, `derive`, `select`, `sort`, `group_by` or `scalar`.
        step: &'static str,
        /// What is wrong, from the transform or the expression checker — the same sentence
        /// the run time would have produced.
        reason: String,
    },
    /// A cell whose `from` names a control rather than a table. A pipeline's first input is
    /// the table it reshapes, and a slider is not one.
    NotATable {
        /// The cell.
        cell: String,
        /// What its `from` named.
        from: String,
        /// What that turned out to be.
        found: String,
    },
    /// A SQL cell that does not parse, is outside the dialect, or does not mean anything.
    Sql {
        /// The cell.
        cell: String,
        /// What is wrong, from [`crate::sql::SqlError`].
        reason: String,
        /// The offending line of the statement with a caret under it — the only useful shape
        /// for an error about a multi-line string.
        point: String,
    },
    /// A pane shows a cell that was never declared.
    UnknownPaneCell {
        /// The pane's id.
        pane: String,
        /// The cell name it named.
        cell: String,
    },
    /// Two panes resolved to the same id. Ids address panes on the wire, so a duplicate would
    /// send one pane's view to the other. Carries the id.
    DuplicatePane(String),
    /// A step that follows a `scalar` or `count`.
    StepAfterScalar {
        /// The cell whose pipeline continues past a scalar.
        cell: String,
    },
    /// A filter's `value` or `skip_when` is a TOML array or table. A filter compares against
    /// one thing; flattening a list into one would be inventing a semantics.
    UnsupportedLiteral {
        /// Where, by cell and column.
        at: String,
        /// What kind of TOML value was found.
        found: String,
    },
    /// The wiring is well-formed but the graph it describes is not — a duplicate cell name,
    /// or a cycle. Carries the rendered [`dagpane_core::BuildError`].
    Graph(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Parse(e) => write!(f, "{e}"),
            ManifestError::Source { name, reason } => {
                write!(f, "source `{name}`: {reason}")
            }
            ManifestError::Renderer { path, reason } => {
                write!(f, "renderer `{path}` {reason}")
            }
            ManifestError::NotExactlyOne { at, found, allowed } => {
                if found.is_empty() {
                    write!(f, "{at}: set exactly one of {allowed}; none was set")
                } else {
                    write!(
                        f,
                        "{at}: set exactly one of {allowed}; found {}",
                        found.join(", ")
                    )
                }
            }
            ManifestError::FilterOperand { cell, column } => write!(
                f,
                "cell `{cell}`: the filter on `{column}` needs exactly one of `value` (a fixed \
                 comparison) or `param` (an input to read it from)"
            ),
            ManifestError::UnknownParam { cell, param } => {
                write!(
                    f,
                    "cell `{cell}` reads `{param}`, which is not an input or a cell"
                )
            }
            ManifestError::UnknownPaneCell { pane, cell } => {
                write!(f, "pane `{pane}` shows `{cell}`, which is not a cell")
            }
            ManifestError::Sql {
                cell,
                reason,
                point,
            } => write!(f, "cell `{cell}`: {reason}\n{point}"),
            ManifestError::Step { cell, step, reason } => {
                write!(f, "cell `{cell}`, step `{step}`: {reason}")
            }
            ManifestError::NotATable { cell, from, found } => write!(
                f,
                "cell `{cell}` reads `{from}`, which is {found} and not a table; a cell's \
                 `from` is the table its steps reshape"
            ),
            ManifestError::DuplicatePane(id) => write!(f, "two panes are named `{id}`"),
            ManifestError::StepAfterScalar { cell } => write!(
                f,
                "cell `{cell}`: `scalar` and `count` produce a number, so nothing can follow them"
            ),
            ManifestError::UnsupportedLiteral { at, found } => {
                write!(
                    f,
                    "{at}: {found} is not a value a filter can compare against"
                )
            }
            ManifestError::Graph(e) => f.write_str(e),
            ManifestError::Placement(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for ManifestError {}

// ── the compiled pipeline ──────────────────────────────────────────────────────────────

/// Where a filter's right-hand side comes from.
#[derive(Clone, Debug)]
enum Operand {
    Literal(Value),
    /// An index into the cell's inputs. Index 0 is always the `from` table, so a parameter
    /// is always 1 or greater.
    Param(usize),
}

#[derive(Clone, Debug)]
enum StepOp {
    Filter {
        column: String,
        op: Comparison,
        operand: Operand,
        skip_when: Option<Value>,
    },
    Derive {
        name: String,
        expr: Expr,
        /// The `$name`s the expression reads, paired with their index into the cell's inputs.
        /// Built from `Expr::params` and from nothing else, which is what makes a derived
        /// column's edges exactly the ones somebody typed.
        params: Vec<(String, usize)>,
    },
    Join {
        /// The index into the cell's inputs holding the right-hand table. Built from `with`,
        /// through the same `bind_param` a filter's `param` uses.
        at: usize,
        spec: Join,
    },
    Select(Vec<String>),
    Sort {
        column: String,
        descending: bool,
    },
    Limit(usize),
    Group(GroupBy),
    Scalar {
        column: String,
        row: usize,
    },
    Count,
}

/// Somewhere for the reader to put a source, in whichever representation this build wants.
///
/// The two arms are observably identical — same values, same schema, same digest, so the
/// same cells recompute and the same panes go on the wire. What differs is resident memory:
/// the Arrow arm stores columns contiguously and dictionary-encodes the low-cardinality text
/// ones. `crates/frame-arrow/tests/oracle.rs` is what holds the two arms to being identical.
#[cfg(feature = "arrow-sources")]
fn source_builder() -> Box<dyn dagpane_core::frame::FrameBuilder> {
    Box::new(dagpane_frame_arrow::ArrowFrameBuilder::new())
}

#[cfg(not(feature = "arrow-sources"))]
fn source_builder() -> Box<dyn dagpane_core::frame::FrameBuilder> {
    Box::new(dagpane_core::frame::TableBuilder::new())
}

/// A compiled cell. One `Compute` shared by every session over the app.
#[derive(Debug)]
struct Pipeline {
    steps: Vec<StepOp>,
}

impl Compute for Pipeline {
    fn eval(&self, inputs: Inputs<'_>) -> Result<Value, CellError> {
        if self.steps.is_empty() {
            // A cell with no steps is an alias of what it reads — including when what it
            // reads is a control rather than a table. Demanding a frame here would make the
            // documented "empty is legal and makes this cell an alias" true only for tables,
            // and a cell whose whole job is to depend on a slider is a reasonable cell.
            //
            // Nothing is recorded, so the alias is compared whole. That is right: it *is*
            // the whole value.
            return Ok(inputs.get(0).clone());
        }

        // An `Arc` bump, not a copy — which is what `Cow` was buying before the seam
        // existed, and it now holds for every step rather than only the skipped ones: each
        // verb returns a new handle and the frames share their columns underneath.
        let mut current: Arc<dyn Frame> = inputs.frame_arc(0)?;

        // Which columns of which inputs this cell's output will turn out to depend on. Built
        // beside the pipeline rather than declared ahead of it, so it describes what the
        // verbs actually did to the frame in hand — see `crate::reads`.
        let mut seen = Provenance::of_input(0, current.width());
        // Every input this pipeline treats as a frame. Input 0 always; a `join`'s right-hand
        // side as it is met. Parameters are not here: a slider has no columns.
        let mut frames: Vec<u16> = vec![0];

        for step in &self.steps {
            match step {
                StepOp::Filter {
                    column,
                    op,
                    operand,
                    skip_when,
                } => {
                    let value = match operand {
                        Operand::Literal(v) => v.clone(),
                        Operand::Param(i) => inputs.get(*i).clone(),
                    };
                    if skip_when.as_ref().is_some_and(|s| *s == value) {
                        // The step did not run, so it read nothing. A filter that is off
                        // genuinely does not make the cell depend on its column, and saying
                        // otherwise would cost a recompute every time that column moved
                        // while the filter was disabled. It also leaves the rows untouched,
                        // so a later predicate can still be recorded as a constraint.
                        seen.filter_skipped();
                        continue;
                    }
                    let spec = Filter {
                        column: column.clone(),
                        op: *op,
                        value,
                    };
                    // The rows the predicate picks, taken once and used twice: to build the
                    // filtered frame, and to record what the cell actually depended on.
                    let keep = transform::selection(&*current, &spec)?;
                    if let Some(c) = current.column_index(column) {
                        seen.filtered_by(c, &spec, &keep);
                    }
                    current = current.take_rows(&keep);
                }
                StepOp::Derive { name, expr, params } => {
                    let values: Vec<(String, Value)> = params
                        .iter()
                        .map(|(n, at)| (n.clone(), inputs.get(*at).clone()))
                        .collect();
                    // The columns the expression names, resolved against the frame it is
                    // about to run over. `Expr::columns` is the same list `derive` itself
                    // will look up, so the two cannot disagree about what was read.
                    let read: Vec<usize> = expr
                        .columns()
                        .iter()
                        .filter_map(|c| current.column_index(c))
                        .collect();
                    current = transform::derive(&current, name, expr, &values)?;
                    seen.derived_from(&read);
                }
                StepOp::Join { at, spec } => {
                    let right = inputs.frame_arc(*at)?;
                    let at = *at as u16;
                    if !frames.contains(&at) {
                        frames.push(at);
                    }
                    match crate::reads::join_columns(&*current, &*right, spec) {
                        Some((left_keys, right_keys)) => {
                            let right_width = right.width();
                            // `semi` and `anti` keep only left rows; the other two widen.
                            let widens = matches!(spec.how, How::Inner | How::Left);
                            current = transform::join(&*current, &*right, spec)?;
                            seen.joined(at, &left_keys, &right_keys, right_width, widens);
                        }
                        None => {
                            // A key column that does not resolve. `join` is about to fail on
                            // the same name; if it somehow does not, this cell is compared
                            // whole rather than on a column set built from a guess.
                            seen.give_up(0);
                            seen.give_up(at);
                            current = transform::join(&*current, &*right, spec)?;
                        }
                    }
                }
                StepOp::Select(columns) => {
                    let cols: Vec<usize> = columns
                        .iter()
                        .filter_map(|c| current.column_index(c))
                        .collect();
                    current = transform::select(&*current, columns)?;
                    seen.selected(&cols);
                }
                StepOp::Sort { column, descending } => {
                    // The order the sort puts the rows in, taken once and used twice: to
                    // build the sorted frame, and to record what the cell depended on. A
                    // downstream cell that never shows the sort column depends on the order
                    // and not on the values that produced it.
                    let order = transform::ordering(&*current, column, *descending)?;
                    if let Some(c) = current.column_index(column) {
                        seen.sorted_by(c, column, *descending, &order);
                    }
                    current = current.take_rows(&order);
                }
                StepOp::Limit(n) => {
                    // Which rows survive a `limit` is decided by the order they are already
                    // in, and whatever decided that order is in the row set already.
                    current = transform::limit(&*current, *n);
                    seen.limited();
                }
                StepOp::Group(spec) => match crate::reads::group_columns(&*current, spec) {
                    Some((keys, aggs)) => {
                        current = transform::group_by(&*current, spec)?;
                        seen.grouped(&keys, &aggs);
                    }
                    None => {
                        seen.give_up(0);
                        current = transform::group_by(&*current, spec)?;
                    }
                },
                StepOp::Scalar { column, row } => {
                    let c = current.column_index(column).ok_or_else(|| {
                        CellError::failed(format!("no column `{column}` to read"))
                    })?;
                    seen.report_scalar(&inputs, &frames, c);
                    // A scalar off the end of a short table is `null`, not an error: an
                    // empty result is a normal state for a filtered app, and a metric
                    // reading "—" is better than a page of red.
                    return Ok(if *row < current.rows() {
                        current.value_at(*row, c)
                    } else {
                        Value::Null
                    });
                }
                StepOp::Count => {
                    // The one verb that reads no data at all. Its answer moves when the rows
                    // move and never when a value does, which over a wide frame is the whole
                    // point of this analysis.
                    seen.report_count(&inputs, &frames);
                    return Ok(Value::int(current.rows() as i64));
                }
            }

            // The analysis and the frame must agree about how many columns exist. If they do
            // not, a rule above is wrong, and the answer is to stop claiming rather than to
            // claim something unchecked — see `crate::reads`.
            if !seen.agrees_with(&*current) {
                debug_assert!(
                    false,
                    "cell pipeline: the read analysis lost track of the frame's width"
                );
                for at in &frames {
                    seen.give_up(*at);
                }
            }
        }

        seen.report_frame(&inputs, &frames);
        Ok(Value::frame(current))
    }
}

// ── compiling ──────────────────────────────────────────────────────────────────────────

/// Parse manifest TOML without compiling it. Checks the file's shape, nothing about the app.
///
/// # Errors
///
/// [`ManifestError::Parse`] for malformed TOML, an unknown key, or a missing required one.
pub fn parse(text: &str) -> Result<Manifest, ManifestError> {
    toml::from_str(text).map_err(|e| ManifestError::Parse(e.to_string()))
}

/// Read and compile a manifest from disk. Paths inside it resolve against its own directory.
///
/// # Errors
///
/// Any [`ManifestError`]: the file may not read, not parse, or not compile.
pub fn load(path: &Path) -> Result<App, ManifestError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ManifestError::Parse(format!("{}: {e}", path.display())))?;
    let manifest = parse(&text)?;
    let base = path.parent().unwrap_or(Path::new("."));
    compile(&manifest, base)
}

/// Where a `[[source]]`'s rows come from — the caller's decision, not the filesystem's.
///
/// One manifest, compiled against whichever of these the host can offer. `dagpane run` has a
/// directory; a browser has bytes the page fetched and no filesystem at all. Neither is a
/// special case of the other, and the manifest does not know which it got.
///
/// This is the seam `ROADMAP.md` §4 asks for, in the smallest form that is honest: *where*
/// an app's data is read is a deployment decision rather than a rewrite. It is not yet the
/// larger claim in that section — the placement of each **cell** is still fixed — and this
/// type is not pretending to be it.
#[derive(Clone, Copy, Debug)]
pub enum Sources<'a> {
    /// Resolve every `[[source]]` against this directory, which is the manifest's own.
    Directory(&'a Path),
    /// Everything the host supplies in place of a filesystem.
    ///
    /// A `[[source]]` not in `sources` is an error naming it, never a silent fall-back to a
    /// path: a host that meant to supply the data and mis-keyed the name would otherwise get
    /// a reach for a file it has no business reaching for, or — in a browser — a confusing
    /// failure a long way from the mistake.
    Bound {
        /// Rows, by `[[source]]` name.
        sources: &'a BTreeMap<String, Arc<dyn Source>>,
        /// Renderer scripts, by the path `[app] renderers` declared. A declared script that
        /// is missing here is the same error a missing file is, for the same reason: a
        /// `custom` pane with no renderer is a blank card, and a blank card is the failure
        /// mode this project exists to not have.
        renderers: &'a BTreeMap<String, String>,
    },
}

/// Turn a parsed manifest into a checked app, reading its sources from a directory.
///
/// Everything that can be wrong with an app is found here, before a single session exists.
/// That is the point of a declared graph: a misspelt column is still a run-time error (the
/// data decides), but a misspelt *cell* is not.
///
/// `base_dir` is what a `[[source]]`'s `csv` path resolves against — the manifest's own
/// directory, never the process's working directory, so an app behaves the same whichever
/// directory it is started from.
///
/// # Errors
///
/// Any [`ManifestError`] but [`ManifestError::Parse`], which [`parse`] has already ruled out.
pub fn compile(manifest: &Manifest, base_dir: &Path) -> Result<App, ManifestError> {
    compile_with(manifest, Sources::Directory(base_dir))
}

/// [`compile`], with the caller saying where the data comes from.
///
/// # Errors
///
/// Any [`ManifestError`] but [`ManifestError::Parse`].
pub fn compile_with(manifest: &Manifest, sources: Sources<'_>) -> Result<App, ManifestError> {
    let mut builder = Graph::builder();
    let mut widgets: Vec<Widget> = Vec::new();
    let mut declared: BTreeSet<String> = BTreeSet::new();
    // What each declared cell produces, threaded through the file in order so that a step
    // can be checked against the table it will actually be handed. Sources are loaded right
    // here, which is the only reason any of this is knowable before the app runs.
    let mut produces: BTreeMap<String, Produces> = BTreeMap::new();

    let mut bound: Vec<BoundSource> = Vec::with_capacity(manifest.source.len());
    for spec in &manifest.source {
        let source = match sources {
            Sources::Directory(base_dir) => compile_source(spec, base_dir)?,
            Sources::Bound { sources, .. } => {
                Arc::clone(sources.get(&spec.name).ok_or_else(|| {
                    ManifestError::Source {
                        name: spec.name.clone(),
                        reason: "no rows were supplied for it, and this host has no filesystem \
                             to read them from"
                            .to_string(),
                    }
                })?)
            }
        };
        // The reader fills the backend's arrays directly. Nothing here builds a
        // representation for it to convert, which is the whole point of the builder seam.
        let frame = source
            .load(source_builder())
            .map_err(|e| ManifestError::Source {
                name: spec.name.clone(),
                reason: e.to_string(),
            })?;
        // Taken AFTER the load, deliberately. A version read first and a load read second
        // would record the version of a file that was rewritten between the two, and every
        // later refresh would compare against it and decide nothing had changed. Reading it
        // second can only over-report a change, which costs one reload.
        let version = source.version().map_err(|e| ManifestError::Source {
            name: spec.name.clone(),
            reason: e.to_string(),
        })?;
        produces.insert(spec.name.clone(), Produces::Frame(frame.schema()));
        builder.source(&spec.name, Value::frame(frame));
        declared.insert(spec.name.clone());
        bound.push(BoundSource {
            cell: spec.name.clone(),
            source,
            loaded_version: version,
            refresh_secs: spec.refresh_secs,
        });
    }

    for input in &manifest.input {
        let widget = compile_input(input)?;
        produces.insert(
            input.name.clone(),
            Ty::of_value(&widget.default)
                .map(Produces::Scalar)
                .unwrap_or(Produces::Unknown),
        );
        builder.source(&input.name, widget.default.clone());
        declared.insert(input.name.clone());
        widgets.push(widget);
    }

    for cell in &manifest.cell {
        let (from, steps) = plan(cell, &produces)?;
        let (pipeline, inputs, output) =
            compile_cell(&cell.name, &from, &steps, &declared, &produces)?;
        builder.cell_with(&cell.name, inputs, std::sync::Arc::new(pipeline));
        declared.insert(cell.name.clone());
        produces.insert(cell.name.clone(), output);
    }

    let graph = builder
        .build()
        .map_err(|e| ManifestError::Graph(e.to_string()))?;

    // Placement is checked here rather than at serve time, so `dagpane check` fails on a cut
    // that cannot work. A backflow edge is a deployment mistake that would otherwise surface
    // as a page missing a pane, which is the worst place to learn about it.
    let placed: Vec<&str> = manifest
        .input
        .iter()
        .filter(|i| i.place == Some(Placement::Client))
        .map(|i| i.name.as_str())
        .chain(
            manifest
                .cell
                .iter()
                .filter(|c| c.place == Some(Placement::Client))
                .map(|c| c.name.as_str()),
        )
        .collect();
    let cut =
        Cut::of_client(&graph, &placed).map_err(|e| ManifestError::Placement(e.to_string()))?;

    let mut panes: Vec<Pane> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for spec in &manifest.pane {
        let pane = compile_pane(spec)?;
        if graph.id(&pane.cell).is_none() {
            return Err(ManifestError::UnknownPaneCell {
                pane: pane.id.clone(),
                cell: pane.cell.clone(),
            });
        }
        if !seen.insert(pane.id.clone()) {
            return Err(ManifestError::DuplicatePane(pane.id));
        }
        panes.push(pane);
    }

    // Once, here — never per session. See `App::split`.
    let split = cut.is_split().then(|| Arc::new(graph.split(&cut)));

    // And what a page would need to compile the other half, for the same reason: the schemas
    // below come from frames this process has already loaded, so deriving them per viewer
    // would be paying again for an answer that cannot change.
    // Read once, here, and used twice: the app serves them and — for a split app — the page's
    // boot block carries them, because a page compiling the re-emitted manifest needs the
    // bytes and not the paths. Compiling them before `client_half` rather than in the struct
    // literal below is what lets that happen without reading them a second time.
    let renderers = compile_renderers(&manifest.app.renderers, sources)?;
    let client_half = if cut.is_split() {
        Some(client_half(manifest, &produces, &renderers)?)
    } else {
        None
    };

    Ok(App {
        title: manifest.app.title.clone(),
        subtitle: manifest.app.subtitle.clone(),
        graph,
        widgets,
        panes,
        sources: bound,
        renderers,
        split,
        client_half,
        cut,
    })
}

/// What a page needs to compile this app's other half.
///
/// The manifest is **re-emitted from what was parsed** rather than re-read from disk: the
/// page then compiles exactly what this process compiled, and not a file somebody edited
/// since. A manifest's bytes are its identity here, so two halves built from different bytes
/// are two different apps wearing one name.
///
/// The schemas come from the **frames already loaded into the graph**, which costs nothing —
/// asking each `Source` again would re-read every CSV once per compile, and a source's
/// `schema()` is explicitly not promised to be cheap.
fn client_half(
    manifest: &Manifest,
    produces: &BTreeMap<String, Produces>,
    renderers: &[Renderer],
) -> Result<crate::wire::ClientHalf, ManifestError> {
    let text = toml::to_string(manifest).map_err(|e| {
        // Unreachable for a `Manifest` that parsed, since every field round-trips; reported
        // rather than unwrapped because the alternative is a panic in a compile path.
        ManifestError::Parse(format!(
            "this manifest cannot be re-emitted for the page: {e}"
        ))
    })?;

    // From `produces`, which this compiler filled in as it went: every `[[source]]` was
    // recorded as `Produces::Frame(frame.schema())` the moment it loaded. Asking each
    // `Source` again instead would re-read every CSV — `Source::schema` is explicitly not
    // promised to be cheap, and for a file it is a full parse.
    let mut sources = BTreeMap::new();
    for spec in &manifest.source {
        let Some(Produces::Frame(columns)) = produces.get(&spec.name) else {
            return Err(ManifestError::Source {
                name: spec.name.clone(),
                reason: "did not load as a table, so it has no shape to send to a page".to_string(),
            });
        };
        sources.insert(
            spec.name.clone(),
            columns
                .iter()
                .map(|(name, ty)| crate::wire::ColumnSpec {
                    name: name.clone(),
                    ty: *ty,
                })
                .collect(),
        );
    }
    Ok(crate::wire::ClientHalf {
        manifest: text,
        sources,
        // The bytes this compile already read, not a second read of the same files. A page
        // that got paths here could not compile its half at all — see `ClientHalf::renderers`.
        renderers: renderers
            .iter()
            .map(|r| (r.path.clone(), r.source.clone()))
            .collect(),
    })
}

/// The renderer scripts an app declares, read at compile time and carried in the app.
///
/// **Read here rather than at request time**, which is the same decision `dagpane-serve`
/// makes about its own client: the process holds the bytes, so serving one is a map lookup
/// and never an `open(2)` on a path a request influenced. It also means `dagpane check` fails
/// on a `custom` pane whose script is missing, instead of a page that loads and has a blank
/// card in it.
///
/// Two checks, both of which are about the path rather than the code. The path must be
/// relative and must not climb out of the app's own directory — a
/// `renderers = ["../../etc/shadow"]` would otherwise be a file read with a manifest for a
/// cover — and after that it must actually be there.
///
/// What this cannot check is that the script registers the renderer a pane names. That is a
/// fact about a page which has not loaded yet, and the client reports it in the pane that
/// wanted it.
fn compile_renderers(
    paths: &[PathBuf],
    sources: Sources<'_>,
) -> Result<Vec<Renderer>, ManifestError> {
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let bad = |reason: String| ManifestError::Renderer {
            path: path.display().to_string(),
            reason,
        };
        if path.is_absolute() {
            return Err(bad("must be relative to the manifest".into()));
        }
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(bad("must not climb out of the app's own directory".into()));
        }
        let Some(name) = path.to_str() else {
            return Err(bad("is not valid UTF-8".into()));
        };
        // `https://evil.example/x.js` is not an absolute PATH — `is_absolute` is false for it
        // on Unix and it contains no `..` — but the client resolves this string with
        // `new URL(path, document.baseURI)`, where it is absolutely a URL. On a server the
        // file read below would fail and catch it; in a browser, whose host supplies the
        // scripts by name, nothing would. A scheme-relative `//host/x.js` is the same hole
        // with a shorter spelling.
        if name.contains(':') || name.starts_with("//") {
            return Err(bad(
                "must be a path inside the app, not a URL — a renderer is served with the app \
                 and never fetched from somewhere else"
                    .into(),
            ));
        }
        let source = match sources {
            Sources::Directory(base_dir) => std::fs::read_to_string(base_dir.join(path))
                .map_err(|e| bad(format!("could not be read: {e}")))?,
            Sources::Bound { renderers, .. } => renderers
                .get(name)
                .cloned()
                .ok_or_else(|| bad("was not supplied to this host".into()))?,
        };
        out.push(Renderer {
            path: name.to_string(),
            source,
        });
    }
    Ok(out)
}

/// One `[app] renderers` script: where it is served from, and what it says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Renderer {
    /// The path the manifest declared, which is also the URL the client imports and the
    /// filename a static export writes.
    pub path: String,
    /// The script itself, read when the app was compiled.
    pub source: String,
}

/// One `[[source]]` entry into the thing that reads it.
///
/// Exactly one of `csv`, `file`, `http` and `sql`, checked by the same [`exactly_one`] that
/// checks a control's kind and a pane's — one mechanism, one error, one message a reader has
/// already seen elsewhere in the file.
fn compile_source(spec: &SourceSpec, base_dir: &Path) -> Result<Arc<dyn Source>, ManifestError> {
    let at = format!("source {}", spec.name);
    exactly_one(
        &at,
        &[
            ("csv", spec.csv.is_some()),
            ("file", spec.file.is_some()),
            ("http", spec.http.is_some()),
            ("sql", spec.sql.is_some()),
        ],
        "csv, file, http, sql",
    )?;

    let misconfigured = |reason: String| ManifestError::Source {
        name: spec.name.clone(),
        reason,
    };

    if let Some(path) = &spec.csv {
        return Ok(Arc::new(FileSource::new(
            base_dir.join(path),
            FileFormat::Csv,
        )));
    }

    if let Some(file) = &spec.file {
        let path = base_dir.join(&file.path);
        return match file.format.as_deref() {
            None => FileSource::of_path(path)
                .map(|s| Arc::new(s) as Arc<dyn Source>)
                .map_err(|e| misconfigured(e.to_string())),
            Some("csv") => Ok(Arc::new(FileSource::new(path, FileFormat::Csv))),
            Some(other) => Err(misconfigured(format!(
                "`format = {other:?}` is not a format this build reads; it reads csv"
            ))),
        };
    }

    if let Some(http) = &spec.http {
        return build_http_source(spec, http).map_err(misconfigured);
    }

    if let Some(sql) = &spec.sql {
        return build_sql_source(spec, sql).map_err(misconfigured);
    }

    unreachable!("exactly_one accepted a source with no kind set")
}

#[cfg(feature = "http")]
fn build_http_source(_spec: &SourceSpec, http: &HttpSpec) -> Result<Arc<dyn Source>, String> {
    let mut source = dagpane_connect::HttpSource::new(&http.url).map_err(|e| e.to_string())?;
    if let Some(secs) = http.timeout_secs {
        source = source.with_timeout(std::time::Duration::from_secs(secs));
    }
    Ok(Arc::new(source))
}

/// The same manifest against a binary built without the feature.
///
/// An error at **compile time of the app**, naming the feature, rather than at the first
/// refresh — and never a source that silently loads nothing. The message says what to
/// rebuild because the person reading it is usually holding a binary somebody else built.
#[cfg(not(feature = "http"))]
fn build_http_source(_spec: &SourceSpec, http: &HttpSpec) -> Result<Arc<dyn Source>, String> {
    Err(format!(
        "this build cannot read {}: it was compiled without the `http` feature \
         (`cargo build --features http`)",
        http.url
    ))
}

#[cfg(feature = "sql")]
fn build_sql_source(_spec: &SourceSpec, sql: &SqlSpec) -> Result<Arc<dyn Source>, String> {
    let mut source =
        dagpane_connect::SqlSource::new(&sql.dsn, &sql.query).map_err(|e| e.to_string())?;
    if let Some(column) = &sql.watch {
        source = source.watching(column);
    }
    Ok(Arc::new(source))
}

#[cfg(not(feature = "sql"))]
fn build_sql_source(_spec: &SourceSpec, _sql: &SqlSpec) -> Result<Arc<dyn Source>, String> {
    Err(
        "this build cannot read a database: it was compiled without the `sql` feature \
         (`cargo build --features sql`)"
            .to_string(),
    )
}

fn exactly_one(at: &str, set: &[(&str, bool)], allowed: &'static str) -> Result<(), ManifestError> {
    let found: Vec<String> = set
        .iter()
        .filter(|(_, present)| *present)
        .map(|(n, _)| format!("`{n}`"))
        .collect();
    if found.len() == 1 {
        Ok(())
    } else {
        Err(ManifestError::NotExactlyOne {
            at: at.to_string(),
            found,
            allowed,
        })
    }
}

fn compile_input(spec: &InputSpec) -> Result<Widget, ManifestError> {
    exactly_one(
        &format!("input `{}`", spec.name),
        &[
            ("slider", spec.slider.is_some()),
            ("number", spec.number.is_some()),
            ("select", spec.select.is_some()),
            ("checkbox", spec.checkbox.is_some()),
            ("text", spec.text.is_some()),
        ],
        "`slider`, `number`, `select`, `checkbox`, `text`",
    )?;

    let label = spec.label.clone().unwrap_or_else(|| spec.name.clone());
    let (kind, default) = if let Some(s) = &spec.slider {
        (
            WidgetKind::Slider {
                min: s.min,
                max: s.max,
                step: s.step,
            },
            Value::float(s.default),
        )
    } else if let Some(n) = &spec.number {
        (
            WidgetKind::Number {
                min: n.min,
                max: n.max,
            },
            Value::float(n.default),
        )
    } else if let Some(s) = &spec.select {
        (
            WidgetKind::Select {
                options: s.options.clone(),
            },
            Value::text(s.default.clone()),
        )
    } else if let Some(c) = &spec.checkbox {
        (WidgetKind::Checkbox, Value::bool(c.default))
    } else {
        let t = spec.text.as_ref().expect("exactly_one checked this");
        (
            WidgetKind::Text {
                placeholder: t.placeholder.clone(),
            },
            Value::text(t.default.clone()),
        )
    };

    Ok(Widget {
        cell: spec.name.clone(),
        label,
        default,
        kind,
    })
}

/// What a cell produces, as far as compile time can tell.
///
/// `Unknown` is the conservative arm and it is load-bearing. A cell's `from` may name a cell
/// declared *later* in the file — the graph resolves every name at once — and that cell's
/// schema is not known while this one compiles. A checker that guessed there would reject
/// apps that work. So nothing downstream of an `Unknown` is checked before the app runs, and
/// all of it is still checked while it does, by the same code.
#[derive(Clone, Debug)]
enum Produces {
    /// A table whose columns are known here.
    Frame(Vec<(String, ColumnType)>),
    /// One value, of a known type — an input's default, or a pipeline ending in `scalar` or
    /// `count`.
    Scalar(Ty),
    /// Not knowable before the app runs.
    Unknown,
}

fn step_error(cell: &str, step: &'static str, reason: impl Into<String>) -> ManifestError {
    ManifestError::Step {
        cell: cell.to_string(),
        step,
        reason: reason.into(),
    }
}

/// The type of a column that must be there, or the error that names it.
///
/// The same sentence [`dagpane_core::transform`] produces at run time, down to the "did you
/// mean" — through the same [`dagpane_core::expr::nearest`], because a misspelt column is a
/// misspelt column whenever it is caught, and two spellings of that message is how the two
/// drift apart.
fn must_have(
    cell: &str,
    step: &'static str,
    schema: &[(String, ColumnType)],
    name: &str,
) -> Result<ColumnType, ManifestError> {
    if let Some((_, ty)) = schema.iter().find(|(c, _)| c == name) {
        return Ok(*ty);
    }
    let hint = dagpane_core::expr::nearest(name, schema.iter().map(|(c, _)| c.as_str()))
        .map(|n| format!(" — did you mean `{n}`?"))
        .unwrap_or_default();
    Err(step_error(
        cell,
        step,
        format!(
            "no column `{name}`{hint}; the table here has {}",
            if schema.is_empty() {
                "no columns".to_string()
            } else {
                schema
                    .iter()
                    .map(|(c, _)| format!("`{c}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ),
    ))
}

/// One step, normalised: exactly one verb, TOML literals turned into [`Value`]s, expressions
/// parsed, and every check that does not need a schema already made.
///
/// **This is the type a SQL cell lowers to.** A SQL front end that produced its own kind of
/// step would be a second implementation of every rule below it — the edge binding, the column
/// checks, the schema propagation — and two implementations of those is how they come to
/// disagree. Lowering to the same `Step` a `[[cell.step]]` becomes means a SQL cell is a
/// pipeline somebody did not have to write, and nothing downstream can tell which it was.
#[derive(Clone, Debug)]
pub(crate) enum Step {
    Filter {
        column: String,
        op: Comparison,
        operand: Rhs,
        skip_when: Option<Value>,
    },
    Derive {
        name: String,
        expr: Expr,
    },
    Join {
        with: String,
        spec: Join,
    },
    Select(Vec<String>),
    Sort {
        column: String,
        descending: bool,
    },
    Limit(usize),
    Group(GroupBy),
    Scalar {
        column: String,
        row: usize,
    },
    Count,
}

/// A filter's right-hand side, before input indices exist: a constant, or the name of a cell.
#[derive(Clone, Debug)]
pub(crate) enum Rhs {
    Literal(Value),
    Param(String),
}

/// What a cell's pipeline is: the cell it reads and the steps it runs.
///
/// Two spellings, one result. A `from` with `[[cell.step]]`s is the pipeline written out; a
/// `sql` statement is the same pipeline, lowered. Nothing below this function can tell which
/// was written, which is the property ADR-0005 exists to keep.
fn plan(
    spec: &CellSpec,
    produces: &BTreeMap<String, Produces>,
) -> Result<(String, Vec<Step>), ManifestError> {
    exactly_one(
        &format!("cell `{}`", spec.name),
        &[("from", spec.from.is_some()), ("sql", spec.sql.is_some())],
        "`from`, `sql`",
    )?;

    if let Some(from) = &spec.from {
        let steps = spec
            .step
            .iter()
            .map(|s| normalise(s, &spec.name))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok((from.clone(), steps));
    }

    let sql = spec.sql.as_ref().expect("exactly_one accepted one of them");
    if !spec.step.is_empty() {
        return Err(ManifestError::Sql {
            cell: spec.name.clone(),
            reason: "a SQL cell is the whole pipeline, so it takes no `[[cell.step]]`".to_string(),
            point: String::new(),
        });
    }
    // The schema is consulted for one thing — expanding `select *` — and through the same
    // map every other check reads, so a `*` can never see a schema the checker cannot.
    let schema_of = |name: &str| match produces.get(name) {
        Some(Produces::Frame(s)) => Some(s.clone()),
        _ => None,
    };
    let lowered = crate::sql::lower(sql, &schema_of).map_err(|e| ManifestError::Sql {
        cell: spec.name.clone(),
        reason: e.to_string(),
        point: crate::sql::point_at(sql, e.at()),
    })?;
    Ok((lowered.from, lowered.steps))
}

/// One `[[cell.step]]` as a [`Step`]. Everything here is decided by the step alone — no
/// schema, no other cell — which is why a SQL cell can produce the same thing without it.
fn normalise(spec: &StepSpec, cell: &str) -> Result<Step, ManifestError> {
    // `count` and `text` are flags, so only `= true` selects them. Testing `is_some()` meant
    // `count = false` compiled to a counting step — the manifest said one thing and the app
    // did the other, silently.
    exactly_one(
        &format!("a step of cell `{cell}`"),
        &[
            ("filter", spec.filter.is_some()),
            ("derive", spec.derive.is_some()),
            ("join", spec.join.is_some()),
            ("select", spec.select.is_some()),
            ("sort", spec.sort.is_some()),
            ("limit", spec.limit.is_some()),
            ("group_by", spec.group_by.is_some()),
            ("scalar", spec.scalar.is_some()),
            ("count", spec.count == Some(true)),
        ],
        "`filter`, `derive`, `join`, `select`, `sort`, `limit`, `group_by`, `scalar`, \
         `count = true`",
    )?;

    if let Some(f) = &spec.filter {
        let operand = match (&f.value, &f.param) {
            (Some(v), None) => Rhs::Literal(literal(
                v,
                &format!("cell `{cell}`, filter on `{}`", f.column),
            )?),
            (None, Some(p)) => Rhs::Param(p.clone()),
            _ => {
                return Err(ManifestError::FilterOperand {
                    cell: cell.to_string(),
                    column: f.column.clone(),
                })
            }
        };
        let skip_when = match &f.skip_when {
            Some(v) => Some(literal(
                v,
                &format!("cell `{cell}`, `skip_when` on `{}`", f.column),
            )?),
            None => None,
        };
        return Ok(Step::Filter {
            column: f.column.clone(),
            op: f.op,
            operand,
            skip_when,
        });
    }

    if let Some(d) = &spec.derive {
        let expr = Expr::parse(&d.expr)
            .map_err(|e| step_error(cell, "derive", format!("`{} = {}`: {e}", d.name, d.expr)))?;
        return Ok(Step::Derive {
            name: d.name.clone(),
            expr,
        });
    }

    if let Some(j) = &spec.join {
        let (left_on, right_on) = match (&j.on, &j.left_on, &j.right_on) {
            (Some(on), None, None) => (on.list(), on.list()),
            (None, Some(l), Some(r)) => (l.list(), r.list()),
            _ => {
                return Err(step_error(
                    cell,
                    "join",
                    "a join names its keys with `on` when both sides agree, or with \
                     `left_on` and `right_on` together when they do not",
                ))
            }
        };
        // `semi` and `anti` carry no columns across, so the two fields that describe what is
        // carried have nothing to say there. Ignoring them would let a manifest state
        // something the app does not do.
        if matches!(j.how, How::Semi | How::Anti) {
            for (field, set) in [("multiple", j.multiple), ("suffix", !j.suffix.is_empty())] {
                if set {
                    return Err(step_error(
                        cell,
                        "join",
                        format!(
                            "`{}` adds no columns and no rows, so `{field}` has nothing to \
                             say about it",
                            j.how
                        ),
                    ));
                }
            }
        }
        return Ok(Step::Join {
            with: j.with.clone(),
            spec: Join {
                left_on,
                right_on,
                how: j.how,
                suffix: j.suffix.clone(),
                multiple: j.multiple,
            },
        });
    }

    if let Some(columns) = &spec.select {
        return Ok(Step::Select(columns.clone()));
    }
    if let Some(sort) = &spec.sort {
        return Ok(Step::Sort {
            column: sort.column.clone(),
            descending: sort.descending,
        });
    }
    if let Some(n) = spec.limit {
        return Ok(Step::Limit(n));
    }
    if let Some(g) = &spec.group_by {
        let mut aggs = Vec::with_capacity(g.agg.len());
        for a in &g.agg {
            // Not a schema question: `count` reads no column and everything else reads one,
            // whatever the table turns out to hold.
            let column = match (&a.column, a.agg) {
                (_, Agg::Count) => String::new(),
                (Some(c), _) => c.clone(),
                (None, agg) => {
                    return Err(step_error(
                        cell,
                        "group_by",
                        format!("`{agg}` needs a column to aggregate; only `count` reads none"),
                    ))
                }
            };
            aggs.push(AggSpec {
                column,
                agg: a.agg,
                as_name: a.as_name.clone(),
            });
        }
        return Ok(Step::Group(GroupBy {
            by: g.by.clone(),
            aggs,
        }));
    }
    if let Some(sc) = &spec.scalar {
        return Ok(Step::Scalar {
            column: sc.column.clone(),
            row: sc.row,
        });
    }
    Ok(Step::Count)
}

fn compile_cell(
    cell: &str,
    from: &str,
    plan: &[Step],
    declared: &BTreeSet<String>,
    produces: &BTreeMap<String, Produces>,
) -> Result<(Pipeline, Vec<String>, Produces), ManifestError> {
    // Input 0 is the table this cell reads. Parameters follow, in first-use order, so a
    // compiled index is stable and a reader of `dagpane graph` sees them in the order the
    // steps mention them.
    let mut inputs: Vec<String> = vec![from.to_string()];
    let mut steps: Vec<StepOp> = Vec::with_capacity(plan.len());
    let mut terminated = false;

    // The schema as it stands at the step about to be compiled. Every check below is
    // conditional on this being `Frame`, which is what keeps a checker that cannot see far
    // enough from inventing an error.
    let mut here = produces.get(from).cloned().unwrap_or(Produces::Unknown);

    for step in plan {
        if terminated {
            return Err(ManifestError::StepAfterScalar {
                cell: cell.to_string(),
            });
        }
        // Only a *step* needs a table. A cell with none is an alias and may alias anything;
        // one with a step is reshaping something, and a slider is not something to reshape.
        // `terminated` above is what makes this reachable only from `from`: the two steps
        // that turn a pipeline into a scalar also end it.
        if let Produces::Scalar(ty) = &here {
            return Err(ManifestError::NotATable {
                cell: cell.to_string(),
                from: from.to_string(),
                found: format!("a {ty}"),
            });
        }

        // The schema at this step, when there is one.
        let schema: Option<Vec<(String, ColumnType)>> = match &here {
            Produces::Frame(s) => Some(s.clone()),
            _ => None,
        };

        match step {
            Step::Filter {
                column,
                op,
                operand,
                skip_when,
            } => {
                let operand = match operand {
                    Rhs::Literal(v) => Operand::Literal(v.clone()),
                    Rhs::Param(p) => Operand::Param(bind_param(&mut inputs, declared, cell, p)?),
                };
                if let Some(schema) = &schema {
                    must_have(cell, "filter", schema, column)?;
                }
                steps.push(StepOp::Filter {
                    column: column.clone(),
                    op: *op,
                    operand,
                    skip_when: skip_when.clone(),
                });
            }
            Step::Derive { name, expr } => {
                let quoted = format!("`{name} = {}`", expr.text());

                // The edge set, and the only place it comes from: the `$` tokens the lexer
                // found. Resolved exactly the way a filter's `param` is, so a misspelt edge is
                // the same error wherever it is written.
                let mut params: Vec<(String, usize)> = Vec::new();
                let mut types: Vec<Option<Ty>> = Vec::new();
                for param in expr.params() {
                    let at = bind_param(&mut inputs, declared, cell, &param)?;
                    types.push(match produces.get(&param) {
                        Some(Produces::Scalar(ty)) => Some(*ty),
                        Some(Produces::Frame(_)) => {
                            return Err(step_error(
                                cell,
                                "derive",
                                format!(
                                    "{quoted}: `${param}` is a table, and an expression reads \
                                     one row's values at a time"
                                ),
                            ))
                        }
                        _ => None,
                    });
                    params.push((param, at));
                }

                // Bind only when every name can be typed. A parameter whose type is not
                // knowable here would bind as "no such parameter", which is a false error
                // about a correct app — so the whole expression waits for the run time.
                let known = schema
                    .as_ref()
                    .zip(types.iter().all(Option::is_some).then_some(()));
                let output = match known {
                    Some((schema, ())) => {
                        if schema.iter().any(|(c, _)| c == name) {
                            return Err(step_error(
                                cell,
                                "derive",
                                format!(
                                    "this table already has a column `{name}`; a derived \
                                     column needs a name of its own, because every step after \
                                     this one that names `{name}` would still find the original"
                                ),
                            ));
                        }
                        let mut scope = Scope::new().with_columns(schema.clone());
                        for ((param, _), ty) in params.iter().zip(&types) {
                            scope = scope.with_param(param, ty.expect("checked above"));
                        }
                        let program = expr
                            .bind(&scope)
                            .map_err(|e| step_error(cell, "derive", format!("{quoted}: {e}")))?;
                        Some(program.output_type())
                    }
                    None => None,
                };

                here = match (schema, output) {
                    (Some(mut schema), Some(ty)) => {
                        schema.push((name.clone(), ty));
                        Produces::Frame(schema)
                    }
                    _ => Produces::Unknown,
                };
                steps.push(StepOp::Derive {
                    name: name.clone(),
                    expr: expr.clone(),
                    params,
                });
            }
            Step::Join { with, spec } => {
                let at = bind_param(&mut inputs, declared, cell, with)?;
                let right_schema = match produces.get(with) {
                    Some(Produces::Frame(s)) => Some(s.clone()),
                    Some(Produces::Scalar(ty)) => {
                        return Err(step_error(
                            cell,
                            "join",
                            format!("`{with}` is {ty}; a join matches two tables"),
                        ))
                    }
                    _ => None,
                };
                here = match (&schema, &right_schema) {
                    // The same `join_schema` the transform calls, so a join `dagpane check`
                    // passes is one the run time will accept, and the schema threaded on from
                    // here is the one it will actually produce.
                    (Some(left), Some(right)) => {
                        Produces::Frame(transform::join_schema(left, right, spec).map_err(|e| {
                            step_error(cell, "join", format!("joining with `{with}`: {e}"))
                        })?)
                    }
                    // A filter's output schema is its input's, whatever is on the other side —
                    // so an unseeable right-hand table costs the key check and nothing else.
                    (Some(left), None) if matches!(spec.how, How::Semi | How::Anti) => {
                        Produces::Frame(left.clone())
                    }
                    _ => Produces::Unknown,
                };
                steps.push(StepOp::Join {
                    at,
                    spec: spec.clone(),
                });
            }
            Step::Select(columns) => {
                if let Some(schema) = &schema {
                    let mut picked = Vec::with_capacity(columns.len());
                    for name in columns {
                        picked.push((name.clone(), must_have(cell, "select", schema, name)?));
                    }
                    here = Produces::Frame(picked);
                }
                steps.push(StepOp::Select(columns.clone()));
            }
            Step::Sort { column, descending } => {
                if let Some(schema) = &schema {
                    must_have(cell, "sort", schema, column)?;
                }
                steps.push(StepOp::Sort {
                    column: column.clone(),
                    descending: *descending,
                });
            }
            Step::Limit(n) => steps.push(StepOp::Limit(*n)),
            Step::Group(group) => {
                if let Some(schema) = &schema {
                    let mut out = Vec::with_capacity(group.by.len() + group.aggs.len());
                    for name in &group.by {
                        out.push((name.clone(), must_have(cell, "group_by", schema, name)?));
                    }
                    for agg in &group.aggs {
                        if agg.agg == Agg::Count {
                            out.push((agg.as_name.clone(), ColumnType::Int));
                            continue;
                        }
                        let input = must_have(cell, "group_by", schema, &agg.column)?;
                        let ty = transform::agg_output_type(input, agg.agg).ok_or_else(|| {
                            step_error(
                                cell,
                                "group_by",
                                format!(
                                    "cannot compute `{}` over a {input} column (`{}`)",
                                    agg.agg, agg.column
                                ),
                            )
                        })?;
                        out.push((agg.as_name.clone(), ty));
                    }
                    here = Produces::Frame(out);
                }
                steps.push(StepOp::Group(group.clone()));
            }
            Step::Scalar { column, row } => {
                here = match &schema {
                    Some(schema) => {
                        Produces::Scalar(Ty::of_column(must_have(cell, "scalar", schema, column)?))
                    }
                    None => Produces::Unknown,
                };
                steps.push(StepOp::Scalar {
                    column: column.clone(),
                    row: *row,
                });
                terminated = true;
            }
            Step::Count => {
                here = Produces::Scalar(Ty::Int);
                steps.push(StepOp::Count);
                terminated = true;
            }
        }
    }

    Ok((Pipeline { steps }, inputs, here))
}

/// Resolve one `param` — a filter's, or a `$name` inside an expression — to an input index,
/// appending it to the cell's inputs the first time it is seen.
///
/// The one place an edge is made. A name that is not declared *above this cell* is an error
/// rather than an edge that quietly does not exist, which is what makes a misspelt edge
/// impossible instead of merely unlikely: a cell wired to nothing never recomputes, and a
/// cell that never recomputes shows a stale number on a page that looks correct.
fn bind_param(
    inputs: &mut Vec<String>,
    declared: &BTreeSet<String>,
    cell: &str,
    param: &str,
) -> Result<usize, ManifestError> {
    if !declared.contains(param) {
        return Err(ManifestError::UnknownParam {
            cell: cell.to_string(),
            param: param.to_string(),
        });
    }
    Ok(inputs.iter().position(|i| i == param).unwrap_or_else(|| {
        inputs.push(param.to_string());
        inputs.len() - 1
    }))
}

/// A TOML scalar as a dagpane value. Arrays and tables are refused rather than flattened:
/// a filter compares against one thing.
fn literal(v: &toml::Value, at: &str) -> Result<Value, ManifestError> {
    Ok(match v {
        toml::Value::Integer(i) => Value::int(*i),
        toml::Value::Float(f) => Value::float(*f),
        toml::Value::String(s) => Value::text(s.clone()),
        toml::Value::Boolean(b) => Value::bool(*b),
        other => {
            return Err(ManifestError::UnsupportedLiteral {
                at: at.to_string(),
                found: format!("a {}", other.type_str()),
            })
        }
    })
}

fn compile_pane(spec: &PaneSpec) -> Result<Pane, ManifestError> {
    let id = spec.id.clone().unwrap_or_else(|| spec.cell.clone());
    exactly_one(
        &format!("pane `{id}`"),
        &[
            ("text", spec.text == Some(true)),
            ("metric", spec.metric.is_some()),
            ("table", spec.table.is_some()),
            ("bar", spec.bar.is_some()),
            ("line", spec.line.is_some()),
            ("custom", spec.custom.is_some()),
        ],
        "`text = true`, `metric`, `table`, `bar`, `line`, `custom`",
    )?;

    let kind = if let Some(m) = &spec.metric {
        PaneKind::Metric {
            label: m.label.clone(),
            decimals: m.decimals,
            prefix: m.prefix.clone(),
            suffix: m.suffix.clone(),
        }
    } else if let Some(t) = &spec.table {
        PaneKind::Table {
            max_rows: t.max_rows,
        }
    } else if let Some(b) = &spec.bar {
        PaneKind::Bar {
            label_column: b.label_column.clone(),
            value_column: b.value_column.clone(),
        }
    } else if let Some(l) = &spec.line {
        PaneKind::Line {
            x_column: l.x_column.clone(),
            y_column: l.y_column.clone(),
        }
    } else if let Some(c) = &spec.custom {
        PaneKind::Custom {
            renderer: c.renderer.clone(),
            columns: c.columns.clone(),
            max_rows: c.max_rows,
            options: c.options.clone(),
        }
    } else {
        PaneKind::Text
    };

    Ok(Pane {
        id,
        cell: spec.cell.clone(),
        title: spec.title.clone(),
        kind,
    })
}
