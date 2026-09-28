//! Which columns of a cell's inputs its output depends on, worked out as the pipeline runs.
//!
//! # What this is for
//!
//! `dagpane_core::reads` gives a compute somewhere to say "my output depended on only these
//! columns of that input", and the engine then compares only those columns' digests. This
//! module is what works out the answer for a compiled pipeline.
//!
//! It is a dataflow analysis over the nine verbs, and it is small because they are nine and
//! closed. Every column of the frame in hand carries the set of **input** columns it derives
//! from; a separate set carries the input columns that decided which rows are present and in
//! what order. When the pipeline ends, the answer is the union of the surviving columns'
//! origins and the row origins.
//!
//! That second set is the part worth stating twice. A `filter` on `region` adds nothing to
//! any column's origins — it does not change a single value — but every value in the result
//! depends on it, because it decided which rows are there at all. Forgetting that would be a
//! cell that sleeps through a filter column changing, which is the silent-staleness failure
//! this whole scheme has to be arranged not to have.
//!
//! # Where it gives up, on purpose
//!
//! [`Provenance::give_up`] widens an input back to whole. Any step this module cannot
//! account for calls it, and the cell is then compared exactly as it was before sub-node
//! invalidation existed — correct, merely coarse. The width check in
//! [`Provenance::agrees_with`] is the same idea applied to the analysis itself: if the
//! columns this module thinks exist and the columns the frame actually has ever disagree,
//! the analysis has a bug, and the response is to stop claiming anything rather than to
//! claim something unverified.

use dagpane_core::digest::Digest;
use dagpane_core::frame::Frame;
use dagpane_core::graph::Inputs;
use dagpane_core::reads::{RowConstraint, RowRule};
use dagpane_core::transform::{ordering_digest, selection_digest, Agg, Filter, GroupBy, Join};

/// A `group_by`'s columns, resolved: the key columns, then each aggregate paired with the
/// column it reads.
type GroupColumns = (Vec<usize>, Vec<(Agg, usize)>);

/// One column of one of the cell's inputs: `(input index, column index)`.
type Origin = (u16, u32);

/// Merge `from` into `into`, keeping it sorted and deduplicated.
fn absorb(into: &mut Vec<Origin>, from: &[Origin]) {
    into.extend_from_slice(from);
    into.sort_unstable();
    into.dedup();
}

/// Where one column of the frame in hand came from.
#[derive(Clone, Debug)]
struct Col {
    /// Every input column its values derive from.
    origins: Vec<Origin>,
    /// `Some` when this column **is** that input column, value for value, in the input's own
    /// row order — no step has touched it.
    ///
    /// Only a passthrough column can carry a row constraint, because a constraint is re-run
    /// against the *input* frame and has to mean the same thing there as it did here.
    passthrough: Option<Origin>,
}

impl Col {
    fn of_input(at: u16, col: u32) -> Col {
        Col {
            origins: vec![(at, col)],
            passthrough: Some((at, col)),
        }
    }

    fn derived(origins: Vec<Origin>) -> Col {
        Col {
            origins,
            passthrough: None,
        }
    }
}

/// A rule this pipeline applied, waiting to find out whether it can stay a constraint.
///
/// It can only stay one if the column's *values* never reach the output. A cell that filters
/// on `amount` and then shows `amount` depends on those values like any other column, and the
/// constraint buys nothing — so the decision is deferred until the pipeline ends and the
/// surviving columns are known. A `sort` is the same bargain with a different answer: pin the
/// order, not the values that produced it.
struct Pending {
    origin: Origin,
    rule: RowRule,
    rows: Digest,
}

/// The analysis state: where every column of the frame in hand came from.
pub(crate) struct Provenance {
    /// One entry per column of the current frame, in the frame's own column order.
    columns: Vec<Col>,
    /// Input columns that decided which rows are present and in what order, and whose values
    /// are therefore needed in full.
    rows: Vec<Origin>,
    /// Rules that may yet be recorded as constraints instead of as row dependencies.
    pending: Vec<Pending>,
    /// True while no step has changed which rows are present, or their order.
    ///
    /// A row constraint is validated by re-running its rule against the **input** frame, so
    /// the row indices recorded have to be indices into the input's rows. That is only true
    /// while this holds — after one `filter`, a `sort`, a `limit`, a `group_by` or a `join`,
    /// a later rule's row indices mean something else entirely, and it becomes an ordinary
    /// row dependency instead. Only the first row-changing step of a pipeline can be a
    /// constraint, and that is not a limitation of the implementation but of what the
    /// recorded answer would mean.
    pristine: bool,
    /// Inputs this analysis has stopped claiming anything about.
    surrendered: Vec<u16>,
}

impl Provenance {
    /// The state at the top of a pipeline: column `i` of input `at` is its own origin, and
    /// nothing has yet decided the rows.
    pub(crate) fn of_input(at: u16, width: usize) -> Provenance {
        Provenance {
            columns: (0..width as u32).map(|c| Col::of_input(at, c)).collect(),
            rows: Vec::new(),
            pending: Vec::new(),
            pristine: true,
            surrendered: Vec::new(),
        }
    }

    /// Stop claiming anything about this input. Irreversible, and always safe.
    pub(crate) fn give_up(&mut self, at: u16) {
        if !self.surrendered.contains(&at) {
            self.surrendered.push(at);
        }
    }

    /// Whether the analysis still believes the same frame the pipeline is holding.
    ///
    /// A disagreement means a verb reshaped the frame in a way a rule here did not model. In
    /// a debug build that is a failed assertion, because it is a bug and a test should say
    /// so; in a release build the caller surrenders every input, because a wrong column set
    /// shows a user a stale number and a coarse one only costs a recompute.
    pub(crate) fn agrees_with(&self, frame: &dyn Frame) -> bool {
        self.columns.len() == frame.width()
    }

    /// Row membership and order now depend on this column as well.
    fn rows_depend_on(&mut self, col: usize) {
        let origins = self
            .columns
            .get(col)
            .map(|c| c.origins.clone())
            .unwrap_or_default();
        absorb(&mut self.rows, &origins);
    }

    /// `sort`: no value changes, but the order every later step sees is decided by this
    /// column.
    ///
    /// Under the same two conditions a `filter` needs — the column is a passthrough of an
    /// input column, and nothing has moved the rows yet — this is recorded as a
    /// **constraint**: the order the rows ended up in, rather than the values that put them
    /// there. A column whose values all shift by the same amount ranks the rows identically,
    /// and so does one whose values move without crossing each other, which is what most
    /// drift in a metric column looks like. Otherwise it is an ordinary row dependency,
    /// which is what this verb always used to be.
    pub(crate) fn sorted_by(
        &mut self,
        col: usize,
        column: &str,
        descending: bool,
        order: &[usize],
    ) {
        let passthrough = self.columns.get(col).and_then(|c| c.passthrough);
        match (self.pristine, passthrough) {
            (true, Some(origin)) => self.pending.push(Pending {
                origin,
                rule: RowRule::Sort {
                    column: column.to_string(),
                    descending,
                },
                rows: ordering_digest(order),
            }),
            _ => self.rows_depend_on(col),
        }
        self.pristine = false;
    }

    /// `filter`: which rows survive is decided by this column.
    ///
    /// When the column is a passthrough of an input column and nothing has moved the rows
    /// yet, this is recorded as a **constraint** — the rows the predicate picked, rather than
    /// the values it picked them by. Otherwise it is an ordinary row dependency, which is
    /// what this verb always used to be.
    pub(crate) fn filtered_by(&mut self, col: usize, spec: &Filter, keep: &[usize]) {
        let passthrough = self.columns.get(col).and_then(|c| c.passthrough);
        match (self.pristine, passthrough) {
            (true, Some(origin)) => self.pending.push(Pending {
                origin,
                rule: RowRule::Filter(spec.clone()),
                rows: selection_digest(keep),
            }),
            _ => self.rows_depend_on(col),
        }
        self.pristine = false;
    }

    /// A `filter` whose `skip_when` turned it off. It read nothing and changed nothing, so
    /// the rows are still the input's and a later rule can still be a constraint.
    pub(crate) fn filter_skipped(&mut self) {}

    /// `derive`: one column appended, deriving from the columns its expression read.
    pub(crate) fn derived_from(&mut self, read: &[usize]) {
        let mut origins = Vec::new();
        for c in read {
            if let Some(o) = self.columns.get(*c) {
                absorb(&mut origins, &o.origins);
            }
        }
        self.columns.push(Col::derived(origins));
    }

    /// `select`: the surviving columns keep their origins and their passthrough status, in
    /// the new order.
    pub(crate) fn selected(&mut self, cols: &[usize]) {
        self.columns = cols
            .iter()
            .map(|c| {
                self.columns
                    .get(*c)
                    .cloned()
                    .unwrap_or_else(|| Col::derived(Vec::new()))
            })
            .collect();
    }

    /// `limit`: which rows survive is decided by the order they are already in, and whatever
    /// decided that order is in the row set already.
    pub(crate) fn limited(&mut self) {
        self.pristine = false;
    }

    /// `group_by`: the keys decide the groups, so they decide the rows. The output is the key
    /// columns followed by one column per aggregate.
    ///
    /// [`Agg::Count`] contributes nothing: it counts rows and ignores the column it names, so
    /// a `count` aggregate depends on what decided the rows and on no data at all. That is
    /// not a micro-optimisation — a per-group row count over a wide frame is one of the
    /// commonest panes there is, and getting it right is most of why this analysis pays.
    pub(crate) fn grouped(&mut self, keys: &[usize], aggs: &[(Agg, usize)]) {
        for k in keys {
            self.rows_depend_on(*k);
        }
        let mut out: Vec<Col> = keys
            .iter()
            .map(|k| {
                Col::derived(
                    self.columns
                        .get(*k)
                        .map(|c| c.origins.clone())
                        .unwrap_or_default(),
                )
            })
            .collect();
        for (agg, col) in aggs {
            out.push(Col::derived(match agg {
                Agg::Count => Vec::new(),
                _ => self
                    .columns
                    .get(*col)
                    .map(|c| c.origins.clone())
                    .unwrap_or_default(),
            }));
        }
        self.columns = out;
        self.pristine = false;
    }

    /// `join`: the left's columns carry over unchanged, then — for the two `how`s that widen
    /// — the right's non-key columns.
    ///
    /// Which rows survive is decided by both sides' key columns, so those go into the row
    /// set. The right's key columns never appear in the output, because they equal the
    /// left's by construction, which is why they are skipped here as well as there.
    ///
    /// **`semi` and `anti` add no columns at all.** They are filters that happen to read
    /// another table, so the right-hand side reaches this cell's output only through which
    /// rows survived — its key columns, and nothing else. A `semi` join against a
    /// two-hundred-column dimension table makes this cell depend on the one column it
    /// joined on, which is the honest answer and a large one.
    pub(crate) fn joined(
        &mut self,
        right_at: u16,
        left_keys: &[usize],
        right_keys: &[usize],
        right_width: usize,
        widens: bool,
    ) {
        for k in left_keys {
            self.rows_depend_on(*k);
        }
        let right_key_origins: Vec<Origin> =
            right_keys.iter().map(|k| (right_at, *k as u32)).collect();
        absorb(&mut self.rows, &right_key_origins);
        // A joined column is the right's values rearranged onto the left's rows, so it is no
        // longer that input's column in that input's order: not a passthrough.
        if widens {
            for c in 0..right_width as u32 {
                if right_keys.contains(&(c as usize)) {
                    continue;
                }
                self.columns.push(Col::derived(vec![(right_at, c)]));
            }
        }
        self.pristine = false;
    }

    /// The origins of one column, plus whatever decided the rows — what a `scalar` reads.
    fn one_column_and_the_rows(&self, col: usize) -> Vec<Origin> {
        let mut all = self.rows.clone();
        if let Some(o) = self.columns.get(col) {
            absorb(&mut all, &o.origins);
        }
        all
    }

    /// Everything the whole frame depends on: every surviving column, and the rows.
    fn everything(&self) -> Vec<Origin> {
        let mut all = self.rows.clone();
        for o in &self.columns {
            absorb(&mut all, &o.origins);
        }
        all
    }

    /// Record the answer against `inputs`, for a pipeline that ended holding a frame.
    pub(crate) fn report_frame(&self, inputs: &Inputs<'_>, of: &[u16]) {
        self.report(inputs, of, self.everything());
    }

    /// Record the answer for a pipeline that ended in a `scalar` off one column.
    pub(crate) fn report_scalar(&self, inputs: &Inputs<'_>, of: &[u16], col: usize) {
        self.report(inputs, of, self.one_column_and_the_rows(col));
    }

    /// Record the answer for a `count`, which depends on what decided the rows and on no
    /// column's data whatsoever.
    pub(crate) fn report_count(&self, inputs: &Inputs<'_>, of: &[u16]) {
        self.report(inputs, of, self.rows.clone());
    }

    /// Split one origin set across the inputs it names and write it into the read log.
    ///
    /// `of` is every input this pipeline treated as a frame. An input that appears there and
    /// in no origin is narrowed to the empty set, which is a real claim — "the shape of this
    /// frame matters and none of its data does" — and not an omission. An input that was
    /// surrendered is left alone, so it keeps the whole-value comparison.
    ///
    /// A pending rule becomes a constraint only if its column is **not** in the hard set
    /// already. A cell that filters on `amount` and then shows `amount` needs those values
    /// whatever the predicate did, so the constraint would be a second, weaker statement about
    /// a column already depended on in full — dead weight to store and to check. The same
    /// goes for a cell that sorts by `latency` and then charts `latency`.
    fn report(&self, inputs: &Inputs<'_>, of: &[u16], origins: Vec<Origin>) {
        for at in of {
            if self.surrendered.contains(at) {
                inputs.reads_whole(*at as usize);
                continue;
            }
            let cols: Vec<usize> = origins
                .iter()
                .filter(|(i, _)| i == at)
                .map(|(_, c)| *c as usize)
                .collect();
            inputs.reads_only_columns(*at as usize, &cols);
            for p in &self.pending {
                if p.origin.0 != *at || origins.contains(&p.origin) {
                    continue;
                }
                inputs.reads_constraint(
                    *at as usize,
                    RowConstraint {
                        column: p.origin.1,
                        rule: p.rule.clone(),
                        rows: p.rows,
                    },
                );
            }
        }
    }
}

/// The columns a `group_by` aggregates, resolved against the frame it is about to run on.
///
/// Returns `None` if any name does not resolve — the verb itself is about to fail on the
/// same name, and an analysis built on a guess is worse than no analysis.
pub(crate) fn group_columns(frame: &dyn Frame, spec: &GroupBy) -> Option<GroupColumns> {
    let keys: Option<Vec<usize>> = spec.by.iter().map(|n| frame.column_index(n)).collect();
    let aggs: Option<Vec<(Agg, usize)>> = spec
        .aggs
        .iter()
        .map(|a| {
            // `Count` ignores its column, so a name that does not resolve is not a reason to
            // abandon the analysis — `group_by` will not look it up either.
            match a.agg {
                Agg::Count => Some((Agg::Count, usize::MAX)),
                _ => frame.column_index(&a.column).map(|c| (a.agg, c)),
            }
        })
        .collect();
    Some((keys?, aggs?))
}

/// A join's key columns on both sides, resolved. `None` if any name does not resolve.
pub(crate) fn join_columns(
    left: &dyn Frame,
    right: &dyn Frame,
    spec: &Join,
) -> Option<(Vec<usize>, Vec<usize>)> {
    let l: Option<Vec<usize>> = spec.left_on.iter().map(|n| left.column_index(n)).collect();
    let r: Option<Vec<usize>> = spec
        .right_on
        .iter()
        .map(|n| right.column_index(n))
        .collect();
    Some((l?, r?))
}
