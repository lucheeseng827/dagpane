//! What a host can now say about its viewers: which sessions are live, over which app, and what
//! each one holds beyond the sources it shares.
//!
//! The pair at the centre is the one `benches/sessions/` measured from outside as resident
//! memory — one CSV, a pipeline that keeps rows and one that aggregates them — counted here from
//! inside, per session. None of it changes what the budget admits or evicts; that stays a
//! decision written in source bytes, and the last test is about the gap that leaves.

use std::sync::Arc;
use std::time::Duration;

use dagpane_core::Value;
use dagpane_host::{AppId, Budget, Host, MemAppSource};

const ROWS: usize = 20_000;

fn write_csv(dir: &std::path::Path) {
    let mut text = String::from("id,region,amount\n");
    for i in 0..ROWS {
        text.push_str(&format!("{i},r{},{}.0\n", i % 4, (i % 97) * 10));
    }
    std::fs::write(dir.join("sales.csv"), text).unwrap();
}

/// Every row survives the default filter and is held in `kept`'s slot.
const ROW_KEEPING: &str = r#"
[app]
title = "keeps rows"
[[source]]
name = "sales"
csv = "sales.csv"
[[input]]
name = "min_amount"
slider = { min = 0.0, max = 1000.0, step = 10.0, default = 0.0 }
[[cell]]
name = "kept"
from = "sales"
[[cell.step]]
filter = { column = "amount", op = "ge", param = "min_amount" }
[[pane]]
cell = "kept"
table = { max_rows = 10 }
"#;

/// The same filter, then four rows: the filtered frame is an intermediate inside one cell and
/// is gone the moment the cell returns.
const AGGREGATING: &str = r#"
[app]
title = "aggregates"
[[source]]
name = "sales"
csv = "sales.csv"
[[input]]
name = "min_amount"
slider = { min = 0.0, max = 1000.0, step = 10.0, default = 0.0 }
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
"#;

struct Fixture {
    _dir: tempfile::TempDir,
    source: Arc<MemAppSource>,
    host: Host,
}

fn fixture(budget_bytes: u64) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    write_csv(dir.path());
    let source = Arc::new(MemAppSource::new());
    source.publish(
        "rows.test",
        &AppId::parse("rows").unwrap(),
        ROW_KEEPING.to_string(),
        dir.path(),
    );
    source.publish(
        "agg.test",
        &AppId::parse("agg").unwrap(),
        AGGREGATING.to_string(),
        dir.path(),
    );
    let host = Host::new(
        Arc::clone(&source) as Arc<dyn dagpane_host::AppSource>,
        Budget::new(budget_bytes, Duration::from_secs(3600)),
    );
    Fixture {
        _dir: dir,
        source,
        host,
    }
}

fn source_bytes(host: &Host, app: &str) -> u64 {
    host.resident()
        .iter()
        .find(|s| s.key.app_id.as_str() == app)
        .map(|s| s.footprint.source_bytes)
        .expect("resident")
}

#[test]
fn a_session_that_has_not_run_holds_nothing_of_its_own() {
    let f = fixture(64 << 20);
    let (session, lease) = f.host.session("rows.test").unwrap();
    // Its source slots are the app's own allocations, and nothing else has computed yet.
    assert_eq!(lease.record(&session).bytes, 0);
    assert!(
        source_bytes(&f.host, "rows") > 0,
        "the source itself is real and resident"
    );
}

#[test]
fn a_session_is_counted_while_its_lease_lives_and_not_after() {
    let f = fixture(64 << 20);
    let (mut session, lease) = f.host.session("rows.test").unwrap();
    session.refresh();
    let held = lease.record(&session).bytes;
    assert!(held > 0);

    let live = f.host.sessions();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].key.app_id.as_str(), "rows");
    assert_eq!(live[0].bytes, held);
    assert_eq!(f.host.session_bytes(), held);

    // Ending a connection is dropping its lease. Nothing else has to be said.
    drop(lease);
    assert!(f.host.sessions().is_empty());
    assert_eq!(f.host.session_bytes(), 0);
}

/// The measurement this crate was missing, on the data `benches/sessions/` used to find it.
/// Measured from outside, the row-keeping app cost 7 248 kB a session and the aggregating one
/// 145 kB on 200 000 rows — 50× apart on identical data. The same shape has to appear here or
/// one of the two instruments is wrong.
#[test]
fn a_row_keeping_session_holds_its_rows_and_an_aggregating_one_holds_four() {
    let f = fixture(64 << 20);
    let (mut rows, rows_lease) = f.host.session("rows.test").unwrap();
    let (mut agg, agg_lease) = f.host.session("agg.test").unwrap();
    rows.refresh();
    agg.refresh();
    let rows_held = rows_lease.record(&rows).bytes;
    let agg_held = agg_lease.record(&agg).bytes;
    let source = source_bytes(&f.host, "rows");
    assert_eq!(
        source,
        source_bytes(&f.host, "agg"),
        "one CSV, one source size"
    );

    // Every row survives the default filter, so the session holds the whole filtered frame —
    // and is billed for every byte of data in it, and not for the region dictionary, which
    // `take` kept rather than copied. Stated against the data, which is what both sides should
    // be: the source is the CSV loaded, and the session's frame is that again.
    let data = (ROWS * (8 + 4 + 8)) as u64; // id, region's dictionary key, amount

    // And the source is its data. The budget is summed from `memory_size`, which counts a
    // buffer's capacity, and the CSV reader's dictionary builder used to start at Arrow's
    // default and double — 56 652 bytes over the data here, all of it charged to the app. Now
    // it is sized from the reader's counts, and what is left over is the dictionary itself and
    // the arrays' own structs.
    assert!(
        source - data < 1024,
        "a {source}-byte source for {data} bytes of data: the budget is counting builder slack"
    );
    let kept = rows
        .get("kept")
        .unwrap()
        .value()
        .and_then(|v| v.as_frame())
        .unwrap()
        .memory_size() as u64;
    assert!(
        rows_held >= data,
        "billed {rows_held} for {data} bytes of rows it holds"
    );
    assert!(
        rows_held < kept,
        "the dictionary is shared, and {kept} counts it"
    );
    assert!(
        rows_held < source,
        "the source carries the dictionary the viewer shares: {rows_held} vs {source}"
    );
    // Four rows, two columns.
    assert!(
        agg_held * 100 < source,
        "aggregating session holds {agg_held} of a {source}-byte source"
    );
    eprintln!(
        "source {source} B · data {data} B · kept frame {kept} B · billed: keeps rows {rows_held} B, \
         aggregates {agg_held} B"
    );
}

#[test]
fn moving_a_control_moves_the_figure_at_the_next_record() {
    let f = fixture(64 << 20);
    let (mut session, lease) = f.host.session("rows.test").unwrap();
    session.refresh();
    let everything = lease.record(&session).bytes;

    // Keep roughly the top tenth of the amounts.
    session.set("min_amount", Value::float(870.0)).unwrap();
    session.commit();
    let a_tenth = lease.record(&session).bytes;
    assert!(
        a_tenth < everything / 5,
        "{a_tenth} after filtering, {everything} before"
    );
    assert_eq!(
        f.host.session_bytes(),
        a_tenth,
        "the host reads the latest record"
    );
}

#[test]
fn two_sessions_of_one_app_share_its_source_and_are_each_billed_for_their_own_rows() {
    let f = fixture(64 << 20);
    let (mut a, lease_a) = f.host.session("rows.test").unwrap();
    let (mut b, lease_b) = f.host.session("rows.test").unwrap();
    a.refresh();
    b.refresh();
    let held_a = lease_a.record(&a).bytes;
    let held_b = lease_b.record(&b).bytes;
    // One source in the budget, two viewers' rows in the report.
    assert_eq!(f.host.resident().len(), 1);
    assert_eq!(held_a, held_b, "same inputs, same rows");
    assert_eq!(f.host.session_bytes(), held_a + held_b);
}

/// The gap a budget written in source bytes leaves, made visible rather than closed. A
/// redeploy evicts the old version at once and the budget stops counting its sources, but a
/// viewer still connected holds that version's graph — and so its sources — until they leave.
/// Before this, nothing in the process could see that memory at all.
#[test]
fn an_evicted_version_still_held_by_a_viewer_is_still_reported() {
    let f = fixture(64 << 20);
    let (mut old, old_lease) = f.host.session("rows.test").unwrap();
    old.refresh();
    old_lease.record(&old);
    let old_key = old_lease.key().clone();
    let old_source = source_bytes(&f.host, "rows");

    // A new manifest under the same name: a new key, and the old one is evicted on first open.
    f.source.publish(
        "rows.test",
        &AppId::parse("rows").unwrap(),
        ROW_KEEPING.replace("keeps rows", "keeps rows, redeployed"),
        f._dir.path(),
    );
    let (_new, _new_lease) = f.host.session("rows.test").unwrap();
    assert!(
        f.host.resident().iter().all(|s| s.key != old_key),
        "the old version left residency"
    );

    let still_held = f
        .host
        .sessions()
        .into_iter()
        .find(|s| s.key == old_key)
        .expect("the viewer on the old version is still counted");
    assert_eq!(still_held.source_bytes, old_source);
    assert!(still_held.bytes > 0);

    drop(old_lease);
    assert!(f.host.sessions().iter().all(|s| s.key != old_key));
}

/// The same gap from the side a key cannot see. A version evicted and then opened again is
/// compiled again — a second copy of its sources, under the *same* key — while the viewer who
/// connected before the eviction is still holding the first. Matched by key, that viewer would
/// look like one more session of the resident copy, and the copy they hold would be reported
/// nowhere.
#[test]
fn a_viewer_on_an_earlier_admission_is_not_one_of_the_resident_copy() {
    let dir = tempfile::tempdir().unwrap();
    write_csv(dir.path());
    let source = Arc::new(MemAppSource::new());
    source.publish(
        "rows.test",
        &AppId::parse("rows").unwrap(),
        ROW_KEEPING.to_string(),
        dir.path(),
    );
    // Idle after no time at all, so a sweep takes whatever it finds.
    let host = Host::new(
        source as Arc<dyn dagpane_host::AppSource>,
        Budget::new(64 << 20, Duration::ZERO),
    );

    let (mut early, early_lease) = host.session("rows.test").unwrap();
    early.refresh();
    early_lease.record(&early);
    assert_eq!(host.sweep_idle().len(), 1, "the sweep took it");
    assert!(host.resident().is_empty());

    let (_late, _late_lease) = host.session("rows.test").unwrap();
    let resident = host.resident();
    assert_eq!(resident.len(), 1, "opened again");
    let slot = &resident[0];
    assert_eq!(
        &slot.key,
        early_lease.key(),
        "the same version, under the same key"
    );

    let live = host.sessions();
    assert_eq!(live.len(), 2);
    let (now, before): (Vec<_>, Vec<_>) = live.iter().partition(|s| s.admission == slot.admission);
    assert_eq!(
        now.len(),
        1,
        "only the viewer who opened it since is on the resident copy"
    );
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].key, slot.key);
    assert_eq!(
        before[0].source_bytes, slot.footprint.source_bytes,
        "the early viewer holds a second copy of the same sources"
    );
    assert!(before[0].bytes > 0);
}
