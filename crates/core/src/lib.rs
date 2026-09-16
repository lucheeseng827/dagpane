//! `dagpane-core` — the reactive engine behind dagpane. PURE: no I/O, no async runtime, no
//! network, no clock, no `unsafe`. Everything in here is a function of the app's declared
//! graph plus the values currently in a session, and that is deliberate: this is the only
//! surface where the product's claim can be *wrong*. A missed invalidation shows somebody a
//! stale number and the page looks fine; a spurious one is the exact tax this project exists
//! to remove. Both are bugs in this crate and nowhere else, so it stays auditable
//! line-by-line and testable with plain values — `cargo test -p dagpane-core` is a complete
//! test of the reactive semantics on a machine with no browser and no network.
//!
//! # The shape of it
//!
//! ```
//! use dagpane_core::{Graph, Session, Value};
//!
//! let mut b = Graph::builder();
//! b.source("threshold", Value::float(10.0));
//! b.source("readings", Value::list(vec![Value::float(4.0), Value::float(20.0)]));
//! b.cell("above", ["threshold", "readings"], |i| {
//!     let t = i.float(0)?;
//!     let n = i.get(1).as_list().unwrap_or(&[]).iter()
//!         .filter(|v| v.as_float().is_some_and(|x| x >= t))
//!         .count();
//!     Ok(Value::int(n as i64))
//! });
//! b.cell("label", ["above"], |i| Ok(Value::text(format!("{} above", i.int(0)?))));
//! let graph = b.build().unwrap();
//!
//! let mut s = Session::new(graph);
//! let first = s.refresh();                 // the first pass computes everything
//! assert_eq!(first.evaluated(), 2);
//!
//! s.set("threshold", Value::float(1.0)).unwrap();
//! let t = s.commit();                      // and this one computes what depends on it
//! assert_eq!(t.evaluated(), 2);
//! assert_eq!(s.get("label").unwrap().value().unwrap().as_text(), Some("2 above"));
//!
//! // Setting an input to the value it already holds is not a change.
//! s.set("threshold", Value::float(1.0)).unwrap();
//! assert_eq!(s.commit().visited(), 0);
//! ```
//!
//! # The four ideas, in the order they matter
//!
//! 1. **Edges are declared, so the graph is checked once and shared.** A cycle is a build
//!    error rather than something a user runs into. [`graph`] says why, at length, including
//!    what it costs.
//! 2. **Evaluation is in height order, which is what makes it glitch-free.** No cell ever
//!    sees a mix of old and new upstream values, and no pass ever runs a cell twice.
//!    [`session`] has the argument.
//! 3. **A value that did not change stops the pass.** Cells are compared by 128-bit content
//!    [`digest`], taken once when a value is produced, so the comparison is two `u64`s
//!    whether the value is a boolean or a million rows.
//! 4. **Every pass reports what it did.** [`trace::Trace`] is how "only the cells that
//!    depend on it recompute" stops being a sentence and becomes an assertion in a test and
//!    a number on the wire.
//!
//! # Errors are values
//!
//! A cell whose compute fails holds a [`error::CellError`] instead of a value, and every
//! cell below it holds `Upstream` naming the cell that actually failed. The pass finishes.
//! One broken column takes out one number, not the page — and the slider that will fix it
//! still works.

#![forbid(unsafe_code)]
#![deny(missing_debug_implementations)]
#![deny(missing_docs)]

pub mod digest;
pub mod error;
pub mod frame;
pub mod graph;
pub mod session;
pub mod trace;
pub mod transform;
pub mod value;

pub use digest::{Digest, Digestible};
pub use error::{BuildError, CellError, SessionError};
pub use graph::{CellId, Compute, Graph, GraphBuilder, Inputs};
pub use session::{Outcome, Session};
pub use trace::{Step, StepOutcome, Trace};
pub use value::{Column, ColumnData, ColumnType, Table, Value};
