//! The adversarial suite. Every test here is an invariant stated as a paragraph somewhere
//! else in this module; the point of the file is that none of them stays a paragraph.
//!
//! Two parties, one process, one port. What follows is what that is allowed to mean.

use std::sync::Arc;
use std::time::Duration;

use dagpane_core::Value;
use dagpane_host::{AppId, AppKey, Budget, EvictReason, Footprint, Host, HostError, MemAppSource};

/// A manifest with a CSV source, an input, a filtered cell and a metric. Parameterised by
/// title only, so two parties can deploy byte-identical apps — the case that makes sharing
/// tempting and sharing wrong.
fn manifest(title: &str, csv: &str) -> String {
    format!(
        r#"
[app]
title = "{title}"

[[source]]
name = "sales"
csv = "{csv}"

[[input]]
name = "min_amount"
label = "Minimum"
slider = {{ min = 0.0, max = 1000.0, step = 10.0, default = 0.0 }}

[[cell]]
name = "big"
from = "sales"
[[cell.step]]
filter = {{ column = "amount", op = "ge", param = "min_amount" }}

[[cell]]
name = "total"
from = "big"
[[cell.step]]
group_by = {{ agg = [{{ column = "amount", agg = "sum", as = "t" }}] }}
[[cell.step]]
scalar = {{ column = "t" }}

[[pane]]
cell = "total"
metric = {{ label = "Total" }}
"#
    )
}

fn write_csv(dir: &std::path::Path, name: &str, rows: usize) -> String {
    let mut text = String::from("region,amount\n");
    for i in 0..rows {
        text.push_str(&format!("r{},{}.0\n", i % 4, (i % 97) * 10));
    }
    std::fs::write(dir.join(name), text).unwrap();
    name.to_string()
}

fn budget() -> Budget {
    // Generous enough that nothing is evicted for size unless a test means it to be, and
    // long enough that nothing is idle unless a test sweeps with a zero budget of its own.
    Budget::new(64 * 1024 * 1024, Duration::from_secs(3600))
}

// ── the isolation invariant ───────────────────────────────────────────────────────────────

#[test]
fn two_parties_on_one_graph_do_not_observe_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let csv = write_csv(dir.path(), "sales.csv", 200);
    let text = manifest("Sales", &csv);

    // The same bytes, deployed twice under two names. This is the tempting case: one digest,
    // one compile, one `Arc<App>` — and two parties who must not be able to tell.
    let src = Arc::new(MemAppSource::new());
    let a = AppId::parse("acme").unwrap();
    let b = AppId::parse("globex").unwrap();
    src.publish("acme.example.com", &a, &text, dir.path());
    src.publish("globex.example.com", &b, &text, dir.path());

    let host = Host::new(src.clone(), budget());
    let mut session_a = host.session("acme.example.com").unwrap();
    let mut session_b = host.session("globex.example.com").unwrap();

    session_a.refresh();
    session_b.refresh();
    let before = session_a.get("total").unwrap().value().cloned();

    // B moves its slider all the way up: every row is filtered out and its total collapses.
    session_b.set("min_amount", Value::float(1000.0)).unwrap();
    session_b.commit();

    let after = session_a.get("total").unwrap().value().cloned();
    assert_eq!(
        before, after,
        "A's total moved when B set an input — the graph is shared, the session must not be"
    );
    assert_ne!(
        session_b.get("total").unwrap().value().cloned(),
        after,
        "B's own total did not move, so this test proved nothing"
    );
    assert_eq!(
        session_a.get("min_amount").unwrap().value().cloned(),
        Some(Value::float(0.0)),
        "A's input is A's"
    );
}

#[test]
fn one_arc_per_key_however_many_viewers_and_however_many_parties() {
    let dir = tempfile::tempdir().unwrap();
    let csv = write_csv(dir.path(), "sales.csv", 50);
    let text = manifest("Sales", &csv);

    let src = Arc::new(MemAppSource::new());
    let a = AppId::parse("acme").unwrap();
    src.publish("acme.example.com", &a, &text, dir.path());

    let host = Host::new(src.clone(), budget());
    let first = host.open("acme.example.com").unwrap();
    let viewers: Vec<_> = (0..50)
        .map(|_| host.open("ACME.example.com:8443").unwrap())
        .collect();

    for (i, v) in viewers.iter().enumerate() {
        assert!(
            Arc::ptr_eq(&first, v),
            "viewer {i} got a second copy of the graph"
        );
    }
    assert_eq!(
        src.fetches(),
        1,
        "the manifest was fetched more than once for one key"
    );
    assert_eq!(host.resident().len(), 1);
}

#[test]
fn two_parties_deploying_identical_bytes_still_get_two_slots() {
    // The mirror image of the test above, and the reason `AppKey` carries the app id at all.
    // One digest is not one app: the two deployments have separate lifetimes, separate
    // budgets and separate routes, and folding them together because their bytes match would
    // mean one party's redeploy evicting the other's running app.
    let dir = tempfile::tempdir().unwrap();
    let csv = write_csv(dir.path(), "sales.csv", 20);
    let text = manifest("Sales", &csv);

    let src = Arc::new(MemAppSource::new());
    let a = AppId::parse("acme").unwrap();
    let b = AppId::parse("globex").unwrap();
    src.publish("acme.example.com", &a, &text, dir.path());
    src.publish("globex.example.com", &b, &text, dir.path());

    let host = Host::new(src, budget());
    let app_a = host.open("acme.example.com").unwrap();
    let app_b = host.open("globex.example.com").unwrap();

    assert!(!Arc::ptr_eq(&app_a, &app_b));
    assert_eq!(host.resident().len(), 2);
}

// ── the key is the manifest ───────────────────────────────────────────────────────────────

#[test]
fn a_redeploy_evicts_the_graph_it_deployed_over() {
    let dir = tempfile::tempdir().unwrap();
    let csv = write_csv(dir.path(), "sales.csv", 20);

    let src = Arc::new(MemAppSource::new());
    let a = AppId::parse("acme").unwrap();
    src.publish("acme.example.com", &a, manifest("Before", &csv), dir.path());

    let host = Host::new(src.clone(), budget());
    let old = host.open("acme.example.com").unwrap();
    assert_eq!(old.title, "Before");
    let old_key = host.resident()[0].key.clone();

    src.publish("acme.example.com", &a, manifest("After", &csv), dir.path());
    let new = host.open("acme.example.com").unwrap();

    assert_eq!(new.title, "After");
    assert!(!Arc::ptr_eq(&old, &new));
    assert_eq!(
        host.resident().len(),
        1,
        "the old graph is still resident after a redeploy"
    );
    let log = host.evictions();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].key, old_key);
    assert_eq!(log[0].reason, EvictReason::Redeployed);
}

#[test]
fn a_rollback_is_a_key_change_and_not_a_recompile_of_something_new() {
    let dir = tempfile::tempdir().unwrap();
    let csv = write_csv(dir.path(), "sales.csv", 20);

    let src = Arc::new(MemAppSource::new());
    let a = AppId::parse("acme").unwrap();
    let v1 = manifest("v1", &csv);
    let v2 = manifest("v2", &csv);

    src.publish("acme.example.com", &a, &v1, dir.path());
    let host = Host::new(src.clone(), budget());
    let first = host.open("acme.example.com").unwrap();
    let key_v1 = host.resident()[0].key.clone();

    src.publish("acme.example.com", &a, &v2, dir.path());
    host.open("acme.example.com").unwrap();

    // Roll back: the same bytes as before, so the same key as before.
    src.publish("acme.example.com", &a, &v1, dir.path());
    let back = host.open("acme.example.com").unwrap();

    assert_eq!(back.title, "v1");
    assert_eq!(host.resident()[0].key, key_v1, "the key did not come back");
    assert_eq!(
        key_v1,
        AppKey::of_manifest(a.clone(), &v1),
        "the key is a function of the bytes, computable without the host"
    );
    // The graph is a fresh compile — the old one was evicted at the v2 deploy — but the key
    // it is filed under is the one the control plane asked for, which is the property a
    // rollback needs.
    assert!(!Arc::ptr_eq(&first, &back));
}

#[test]
fn a_source_that_answers_with_the_wrong_bytes_compiles_nothing() {
    /// A source that resolves one manifest and hands back another. A registry that has
    /// deployed between the two calls looks exactly like this, and so does one that is lying.
    #[derive(Debug)]
    struct Liar {
        key: AppKey,
        bytes: dagpane_host::ManifestBytes,
    }

    impl dagpane_host::AppSource for Liar {
        fn resolve(&self, _host: &str) -> Result<AppKey, HostError> {
            Ok(self.key.clone())
        }
        fn fetch(&self, _key: &AppKey) -> Result<dagpane_host::ManifestBytes, HostError> {
            Ok(self.bytes.clone())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let csv = write_csv(dir.path(), "sales.csv", 10);
    let a = AppId::parse("acme").unwrap();

    let host = Host::new(
        Arc::new(Liar {
            key: AppKey::of_manifest(a, &manifest("asked for", &csv)),
            bytes: dagpane_host::ManifestBytes::new(manifest("served", &csv), dir.path()),
        }),
        budget(),
    );

    match host.open("acme.example.com") {
        Err(HostError::DigestMismatch { .. }) => {}
        other => panic!("expected a digest mismatch, got {other:?}"),
    }
    assert!(
        host.resident().is_empty(),
        "nothing may be admitted when the bytes do not match the key"
    );
}

#[test]
fn nothing_routed_there_is_an_error_and_not_a_default_app() {
    let host = Host::new(Arc::new(MemAppSource::new()), budget());
    match host.open("nobody.example.com") {
        Err(HostError::UnknownHost(h)) => assert_eq!(h, "nobody.example.com"),
        other => panic!("expected UnknownHost, got {other:?}"),
    }
}

#[test]
fn a_manifest_that_does_not_compile_names_the_app_and_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let src = Arc::new(MemAppSource::new());
    let a = AppId::parse("acme").unwrap();
    src.publish(
        "acme.example.com",
        &a,
        "[app]\ntitle = \"broken\"\n\n[[pane]]\ncell = \"nope\"\nmetric = { label = \"x\" }\n",
        dir.path(),
    );

    let host = Host::new(src, budget());
    match host.open("acme.example.com") {
        Err(HostError::Manifest { key, reason }) => {
            assert_eq!(key.app_id.as_str(), "acme");
            assert!(
                reason.contains("nope"),
                "the reason must name the cell: {reason}"
            );
        }
        other => panic!("expected a manifest error, got {other:?}"),
    }
    assert!(host.resident().is_empty());
}

// ── the budget ────────────────────────────────────────────────────────────────────────────

#[test]
fn the_least_recently_opened_app_is_the_one_that_goes() {
    let dir = tempfile::tempdir().unwrap();
    let csv = write_csv(dir.path(), "sales.csv", 2_000);
    let text = manifest("Sales", &csv);

    // Measure one app, then set the budget to hold two of them and not three.
    let probe_src = Arc::new(MemAppSource::new());
    let probe_id = AppId::parse("probe").unwrap();
    probe_src.publish("probe.example.com", &probe_id, &text, dir.path());
    let probe = Host::new(probe_src, budget());
    let one = Footprint::of(&probe.open("probe.example.com").unwrap()).source_bytes;
    assert!(
        one > 0,
        "the probe app holds no data, so this test cannot measure eviction"
    );

    let src = Arc::new(MemAppSource::new());
    let ids: Vec<AppId> = ["one", "two", "three"]
        .iter()
        .map(|n| AppId::parse(n).unwrap())
        .collect();
    for (i, id) in ids.iter().enumerate() {
        // A distinct title per app, so the three are three keys rather than one.
        src.publish(
            &format!("{id}.example.com"),
            id,
            manifest(&format!("app {i}"), &csv),
            dir.path(),
        );
    }

    let host = Host::new(
        src,
        Budget::new(one * 2 + one / 2, Duration::from_secs(3600)),
    );
    host.open("one.example.com").unwrap();
    host.open("two.example.com").unwrap();
    // Touch `one` so `two` becomes the least recently opened.
    std::thread::sleep(Duration::from_millis(5));
    host.open("one.example.com").unwrap();
    std::thread::sleep(Duration::from_millis(5));
    host.open("three.example.com").unwrap();

    let resident: Vec<String> = host
        .resident()
        .iter()
        .map(|s| s.key.app_id.to_string())
        .collect();
    assert_eq!(resident.len(), 2, "resident: {resident:?}");
    assert!(
        resident.contains(&"three".to_string()),
        "resident: {resident:?}"
    );
    assert!(
        resident.contains(&"one".to_string()),
        "resident: {resident:?}"
    );

    let log = host.evictions();
    assert_eq!(log.len(), 1, "log: {log:?}");
    assert_eq!(log[0].key.app_id.as_str(), "two");
    assert_eq!(log[0].reason, EvictReason::OverBudget);
    assert!(host.resident_bytes() <= host.budget().max_bytes);
}

#[test]
fn an_app_larger_than_the_whole_budget_is_refused_and_evicts_nobody() {
    let dir = tempfile::tempdir().unwrap();
    let small_csv = write_csv(dir.path(), "small.csv", 10);
    let big_csv = write_csv(dir.path(), "big.csv", 5_000);

    let src = Arc::new(MemAppSource::new());
    let small = AppId::parse("small").unwrap();
    let big = AppId::parse("big").unwrap();
    src.publish(
        "small.example.com",
        &small,
        manifest("small", &small_csv),
        dir.path(),
    );
    src.publish(
        "big.example.com",
        &big,
        manifest("big", &big_csv),
        dir.path(),
    );

    // Room for the small app, nowhere near enough for the big one.
    let probe = Host::new(src.clone(), budget());
    let small_bytes = Footprint::of(&probe.open("small.example.com").unwrap()).source_bytes;
    let big_bytes = Footprint::of(&probe.open("big.example.com").unwrap()).source_bytes;
    assert!(big_bytes > small_bytes * 4, "{small_bytes} vs {big_bytes}");

    let host = Host::new(src, Budget::new(small_bytes * 2, Duration::from_secs(3600)));
    let kept = host.open("small.example.com").unwrap();

    match host.open("big.example.com") {
        Err(HostError::OverBudget { key, bytes, budget }) => {
            assert_eq!(key.app_id.as_str(), "big");
            assert_eq!(bytes, big_bytes);
            assert_eq!(budget, small_bytes * 2);
        }
        other => panic!("expected OverBudget, got {other:?}"),
    }

    assert_eq!(
        host.resident().len(),
        1,
        "the refusal evicted the app that fits"
    );
    assert!(Arc::ptr_eq(&kept, &host.open("small.example.com").unwrap()));
    assert!(host.evictions().is_empty(), "a refusal is not an eviction");
}

#[test]
fn an_idle_app_goes_only_when_something_sweeps() {
    let dir = tempfile::tempdir().unwrap();
    let csv = write_csv(dir.path(), "sales.csv", 20);

    let src = Arc::new(MemAppSource::new());
    let a = AppId::parse("acme").unwrap();
    src.publish("acme.example.com", &a, manifest("Sales", &csv), dir.path());

    // `idle_after` of zero: everything is idle the moment it is not being opened, which is
    // how this is testable without a clock to move.
    let host = Host::new(src.clone(), Budget::new(64 * 1024 * 1024, Duration::ZERO));
    let first = host.open("acme.example.com").unwrap();

    // Opening does not evict, however idle everything is. The request path never sweeps.
    let again = host.open("acme.example.com").unwrap();
    assert!(Arc::ptr_eq(&first, &again));
    assert_eq!(src.fetches(), 1);

    let went = host.sweep_idle();
    assert_eq!(went.len(), 1);
    assert_eq!(went[0].reason, EvictReason::Idle);
    assert!(host.resident().is_empty());

    let after = host.open("acme.example.com").unwrap();
    assert!(
        !Arc::ptr_eq(&first, &after),
        "the sweep did not drop the graph"
    );
    assert_eq!(src.fetches(), 2, "the reopen did not recompile");
}

#[test]
fn a_busy_host_never_sweeps_what_it_is_still_serving() {
    let dir = tempfile::tempdir().unwrap();
    let csv = write_csv(dir.path(), "sales.csv", 20);

    let src = Arc::new(MemAppSource::new());
    let a = AppId::parse("acme").unwrap();
    src.publish("acme.example.com", &a, manifest("Sales", &csv), dir.path());

    let host = Host::new(
        src,
        Budget::new(64 * 1024 * 1024, Duration::from_secs(3600)),
    );
    host.open("acme.example.com").unwrap();
    assert!(host.sweep_idle().is_empty());
    assert_eq!(host.resident().len(), 1);
}

// ── concurrency ───────────────────────────────────────────────────────────────────────────

#[test]
fn a_thundering_herd_of_first_opens_leaves_exactly_one_graph() {
    let dir = tempfile::tempdir().unwrap();
    let csv = write_csv(dir.path(), "sales.csv", 500);
    let text = manifest("Sales", &csv);

    let src = Arc::new(MemAppSource::new());
    let a = AppId::parse("acme").unwrap();
    src.publish("acme.example.com", &a, &text, dir.path());

    let host = Arc::new(Host::new(src, budget()));
    let apps: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let host = host.clone();
                s.spawn(move || host.open("acme.example.com").unwrap())
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Several threads may have compiled — the compile is deliberately outside the lock — but
    // only one graph can be resident, and every caller must be holding that one.
    assert_eq!(host.resident().len(), 1);
    let winner = host.open("acme.example.com").unwrap();
    for (i, app) in apps.iter().enumerate() {
        assert!(
            Arc::ptr_eq(&winner, app),
            "thread {i} kept a losing compile"
        );
    }
}
