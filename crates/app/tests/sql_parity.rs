//! The exit criterion the roadmap sets for a SQL source: **an app over a Postgres table
//! behaves identically to the same app over a CSV export of it.**
//!
//! Identically means pane for pane, rendered — not "the row counts match". A source that
//! produced the right number of rows with a column widened differently, a null read as a
//! zero, or a text column sorted by a different collation would pass a row count and fail a
//! dashboard, and the rendered view is what a viewer actually sees.
//!
//! **Skipped unless `DAGPANE_TEST_PG_URL` names a database**, and skipped loudly:
//!
//! ```sh
//! DAGPANE_TEST_PG_URL=postgres://user@127.0.0.1:5432/postgres \
//!   cargo test -p dagpane-app --features sql --test sql_parity
//! ```

#![cfg(feature = "sql")]

use std::sync::Arc;

use dagpane_app::{App, AppSession};
use dagpane_core::Value;

const SKIP_NOTE: &str = "SKIPPED: set DAGPANE_TEST_PG_URL to check the sql source against \
     a real database";

/// The database to test against, or `None`.
///
/// **`DAGPANE_REQUIRE_PG=1` turns the skip into a failure.** CI sets it alongside the URL,
/// and the reason is the one this repository keeps coming back to: a skip reads as a pass. A
/// green tick over a test that never ran is worse than a red one, because nobody looks
/// again — so in the one environment where the database is guaranteed to be there, not
/// finding it is a build failure rather than a line in a log nobody reads.
fn dsn() -> Option<String> {
    match std::env::var("DAGPANE_TEST_PG_URL") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            assert!(
                std::env::var("DAGPANE_REQUIRE_PG").is_err(),
                "DAGPANE_REQUIRE_PG is set but DAGPANE_TEST_PG_URL is not: this environment \
                 promised a database and there is none, so these tests would have skipped \
                 and reported success"
            );
            eprintln!("{SKIP_NOTE}");
            None
        }
    }
}

/// The rows both halves of the test read. Deliberately awkward: a null in a numeric column,
/// a null in a text column, a negative, a zero, a float that is not representable exactly,
/// and two regions whose names order differently from their totals.
const ROWS: &[(i64, Option<&str>, Option<f64>, Option<bool>)] = &[
    (1, Some("north"), Some(10.5), Some(true)),
    (2, Some("south"), Some(-2.25), Some(false)),
    (3, Some("north"), None, None),
    (4, None, Some(0.0), Some(true)),
    (5, Some("east"), Some(1e-3), Some(false)),
    (6, Some("west"), Some(99999.125), None),
];

/// Seed a table of this test's rows. **One table per test**: these run concurrently, and a
/// shared fixture would make one test's `drop table` the other's flake — which is a real
/// failure that looks exactly like a bug in the source under test.
fn seed(dsn: &str, table: &str) {
    let mut client = postgres::Client::connect(dsn, postgres::NoTls).unwrap();
    client
        .batch_execute(&format!(
            "drop table if exists {table};
             create table {table} (
                 id int8 not null, region text, amount float8, ok bool
             );"
        ))
        .unwrap();
    for (id, region, amount, ok) in ROWS {
        client
            .execute(
                &format!("insert into {table} values ($1, $2, $3, $4)"),
                &[id, &region.map(str::to_string), amount, ok],
            )
            .unwrap();
    }
}

/// The same rows as a CSV, written the way an export would.
fn csv() -> String {
    let mut text = String::from("id,region,amount,ok\n");
    for (id, region, amount, ok) in ROWS {
        text.push_str(&format!(
            "{id},{},{},{}\n",
            region.unwrap_or(""),
            // Postgres renders a float8 with `float8out`; this has to match what the CSV
            // reader will parse back, so the values above are chosen to round-trip exactly
            // through both. A value that did not would be a finding about this runtime's
            // float handling rather than about the sources, and it would belong in its own
            // test.
            amount.map(|a| a.to_string()).unwrap_or_default(),
            ok.map(|b| b.to_string()).unwrap_or_default(),
        ));
    }
    text
}

/// An app manifest with every pane kind this runtime has, over whichever source.
fn manifest(source: &str) -> String {
    format!(
        r#"
[app]
title = "Parity"

{source}

[[input]]
name = "min_amount"
slider = {{ min = -10.0, max = 100.0, step = 0.5, default = -10.0 }}

[[cell]]
name = "filtered"
from = "sales"
[[cell.step]]
filter = {{ column = "amount", op = "ge", param = "min_amount" }}

[[cell]]
name = "by_region"
from = "filtered"
[[cell.step]]
group_by = {{ by = ["region"], agg = [
  {{ agg = "count", as = "orders" }},
  {{ column = "amount", agg = "sum", as = "revenue" }},
] }}
[[cell.step]]
sort = {{ column = "revenue", descending = true }}

[[cell]]
name = "revenue"
from = "by_region"
[[cell.step]]
group_by = {{ agg = [{{ column = "revenue", agg = "sum", as = "t" }}] }}
[[cell.step]]
scalar = {{ column = "t" }}

[[cell]]
name = "rows"
from = "filtered"
[[cell.step]]
count = true

[[cell]]
name = "top"
from = "filtered"
[[cell.step]]
sort = {{ column = "amount", descending = true }}
[[cell.step]]
select = ["id", "region", "amount", "ok"]

[[pane]]
cell = "revenue"
metric = {{ label = "Revenue", decimals = 3 }}

[[pane]]
cell = "rows"
metric = {{ label = "Rows" }}

[[pane]]
cell = "by_region"
title = "By region"
bar = {{ label_column = "region", value_column = "revenue" }}

[[pane]]
cell = "top"
title = "Every row"
table = {{ max_rows = 50 }}
"#
    )
}

/// Every pane's rendered view, as JSON — the bytes a browser would receive.
fn rendered(app: Arc<App>, set: Option<(&str, Value)>) -> serde_json::Value {
    let (mut session, _) = AppSession::open(app);
    if let Some((name, value)) = set {
        let mut values = std::collections::BTreeMap::new();
        values.insert(name.to_string(), value);
        session.set(&values).unwrap();
        session.commit();
    }
    serde_json::to_value(session.full_views()).unwrap()
}

#[test]
fn a_postgres_table_and_a_csv_export_of_it_render_the_same_panes() {
    let Some(dsn) = dsn() else { return };
    let table = "dagpane_parity_render";
    seed(&dsn, table);

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sales.csv"), csv()).unwrap();

    let csv_app = {
        let path = dir.path().join("csv.toml");
        std::fs::write(
            &path,
            manifest("[[source]]\nname = \"sales\"\ncsv = \"sales.csv\""),
        )
        .unwrap();
        Arc::new(dagpane_app::load(&path).expect("the csv app compiles"))
    };

    let sql_app = {
        let path = dir.path().join("sql.toml");
        std::fs::write(
            &path,
            manifest(&format!(
                "[[source]]\nname = \"sales\"\nsql = {{ dsn = \"{dsn}\", \
                 query = \"select id, region, amount, ok from {table} order by id\", \
                 watch = \"id\" }}"
            )),
        )
        .unwrap();
        Arc::new(dagpane_app::load(&path).expect("the sql app compiles"))
    };

    // The first render, before anybody touches a control.
    assert_eq!(
        rendered(csv_app.clone(), None),
        rendered(sql_app.clone(), None),
        "the two sources render different panes on the first paint"
    );

    // And after an interaction, because a filter is where a null or a float that parsed
    // differently would first show itself.
    for floor in [-10.0, 0.0, 1.0, 100.0] {
        assert_eq!(
            rendered(csv_app.clone(), Some(("min_amount", Value::float(floor)))),
            rendered(sql_app.clone(), Some(("min_amount", Value::float(floor)))),
            "the two sources diverge at min_amount = {floor}"
        );
    }
}

#[test]
fn the_sql_app_can_be_refreshed_and_an_unchanged_table_visits_nothing() {
    let Some(dsn) = dsn() else { return };
    let table = "dagpane_parity_refresh";
    seed(&dsn, table);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sql.toml");
    std::fs::write(
        &path,
        manifest(&format!(
            "[[source]]\nname = \"sales\"\nsql = {{ dsn = \"{dsn}\", \
             query = \"select id, region, amount, ok from {table} order by id\", \
             watch = \"id\" }}"
        )),
    )
    .unwrap();
    let app = dagpane_app::load(&path).unwrap();

    let mut session = dagpane_core::Session::new(app.graph.clone());
    session.refresh();
    let versions: std::collections::BTreeMap<_, _> = app
        .sources
        .iter()
        .map(|b| (b.cell.clone(), b.loaded_version))
        .collect();

    let result = dagpane_app::refresh::refresh(
        &app,
        &mut session,
        &versions,
        dagpane_app::refresh::RefreshOutcome::IfChanged,
        || Box::new(dagpane_core::frame::TableBuilder::new()),
    );
    assert_eq!(result.trace.visited(), 0, "an untouched table repainted");

    // Five, not six: `filtered` drops the row whose amount is null, because a null is not a
    // number and `>=` on one is not true. That is the behaviour the parity test above pins
    // across both sources, and it is worth naming here so the count below reads as arithmetic
    // rather than as a magic number.
    let before = session.get("rows").unwrap().value().unwrap().as_int();
    assert_eq!(before, Some(5));

    // Insert a row: the watched column's maximum moves, so the next refresh reads.
    let mut client = postgres::Client::connect(&dsn, postgres::NoTls).unwrap();
    client
        .execute(
            &format!("insert into {table} values (7, 'north', 5.0, true)"),
            &[],
        )
        .unwrap();

    let after = dagpane_app::refresh::refresh(
        &app,
        &mut session,
        &result.versions,
        dagpane_app::refresh::RefreshOutcome::IfChanged,
        || Box::new(dagpane_core::frame::TableBuilder::new()),
    );
    assert_eq!(after.trace.roots, vec!["sales".to_string()]);
    assert!(after.trace.visited() > 0);
    assert_eq!(
        session.get("rows").unwrap().value().unwrap().as_int(),
        Some(6),
        "the inserted row did not reach the pane"
    );
}
