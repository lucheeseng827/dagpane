# The two apps the decisive comparison in this directory is made of, generated into $WORK.
#
# Sourced by `run.sh` and by `busy.sh` rather than written twice. The rigs weigh a **held**
# viewer and a **busy** one, and putting their answers side by side only means something if
# both were weighing the same two apps — two copies of this file are two apps that stay the
# same right up until one of them is edited.
#
#   1. $WORK/rows/rows.toml — the bundled pipeline over 200 000 rows. Its cells KEEP rows.
#   2. $WORK/agg/agg.toml   — the same 200 000 rows, filtered and then aggregated. Nothing
#                             downstream of the source keeps a row.
#
# The schema is the bundled CSV's own, repeated with a unique id, on purpose: the pipeline has
# to be the *same pipeline*, or the comparison is about two apps rather than about two shapes.
#
# Expects: $WORK (a directory that already exists) and a working directory of the module root.

fixtures() {
  mkdir -p "$WORK/rows" "$WORK/agg"
  python3 - "$WORK" <<'PY'
import csv, sys
work = sys.argv[1]
src = list(csv.reader(open("examples/sales.csv")))
header, rows = src[0], src[1:]
with open(f"{work}/rows/sales.csv", "w", newline="") as f:
    w = csv.writer(f); w.writerow(header)
    for i in range(200_000):
        r = list(rows[i % len(rows)]); r[0] = str(i + 1); w.writerow(r)
PY
  cp examples/sales.toml "$WORK/rows/rows.toml"
  cp "$WORK/rows/sales.csv" "$WORK/agg/sales.csv"
  cat > "$WORK/agg/agg.toml" <<'TOML'
[app]
title = "Aggregates only"
[[source]]
name = "sales"
csv = "sales.csv"
[[input]]
name = "min_amount"
slider = { min = 0.0, max = 800.0, step = 25.0, default = 0.0 }
[[cell]]
name = "by_region"
from = "sales"
[[cell.step]]
filter = { column = "amount", op = "ge", param = "min_amount" }
[[cell.step]]
group_by = { by = ["region"], agg = [{ column = "amount", agg = "sum", as = "revenue" }] }
[[pane]]
cell = "by_region"
bar = { label_column = "region", value_column = "revenue" }
TOML
}
