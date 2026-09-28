# From a notebook to a panel

A notebook is a thing **you run**. A panel is a thing **other people open**. This directory is
about the handoff, and about the one question that handoff normally leaves unanswered: *is the
page I just built going to be cheap to interact with?*

```
quickstart.ipynb        the walkthrough — run it top to bottom
dagpane_notebook.py     the bridge: no dependencies, works with pandas, polars or plain rows
```

## The shape of it

| step | where it happens |
|---|---|
| load and shape the data | your notebook, in Python, however you like |
| declare the panel | `Panel` — a thin mirror of dagpane's TOML |
| write it out | a directory: one CSV per source, one `app.toml` |
| **prove an interaction is cheap** | `.explain()`, in the notebook, before anyone opens a browser |
| serve it | one static binary reading that directory |

After the fourth row **the kernel is not in the loop**. Restart it, delete the venv, hand the
directory to someone who has never installed Python — the panel still serves, because what
serves it is a binary reading files. That is the reason to reach for this rather than leaving
the analysis in the notebook or wrapping it in a Python app server.

## The smallest thing that works

```python
import sys; sys.path.insert(0, 'examples/notebook')
from dagpane_notebook import Panel

p = Panel('Signups', subtitle='last 28 days')
p.source('signups', df)                      # a DataFrame, or a list of dicts
p.select('channel', ['all', 'web', 'app'])
p.cell('scoped', 'signups').filter('channel', 'eq', param='channel', skip_when='all')
p.cell('total', 'scoped').count()
p.metric('total', 'Signups')

p.write('build/signups')     # build/signups/app.toml + build/signups/signups.csv
p.check()                    # compile it; raises on anything wrong, naming a line
p.explain(channel='web')     # -> dict: what that interaction actually cost
p.serve()                    # http://127.0.0.1:8787
```

`Panel` maps one-to-one onto the manifest — `source`, `select`/`slider`/`checkbox`/`text`,
`cell(...).filter(...).group_by(...)`, `metric`/`bar`/`table`/`line`. That is deliberate: the
TOML it writes is one you could have typed, so reading it teaches the authoring surface
instead of hiding it. When you outgrow the builder, delete it and keep the `.toml`. Nothing
here is load-bearing at serve time.

## Why `.explain()` is the point

Every other way of building a data app answers "is this fast?" with a stopwatch, after it is
built, in a browser, on your laptop. `explain` answers it with a **count**, in the notebook,
next to the analysis that produced the page:

```python
cost = panel.explain(channel='web')

assert 'lifetime_revenue' in cost['untouched'],  'the headline number is downstream of a control'
assert len(cost['panes_sent']) < cost['panes_total'], 'every pane repainted'
```

Those are assertions about the *structure* of the app, so they hold for every dataset, not just
today's. Lift them into CI unchanged — `dagpane explain --json` gives a shell the same
structure, and `examples/tools/verify.py` is a worked version of exactly that gate.

## Getting the data across

`write_source` (used by `Panel.source`) takes, in order of preference:

| you have | what happens |
|---|---|
| a pandas DataFrame | `to_csv(index=False)` — the index is dropped deliberately; an unnamed index column would become a real column in the manifest's world |
| a polars DataFrame | `write_csv` |
| a list of dicts | keys of the first row become the header |
| a list of rows | the first row is the header |

Columns are typed by inference when the binary loads the CSV: all-integer becomes an integer
column, all-numeric a float, `true`/`false` a boolean, anything else text. Two consequences
worth knowing before they surprise you:

* **A checkbox can only toggle a filter on a boolean column.** `skip_when` is compared against
  the value the filter *resolves to*, so a toggle must be written with `param=` — and the
  column it compares against has to be the same kind of thing the checkbox holds. If your
  notebook has a status string, emit a boolean column beside it. The builder refuses a
  literal-plus-`skip_when` outright, because that combination compiles, renders, and silently
  never skips.
* **A date column is text**, so it cannot be a `line` chart's x-axis. Emit an integer
  `day_index` beside it — every time-series example in `../apps/` does exactly this.

## Where this fits

* `../README.md` — fifteen worked apps by role, and when *not* to reach for this.
* `../bucket/README.md` — the same handoff, with the data arriving from object storage on a
  refresh loop instead of from a kernel.
* `../apps/` — the manifests to read once the builder stops being the interesting part.

## One caution

**Authentication is off unless you turn it on**, and `serve()` binds loopback by default.
Binding anything else makes the page reachable by anyone who can route to it, notebook-built
or not. Put something that authenticates in front of it first, or run the binary with
`--auth-jwks`; `SECURITY.md` covers both in full.
