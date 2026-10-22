//! A refresh of a resident app's data, from the host's side: the slot the next viewer opens,
//! the budget it is spent against, and the figures its live viewers report.
//!
//! `crates/serve` owns the clock that decides when to read; everything here is what a read,
//! once made, does to the map — which is why it can be tested with nothing listening on
//! anything.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use dagpane_app::refresh::{read, RefreshOutcome};
use dagpane_app::{App, AppSession, Version};
use dagpane_core::frame::FrameBuilder;
use dagpane_host::{AppId, Budget, Host, HostError, MemAppSource};

const MANIFEST: &str = r#"
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

fn csv(rows: usize) -> String {
    let mut text = String::from("id,region,amount\n");
    for i in 0..rows {
        text.push_str(&format!("{i},r{},{}.0\n", i % 4, (i % 97) * 10));
    }
    text
}

/// The compile's own representation, so a refreshed app weighs what a compiled one would.
fn builder() -> Box<dyn FrameBuilder> {
    dagpane_app::source_builder()
}

struct Fixture {
    dir: tempfile::TempDir,
    source: Arc<MemAppSource>,
    host: Host,
}

fn fixture(rows: usize, budget_bytes: u64) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sales.csv"), csv(rows)).unwrap();
    let source = Arc::new(MemAppSource::new());
    source.publish(
        "sales.test",
        &AppId::parse("sales").unwrap(),
        MANIFEST.to_string(),
        dir.path(),
    );
    let host = Host::new(
        Arc::clone(&source) as Arc<dyn dagpane_host::AppSource>,
        Budget::new(budget_bytes, Duration::from_secs(3600)),
    );
    Fixture { dir, source, host }
}

/// Rewrite the CSV and read it back the way a schedule would, building the app over it.
fn refreshed(f: &Fixture, app: &App, rows: usize) -> Arc<App> {
    std::fs::write(f.dir.path().join("sales.csv"), csv(rows)).unwrap();
    let versions: BTreeMap<String, Version> = app
        .sources
        .iter()
        .map(|b| (b.cell.clone(), b.loaded_version))
        .collect();
    let reading = read(app, &versions, RefreshOutcome::Force, |_| true, builder);
    Arc::new(
        app.with_sources(&reading)
            .unwrap()
            .expect("more rows are new rows"),
    )
}

fn resident_bytes(host: &Host) -> u64 {
    host.resident_bytes()
}

#[test]
fn a_refresh_replaces_the_resident_slot_and_the_next_viewer_opens_on_it() {
    let f = fixture(2_000, 64 << 20);
    let (app, lease) = f.host.open_session("sales.test").unwrap();
    let before = resident_bytes(&f.host);

    let next = refreshed(&f, &app, 4_000);
    let reload = f
        .host
        .reload(lease.key(), lease.admission(), Arc::clone(&next))
        .unwrap();

    assert!(reload.resident);
    assert!(
        Arc::ptr_eq(&f.host.open("sales.test").unwrap(), &next),
        "the next viewer opens on the rows from before the refresh"
    );
    // The budget is spent against what the app holds now.
    assert_eq!(resident_bytes(&f.host), reload.source_bytes);
    assert!(
        reload.source_bytes > before,
        "twice the rows weigh the same"
    );
    // Same key, same admission: a refresh is not a redeploy, and a viewer on this admission
    // is still on the resident copy.
    let slot = &f.host.resident()[0];
    assert_eq!(slot.admission, lease.admission());
    assert_eq!(f.host.evictions(), Vec::new());
}

#[test]
fn a_lease_moved_with_its_session_is_weighed_against_the_new_rows() {
    let f = fixture(2_000, 64 << 20);
    let (app, mut lease) = f.host.open_session("sales.test").unwrap();
    // The viewer filters out everything — no amount reaches 1 000 — so what they hold is
    // nearly nothing beyond the source.
    let (mut session, _) = AppSession::open(Arc::clone(&app));
    session
        .set(&BTreeMap::from([(
            "min_amount".to_string(),
            dagpane_core::Value::float(1_000.0),
        )]))
        .unwrap();
    session.commit();
    let small = lease.record(session.session()).bytes;

    let next = refreshed(&f, &app, 4_000);
    let reload = f
        .host
        .reload(lease.key(), lease.admission(), Arc::clone(&next))
        .unwrap();
    session.rebase(Arc::clone(&reload.app)).unwrap();

    // Weighed against the rows it left, the session would be billed for the new source as
    // though it were its own — the whole refreshed table, per viewer.
    let stale = lease.record(session.session()).bytes;
    assert!(
        stale >= reload.source_bytes / 2,
        "against the old baseline the new rows should look like the viewer's: {stale}"
    );
    lease.rebase(Arc::clone(&reload.baseline), reload.source_bytes);
    let moved = lease.record(session.session()).bytes;
    assert!(
        moved < 1_024 && moved <= small + 64,
        "a viewer who keeps no rows is billed {moved} bytes after the refresh (was {small})"
    );
    assert_eq!(f.host.sessions()[0].source_bytes, reload.source_bytes);
}

#[test]
fn a_refresh_too_big_for_the_whole_budget_is_refused_and_the_old_rows_stay() {
    let f = fixture(500, 64 << 20);
    let (app, lease) = f.host.open_session("sales.test").unwrap();
    let held = resident_bytes(&f.host);
    let next = refreshed(&f, &app, 200_000);

    // A host whose budget fits the old rows and not the new.
    let tight = Host::new(
        Arc::clone(&f.source) as Arc<dyn dagpane_host::AppSource>,
        Budget::new(held * 4, Duration::from_secs(3600)),
    );
    std::fs::write(f.dir.path().join("sales.csv"), csv(500)).unwrap();
    let (old, tight_lease) = tight.open_session("sales.test").unwrap();
    drop(lease);

    let refused = tight
        .reload(tight_lease.key(), tight_lease.admission(), next)
        .unwrap_err();
    assert!(matches!(refused, HostError::OverBudget { .. }), "{refused}");
    assert!(
        Arc::ptr_eq(&tight.open("sales.test").unwrap(), &old),
        "a refused refresh replaced the slot anyway"
    );
}

#[test]
fn a_refresh_that_outgrows_the_room_left_evicts_the_least_recently_opened_other_app() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sales.csv"), csv(2_000)).unwrap();
    let source = Arc::new(MemAppSource::new());
    for name in ["sales", "other"] {
        source.publish(
            &format!("{name}.test"),
            &AppId::parse(name).unwrap(),
            MANIFEST.to_string(),
            dir.path(),
        );
    }
    let probe = Host::new(
        Arc::clone(&source) as Arc<dyn dagpane_host::AppSource>,
        Budget::new(u64::MAX, Duration::from_secs(3600)),
    );
    probe.open("sales.test").unwrap();
    let one = probe.resident_bytes();

    // Room for two apps of the old size and not for a refreshed one beside another.
    let host = Host::new(
        Arc::clone(&source) as Arc<dyn dagpane_host::AppSource>,
        Budget::new(one * 2 + one / 2, Duration::from_secs(3600)),
    );
    let (app, lease) = host.open_session("sales.test").unwrap();
    host.open("other.test").unwrap();

    std::fs::write(dir.path().join("sales.csv"), csv(4_000)).unwrap();
    let versions: BTreeMap<String, Version> = app
        .sources
        .iter()
        .map(|b| (b.cell.clone(), b.loaded_version))
        .collect();
    let reading = read(&app, &versions, RefreshOutcome::Force, |_| true, builder);
    let next = Arc::new(app.with_sources(&reading).unwrap().unwrap());
    host.reload(lease.key(), lease.admission(), next).unwrap();

    let left: Vec<String> = host
        .resident()
        .iter()
        .map(|s| s.key.app_id.as_str().to_string())
        .collect();
    assert_eq!(left, ["sales"], "the refreshed app is the one kept");
    assert_eq!(host.evictions().len(), 1);
}

#[test]
fn a_refresh_of_an_evicted_admission_replaces_nothing_and_still_reports() {
    let f = fixture(2_000, 64 << 20);
    let (app, lease) = f.host.open_session("sales.test").unwrap();
    // Gone from residency — a redeploy of the same app under new manifest bytes — while this
    // viewer still holds it.
    f.source.publish(
        "sales.test",
        &AppId::parse("sales").unwrap(),
        format!("{MANIFEST}\n# a comment is new manifest bytes\n"),
        f.dir.path(),
    );
    let redeployed = f.host.open("sales.test").unwrap();

    let next = refreshed(&f, &app, 4_000);
    let reload = f
        .host
        .reload(lease.key(), lease.admission(), Arc::clone(&next))
        .unwrap();
    assert!(!reload.resident);
    assert!(
        Arc::ptr_eq(&f.host.open("sales.test").unwrap(), &redeployed),
        "a refresh of the old version replaced the new one"
    );
    assert!(reload.source_bytes > 0);
}
