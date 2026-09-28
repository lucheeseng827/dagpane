"""The fleet benchmark's app, in marimo.

Written the way marimo asks to be written. marimo is the closer comparison of the two
Python baselines — it also builds a dependency graph and reruns only what is downstream —
so expressing it idiomatically matters more here, not less: this is the baseline that could
plausibly win.

marimo derives its graph from the names a cell *reads*, discovered by parsing the cell. So
the cells below are split the way a marimo author would split them — one idea per cell, so
the graph has something to work with — and ``all_time_revenue`` is deliberately in a cell
that reads ``sales`` and never ``min_amount``, which is exactly the edge dagpane declares in
its manifest. If marimo's analysis is right, that cell does not rerun when the control moves
either.
"""

import marimo

app = marimo.App(width="medium")


@app.cell
def _():
    from pathlib import Path

    import marimo as mo
    import pandas as pd

    return Path, mo, pd


@app.cell
def _(Path, pd):
    sales = pd.read_csv(Path(__file__).parent / "sales.csv")
    return (sales,)


@app.cell
def _(mo):
    min_amount = mo.ui.number(start=0.0, stop=800.0, value=0.0, label="Minimum order value")
    min_amount
    return (min_amount,)


@app.cell
def _(min_amount, sales):
    filtered = sales[sales["amount"] >= min_amount.value]
    return (filtered,)


@app.cell
def _(filtered):
    revenue = float(filtered["amount"].sum())
    order_count = int(len(filtered))
    return order_count, revenue


@app.cell
def _(sales):
    # Reads the source and not the filter, so marimo's graph should leave it alone when the
    # control moves — the same property dagpane's manifest declares.
    all_time_revenue = float(sales["amount"].sum())
    return (all_time_revenue,)


@app.cell
def _(filtered):
    by_region = (
        filtered.groupby("region", as_index=False)["amount"]
        .sum()
        .rename(columns={"amount": "revenue"})
        .sort_values("revenue", ascending=False)
    )
    return (by_region,)


@app.cell
def _(all_time_revenue, mo, order_count, revenue):
    mo.hstack(
        [
            mo.stat(label="Revenue", value=f"{revenue:,.2f}"),
            mo.stat(label="Orders", value=f"{order_count}"),
            mo.stat(label="All-time revenue", value=f"{all_time_revenue:,.2f}"),
        ]
    )
    return


@app.cell
def _(by_region, mo):
    mo.vstack([mo.md("### Revenue by region"), mo.ui.table(by_region, selection=None)])
    return


if __name__ == "__main__":
    app.run()
