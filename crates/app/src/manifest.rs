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

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dagpane_core::frame::Frame;
use dagpane_core::graph::Inputs;
use dagpane_core::transform::{self, Agg, AggSpec, Comparison, Filter, GroupBy};
use dagpane_core::{CellError, Compute, Graph, Value};
use serde::{Deserialize, Serialize};

use crate::view::{Pane, PaneKind};
use crate::widget::{Widget, WidgetKind};
use crate::App;

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
}

/// Data loaded once at start-up and shared by every session.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSpec {
    /// The cell name this data is bound to. What a `[[cell]]`'s `from` refers to.
    pub name: String,
    /// Relative to the manifest's own directory, never to the process's working directory —
    /// an app must behave the same whichever directory it is started from.
    pub csv: PathBuf,
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
    /// The cell this one reads: a source, or another cell.
    pub from: String,
    /// The steps, applied in order to `from`'s table. Empty is legal and makes this cell an
    /// alias of `from` — and one that costs nothing, since a pipeline that rewrites nothing
    /// never copies the table.
    #[serde(default)]
    pub step: Vec<StepSpec>,
}

/// One step. Exactly one field may be set; the compiler says so by name when more or fewer
/// are.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StepSpec {
    /// Keep the rows matching a comparison. The only step that can carry an edge, via its
    /// `param`.
    #[serde(default)]
    pub filter: Option<FilterSpec>,
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
        // An `Arc` bump, not a copy — which is what `Cow` was buying before the seam
        // existed, and it now holds for every step rather than only the skipped ones: each
        // verb returns a new handle and the frames share their columns underneath.
        let mut current: Arc<dyn Frame> = inputs.frame_arc(0)?;

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
                        continue;
                    }
                    current = transform::filter(
                        &*current,
                        &Filter {
                            column: column.clone(),
                            op: *op,
                            value,
                        },
                    )?;
                }
                StepOp::Select(columns) => {
                    current = transform::select(&*current, columns)?;
                }
                StepOp::Sort { column, descending } => {
                    current = transform::sort(&*current, column, *descending)?;
                }
                StepOp::Limit(n) => {
                    current = transform::limit(&*current, *n);
                }
                StepOp::Group(spec) => {
                    current = transform::group_by(&*current, spec)?;
                }
                StepOp::Scalar { column, row } => {
                    let c = current.column_index(column).ok_or_else(|| {
                        CellError::failed(format!("no column `{column}` to read"))
                    })?;
                    // A scalar off the end of a short table is `null`, not an error: an
                    // empty result is a normal state for a filtered app, and a metric
                    // reading "—" is better than a page of red.
                    return Ok(if *row < current.rows() {
                        current.value_at(*row, c)
                    } else {
                        Value::Null
                    });
                }
                StepOp::Count => return Ok(Value::int(current.rows() as i64)),
            }
        }

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

/// Turn a parsed manifest into a checked app.
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
    let mut builder = Graph::builder();
    let mut widgets: Vec<Widget> = Vec::new();
    let mut declared: BTreeSet<String> = BTreeSet::new();

    for source in &manifest.source {
        let path = base_dir.join(&source.csv);
        // The reader fills the backend's arrays directly. Nothing here builds a
        // representation for it to convert, which is the whole point of the builder seam.
        let frame =
            crate::csv::load_into(&path, source_builder()).map_err(|e| ManifestError::Source {
                name: source.name.clone(),
                reason: format!("{}: {e}", path.display()),
            })?;
        builder.source(&source.name, Value::frame(frame));
        declared.insert(source.name.clone());
    }

    for input in &manifest.input {
        let widget = compile_input(input)?;
        builder.source(&input.name, widget.default.clone());
        declared.insert(input.name.clone());
        widgets.push(widget);
    }

    for cell in &manifest.cell {
        let (pipeline, inputs) = compile_cell(cell, &declared)?;
        builder.cell_with(&cell.name, inputs, std::sync::Arc::new(pipeline));
        declared.insert(cell.name.clone());
    }

    let graph = builder
        .build()
        .map_err(|e| ManifestError::Graph(e.to_string()))?;

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

    Ok(App {
        title: manifest.app.title.clone(),
        subtitle: manifest.app.subtitle.clone(),
        graph,
        widgets,
        panes,
    })
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

fn compile_cell(
    spec: &CellSpec,
    declared: &BTreeSet<String>,
) -> Result<(Pipeline, Vec<String>), ManifestError> {
    // Input 0 is the table this cell reads. Parameters follow, in first-use order, so a
    // compiled index is stable and a reader of `dagpane graph` sees them in the order the
    // steps mention them.
    let mut inputs: Vec<String> = vec![spec.from.clone()];
    let mut steps: Vec<StepOp> = Vec::with_capacity(spec.step.len());
    let mut terminated = false;

    for step in &spec.step {
        if terminated {
            return Err(ManifestError::StepAfterScalar {
                cell: spec.name.clone(),
            });
        }
        // `count` and `text` are flags, so only `= true` selects them. Testing `is_some()`
        // meant `count = false` compiled to a counting step — the manifest said one thing and
        // the app did the other, silently.
        exactly_one(
            &format!("a step of cell `{}`", spec.name),
            &[
                ("filter", step.filter.is_some()),
                ("select", step.select.is_some()),
                ("sort", step.sort.is_some()),
                ("limit", step.limit.is_some()),
                ("group_by", step.group_by.is_some()),
                ("scalar", step.scalar.is_some()),
                ("count", step.count == Some(true)),
            ],
            "`filter`, `select`, `sort`, `limit`, `group_by`, `scalar`, `count = true`",
        )?;

        if let Some(f) = &step.filter {
            let operand = match (&f.value, &f.param) {
                (Some(v), None) => Operand::Literal(literal(
                    v,
                    &format!("cell `{}`, filter on `{}`", spec.name, f.column),
                )?),
                (None, Some(p)) => {
                    if !declared.contains(p) {
                        return Err(ManifestError::UnknownParam {
                            cell: spec.name.clone(),
                            param: p.clone(),
                        });
                    }
                    let at = inputs.iter().position(|i| i == p).unwrap_or_else(|| {
                        inputs.push(p.clone());
                        inputs.len() - 1
                    });
                    Operand::Param(at)
                }
                _ => {
                    return Err(ManifestError::FilterOperand {
                        cell: spec.name.clone(),
                        column: f.column.clone(),
                    })
                }
            };
            let skip_when = match &f.skip_when {
                Some(v) => Some(literal(
                    v,
                    &format!("cell `{}`, `skip_when` on `{}`", spec.name, f.column),
                )?),
                None => None,
            };
            steps.push(StepOp::Filter {
                column: f.column.clone(),
                op: f.op,
                operand,
                skip_when,
            });
        } else if let Some(columns) = &step.select {
            steps.push(StepOp::Select(columns.clone()));
        } else if let Some(s) = &step.sort {
            steps.push(StepOp::Sort {
                column: s.column.clone(),
                descending: s.descending,
            });
        } else if let Some(n) = step.limit {
            steps.push(StepOp::Limit(n));
        } else if let Some(g) = &step.group_by {
            steps.push(StepOp::Group(GroupBy {
                by: g.by.clone(),
                aggs: g
                    .agg
                    .iter()
                    .map(|a| AggSpec {
                        column: a.column.clone().unwrap_or_default(),
                        agg: a.agg,
                        as_name: a.as_name.clone(),
                    })
                    .collect(),
            }));
        } else if let Some(s) = &step.scalar {
            steps.push(StepOp::Scalar {
                column: s.column.clone(),
                row: s.row,
            });
            terminated = true;
        } else {
            steps.push(StepOp::Count);
            terminated = true;
        }
    }

    Ok((Pipeline { steps }, inputs))
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
        ],
        "`text = true`, `metric`, `table`, `bar`, `line`",
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
