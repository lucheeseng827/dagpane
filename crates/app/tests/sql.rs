//! SQL cells, and the one assertion that matters: **a statement and the pipeline it lowers to
//! are the same app.**
//!
//! `the_same_question_written_both_ways_gives_the_same_answer` is that, as a differential test
//! over eight queries. Everything else here is about the two things the ROADMAP said would
//! decide whether SQL could exist at all — that the edges are resolved rather than scanned,
//! and that what cannot be resolved is refused rather than guessed at.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use dagpane_app::{compile, manifest, App, AppSession, ManifestError, View};
use dagpane_core::Value;

const HEAD: &str = r#"
[app]
title = "t"
[[source]]
name = "sales"
csv = "sales.csv"
[[input]]
name = "floor"
slider = { min = 0.0, max = 800.0, step = 25.0, default = 0.0 }
[[input]]
name = "region"
select = { options = ["all", "north", "south", "east", "west"], default = "north" }
"#;

fn build(body: &str) -> Result<App, ManifestError> {
    let text = format!("{HEAD}{body}");
    let m = manifest::parse(&text).unwrap_or_else(|e| panic!("this fixture parses: {e}\n{text}"));
    compile(
        &m,
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples"),
    )
}

fn err(body: &str) -> ManifestError {
    build(body).expect_err("should not have compiled")
}

/// Every pane of a freshly opened app, rendered.
fn views(body: &str) -> Vec<View> {
    let app = Arc::new(build(body).expect("compiles"));
    let mut session = AppSession::open(Arc::clone(&app)).0;
    session.full_views().into_iter().map(|v| v.view).collect()
}

/// A cell shown as a table, the only pane kind that carries every column and row.
fn table(sql_or_steps: &str) -> String {
    let body = format!("{sql_or_steps}\n[[pane]]\ncell = \"out\"\ntable = {{ max_rows = 200 }}\n");
    match &views(&body)[0] {
        View::Table {
            head,
            rows,
            total_rows,
        } => {
            let names: Vec<&str> = head.iter().map(|h| h.name.as_str()).collect();
            let body: Vec<String> = rows
                .iter()
                .map(|r| r.iter().map(render).collect::<Vec<_>>().join("|"))
                .collect();
            format!(
                "{} rows of {}\n{}\n{}",
                rows.len(),
                total_rows,
                names.join("|"),
                body.join("\n")
            )
        }
        other => panic!("expected a table: {other:?}"),
    }
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => "—".to_string(),
        Value::Float { v } => format!("{v:.4}"),
        other => format!("{other:?}"),
    }
}

// ── the differential ───────────────────────────────────────────────────────────────────

#[test]
fn the_same_question_written_both_ways_gives_the_same_answer() {
    // The assertion ADR-0005 rests on. If a SQL cell could differ from the pipeline it
    // lowers to, it would be a second evaluator with a second set of null rules, and every
    // sentence this project writes about `transform`'s semantics would have an exception.
    let pairs: [(&str, &str); 8] = [
        (
            "select region, amount from sales where amount >= 400.0",
            r#"from = "sales"
[[cell.step]]
filter = { column = "amount", op = "ge", value = 400.0 }
[[cell.step]]
select = ["region", "amount"]"#,
        ),
        (
            "select region, sum(amount) as revenue from sales group by region order by revenue desc",
            r#"from = "sales"
[[cell.step]]
group_by = { by = ["region"], agg = [{ column = "amount", agg = "sum", as = "revenue" }] }
[[cell.step]]
sort = { column = "revenue", descending = true }
[[cell.step]]
select = ["region", "revenue"]"#,
        ),
        (
            "select region, count(*) as n from sales where region = 'north' group by region",
            r#"from = "sales"
[[cell.step]]
filter = { column = "region", op = "eq", value = "north" }
[[cell.step]]
group_by = { by = ["region"], agg = [{ agg = "count", as = "n" }] }
[[cell.step]]
select = ["region", "n"]"#,
        ),
        (
            "select order_id, amount * 0.9 as net from sales order by amount desc limit 5",
            r#"from = "sales"
[[cell.step]]
derive = { name = "net", expr = "amount * 0.9" }
[[cell.step]]
sort = { column = "amount", descending = true }
[[cell.step]]
select = ["order_id", "net"]
[[cell.step]]
limit = 5"#,
        ),
        (
            "select order_id, amount from sales where amount * 2 > 1400.0 limit 20",
            r#"from = "sales"
[[cell.step]]
derive = { name = "w", expr = "amount * 2 > 1400.0" }
[[cell.step]]
filter = { column = "w", op = "eq", value = true }
[[cell.step]]
select = ["order_id", "amount"]
[[cell.step]]
limit = 20"#,
        ),
        (
            "select region, avg(amount) as mean, min(amount) as low, max(amount) as high from sales group by region order by region",
            r#"from = "sales"
[[cell.step]]
group_by = { by = ["region"], agg = [
  { column = "amount", agg = "mean", as = "mean" },
  { column = "amount", agg = "min", as = "low" },
  { column = "amount", agg = "max", as = "high" },
] }
[[cell.step]]
sort = { column = "region" }
[[cell.step]]
select = ["region", "mean", "low", "high"]"#,
        ),
        (
            "select order_id, case when amount >= 500.0 then 'big' else 'small' end as band from sales limit 12",
            r#"from = "sales"
[[cell.step]]
derive = { name = "band", expr = "if(amount >= 500.0, 'big', 'small')" }
[[cell.step]]
select = ["order_id", "band"]
[[cell.step]]
limit = 12"#,
        ),
        (
            "select order_id, region from sales where region in ('north', 'south') and amount between 200.0 and 300.0",
            r#"from = "sales"
[[cell.step]]
derive = { name = "a", expr = "region == 'north' or region == 'south'" }
[[cell.step]]
filter = { column = "a", op = "eq", value = true }
[[cell.step]]
derive = { name = "b", expr = "amount >= 200.0 and amount <= 300.0" }
[[cell.step]]
filter = { column = "b", op = "eq", value = true }
[[cell.step]]
select = ["order_id", "region"]"#,
        ),
    ];

    for (sql, steps) in pairs {
        let from_sql = table(&format!(
            "[[cell]]\nname = \"out\"\nsql = \"\"\"{sql}\"\"\"\n"
        ));
        let by_hand = table(&format!("[[cell]]\nname = \"out\"\n{steps}\n"));
        assert_eq!(from_sql, by_hand, "\n{sql}\n");
        assert!(
            from_sql.lines().count() > 2,
            "the fixture returned nothing, so it asserts nothing: {sql}"
        );
    }
}

// ── the edges are resolved, not scanned ────────────────────────────────────────────────

fn edges(app: &App, cell: &str) -> Vec<String> {
    let id = app.graph.id(cell).expect("declared");
    app.graph
        .inputs_of(id)
        .iter()
        .map(|i| app.graph.name(*i).to_string())
        .collect()
}

#[test]
fn a_from_and_a_colon_parameter_are_edges_and_nothing_else_is() {
    let app = build(
        r#"
[[cell]]
name = "out"
sql = "select region, sum(amount) as revenue from sales where amount >= :floor group by region"
"#,
    )
    .expect("compiles");
    assert_eq!(edges(&app, "out"), vec!["sales", "floor"]);
}

#[test]
fn a_table_name_inside_a_comment_is_not_an_edge() {
    // The exact failure ROADMAP §6 named as the reason SQL was last: a regular expression
    // over this text finds `floor` and `other` and wires the cell to both. A parser does not
    // see them at all, because it never sees the comment.
    let app = build(
        r#"
[[cell]]
name = "out"
sql = """
  -- this used to read from other, and :floor was its threshold
  select region /* another mention of other and :floor */, amount
  from sales
"""
"#,
    )
    .expect("compiles");
    assert_eq!(edges(&app, "out"), vec!["sales"], "only the real `from`");
}

#[test]
fn a_table_name_inside_a_string_is_not_an_edge_either() {
    let app = build(
        r#"
[[cell]]
name = "out"
sql = "select order_id, 'from floor and :floor' as note from sales limit 1"
"#,
    )
    .expect("compiles");
    assert_eq!(edges(&app, "out"), vec!["sales"]);
}

#[test]
fn a_quoted_identifier_is_a_column_and_never_a_keyword() {
    // A column really called `from` would defeat any scanner; the lexer just sees a quoted
    // name. (The CSV has no such column, so this is about resolution, and the error proves
    // the name reached the column checker as a name.)
    let message = err(r#"
[[cell]]
name = "out"
sql = "select \"from\" from sales"
"#)
    .to_string();
    assert!(message.contains("no column `from`"), "{message}");
}

#[test]
fn an_unresolvable_parameter_is_the_same_error_a_filter_gives() {
    assert_eq!(
        err(r#"
[[cell]]
name = "out"
sql = "select region from sales where amount >= :flour"
"#),
        ManifestError::UnknownParam {
            cell: "out".into(),
            param: "flour".into()
        }
    );
}

#[test]
fn a_sql_cell_is_reactive_on_exactly_the_parameters_it_names() {
    let app = Arc::new(
        build(
            r#"
[[cell]]
name = "out"
sql = "select region, sum(amount) as revenue from sales where amount >= :floor group by region"
[[cell]]
name = "fixed"
sql = "select count(*) as n from sales"
[[pane]]
cell = "out"
table = {}
[[pane]]
cell = "fixed"
table = {}
"#,
        )
        .expect("compiles"),
    );
    let (mut s, _) = AppSession::open(Arc::clone(&app));
    s.full_views();
    let mut values = BTreeMap::new();
    values.insert("floor".to_string(), Value::float(400.0));
    s.set(&values).unwrap();
    let (trace, patch) = s.commit();
    let visited: Vec<&str> = trace.steps.iter().map(|st| st.cell.as_str()).collect();
    assert!(!visited.contains(&"fixed"), "{visited:?}");
    assert_eq!(patch.len(), 1);
    assert_eq!(patch[0].id, "out");
}

// ── what it refuses, and by name ───────────────────────────────────────────────────────

#[track_caller]
fn refuses(sql: &str, wanted: &str) {
    let message = err(&format!(
        "[[cell]]\nname = \"out\"\nsql = \"\"\"{sql}\"\"\"\n"
    ))
    .to_string();
    assert!(
        message.contains(wanted),
        "`{sql}`\n  wanted: {wanted}\n  got:    {message}"
    );
}

#[test]
fn everything_outside_the_dialect_is_refused_by_name() {
    // A closed grammar is only worth having if the refusals say what happened. Each of these
    // is valid SQL somewhere, and each one gets told which.
    refuses(
        "with x as (select 1) select * from x",
        "`with` clause is not supported",
    );
    refuses(
        "select region from sales union select region from sales",
        "`union` is not supported",
    );
    refuses(
        "select region, count(*) as n from sales group by region having n > 1",
        "`having` is not supported",
    );
    refuses(
        "select distinct region from sales",
        "`distinct` is not supported",
    );
    refuses(
        "select region, sum(amount) over () as t from sales",
        "window functions are not supported",
    );
    refuses(
        "select a.region from sales a right join sales b on a.region = b.region",
        "`right join` is not supported",
    );
    refuses(
        "select a.region from sales a full join sales b on a.region = b.region",
        "`full join` is not supported",
    );
    refuses(
        "select a.region from sales a cross join sales b",
        "`cross join` is not supported",
    );
    refuses(
        "select region from sales offset 5",
        "`offset` is not supported",
    );
    refuses(
        "select cast(amount as int) as a from sales",
        "`cast` is not supported",
    );
    refuses(
        "select region from sales where region like 'no%'",
        "`like` is not supported",
    );
    refuses("select region from sales, sales", "comma-separated `from`");
    refuses(
        "select count(amount) as n from sales",
        "counts non-null values",
    );
    refuses(
        "select region from sales where 1 < amount < 2",
        "comparisons do not chain",
    );
    refuses(
        "select region from sales order by region, amount",
        "`order by` takes one column",
    );
    refuses("update sales set amount = 1", "is not something it can do");
    refuses(
        "select uppper(region) as n from sales",
        "did you mean `upper`?",
    );
    refuses(
        "select table.* from sales table",
        "`table.*` is not supported",
    );
}

#[test]
fn a_select_list_item_that_is_computed_needs_a_name() {
    refuses("select amount * 2 from sales", "it needs a name");
    refuses("select sum(amount) from sales", "it needs a name");
    // And one that *is* exactly an aggregate names the group-by output, rather than
    // deriving a copy of a column called `__agg1`.
    let out =
        table("[[cell]]\nname = \"out\"\nsql = \"select sum(amount) as revenue from sales\"\n");
    assert!(out.contains("revenue"), "{out}");
    assert!(!out.contains("__agg"), "{out}");
}

#[test]
fn a_column_that_is_neither_grouped_nor_aggregated_is_refused() {
    // SQL engines differ on this and the lenient ones are wrong: there is no one `order_id`
    // per region, so there is no honest answer to print.
    refuses(
        "select region, order_id, sum(amount) as t from sales group by region",
        "`order_id` is neither grouped nor aggregated",
    );
}

#[test]
fn star_with_a_group_by_is_refused_rather_than_guessed_at() {
    refuses("select * from sales group by region", "do not go together");
}

#[test]
fn an_unknown_table_alias_lists_the_ones_there_are() {
    refuses(
        "select t.region from sales s",
        "`t` is not a table in this statement",
    );
}

#[test]
fn a_joins_on_has_to_say_which_side_is_which() {
    refuses(
        "select s.region from sales s join sales u on region = region",
        "qualify both sides",
    );
}

// ── errors point at the statement ──────────────────────────────────────────────────────

#[test]
fn an_error_points_at_the_line_and_column_it_is_about() {
    let message = err(r#"
[[cell]]
name = "out"
sql = """
  select region
  from sales
  where amont >= :floor
"""
"#)
    .to_string();
    assert!(message.contains("no column `amont`"), "{message}");
    assert!(message.contains("did you mean `amount`?"), "{message}");
}

#[test]
fn a_syntax_error_shows_the_line_with_a_caret_under_it() {
    let message = err(r#"
[[cell]]
name = "out"
sql = """
  select region
  from sales
  where region ~ 'north'
"""
"#)
    .to_string();
    let lines: Vec<&str> = message.lines().collect();
    let text = lines.iter().find(|l| l.contains("where region")).unwrap();
    let caret = lines.iter().find(|l| l.trim() == "^").unwrap();
    assert_eq!(caret.find('^'), text.find('~'), "\n{message}");
}

// ── the shape of the cell ──────────────────────────────────────────────────────────────

#[test]
fn a_cell_is_a_from_or_a_sql_and_never_both_or_neither() {
    let message =
        err("[[cell]]\nname = \"out\"\nfrom = \"sales\"\nsql = \"select * from sales\"\n")
            .to_string();
    assert!(message.contains("exactly one"), "{message}");

    let message = err("[[cell]]\nname = \"out\"\n").to_string();
    assert!(message.contains("`from`, `sql`"), "{message}");

    let message =
        err("[[cell]]\nname = \"out\"\nsql = \"select * from sales\"\n[[cell.step]]\nlimit = 1\n")
            .to_string();
    assert!(message.contains("takes no `[[cell.step]]`"), "{message}");
}

#[test]
fn a_sql_cell_reads_another_sql_cell_like_any_other() {
    let out = table(
        r#"
[[cell]]
name = "mid"
sql = "select region, sum(amount) as revenue from sales group by region"
[[cell]]
name = "out"
sql = "select region, revenue from mid where revenue > 30000.0 order by region"
"#,
    );
    assert!(out.contains("region|revenue"), "{out}");
}

// ── joins ──────────────────────────────────────────────────────────────────────────────

const WITH_TOTALS: &str = r#"
[[cell]]
name = "mid"
sql = "select region, sum(amount) as region_revenue from sales group by region"
"#;

#[test]
fn a_sql_join_names_both_cells_and_both_are_edges() {
    let app = build(&format!(
        r#"{WITH_TOTALS}
[[cell]]
name = "out"
sql = """
  select s.order_id, s.region, s.amount, m.region_revenue
  from sales s
  join mid m on s.region = m.region
"""
"#
    ))
    .expect("compiles");
    assert_eq!(edges(&app, "out"), vec!["sales", "mid"]);
}

#[test]
fn a_sql_join_lowers_to_the_join_verb_and_agrees_with_it() {
    let sql = format!(
        r#"{WITH_TOTALS}
[[cell]]
name = "out"
sql = """
  select s.order_id, s.region, m.region_revenue
  from sales s
  left join mid m on s.region = m.region
  order by s.order_id
  limit 5
"""
"#
    );
    let steps = format!(
        r#"{WITH_TOTALS}
[[cell]]
name = "out"
from = "sales"
[[cell.step]]
join = {{ with = "mid", on = "region", how = "left" }}
[[cell.step]]
sort = {{ column = "order_id" }}
[[cell.step]]
select = ["order_id", "region", "region_revenue"]
[[cell.step]]
limit = 5
"#
    );
    assert_eq!(table(&sql), table(&steps));
}

#[test]
fn left_semi_and_left_anti_are_the_two_filters_that_read_another_table() {
    let semi = table(&format!(
        r#"{WITH_TOTALS}
[[cell]]
name = "out"
sql = """
  select s.order_id
  from sales s
  left semi join mid m on s.region = m.region
  limit 3
"""
"#
    ));
    assert!(semi.contains("order_id"), "{semi}");
    assert!(
        !semi.contains("region_revenue"),
        "a semi join carries nothing across: {semi}"
    );

    let anti = table(&format!(
        r#"{WITH_TOTALS}
[[cell]]
name = "out"
sql = """
  select s.order_id
  from sales s
  left anti join mid m on s.region = m.region
"""
"#
    ));
    // Every region of `sales` is in `mid`, because `mid` was grouped from `sales`.
    assert!(anti.starts_with("0 rows of 0"), "{anti}");
}

#[test]
fn a_sql_join_is_a_lookup_and_says_so_when_it_would_not_be() {
    // The verb's default, and a SQL cell has no word for meaning otherwise. `dupes` has many
    // rows per region, so this join would fan out; the message names the alternative that is
    // writeable from here.
    let app = Arc::new(
        build(
            r#"
[[cell]]
name = "dupes"
sql = "select region, order_id as tag from sales"
[[cell]]
name = "out"
sql = "select s.order_id from sales s join dupes d on s.region = d.region"
[[pane]]
cell = "out"
table = {}
"#,
        )
        .expect("compiles: row counts are not a schema"),
    );
    let mut session = AppSession::open(Arc::clone(&app)).0;
    let views = session.full_views();
    let View::Error { message, .. } = &views[0].view else {
        panic!("the pane reports the failure: {:?}", views[0].view)
    };
    assert!(
        message.contains("aggregate the right-hand side first"),
        "{message}"
    );
}

#[test]
fn a_column_on_both_sides_of_a_sql_join_is_refused() {
    let message = err(r#"
[[cell]]
name = "out"
sql = "select s.order_id from sales s join sales t on s.order_id = t.order_id"
"#)
    .to_string();
    assert!(message.contains("both tables have a column"), "{message}");
}

#[test]
fn star_after_a_join_is_the_columns_the_join_produces() {
    let out = table(&format!(
        r#"{WITH_TOTALS}
[[cell]]
name = "out"
sql = "select * from sales s join mid m on s.region = m.region limit 2"
"#
    ));
    let header = out.lines().nth(1).unwrap();
    assert_eq!(
        header, "order_id|day|region|channel|units|amount|region_revenue",
        "the key appears once and the right's column is appended"
    );
}

#[test]
fn star_over_a_cell_whose_columns_are_not_knowable_is_refused_by_name() {
    // `later` is declared below this cell, so its schema is not known while this one
    // compiles. A checker that guessed would be inventing columns.
    let message = err(r#"
[[cell]]
name = "out"
sql = "select * from later"
[[cell]]
name = "later"
from = "sales"
"#)
    .to_string();
    assert!(
        message.contains("not knowable before this app runs"),
        "{message}"
    );
    assert!(message.contains("name the columns instead"), "{message}");
}

// ── the bundled example ────────────────────────────────────────────────────────────────

#[test]
fn the_bundled_sql_example_keeps_the_team_that_never_deploys() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/apps/18-team-load.toml");
    let app = Arc::new(dagpane_app::load(&path).expect("the bundled example must compile"));
    let mut session = AppSession::open(Arc::clone(&app)).0;
    let views = session.full_views();

    let by_id = |id: &str| {
        views
            .iter()
            .find(|v| v.id == id)
            .unwrap_or_else(|| panic!("no pane `{id}`"))
            .view
            .clone()
    };

    // `left anti join` finds the team that ships work and has no deploy log.
    let View::Table { rows, .. } = by_id("no_deploys") else {
        panic!("a table")
    };
    let teams: Vec<&str> = rows.iter().filter_map(|r| r[0].as_text()).collect();
    assert_eq!(teams, vec!["ml"]);

    // And `left join` keeps it on the page with a gap rather than dropping it, which is the
    // difference between a risk register and a risk register that is quietly missing a row.
    let View::Table { head, rows, .. } = by_id("teams") else {
        panic!("a table")
    };
    let names: Vec<&str> = head.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "team",
            "items",
            "points",
            "incidents",
            "mean_cycle",
            "deploys",
            "mean_lead",
            "deploys_per_item"
        ],
        "the select list's order, not the group-by's"
    );
    let ml = rows
        .iter()
        .find(|r| r[0].as_text() == Some("ml"))
        .expect("`ml` is still on the page");
    assert!(matches!(ml[5], Value::Null), "no deploy count: {:?}", ml[5]);
    assert!(
        matches!(ml[7], Value::Null),
        "and no ratio, rather than a zero: {:?}",
        ml[7]
    );
    // `sum(case when kind = 'incident' then 1 else 0 end)` really counted incidents.
    assert!(ml[3].as_float().is_some_and(|n| n > 0.0), "{:?}", ml[3]);
}
