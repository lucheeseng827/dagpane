# ADR-0005: SQL cells are a front end over the verbs, in a grammar small enough to close

**Status:** accepted · **Date:** 2026-09

## Context

`ROADMAP.md` §6 put SQL last and said why:

> The manifest's job is to produce *edges*. Extracting the dependencies of a SQL statement
> without a real SQL parser means matching table and column names out of text with a regular
> expression, and a regular expression over SQL is wrong eventually — against a CTE, a
> subquery alias, a quoted identifier, a comment containing a table name. A missed edge is a
> cell that does not recompute when it should, which is a stale number on a page that looks
> like it is working.

And it set the gate: *a parser that can be shown to reject what it cannot resolve, rather than
guess.* Two decisions follow from taking that seriously, and they are the whole of this record.

## Decision

### 1. A closed grammar, hand-written

The parser accepts exactly the statements that lower, and nothing else. `with`, `union`,
`having`, `distinct`, window functions, subqueries, `right` and `full` joins, `cast`, `like`
and a comma-separated `from` are refused **by name**, with what to write instead where there is
something.

The ROADMAP anticipated a dependency here — "that is a dependency, a grammar decision, and a
new class of build error" — and this does not take one. The reason is the gate itself.

A third-party SQL parser accepts all of SQL. Using one would mean writing a **blocklist** of
everything this runtime cannot lower, and a blocklist of SQL features is never complete: every
upgrade of the dependency can start accepting a construct the lowering has no case for, and the
failure mode of a missed case is a statement that compiles into the wrong pipeline. That is the
same "wrong eventually" the ROADMAP says about regular expressions, one level up. A grammar
that only accepts what lowers makes "refuses what it cannot resolve" a **property of the
parser** rather than a list somebody has to keep complete.

The cost is stated rather than hidden: **this is not SQL.** It is a dialect that fits on a page,
and an author who knows SQL will hit its edges. `crates/app/src/sql/` documents them and the
errors name them.

### 2. It lowers to the nine verbs, and there is no second evaluator

A statement compiles to the same `Step` values a `[[cell.step]]` compiles to. `where` becomes
`filter`, `group by` becomes `group_by`, `join` becomes `join`, an expression in the select
list becomes `derive`, and the select list becomes `select`. By the time anything runs there is
no SQL left.

This is the load-bearing half. Nulls, type rules, division by zero, the duplicate-key refusal
on a join, the digest short-circuit — every one of those belongs to a verb that already exists,
is already tested, and is already documented. A SQL cell **cannot** behave differently from the
pipeline it compiles to, because it is that pipeline.

`crates/app/tests/sql.rs` holds the differential test that keeps it true: eight questions,
each written once as SQL and once as steps, asserted to render identically.

### 3. Tables are resolved names; parameters are `:name`

`from sales` makes the cell depend on the cell `sales`. That edge comes from the **parse tree**
— resolved through the statement's aliases — and never from scanning the text, so a table name
inside a comment or a string literal is not an edge and a quoted identifier is not a keyword.
Both cases are tested.

A reference to a control is written `:name`, on the same terms as `derive`'s `$name`: explicit,
and the only kind of name in a statement that can reach outside the tables it reads. ADR-0001's
rule is unchanged — every edge in a compiled app is one somebody typed.

## Consequences

**An error about a SQL cell names the step it lowered to.** A statement with no `join` step
written in it is reported against one:

```text
cell `teams`, step `join`: both tables have a column `x`
```

That is correct and it is the point: the steps are what the cell *is*, and the alternative
— a parallel set of SQL-flavoured error messages — would be a second implementation of every
check.

**The dialect cannot say some things the verbs can.** There is no `skip_when`, so the "all"
option on a dropdown is not expressible in SQL and stays a `filter` step. There is no `scalar`
and no `count = true` terminator, so a statement always produces a table and a metric reads its
number out with a step. A SQL join is a **lookup** — `multiple = false`, with no word for
meaning otherwise — so a one-to-many join is written as a `join` step. `examples/apps/18-team-load.toml`
does each of those in the open rather than working around them quietly.

**Growing the grammar is a code change and a release note**, exactly like growing the function
set in `dagpane_core::expr`. That is the price of the property in decision 1, and it is the
right price: a dialect that grows by accident is a dialect whose refusals cannot be trusted.

**The two authoring surfaces are one surface.** `dagpane graph`, `dagpane explain`, the digest
short-circuit, the patch protocol and the compile-time column checks all work on a SQL cell
without knowing it is one. Nothing in this repository has a "is it SQL?" branch below
`crates/app/src/manifest.rs`'s `plan`.
