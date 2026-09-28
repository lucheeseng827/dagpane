"""Hand a notebook's result to dagpane, and drive the binary from the notebook.

Standard library only — no pandas import, no TOML writer dependency, nothing to install
beside whatever the notebook already has. It works with a pandas DataFrame, a polars
DataFrame, a list of dicts or a list of rows, because it asks the object what it can do
rather than checking what it is.

WHAT THIS IS FOR, and it is worth being precise because the alternative tools blur it:

    the notebook is where the analysis is DONE.  dagpane is where it is SERVED.

The handoff is a directory — one CSV per source and one manifest — and after the handoff the
notebook is not in the loop. The kernel can be dead, the venv can be deleted, and the
dashboard still serves, because what serves it is a single static binary reading files. That
is the whole reason to reach for this instead of leaving the analysis in the notebook: a
notebook is a thing you run, and a panel is a thing other people open.

    from dagpane_notebook import Panel

    p = Panel("Signups", subtitle="last 28 days")
    p.source("signups", df)                       # DataFrame -> signups.csv
    p.select("channel", ["all", "web", "app"])
    p.cell("scoped", "signups").filter("channel", "eq", param="channel", skip_when="all")
    p.cell("total", "scoped").count()
    p.metric("total", "Signups")
    p.write("build/")                             # build/app.toml + build/signups.csv

    p.check()                                     # compile it, raise on anything wrong
    p.explain(channel="web")                      # -> dict: what that interaction cost
    p.serve()                                     # http://127.0.0.1:8787, blocks

`Panel` is a thin mirror of the manifest, deliberately. Every method maps onto one TOML
construct with the same name, so the manifest it writes is one you could have typed, and
reading it teaches the authoring surface rather than hiding it. When you outgrow this, delete
the builder and keep the `.toml` — nothing here is load-bearing at serve time.
"""

from __future__ import annotations

import csv
import json
import os
import pathlib
import shutil
import subprocess

__all__ = ["Panel", "write_source", "find_dagpane", "DagpaneError"]


class DagpaneError(RuntimeError):
    """A `dagpane` invocation exited non-zero. Carries what it printed."""


# ── finding the binary ──────────────────────────────────────────────────────────────────

def find_dagpane():
    """The `dagpane` binary: `$DAGPANE`, then `PATH`, then a release build in this checkout.

    Raises with the three things to try rather than a bare `FileNotFoundError`, because
    "binary not found" inside a notebook cell is otherwise a five-minute detour.
    """
    explicit = os.environ.get("DAGPANE")
    if explicit:
        if pathlib.Path(explicit).exists():
            return explicit
        raise DagpaneError(f"$DAGPANE is set to {explicit!r}, which does not exist")

    found = shutil.which("dagpane")
    if found:
        return found

    here = pathlib.Path(__file__).resolve()
    for parent in here.parents:
        candidate = parent / "target" / "release" / "dagpane"
        if candidate.exists():
            return str(candidate)

    raise DagpaneError(
        "no `dagpane` binary found. Either:\n"
        "  cargo install --path crates/cli        # onto PATH\n"
        "  cargo build --release -p dagpane-cli   # into ./target/release\n"
        "  os.environ['DAGPANE'] = '/path/to/dagpane'"
    )


# ── getting data out of the notebook ────────────────────────────────────────────────────

def write_source(data, path):
    """Write `data` as a CSV dagpane can load. Returns the row count.

    Accepts, in this order: anything with `to_csv` (pandas), anything with `write_csv`
    (polars), a list of dicts, or a (header, rows) pair. Everything else is a TypeError
    naming what it got — a source that silently wrote zero rows would surface as an empty
    dashboard much later.
    """
    path = pathlib.Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)

    # pandas. index=False matters: an unnamed index column becomes a column in the manifest's
    # world, and `dagpane check` would accept it without comment.
    if hasattr(data, "to_csv") and hasattr(data, "columns"):
        data.to_csv(path, index=False)
        return len(data)

    # polars
    if hasattr(data, "write_csv"):
        data.write_csv(path)
        return len(data)

    rows = list(data)
    if not rows:
        raise TypeError(f"refusing to write an empty source to {path}")

    if isinstance(rows[0], dict):
        header = list(rows[0].keys())
        with path.open("w", newline="", encoding="utf-8") as fh:
            w = csv.DictWriter(fh, fieldnames=header, lineterminator="\n")
            w.writeheader()
            w.writerows(rows)
        return len(rows)

    if isinstance(rows[0], (list, tuple)) and len(rows) >= 2:
        with path.open("w", newline="", encoding="utf-8") as fh:
            csv.writer(fh, lineterminator="\n").writerows(rows)
        return len(rows) - 1

    raise TypeError(
        f"don't know how to write {type(data).__name__} as a dagpane source; "
        "pass a DataFrame, a list of dicts, or a list of rows whose first row is the header"
    )


# ── a very small TOML writer, for exactly this schema ───────────────────────────────────

def _toml_value(v):
    """One TOML scalar, array or inline table. Strings are escaped; bools are not quoted."""
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return repr(v)
    if isinstance(v, (list, tuple)):
        return "[" + ", ".join(_toml_value(x) for x in v) + "]"
    if isinstance(v, dict):
        return "{ " + ", ".join(f"{k} = {_toml_value(x)}" for k, x in v.items()) + " }"
    s = str(v).replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n")
    return f'"{s}"'


def _inline(d):
    """A dict as a TOML inline table — the `{ key = value, ... }` every spec here uses."""
    return "{ " + ", ".join(f"{k} = {_toml_value(v)}" for k, v in d.items()) + " }"


# ── the builder ─────────────────────────────────────────────────────────────────────────

class _Cell:
    """A cell under construction. Every method appends one `[[cell.step]]`, in order."""

    def __init__(self, name, source):
        """`name` is the cell; `source` is the source or earlier cell it reads."""
        self.name = name
        self.source = source
        self.steps = []

    def filter(self, column, op, value=None, param=None, skip_when=None):
        """One `filter` step.

        Exactly one of `value` (a literal) and `param` (an input name — this is the edge)
        must be given. `skip_when` is compared against whichever of the two the filter
        resolves to, which is why a checkbox or an "all" option has to be written with
        `param`: with a literal the comparison is against the literal, so the step never
        skips and the control is wired to nothing.
        """
        if (value is None) == (param is None):
            raise ValueError(
                f"filter on `{column}`: give exactly one of value= (a literal) "
                "or param= (an input name)"
            )
        if skip_when is not None and param is None:
            raise ValueError(
                f"filter on `{column}`: skip_when= needs param=. It is compared against the "
                "value the filter resolves to, so with a literal it can never match and the "
                "step would never skip."
            )
        spec = {"column": column, "op": op}
        if value is not None:
            spec["value"] = value
        else:
            spec["param"] = param
        if skip_when is not None:
            spec["skip_when"] = skip_when
        self.steps.append(("filter", spec))
        return self

    def select(self, columns):
        """Keep only these columns, in this order. A `select` STEP — not the `select` input."""
        self.steps.append(("select", list(columns)))
        return self

    def sort(self, column, descending=False):
        """Reorder rows by one column. Nulls sort last either way.

        This is what decides a `line` pane's direction: a line is one point per row in row
        order, so the chart has no ordering opinion of its own.
        """
        self.steps.append(("sort", {"column": column, "descending": descending}))
        return self

    def limit(self, n):
        """Keep the first `n` rows. `sort` then `limit` is how a top-N leaderboard is written."""
        self.steps.append(("limit", int(n)))
        return self

    def group_by(self, by=None, agg=None):
        """One `group_by`. `agg` is a list of dicts: {column?, agg, as}.

        Omit `by` for a single group over the whole table, which is how a summary metric is
        written.
        """
        if not agg:
            raise ValueError("group_by needs at least one aggregate")
        self.steps.append(("group_by", {"by": list(by or []), "agg": list(agg)}))
        return self

    def scalar(self, column, row=0):
        """Take one cell of the table as a single value — how a metric pane is fed.

        Must be the last step: the compiler rejects anything after a `scalar` or `count`.
        Beware `row`: it indexes the table AS ORDERED, so reading row 0 of a list sorted by one
        column while labelling it by another is a confidently wrong number.
        """
        self.steps.append(("scalar", {"column": column, "row": int(row)}))
        return self

    def count(self):
        """The number of rows, as an integer value. Must be the last step, like `scalar`."""
        self.steps.append(("count", True))
        return self

    def _render(self):
        """This cell and its steps as TOML, in declaration order."""
        out = [f"[[cell]]\nname = {_toml_value(self.name)}\nfrom = {_toml_value(self.source)}"]
        for kind, spec in self.steps:
            if kind == "group_by":
                aggs = ",\n  ".join(_inline(a) for a in spec["agg"])
                by = f"by = {_toml_value(spec['by'])}, " if spec["by"] else ""
                out.append(f"[[cell.step]]\ngroup_by = {{ {by}agg = [\n  {aggs},\n] }}")
            elif kind in ("select", "limit", "count"):
                out.append(f"[[cell.step]]\n{kind} = {_toml_value(spec)}")
            else:
                out.append(f"[[cell.step]]\n{kind} = {_inline(spec)}")
        return "\n".join(out)


class Panel:
    """An app under construction, and a handle on the binary once it is written."""

    def __init__(self, title, subtitle=None):
        """An empty app. Add sources, inputs, cells and panes, then `write()` it."""
        self.title = title
        self.subtitle = subtitle
        self._sources = []      # (name, csv_filename, data_or_None)
        self._inputs = []
        self._cells = []
        self._panes = []
        self.dir = None
        self.path = None

    # ── sources ──
    def source(self, name, data=None, csv_path=None):
        """Declare a source. Pass `data` to have it written for you, or `csv_path` to point
        at a file you already produced — a bucket sync, a warehouse unload, an earlier cell.
        """
        if (data is None) == (csv_path is None):
            raise ValueError(f"source `{name}`: give exactly one of data= or csv_path=")
        self._sources.append((name, csv_path or f"{name}.csv", data))
        return self

    # ── inputs ──
    def slider(self, name, min, max, default=None, step=1.0, label=None):
        """A numeric slider. `default` falls back to `min`, which is what a viewer sees first."""
        self._inputs.append((name, label, "slider", {
            "min": float(min), "max": float(max), "step": float(step),
            "default": float(default if default is not None else min),
        }))
        return self

    def number(self, name, default, min=None, max=None, label=None):
        """A typed number box. Use it over a slider for a threshold people argue about."""
        spec = {"default": float(default)}
        if min is not None:
            spec["min"] = float(min)
        if max is not None:
            spec["max"] = float(max)
        self._inputs.append((name, label, "number", spec))
        return self

    def select(self, name, options, default=None, label=None):
        """A dropdown. Include an "all" option and pair it with `skip_when="all"` on the filter,
        which makes that choice a step that does nothing rather than a conditional."""
        options = list(options)
        self._inputs.append((name, label, "select", {
            "options": options, "default": default if default is not None else options[0],
        }))
        return self

    def checkbox(self, name, default=False, label=None):
        """A boolean toggle.

        It can only gate a filter over a BOOLEAN column, wired as
        `filter(col, "eq", param="<this input>", skip_when=False)`. If your data has a status
        string, emit a boolean column beside it.
        """
        self._inputs.append((name, label, "checkbox", {"default": bool(default)}))
        return self

    def text(self, name, default="", placeholder="", label=None):
        """A free-text box, usually feeding a `contains` filter. `skip_when=""` makes an empty
        box mean "no filter". Note the value is unbounded in length."""
        self._inputs.append((name, label, "text", {
            "default": default, "placeholder": placeholder,
        }))
        return self

    # ── cells ──
    def cell(self, name, source):
        """Declare a cell reading `source`, and return it so steps can be chained onto it.

        What it reads is the whole reactive wiring. A cell reading the SOURCE rather than a
        filtered cell is never recomputed by any control on the page — that is how a company
        total is fenced off from a regional filter.
        """
        c = _Cell(name, source)
        self._cells.append(c)
        return c

    # ── panes ──
    def metric(self, cell, label, decimals=None, prefix="", suffix="", id=None):
        """A single number. The cell must end in `scalar` or `count`."""
        spec = {"label": label}
        if decimals is not None:
            spec["decimals"] = int(decimals)
        if prefix:
            spec["prefix"] = prefix
        if suffix:
            spec["suffix"] = suffix
        self._panes.append((cell, id, None, "metric", spec))
        return self

    def table(self, cell, title=None, max_rows=50, id=None):
        """A table of the cell's rows, truncated to `max_rows`."""
        self._panes.append((cell, id, title, "table", {"max_rows": int(max_rows)}))
        return self

    def bar(self, cell, label_column, value_column, title=None, id=None):
        """A bar chart. `value_column` must be numeric at run time."""
        self._panes.append((cell, id, title, "bar", {
            "label_column": label_column, "value_column": value_column,
        }))
        return self

    def line(self, cell, x_column, y_column, title=None, id=None):
        """A line chart, one point per row in row order — so the cell's `sort` sets its direction.

        Both columns must be numeric at run time. A date column is TEXT, so a time axis needs an
        integer index column beside it; every time-series example under `../apps/` does this.
        """
        self._panes.append((cell, id, title, "line", {
            "x_column": x_column, "y_column": y_column,
        }))
        return self

    # ── output ──
    def _check_pane_ids(self):
        """A pane's id addresses it on the wire, so two panes cannot resolve to the same one.

        The binary catches this too, and says so clearly. This catches it a step earlier and
        names the argument to pass, because the id defaults to the cell name — so showing one
        cell as both a bar and a table, which is an entirely reasonable thing to want, collides
        by default.
        """
        seen = {}
        for cell, pane_id, *_ in self._panes:
            resolved = pane_id or cell
            if resolved in seen:
                raise ValueError(
                    f"two panes resolve to the id `{resolved}`. A pane's id defaults to its "
                    f"cell name, so showing `{cell}` twice needs an explicit one on the "
                    f"second: .table({cell!r}, id={resolved + '_table'!r})"
                )
            seen[resolved] = True

    def to_toml(self):
        """The whole app as a manifest string. This is the file you keep; the builder is not.

        Raises `ValueError` if two panes resolve to the same id.
        """
        self._check_pane_ids()
        out = [
            "# Written by dagpane_notebook.Panel. This file is the app — the notebook that",
            "# produced it is not in the loop once it exists, and editing it here is the",
            "# expected next step rather than a workaround.",
            "",
            f"[app]\ntitle = {_toml_value(self.title)}",
        ]
        if self.subtitle:
            out.append(f"subtitle = {_toml_value(self.subtitle)}")
        for name, filename, _ in self._sources:
            out.append(f"\n[[source]]\nname = {_toml_value(name)}\ncsv = {_toml_value(filename)}")
        for name, label, kind, spec in self._inputs:
            block = f"\n[[input]]\nname = {_toml_value(name)}"
            if label:
                block += f"\nlabel = {_toml_value(label)}"
            block += f"\n{kind} = {_inline(spec)}"
            out.append(block)
        for c in self._cells:
            out.append("\n" + c._render())
        for cell, pane_id, title, kind, spec in self._panes:
            block = f"\n[[pane]]\ncell = {_toml_value(cell)}"
            if pane_id:
                block += f"\nid = {_toml_value(pane_id)}"
            if title:
                block += f"\ntitle = {_toml_value(title)}"
            block += f"\n{kind} = {_inline(spec)}"
            out.append(block)
        return "\n".join(out) + "\n"

    def write(self, directory, filename="app.toml"):
        """Write the manifest and every source with attached data into `directory`.

        This directory is the deliverable. It is what you mount into the container, upload to
        a bucket, or commit — and it is self-contained, because a manifest's paths resolve
        against its own directory.
        """
        self.dir = pathlib.Path(directory)
        self.dir.mkdir(parents=True, exist_ok=True)
        for name, filename_, data in self._sources:
            if data is not None:
                n = write_source(data, self.dir / filename_)
                print(f"  {filename_}  {n} rows")
        self.path = self.dir / filename
        self.path.write_text(self.to_toml(), encoding="utf-8")
        print(f"  {filename}  {len(self._cells)} cells, {len(self._panes)} panes")
        return self.path

    # ── driving the binary ──
    def _require_written(self):
        """The manifest path, or a `DagpaneError` if `write()` has not been called yet."""
        if self.path is None:
            raise DagpaneError("call .write(directory) before running the app")
        return str(self.path)

    def _run(self, args, check=True):
        """Run the `dagpane` binary and return its stdout, raising `DagpaneError` on failure."""
        proc = subprocess.run([find_dagpane(), *args], capture_output=True, text=True)
        if check and proc.returncode != 0:
            raise DagpaneError((proc.stderr or proc.stdout).strip())
        return proc.stdout

    def check(self):
        """Compile the app. Raises `DagpaneError` carrying the message, which names a line."""
        out = self._run(["check", self._require_written()])
        print(out.strip())
        return self

    def graph(self, format="text"):
        """The dependency graph, before anything runs. `format` is text, mermaid or json."""
        out = self._run(["graph", self._require_written(), "--format", format])
        if format == "json":
            return json.loads(out)
        print(out.strip())
        return self

    def explain(self, _quiet=False, **sets):
        """Run one interaction and return what it cost, as a dict.

        This is the method worth having in a notebook. It turns "is this page going to be
        cheap to interact with" into something you assert on beside the analysis that
        produced it, before anyone opens a browser:

            cost = p.explain(channel="web")
            assert len(cost["panes_sent"]) < cost["panes_total"]
        """
        args = ["explain", self._require_written(), "--json"]
        for k, v in sets.items():
            if isinstance(v, bool):
                v = "true" if v else "false"
            args += ["--set", f"{k}={v}"]
        trace = json.loads(self._run(args))
        interaction = trace["interaction"]
        if not _quiet:
            ran = [s["cell"] for s in interaction["trace"]["steps"] if s["outcome"] == "evaluated"]
            print(
                f"{interaction['trace']['total_cells']} cells: "
                f"{len(ran)} ran, {len(interaction['untouched'])} never looked at | "
                f"{len(interaction['panes_sent'])} of {interaction['panes_total']} panes sent"
            )
        return interaction

    def serve(self, port=8787, host="127.0.0.1", background=False):
        """Serve the app.

        Blocks by default, which is usually what a notebook cell should do — you stop it with
        the interrupt button. `background=True` returns the `Popen` so a later cell can
        `.terminate()` it; remember that a kernel restart orphans it.

        There is NO AUTHENTICATION in this version. The default bind is loopback. Anything
        else is reachable by anyone who can route to it, so put an authenticating proxy in
        front before you widen it.
        """
        args = [find_dagpane(), "run", self._require_written(), "--port", str(port), "--host", host]
        if background:
            proc = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            print(f"dagpane serving in the background on http://{host}:{port} (pid {proc.pid})")
            return proc
        print(f"dagpane: http://{host}:{port} — interrupt the cell to stop")
        subprocess.run(args)
