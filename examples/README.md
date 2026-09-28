# Nineteen panels, and when to reach for this

Every app in here is a real, running dagpane app over committed data. Every number quoted
below was printed by `dagpane explain` on the manifest beside it — none of them are estimates,
and you can reprint all of them in about a second.

```
apps/        nineteen manifests, grouped by who asks the question
data/        the CSVs they read, regenerated from one seeded script
tools/       the generator, and the gate that keeps all nineteen honest
notebook/    Jupyter -> a panel other people can open
bucket/      object storage -> a panel that follows it
```

---

## Start here

```console
$ cargo build --release -p dagpane-cli          # or: cargo install --path crates/cli
$ cd examples

$ dagpane check apps/13-weekly-review.toml
dagpane: Weekly business review — 14 cells (3 inputs, 10 computed), 9 panes
  1 data source(s) loaded at start-up and shared by every session
  ok

$ dagpane explain apps/06-data-quality.toml --set sla_only=true
  epoch 2 — looked at 3 of 14 cells
    set      sla_only
    ran      late_loads           changed
    ran      late_load_count      changed
  11 cell(s) never looked at: health, max_null_pct, domain, scoped, tables_tracked,
                              breaching, by_table, domains_present, breach_count,
                              dirtiest, domain_count
  patch: 1 of 6 panes — late_load_count

$ dagpane run apps/13-weekly-review.toml
dagpane: http://127.0.0.1:8787
```

Read the last two lines of the `explain`. Ticking one checkbox on a fourteen-cell page looked
at three cells and sent one pane. Eleven cells were **never even visited** — including the one
holding the source data. Nobody annotated that; it falls out of the edges the manifest
declares.

Run the whole set at once:

```console
$ python3 tools/verify.py
  app                          cells  ran  skipped  panes  sent
  01-funnel.toml                  11    6        3      7     5
  ...
  ok — 20 apps compile, and every one of them skipped work.
```

---

## What "data aware" actually buys you

Three words get used loosely, so here is what each one means in a terminal.

**It knows what depends on what.** Edges are declared, so the graph exists before the app
runs — `dagpane graph` prints it, and a cycle or a misspelt edge is a build error naming the
problem rather than a control that silently updates nothing.

**It knows what changed.** Every value carries a 128-bit content digest taken when it was
produced, so "did this move?" is a two-`u64` comparison whether the value is a boolean or a
table. A cell that recomputes and lands on the value it already had **stops the pass there**.

**It tells you, every time.** Every pass returns a record of itself. That is what makes the
claims on this page checkable rather than marketing, and it is why `explain` is a first-class
command and not a debug flag.

The consequence worth internalising: on this page,

```
    ran      channels_present     same value — nothing below it ran
    reused   channel_count        its inputs had not moved
  3 cell(s) never looked at: campaigns, channel, lifetime_spend
```

there are **three different kinds of not-working**, and a runtime that reruns your script
cannot express any of them.

---

## The nineteen, by who is asking

| # | app | role | the question | what it teaches |
|---|---|---|---|---|
| 01 | `01-funnel` | Analyst | where does the funnel leak for this slice? | an insensitive cell stops the pass |
| 02 | `02-experiment` | Analyst | did the treatment move it, and did it break anything? | a guardrail fenced off from every control |
| 03 | `03-cohort` | Analyst | which cohort is worth the acquisition spend? | `skip_when` — a dropdown's "all" is not a conditional |
| 04 | `04-campaigns` | Analyst | which campaigns earn their spend? | a top-N leaderboard is digested like any value |
| 05 | `05-pipeline-freshness` | Data engineer | what is late, and whose is it? | a threshold you can argue with, live |
| 06 | `06-data-quality` | Data engineer | which tables would embarrass me? | a checkbox wired as a real edge |
| 07 | `07-warehouse-spend` | Data engineer | who is spending the query budget? | a diamond, and why height ordering makes it safe |
| 08 | `08-schema-drift` | Data engineer | which columns appeared, which went away? | free text into a `contains` filter, no debounce |
| 09 | `09-deploys` | DevOps / SRE | are we shipping, and does it stick? | a `line` pane, and `sort` as its only opinion |
| 10 | `10-slo-burn` | SRE | which budgets are on fire? | two chains over one source that cannot disagree |
| 11 | `11-ci-builds` | DevOps | what makes CI slow, and what makes it a coin toss? | insensitivity on purpose |
| 12 | `12-capacity` | Platform | can I drain a pool tonight? | two thresholds, and everything else fenced off |
| 13 | `13-weekly-review` | Manager / exec | the WBR | the company number that provably cannot move |
| 14 | `14-team-throughput` | Eng manager | what shipped, and what did it cost in cycle time? | the page stays legible as the question changes |
| 15 | `15-unit-economics` | Exec / finance | which accounts are worth serving? | a fixed reference point under a moving slice |
| 16 | `16-margins` | Exec / finance | which accounts actually make money? | a derived column, and the half of one that is not an edge |
| 17 | `17-service-risk` | SRE | is the service burning budget the one we keep shipping to? | two tables, and a control on each side of the join |
| 18 | `18-team-load` | Eng manager | which teams carry the most, and which ship with no deploy log? | every cell is a `select`, and it lowers to the same verbs |
| 19 | `19-wide-telemetry` | SRE / platform | what is the fleet doing, out of a table nobody designed? | 122 columns, five read — a column nothing reads costs nothing, and neither does one only a ranking reads |

Each manifest opens with a comment naming its mechanic and what to watch. Read the file; it is
shorter than this table suggests.

Nineteen is the odd one out, and deliberately: every other app here teaches something about
the *controls*, and that one teaches something about the **data moving**. Its source is 122
columns wide because an exporter wrote it rather than a person, and the thing to run is not
`--set` but:

```console
$ dagpane explain apps/19-wide-telemetry.toml --change-column metrics.disk_sdb_write_ops
  changed 1 column of a 122-column frame; recomputed 0 of 11 cells
```

It also carries the one cell in the set that earns an **ordering** constraint:
`busiest_ten_memory` ranks the scrape by `cpu0_user` and averages memory over the top ten, so
it depends on the order that column puts the rows in and not on its values. Move every value
in `cpu0_user` and the three cells that *filter* on it run while that one sleeps.

---

## Analyst

> You are exploring. You will move a control, look, move it back, and move a different one —
> forty times in ten minutes. What you want is for the page not to flicker and not to make you
> wait, and for the number you are anchoring on to stay put.

```console
$ dagpane explain apps/01-funnel.toml --set device=mobile
    ran      scoped               changed
    ran      by_step              changed
    ran      steps_present        same value — nothing below it ran
    ran      funnel_revenue       changed
    reused   step_count           its inputs had not moved
    ran      visits               changed
    ran      subscribers          changed
  3 cell(s) never looked at: events, variant, all_sessions
  patch: 5 of 7 panes — visits, subscribers, funnel_revenue, by_step, step_table
```

`steps_present` is the set of funnel steps in the slice. Narrowing to mobile does not change
which steps exist, so it produced the value it already held and the pass stopped: `step_count`
did not run and the step-label pane was not repainted. `all_sessions` reads the source rather
than the filtered cell, so it was not looked at at all — the denominator the page is read
against stays on screen, untouched.

`02-experiment` puts that to work where it matters most: the guardrail metric reads the raw
events, so no control on the readout page can make it move. `04-campaigns` shows the same
thing for a top-10 leaderboard — raise the spend floor and if the same ten campaigns come back
in the same order, the table does not repaint even though the cell recomputed.

---

## Data engineer

> You are not exploring, you are on the hook. The page's job is to answer "is it broken, and
> whose is it" before someone else asks.

```console
$ dagpane explain apps/05-pipeline-freshness.toml --set late_after_hours=4
  epoch 2 — looked at 5 of 12 cells
    set      late_after_hours
    ran      late_runs            changed
    ran      late_count           changed
    ran      late_by_pipeline     changed
    ran      worst_offenders      changed
  7 cell(s) never looked at: runs, team, scoped, total_runs, failures, teams_present, team_count
  patch: 3 of 6 panes — late_count, late_by_pipeline, worst_offenders
```

"Late" is a number two people disagree about. Here it is a control, so the argument happens on
the page in five seconds instead of in a pull request against an alert rule. Moving it touches
five of twelve cells: the failure count, the team census and the window total are all upstream
of it or beside it, so they are not recomputed and they do not flicker while you argue.

`07-warehouse-spend` is the diamond — `by_team`, `by_warehouse` and `expensive` all read one
filtered cell. Height-ordered evaluation means that cell is final before any of the three runs,
so no two panes can ever describe different row sets. Worth running for one more reason:

```console
$ dagpane explain apps/07-warehouse-spend.toml --set min_scanned_gb=10
    ran      expensive            same value — nothing below it ran
    ran      teams_present        same value — nothing below it ran
```

The ten most expensive queries all scan more than 10GB anyway, so raising the floor cannot
change the leaderboard — and the runtime worked that out from the value, not from an
annotation anyone wrote.

`08-schema-drift` types into a `contains` filter with no debounce logic anywhere, and
`06-data-quality` is the cheapest interaction in the set: **3 of 14 cells, 1 of 6 panes**.

---

## DevOps and SRE

> Two people are looking at this at 3am with different theories. The page has to survive both
> of them changing a control, and must never show two panes computed from different slices.

```console
$ dagpane explain apps/12-capacity.toml --set cpu_ceiling=60
  epoch 2 — looked at 4 of 15 cells
    set      cpu_ceiling
    ran      hot_cpu              changed
    ran      hot_cpu_count        changed
    ran      hot_detail           same value — nothing below it ran
  11 cell(s) never looked at: nodes, mem_ceiling, pool, scoped, fleet_size,
                              fleet_cost_per_hour, hot_mem, by_pool, by_zone,
                              node_count, hot_mem_count
  patch: 1 of 8 panes — hot_cpu_count
```

Moving the CPU ceiling touched four cells of fifteen and repainted one pane of eight. The
memory side of the page, the fleet totals and the per-pool breakdown are all structurally
elsewhere — not "cached", not "fast", *not looked at*. And `hot_detail` recomputed to the same
twelve nodes, because the hottest twelve are above 60% either way.

`09-deploys` is the `line` pane: one point per row in row order, so the `sort` step is the
chart's only opinion and the cell above it is the only thing to reason about. (A date column
is text, so a time axis needs an integer `day_index` beside it — every time-series app here
does this.) `10-slo-burn` runs two independent chains over one source, and `11-ci-builds` keeps
the job list still while every duration number moves.

`17-service-risk` is the one with two sources. The SLO feed knows about error budget, the
deploy log knows about shipping, and the question needs both on one row — so it is the app that
could not be written before `join`. What to watch is that each control reaches **one side**:

```console
$ dagpane explain apps/17-service-risk.toml --set environment=staging
  6 cell(s) never looked at: slo, deploys, tier, slo_scoped, burn_by_service,
                             services_watched
  patch: 1 of 5 panes — risk
```

Filtering the deploy log cannot touch the SLO half of the page, and `dagpane graph` shows why
before you run it: `risk` has two edges in, and only one of them has a control above it.
`unshipped` is the same join written `how = "anti"` — the services holding an error budget that
nothing deploys to, which on this data is `reporting` — and it recomputed to the same one row,
so its pane was not repainted.

`18-team-load` is the one written entirely in SQL, and it is worth opening beside `17` to see
that the two authoring surfaces produce the same kind of app:

```console
$ dagpane explain apps/18-team-load.toml --set environment=staging
  7 cell(s) never looked at: delivery_items, deploys, max_cycle, work, totals,
                             team_count, points_total
  patch: 1 of 5 panes — teams
```

Every cell there is a `select`. They are parsed, their tables are resolved, and they lower to
the same nine verbs — so the graph, the trace and the patch are the ones a hand-written
pipeline would have produced, and the cycle-time slider is provably not upstream of the deploy
side. What it demonstrates on purpose: `sum(case when kind = 'incident' then 1 else 0 end)`,
which as steps is a `derive` and a `group_by` and a column nobody wanted to name; a `left join`
that keeps the team with no deploy log on the page instead of dropping it; and a
`left anti join` asking the same question the other way. It also shows the two things the
dialect cannot say — there is no `skip_when`, so its dropdown has no "all", and a `select`
produces a table, so its metrics read their number out with a `scalar` step.


---

## Manager and exec

> You are in a room. Someone says "what about just the north region?". The number you opened
> with must not move, and everyone must be able to see that it did not.

```console
$ dagpane explain apps/13-weekly-review.toml --set region=north
  5 cell(s) never looked at: weekly, segment, from_week,
                             company_revenue, company_new_accounts
  patch: 7 of 9 panes — revenue, new_accounts, churned, tickets,
                        per_week, by_region, by_segment
```

Seven of nine panes re-sliced. The two that did not are the company totals, and they did not
because they read the source rather than the filtered cell — so no control on that page is
upstream of them, and no control on that page *can* move them. That is a structural property
of the manifest, printable before the meeting, not a convention someone has to remember.

`15-unit-economics` does the same for a portfolio: every relative number moves against a book
total that cannot. `14-team-throughput` makes the uncomfortable question cheap — switch the
work type to `incident` and every throughput number re-slices while the team list holds still.

`16-margins` is the one to read if you are wondering what the eighth verb costs. Margin is a
number per account, so it is per-row arithmetic over the whole source — and moving the margin
floor does not run any of it:

```console
$ dagpane explain apps/16-margins.toml --set target_margin=70
  4 cell(s) never looked at: accounts, plan, costed, book_margin
  patch: 2 of 6 panes — clearing, by_verdict
```

`costed` computes margin, margin percent and revenue per user from columns alone. No `$`, so
no edge, so no control on that page is upstream of it and none can be. The judgement that
*does* read the slider is one line in a cell of its own, and three of the six panes recompute
to the values they already held because they project the judged column away.

---

## The nine mechanics

Everything above is these, recombined.

| mechanic | how it is written | shown by |
|---|---|---|
| **The insensitive cell** | group to a set, `select` the key column, `sort` it | 01, 02, 04, 09, 11 |
| **`skip_when` as "all"** | `param = "x", skip_when = "all"` — a step that does nothing | 01–17 (the SQL dialect has no word for it) |
| **A checkbox as a real edge** | `param = "<checkbox>", skip_when = false`, over a boolean column | 06, 10, 14 |
| **The fence** | a cell reading the *source*, not the filtered cell | 02, 04, 12, 13, 15 |
| **The diamond** | several cells reading one filtered cell | 07, 10, 12 |
| **Top-N** | `sort` then `limit`, digested like any other value | 04, 07, 12, 16 |
| **A derive that is not an edge** | bare names are columns; only `$name` is an edge, so per-row arithmetic over the whole source sits in its own cell and no control reaches it | 16 |
| **Aggregate, then join** | group both sides to one row per key before they meet, so the join is a lookup and no total can be doubled | 17, 18 |
| **A cell written as SQL** | `sql = "select …"` instead of `from` and steps; parsed and lowered, never interpreted | 18 |

One of these is a trap worth stating on its own, because it compiles, renders, and is silently
wrong:

```toml
# WRONG — the checkbox controls nothing.
filter = { column = "paged", op = "eq", value = true, skip_when = false }

# RIGHT — the checkbox is the edge.
filter = { column = "paged", op = "eq", param = "paged_only", skip_when = false }
```

`skip_when` is compared against the value the filter **resolves to**. With a literal that is
the literal itself, so `false == true` is never true, the step never skips, and the control is
wired to nothing. Three apps in this directory were written that way; `tools/verify.py` found
all three, and nothing else would have. Hence the gate.

A cell is a `from` and a list of steps, or a `select` statement that lowers to the same
steps. The vocabulary is `filter`, `derive`, `join`, `select`, `sort`, `limit`, `group_by`,
`scalar`, `count`; comparisons are `eq`, `ne`, `lt`, `le`, `gt`, `ge`, `contains`; aggregates are
`count`, `sum`, `mean`, `min`, `max`; inputs are `slider`, `number`, `select`, `checkbox`,
`text`; panes are `metric`, `table`, `bar`, `line`, `text`. That is all of it, and
`dagpane check` tells you when you have written something outside it rather than ignoring it.

`derive` is the one with an inside. Its expressions are arithmetic (`+ - * / %`), comparisons,
`and`/`or`/`not`, and thirteen functions — `abs`, `round`, `floor`, `ceil`, `min`, `max`,
`lower`, `upper`, `trim`, `len`, `concat`, `coalesce`, `if`. A **bare name is a column**; a
**`$name` is a cell**, and is the only thing in an expression that becomes an edge. Strings use
single quotes so TOML needs no escaping; a column name with a space in it goes in backticks.
Nulls propagate through everything, `and` and `or` included, and `coalesce` is the opt-out —
so is division by zero, which is null rather than an infinity.

`join` is the one with two cells. `how` is `inner`, `left`, `semi` or `anti` and is required —
there is no default, because picking `inner` silently is how a page loses rows nobody asked it
to lose. Keys are `on` when both sides agree on the name and `left_on`/`right_on` when they do
not; the right side's key columns are never carried across twice. **A duplicated key on the
right is refused** unless you write `multiple = true`, a name that appears on both sides is
refused unless `suffix` distinguishes them, and a key that is `int` on one side and `text` on
the other is refused by `dagpane check`. `right` is `left` with the two cells swapped, and
there is no `full`.

The SQL dialect is `select`, `from`, one `join`, `where`, `group by`, `order by` and `limit`,
over the same expressions — plus `case`, `between`, `in`, `is null` and `||`, which are
spellings for things the expression language already says. `from t` is an edge and `:name` is
an edge, both from the parse tree rather than from scanning the text. Everything else is
refused by name: `with`, `union`, `having`, `distinct`, window functions, subqueries, `right`
and `full` joins, `cast`, `like` and a comma-separated `from`. It cannot say `skip_when` and it
has no `scalar`, so those stay steps.

---

## Where the data comes from

**From a notebook** — `notebook/`. The analysis stays in Python; the panel is a directory the
notebook writes and then stops being involved in. `Panel.explain()` returns what an interaction
cost as a dict, so "will this be cheap to click around in" is an assertion you write beside the
analysis that produced it:

```python
cost = panel.explain(channel='web')
assert 'lifetime_revenue' in cost['untouched']
assert len(cost['panes_sent']) < cost['panes_total']
```

**From a bucket** — `bucket/`. Sources load once at start-up and there is no hot reload, so
following a bucket means `sync → compile → swap → restart`, and `serve.sh` is that loop with
the gate in the right place: a snapshot that will not compile never reaches a viewer, and an
unchanged bucket costs nothing.

**From anywhere else** — a CSV on disk. That is the entire source interface, which is a real
limit and is listed as one below.

---

## When not to reach for this

Stated here rather than discovered later.

* **You need authorisation finer than one app.** There *is* authentication now — `--auth-jwks`
  verifies OIDC id tokens against a JWKS file, and `dagpane run` still binds loopback and warns
  when you widen it without one. What there is not is anything below the app: sources are
  shared across sessions, so everyone who gets through sees the same rows and there is no
  per-viewer filtering to add. Split the data into separate apps, and do not put rows in a file
  that some viewers of that app should not see.
* **Your data does not fit in memory, or changes by the second.** Sources are loaded whole, at
  start-up, and the in-tree table is `Vec<Option<T>>` per column — no chunking, no predicate
  pushdown. `frame-arrow` cuts the memory materially and `ARCHITECTURE.md` §6 names the seam a
  real engine plugs into, but today this is for data you can hold.
* **You need transforms the vocabulary cannot express.** No window functions, no `full` outer
  join, no aggregate inside an expression. The SQL dialect is a front end over that same
  vocabulary and not a way past it — it has no subqueries and no `with`. `derive` is one row wide — it computes a column
  from the columns beside it and from named controls, with no user functions — and `join`
  matches on equal keys and nothing else, so there is no range join and no inequality
  condition. Do work outside that upstream — in the notebook, in the warehouse, in the job
  that writes the bucket — and let the manifest do the last mile. For anything else a cell is
  a Rust closure; `dagpane-core` is a library first.
* **You want a page that writes back.** This serves views of data. There are no forms, no
  mutations, no callbacks into your code.
* **Your page genuinely is all-or-nothing.** If every cell is downstream of every control, a
  graph buys you nothing and costs you a manifest. `tools/verify.py` asserts the opposite for
  every app here precisely because that is a real way to build a page that looks fine.

---

## Keeping it honest

```console
$ python3 tools/generate_data.py     # deterministic; a diff under data/ is always deliberate
$ python3 tools/verify.py            # the gate
```

`verify.py` runs one representative interaction per app and asserts both halves: something was
**skipped** (at least one cell untouched, fewer panes sent than exist) and something was
**shown** (at least one pane did change). An app failing the first is one where a graph bought
nothing; an app failing the second has a control wired to nothing. Both are silent without it,
and the second is how the `skip_when` trap above was caught.

It exits non-zero, so it is a CI step. `dagpane explain --json` gives a shell the same
structure the notebook gets, so the same assertions run in either place.
