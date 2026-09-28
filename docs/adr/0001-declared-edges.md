# ADR-0001: A cell declares its inputs; the engine never traces them

**Status:** accepted · **Date:** 2026-09

## Context

There are two live ways to know which cells a cell depends on, and both are shipped by
things people use.

**Declared.** The author names the inputs. Dash's callback graph is the reference
implementation: a callback lists its `Output`s and `Input`s and the framework wires them.
The dependency set exists before anything runs.

**Traced.** The runtime discovers a dependency by *watching a read*. A thread-local
observer is installed, the closure runs, every signal it reads registers itself with the
observer, and the set of registrations is the edge set. This is Observable's dataflow,
marimo's, and the mechanism of every Rust signal library — `leptos-reactive`,
`futures-signals`, and the rest of the family.

Tracing is friendlier to write and, on the one axis that sounds most important, it is
strictly better: it gets the **exact** edges, including edges that only exist on the branch
that ran. A declared graph cannot do that and never will. So the case for declaring is not
a precision argument, and any version of this document that implies otherwise is wrong.

## Decision

`GraphBuilder::cell(name, inputs, compute)` takes the input names, and
`GraphBuilder::build` resolves them all at once — rejecting `BuildError::DuplicateName`,
then `BuildError::UnknownInput`, then `BuildError::Cycle { path }` from Kahn's algorithm,
in that order, because a duplicate name makes every later message ambiguous and an
unresolved name cannot take part in a readable cycle report. What comes out is an
`Arc<Graph>` that never changes again.

Three reasons, all of which point the same way:

1. **A cycle is a build error, with the loop written out, rather than a run-time
   surprise.** A traced graph cannot know its edges until it has run the closure, so it
   finds a cycle by running into it — in front of one user, in one session, on whichever
   branch happened to close the loop. Here `trace_cycle` walks the set of nodes that never
   reached in-degree zero, follows the first input still inside that set until a node
   repeats, and returns a closed path so a reader sees a loop rather than a lasso. Three
   tests cover it — `a_cycle_is_a_build_error_and_names_the_loop`,
   `a_self_referencing_cell_is_a_cycle`, and `a_cycle_in_a_manifest_is_a_compile_error`,
   which is the one that matters, because it means a broken manifest never becomes a
   server.

2. **The graph is built once and shared immutably.** `Kind::Source` holds its starting
   value as `Arc<Outcome>` **with its digest already taken**, at build time, not per
   session. `Session::new` does `Arc::clone`. Two viewers of a 600-row CSV point at one
   allocation, and `two_sessions_share_one_allocation_per_untouched_source` asserts it with
   `Arc::ptr_eq` rather than by measuring memory. Traced edges cannot work this way: they
   are discovered per evaluation, so they live in per-session mutable state and every
   session pays to rediscover the same structure.

3. **`dagpane graph` can print the app before it runs.** The claim this project makes is
   about *which cells run*, and a structure that only exists mid-evaluation cannot be
   printed, diffed in review, or asserted on in CI. `--format text` prints the cells with
   their heights, `--format mermaid` a flowchart with one edge per declared input
   (`graph_mermaid_is_a_flowchart_with_one_edge_per_declared_input`), `--format json` the
   structure a tool would consume.

Names are resolved in `build`, not on the way in, so a cell may be declared before an input
it names (`a_cell_may_be_declared_before_the_input_it_names`) — the manifest compiler has no
reason to topologically sort a TOML file before reading it. Duplicate inputs are
deduplicated during the in-degree count, because a cell may legitimately name the same input
twice and a doubled reverse edge would make the walk do twice the work and the in-degree
never reach zero.

The same decision runs through the manifest: every edge in a compiled app is one somebody
typed, because the manifest's job is to produce edges.

## Amendment, 2026-09: an expression language that still declares

The manifest now has a `derive` step and an expression language behind it. The decision above
is unchanged and this is why.

The thing this ADR refuses is an edge **inferred from text**. A SQL string mentioning a table
name, or an expression in which `rate` is a column or a cell depending on what is declared
elsewhere in the file, both have the same shape: a compiler guessing, with a wrong guess
producing a stale number on a page that looks correct.

`derive` does not guess. **A bare name in an expression is a column — data, never an edge. A
reference to a cell is written `$name`.** The edge set of a derived column is exactly the set
of `$` tokens the lexer produced, and the compiler builds edges from that set and reads the
expression for nothing else. `Expr::params` is that function and `compile_cell` is its only
caller. So:

* a reader can see a derived column's dependencies by reading it, without consulting the rest
  of the file;
* a `$name` that resolves to nothing is a compile error (`UnknownParam`, the same one a
  filter's `param` raises) rather than an edge that quietly does not exist;
* adding, removing or misspelling a *column* reference cannot change the graph at all.

Text the compiler cannot resolve to an edge is still refused outright rather than scanned:
there is no SQL, no `eval`, and no way for an expression to name a table. The constraint in
"Scope, in order of increasing danger" has not moved — only the first item on it has shipped.

## Consequences

**An over-declared edge causes a recomputation, and this is the price.** A cell that reads
an input only on some branch still declares it, so it re-runs when that input changes even
though its output will not move. `an_over_declared_edge_costs_one_recomputation_not_a_cascade`
is that case written down: `report` declares `mode` but only reads it on a branch this test
never takes.

**ADR-0003 is what contains it.** The cell recomputes, produces a value whose digest matches
the one it already held, and **nothing below it runs**. That turns an over-declared edge from
a correctness problem into a cost — one compute, bounded, at the edge's own cell — and
`Trace::short_circuited()` counts exactly those cells, so the cost is visible rather than
inferred. A pass full of them is telling an author an edge is over-declared.

**There are no dynamic dependencies at all, and no escape hatch.** A cell cannot decide at
run time to read a different cell. "Show whichever series the dropdown names" has to be
written as one cell that declares every series and selects among them, which recomputes on
any of them. There is no `Inputs::read(name)` and none is planned: adding one would put the
run-time cycle back and cost reason 1 above.

**A forgotten edge is the author's bug and the engine cannot detect it.** A cell that should
declare an input and does not will serve a stale value on a page that looks correct — the
worst failure this project can have, and one a traced runtime cannot have. That asymmetry is
the honest reason the manifest refuses SQL, and the reason its expression language marks every
edge with a `$` instead of inferring one: an edge inferred from text by a regular expression is
a guess with exactly this failure mode, and a verbose edge list is better than a wrong one.

**`Compute` is `Send + Sync + 'static`.** The graph is shared across sessions and sessions
across threads, and the graph outlives every session that borrows it. A closure capturing a
connection pool satisfies all three; one capturing a `&str` from `main` does not, and that
pressure is intended. `GraphBuilder::cell_with` takes an `Arc<dyn Compute>` for anything that
is not a closure — a struct holding a compiled manifest step, say.

**There is no `#[cell]` proc-macro authoring API.** A macro reading a function's *parameter*
names would still be declaration and would be compatible with all of the above; a macro
reading its *body* to find the reads would be compile-time inference, sharing the failure
mode of the regular expression over SQL. Neither is built. `GraphBuilder::cell` takes a
closure and a list of names.
