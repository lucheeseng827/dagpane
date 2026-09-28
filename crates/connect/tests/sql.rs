//! `SqlSource`, against a real Postgres.
//!
//! **Skipped unless `DAGPANE_TEST_PG_URL` names a database**, and skipped loudly — a test
//! that silently passes when its subject is absent is worse than no test, because the green
//! tick is the thing people read. CI supplies a service container; a developer supplies
//! whatever they have:
//!
//! ```sh
//! DAGPANE_TEST_PG_URL=postgres://user@127.0.0.1:5432/postgres \
//!   cargo test -p dagpane-connect --features sql --test sql
//! ```

#![cfg(feature = "sql")]

use dagpane_connect::{Source, SourceError, SqlSource};
use dagpane_core::frame::{FrameBuilder, TableBuilder};
use dagpane_core::ColumnType;

fn builder() -> Box<dyn FrameBuilder> {
    Box::new(TableBuilder::new())
}

const SKIP_NOTE: &str = "SKIPPED: set DAGPANE_TEST_PG_URL to run the sql source's tests \
     against a real database";

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

/// A table with one row per type this source maps, plus a null in every column.
fn seed(dsn: &str, table: &str) {
    let mut client = postgres::Client::connect(dsn, postgres::NoTls).unwrap();
    client
        .batch_execute(&format!(
            "drop table if exists {table};
             create table {table} (
                 id        int4    not null,
                 big       int8,
                 small     int2,
                 amount    float8,
                 ratio     float4,
                 ok        bool,
                 region    text,
                 code      varchar(8),
                 day       date,
                 seen      timestamptz,
                 updated   int8    not null
             );
             insert into {table} values
                 (1, 100, 7, 10.5, 0.5, true,  'north', 'n1', '2026-08-19', '2026-08-19T10:00:00Z', 1),
                 (2, 200, 8, 20.5, 1.5, false, 'south', 's1', '2026-08-20', '2026-08-20T10:00:00Z', 2),
                 (3, null, null, null, null, null, null, null, null, null, 3);"
        ))
        .unwrap();
}

#[test]
fn a_query_loads_with_every_type_this_runtime_maps() {
    let Some(dsn) = dsn() else { return };
    seed(&dsn, "dagpane_types");

    // `day` and `seen` are cast: a date is not text on the wire, and this source refuses to
    // pretend otherwise. The next test asserts the refusal.
    let source = SqlSource::new(
        &dsn,
        "select id, big, small, amount, ratio, ok, region, code, \
                day::text as day, seen::text as seen, updated \
         from dagpane_types order by id",
    )
    .unwrap();

    let schema = source.schema().unwrap();
    let by_name: std::collections::BTreeMap<_, _> = schema.iter().cloned().collect();
    assert_eq!(by_name["id"], ColumnType::Int);
    assert_eq!(by_name["big"], ColumnType::Int);
    assert_eq!(by_name["small"], ColumnType::Int, "int2 widens to int");
    assert_eq!(by_name["amount"], ColumnType::Float);
    assert_eq!(
        by_name["ratio"],
        ColumnType::Float,
        "float4 widens to float"
    );
    assert_eq!(by_name["ok"], ColumnType::Bool);
    assert_eq!(by_name["region"], ColumnType::Text);
    assert_eq!(by_name["code"], ColumnType::Text);
    // Cast in the query, not read raw. See the parity test below and `column_type`'s docs:
    // the binary protocol sends a `date` as a day offset, so "read it as text" fails at the
    // wire and the source says so rather than guessing.
    assert_eq!(by_name["day"], ColumnType::Text);
    assert_eq!(by_name["seen"], ColumnType::Text);

    let frame = source.load(builder()).unwrap();
    assert_eq!(frame.rows(), 3);

    let col = |name: &str| frame.column_index(name).unwrap();
    assert_eq!(frame.value_at(0, col("small")).as_int(), Some(7));
    assert_eq!(frame.value_at(1, col("ratio")).as_float(), Some(1.5));
    assert_eq!(frame.value_at(0, col("ok")).as_bool(), Some(true));
    assert_eq!(frame.value_at(0, col("region")).as_text(), Some("north"));
    assert_eq!(frame.value_at(0, col("day")).as_text(), Some("2026-08-19"));

    // Every nullable column's third row is null, and a null is never a zero — the
    // distinction every aggregate in this runtime depends on.
    for name in [
        "big", "small", "amount", "ratio", "ok", "region", "code", "day", "seen",
    ] {
        assert!(frame.is_null(2, col(name)), "{name} row 3 is not null");
    }
}

#[test]
fn an_empty_result_is_an_empty_frame_with_the_right_columns_and_not_an_error() {
    let Some(dsn) = dsn() else { return };
    seed(&dsn, "dagpane_empty");

    let source = SqlSource::new(&dsn, "select id, region from dagpane_empty where id < 0").unwrap();
    let frame = source.load(builder()).unwrap();

    assert_eq!(frame.rows(), 0);
    assert_eq!(
        frame.column_names(),
        vec!["id".to_string(), "region".to_string()],
        "an empty answer lost its schema, so a table pane would render as an error"
    );
}

#[test]
fn a_type_this_runtime_has_no_column_for_says_so_and_says_what_to_write() {
    let Some(dsn) = dsn() else { return };

    // `numeric`: rendering it as text would sort `9` after `10` in a bar chart and nobody
    // could see why. `date`: it is not text on the wire at all, whatever it looks like in
    // psql. Both are refused, and the message is the fix rather than a complaint.
    for (expression, column) in [
        ("1::numeric as price", "price"),
        ("'2026-08-19'::date as day", "day"),
        ("now() as seen", "seen"),
        ("gen_random_uuid() as id", "id"),
    ] {
        let source = SqlSource::new(&dsn, format!("select {expression}")).unwrap();

        // Both halves of the trait refuse, and refuse identically: a `schema` that accepted
        // a column `load` then choked on would make `dagpane check` pass and the app fail.
        for err in [
            source.schema().unwrap_err(),
            source.load(builder()).unwrap_err(),
        ] {
            assert!(
                matches!(err, SourceError::Unreadable { .. }),
                "{expression}: {err:?}"
            );
            assert!(err.to_string().contains(column), "{err}");
            assert!(
                err.to_string().contains(&format!("{column}::text")),
                "the message should paste back into the query: {err}"
            );
            assert!(!err.is_retryable(), "{expression}");
        }
    }
}

#[test]
fn the_version_is_of_the_query_and_not_of_a_table_it_mentions() {
    let Some(dsn) = dsn() else { return };
    seed(&dsn, "dagpane_version");

    let all = SqlSource::new(&dsn, "select * from dagpane_version").unwrap();
    let some = SqlSource::new(&dsn, "select * from dagpane_version where id < 3").unwrap();

    assert_eq!(all.version().unwrap(), all.version().unwrap());
    assert_ne!(
        all.version().unwrap(),
        some.version().unwrap(),
        "two queries over one table produced one version"
    );

    // Captured before the insert. Comparing the two queries to *each other* afterwards is
    // what this used to do, and it cannot fail: they already differ, so the assertion held
    // whatever the insert did and the message under it was describing a test that was not
    // running. A version only means anything against its own earlier value.
    let all_before = all.version().unwrap();
    let some_before = some.version().unwrap();

    let mut client = postgres::Client::connect(&dsn, postgres::NoTls).unwrap();
    client
        .execute(
            "insert into dagpane_version values (4,1,1,1,1,true,'e','e1',null,null,4)",
            &[],
        )
        .unwrap();

    assert_eq!(
        some.version().unwrap(),
        some_before,
        "`id < 3` excludes the new row, so the filtered query's version must not have moved"
    );
    assert_ne!(
        all.version().unwrap(),
        all_before,
        "the unfiltered query gained a row and its version did not move"
    );
}

#[test]
fn a_trailing_semicolon_is_accepted_and_then_actually_works() {
    let Some(dsn) = dsn() else { return };
    seed(&dsn, "dagpane_semi");

    // `is_read_only` strips a trailing `;` before it judges, so this is accepted. Everything
    // after that has to work too: `version` wraps the stored query in a subquery, and a
    // semicolon in the middle of one is a syntax error rather than a source that reloads.
    let source = SqlSource::new(&dsn, "select id, region, amount from dagpane_semi;").unwrap();
    let version = source.version().expect("a version, not a syntax error");
    assert_eq!(version, source.version().unwrap());
    assert_eq!(source.schema().unwrap().len(), 3);
    assert_eq!(source.load(builder()).unwrap().rows(), 3);

    // One terminator is a habit. A run of them is the typo the check exists for, and
    // `trim_end_matches` used to swallow the lot.
    assert!(SqlSource::new(&dsn, "select 1;;;").is_err());
    assert!(SqlSource::new(&dsn, "select 1; delete from dagpane_semi").is_err());
}

#[test]
fn a_row_count_alone_misses_an_update_and_a_watched_column_catches_it() {
    let Some(dsn) = dsn() else { return };
    seed(&dsn, "dagpane_watch");

    let blind = SqlSource::new(&dsn, "select id, updated from dagpane_watch").unwrap();
    let watching = SqlSource::new(&dsn, "select id, updated from dagpane_watch")
        .unwrap()
        .watching("updated");

    let blind_before = blind.version().unwrap();
    let watching_before = watching.version().unwrap();

    // An UPDATE: the row count does not move.
    let mut client = postgres::Client::connect(&dsn, postgres::NoTls).unwrap();
    client
        .execute("update dagpane_watch set updated = 99 where id = 1", &[])
        .unwrap();

    // The documented hole, asserted rather than described — this is what `watching` is for.
    assert_eq!(
        blind_before,
        blind.version().unwrap(),
        "a row count alone should NOT notice an update; if this now fails the docs are wrong"
    );
    assert_ne!(
        watching_before,
        watching.version().unwrap(),
        "a watched column did not notice an update"
    );
}

#[test]
fn a_statement_that_is_not_one_read_is_refused_before_a_connection_is_opened() {
    // No DSN needed and none used: the guard runs at construction, so a manifest with a typo
    // fails `dagpane check` rather than at the first refresh against a live database.
    for bad in [
        "delete from sales",
        "update sales set amount = 0",
        "select 1; drop table sales",
        "",
    ] {
        let err = SqlSource::new("postgres://nobody@127.0.0.1:1/none", bad).unwrap_err();
        assert!(
            matches!(err, SourceError::Misconfigured { .. }),
            "{bad}: {err:?}"
        );
        assert!(!err.is_retryable(), "{bad}");
    }
}

#[test]
fn a_database_that_is_not_there_is_unreachable_and_never_quotes_the_password() {
    let source =
        SqlSource::new("postgres://app:hunter2@127.0.0.1:1/none", "select 1 as n").unwrap();

    let err = source.load(builder()).unwrap_err();
    assert!(matches!(err, SourceError::Unreachable { .. }), "{err:?}");
    assert!(err.is_retryable());
    assert!(!err.to_string().contains("hunter2"), "{err}");
    assert!(err.to_string().contains("127.0.0.1:1/none"), "{err}");
}
