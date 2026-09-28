//! What a cell's output actually depended on, recorded while it ran.
//!
//! # The problem this solves
//!
//! The engine's reuse decision compares the digests of a cell's inputs against the digests
//! it recorded last time. That is whole-*value* granularity: a cell reading three columns of
//! a two-hundred-column frame recomputes when any of the two hundred moves, because the
//! frame it was handed has one digest and that digest moved.
//!
//! This module is the finer unit. A compute may declare, as it runs, that its output depends
//! on only some columns of an input. The engine then records the digests of exactly those
//! columns, and a later pass compares those — so a change to a column the cell never read
//! leaves it asleep.
//!
//! # Why silence is conservative, and why that is the whole safety argument
//!
//! [`ReadLog`] starts every input at [`InputReads::Whole`] and a compute has to *narrow* it.
//! A compute that says nothing gets exactly the behaviour this engine had before sub-node
//! invalidation existed: one digest, compared whole. That ordering is deliberate. The
//! failure mode of a granular scheme is under-invalidation — a cell keeps a cached value
//! after something it genuinely read has changed — and the user sees a stale number on a
//! page that looks correct. Nobody reports that. So the default is the safe answer and every
//! narrowing is a positive claim somebody wrote down, which is the kind of claim a test can
//! be pointed at.
//!
//! `crates/core/tests/oracle.rs` is that test: it makes random column-level edits and
//! asserts every cell holds the value a from-scratch run would have produced. A narrowing
//! that is too narrow fails it.
//!
//! # What a narrowing does *not* excuse
//!
//! A column set never covers a frame's **shape**. A cell that reads no columns at all —
//! `count` over an unfiltered table is exactly that — would otherwise reuse forever, through
//! a row being added and a column being dropped alike. Every column-granular key carries
//! [`crate::frame::shape_digest`] beside it, so the row count and the whole schema are part
//! of the comparison whatever the cell claimed to read.

use std::cell::RefCell;

use crate::digest::Digest;
use crate::transform::Filter;

/// A question a cell asked of one column, and the answer it got about the *rows*.
///
/// The *constraint* half of constrained memoization. Two verbs decide rows from a column
/// without their downstream cells depending on that column's values:
///
///   * a `filter` on `amount >= 400` feeding a total of `revenue` depends on **which rows**
///     survived — move a value from 500 to 600 and the same rows do;
///   * a `sort` on `latency` feeding a chart of host names depends on **what order** the rows
///     ended up in — move every latency by the same amount and the ranking is identical.
///
/// Recording the answer rather than the column is what lets the engine know that.
#[derive(Clone, Debug, PartialEq)]
pub struct RowConstraint {
    /// The column the rule reads, as an index into the input frame.
    ///
    /// Carried beside the rule so the engine can check the cheap thing first: if the column's
    /// digest has not moved, the answer cannot have either, and the rule is never re-run.
    pub column: u32,
    /// The question, re-runnable against the input frame.
    pub rule: RowRule,
    /// The answer: [`crate::transform::selection_digest`] of the rows a filter kept, or
    /// [`crate::transform::ordering_digest`] of the order a sort produced.
    pub rows: Digest,
}

/// Which question a [`RowConstraint`] asked.
#[derive(Clone, Debug, PartialEq)]
pub enum RowRule {
    /// `filter`: which rows survived.
    Filter(Filter),
    /// `sort`: what order the rows ended up in.
    Sort {
        /// The column sorted on, by name, so the rule can be re-run against the input frame.
        column: String,
        /// Whether it sorted descending.
        descending: bool,
    },
}

/// Which part of one input a cell's output depends on.
#[derive(Clone, Debug, PartialEq)]
pub enum InputReads {
    /// The whole value. The default for every input, and the only thing a non-frame input
    /// can be: a slider has no columns to be granular about.
    Whole,
    /// Some columns' data, some rules' *answers about rows*, and the frame's shape.
    ///
    /// Both lists empty is legal and is not the same as [`InputReads::Whole`]: it says the
    /// output depends on the frame's shape and on none of its data, which is what `count`
    /// over an unfiltered table honestly depends on.
    Narrowed {
        /// Columns whose values reach the output — sorted, deduplicated.
        columns: Vec<u32>,
        /// Rules whose answer about the rows reaches the output while the column they read
        /// does not: a `filter`'s selection, a `sort`'s ordering.
        constraints: Vec<RowConstraint>,
    },
}

/// What the log knows about one input so far.
///
/// Three states, not two, and the third is the point. "Nothing has been said about this
/// input" and "a step said it could not describe what it read" both come out of
/// [`ReadLog::take`] as [`InputReads::Whole`], but they behave differently while the cell is
/// still running: the first may still be narrowed, the second may not. Collapsing them would
/// let a later, describable step narrow away the columns an opaque earlier one had touched,
/// which is under-invalidation — the exact failure this module is arranged to prevent.
#[derive(Clone, Debug)]
enum Track {
    /// No step has said anything yet. Narrowing starts a set.
    Untouched,
    /// Narrowed to these columns and these row rules.
    Narrowed {
        columns: Vec<u32>,
        constraints: Vec<RowConstraint>,
    },
    /// Widened for good. Nothing narrows it again.
    Whole,
}

impl Track {
    /// The narrowed parts, starting a set if nothing has been said yet. `None` once the
    /// input has been widened for good.
    fn narrowed(&mut self) -> Option<(&mut Vec<u32>, &mut Vec<RowConstraint>)> {
        if matches!(self, Track::Untouched) {
            *self = Track::Narrowed {
                columns: Vec::new(),
                constraints: Vec::new(),
            };
        }
        match self {
            Track::Narrowed {
                columns,
                constraints,
            } => Some((columns, constraints)),
            Track::Whole | Track::Untouched => None,
        }
    }
}

/// Where a compute records what it read.
///
/// Created per evaluation, on the stack, and read once the compute returns. Not shared
/// between passes and not `Sync` — a [`crate::graph::Compute`] is called concurrently by
/// sessions on different threads, and each call gets its own log through its own
/// [`crate::graph::Inputs`].
#[derive(Debug)]
pub struct ReadLog {
    inner: RefCell<Vec<Track>>,
}

impl ReadLog {
    /// A log for a cell with `inputs` declared inputs, nothing yet recorded about any.
    pub fn new(inputs: usize) -> ReadLog {
        ReadLog {
            inner: RefCell::new(vec![Track::Untouched; inputs]),
        }
    }

    /// Narrow input `i` to these columns.
    ///
    /// Repeated calls **union** rather than replace, so a pipeline that reads one column in
    /// one step and another in a later one depends on both. A call against an input that
    /// [`ReadLog::widen_to_whole`] has already claimed is ignored: widening always wins,
    /// because that is the direction that cannot cause staleness.
    pub fn narrow_to_columns(&self, i: usize, cols: &[usize]) {
        let mut inner = self.inner.borrow_mut();
        let Some(slot) = inner.get_mut(i) else {
            return;
        };
        let Some((columns, _)) = slot.narrowed() else {
            return;
        };
        columns.extend(cols.iter().map(|c| *c as u32));
        columns.sort_unstable();
        columns.dedup();
    }

    /// Record that the cell depended on this rule's *answer about the rows* of input `i`,
    /// and not on the values of the column it asked about.
    ///
    /// Strictly weaker than naming the column, so a caller that cannot establish the
    /// conditions this rests on should call [`ReadLog::narrow_to_columns`] instead and lose
    /// nothing but the saving. Those conditions are the caller's to check — see
    /// `dagpane-app`'s `reads` module, which is the only thing that calls this.
    pub fn record_constraint(&self, i: usize, read: RowConstraint) {
        let mut inner = self.inner.borrow_mut();
        let Some(slot) = inner.get_mut(i) else {
            return;
        };
        let Some((_, constraints)) = slot.narrowed() else {
            return;
        };
        constraints.push(read);
    }

    /// Widen input `i` back to the whole value, and keep it there.
    ///
    /// The escape hatch a compute reaches for when it cannot describe what it read — a verb
    /// whose provenance is not modelled, an input handed to something opaque. It costs the
    /// granularity for that input and keeps the answer right, which is the correct trade
    /// every single time it is unclear.
    pub fn widen_to_whole(&self, i: usize) {
        let mut inner = self.inner.borrow_mut();
        if let Some(slot) = inner.get_mut(i) {
            *slot = Track::Whole;
        }
    }

    /// What was recorded, in input order. An input nothing was said about reads whole.
    pub fn take(self) -> Vec<InputReads> {
        self.inner
            .into_inner()
            .into_iter()
            .map(|t| match t {
                Track::Untouched | Track::Whole => InputReads::Whole,
                Track::Narrowed {
                    columns,
                    constraints,
                } => InputReads::Narrowed {
                    columns,
                    constraints,
                },
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transform::Comparison;
    use crate::value::Value;

    fn cols(v: &[u32]) -> InputReads {
        InputReads::Narrowed {
            columns: v.to_vec(),
            constraints: Vec::new(),
        }
    }

    fn a_filter() -> RowConstraint {
        RowConstraint {
            column: 2,
            rule: RowRule::Filter(Filter {
                column: "amount".into(),
                op: Comparison::Ge,
                value: Value::int(400),
            }),
            rows: Digest::EMPTY,
        }
    }

    fn a_sort() -> RowConstraint {
        RowConstraint {
            column: 5,
            rule: RowRule::Sort {
                column: "latency".into(),
                descending: true,
            },
            rows: Digest::EMPTY,
        }
    }

    #[test]
    fn nothing_recorded_is_the_whole_value() {
        let log = ReadLog::new(2);
        assert_eq!(log.take(), vec![InputReads::Whole, InputReads::Whole]);
    }

    #[test]
    fn narrowing_twice_unions_rather_than_replaces() {
        let log = ReadLog::new(1);
        log.narrow_to_columns(0, &[3, 1]);
        log.narrow_to_columns(0, &[2, 1]);
        assert_eq!(log.take(), vec![cols(&[1, 2, 3])]);
    }

    #[test]
    fn widening_wins_over_a_later_narrowing() {
        let log = ReadLog::new(1);
        log.narrow_to_columns(0, &[1]);
        log.widen_to_whole(0);
        log.narrow_to_columns(0, &[2]);
        // A step that could not describe itself has to cost the granularity for good:
        // narrowing again afterwards would forget the columns the opaque step touched.
        assert_eq!(log.take(), vec![InputReads::Whole]);
    }

    #[test]
    fn widening_discards_a_constraint_too() {
        // The same rule, and worth its own test: a row constraint is a *weaker* claim than a
        // column read, so letting one survive a widening would be the one direction that can
        // cause staleness.
        let log = ReadLog::new(1);
        log.record_constraint(0, a_filter());
        log.widen_to_whole(0);
        log.record_constraint(0, a_sort());
        assert_eq!(log.take(), vec![InputReads::Whole]);
    }

    #[test]
    fn an_empty_narrowing_is_not_the_whole_value() {
        // `count` over an unfiltered table reads no column and still depends on the shape.
        let log = ReadLog::new(1);
        log.narrow_to_columns(0, &[]);
        assert_eq!(log.take(), vec![cols(&[])]);
    }

    #[test]
    fn a_constraint_alone_narrows_the_input() {
        let log = ReadLog::new(1);
        log.record_constraint(0, a_filter());
        assert_eq!(
            log.take(),
            vec![InputReads::Narrowed {
                columns: Vec::new(),
                constraints: vec![a_filter()],
            }]
        );
    }

    #[test]
    fn a_filter_and_a_sort_are_different_rules_and_both_are_kept() {
        let log = ReadLog::new(1);
        log.record_constraint(0, a_filter());
        log.record_constraint(0, a_sort());
        assert_eq!(
            log.take(),
            vec![InputReads::Narrowed {
                columns: Vec::new(),
                constraints: vec![a_filter(), a_sort()],
            }]
        );
    }

    #[test]
    fn columns_and_constraints_accumulate_side_by_side() {
        let log = ReadLog::new(1);
        log.narrow_to_columns(0, &[5]);
        log.record_constraint(0, a_filter());
        log.narrow_to_columns(0, &[1]);
        assert_eq!(
            log.take(),
            vec![InputReads::Narrowed {
                columns: vec![1, 5],
                constraints: vec![a_filter()],
            }]
        );
    }

    #[test]
    fn an_out_of_range_input_is_ignored_rather_than_panicking() {
        let log = ReadLog::new(1);
        log.narrow_to_columns(9, &[0]);
        log.record_constraint(9, a_filter());
        assert_eq!(log.take(), vec![InputReads::Whole]);
    }
}
