"""The fleet benchmark's app, in Streamlit.

Written the way Streamlit asks to be written, not as a dagpane app transliterated. The
benchmark is worthless otherwise: a rerun runtime made to imitate a reactive one measures
the imitation.

So, idiomatically:

  * the script is the app, top to bottom, and every widget interaction reruns all of it;
  * the CSV load is behind ``@st.cache_data``, because not caching it would be a strawman —
    caching the source read is the first thing the documentation tells you to do and every
    real Streamlit app does it;
  * the derived frames are plain pandas over the cached one. They are recomputed on every
    rerun, and that is not an oversight — it is the execution model. ``all_time_revenue``
    does not depend on the control and is recomputed anyway.

``st.cache_data`` on the *derived* frames as well would be a third design, somewhere between
the two runtimes. It is a real thing people do; it is also where Streamlit apps acquire
their cache-invalidation bugs. It is not done here because the point of this baseline is the
model Streamlit actually has by default, and a caveat in BENCHMARKS.md says so rather than
this file quietly choosing the flattering variant.
"""

from pathlib import Path

import pandas as pd
import streamlit as st

CSV = Path(__file__).parent / "sales.csv"

st.set_page_config(page_title="Fleet benchmark", layout="wide")


@st.cache_data
def load() -> pd.DataFrame:
    return pd.read_csv(CSV)


sales = load()

min_amount = st.number_input(
    "Minimum order value", min_value=0.0, max_value=800.0, value=0.0, step=25.0
)

filtered = sales[sales["amount"] >= min_amount]

revenue = float(filtered["amount"].sum())
order_count = int(len(filtered))
all_time_revenue = float(sales["amount"].sum())

by_region = (
    filtered.groupby("region", as_index=False)["amount"]
    .sum()
    .rename(columns={"amount": "revenue"})
    .sort_values("revenue", ascending=False)
)

st.title("Fleet benchmark")

a, b, c = st.columns(3)
a.metric("Revenue", f"{revenue:,.2f}")
b.metric("Orders", f"{order_count}")
c.metric("All-time revenue", f"{all_time_revenue:,.2f}")

st.subheader("Revenue by region")
st.dataframe(by_region, hide_index=True)
