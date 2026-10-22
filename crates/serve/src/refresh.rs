//! Scheduled refresh: the clock `refresh_secs` has been waiting for.
//!
//! A manifest has always been able to say `refresh_secs = 300` on a source, and until this
//! module nothing acted on it — `dagpane-app` has no clock, and `dagpane-host` is not allowed
//! one. This crate owns the only clock in the process, so the schedule lives here.
//!
//! # The shape
//!
//! ```text
//!   tick ──> read the sources that are due, once ──> App::with_sources ──> a new generation
//!                (version check first: an                (None when every       │
//!                 untouched file costs a stat)            row is the same)      │
//!                                                                               ▼
//!        every connection viewing that app ◀─── watch channel ◀──── Host::reload (fleet only)
//!          AppSession::rebase: one pass over what read the new rows, and a `reloaded`
//!          frame carrying only the panes that moved — nothing at all if none did
//! ```
//!
//! **Read once, weigh once, held once.** The sources are read once per tick however many
//! viewers there are; the new rows are weighed once, when the new graph is built; and every
//! viewer moves onto that one graph, so the rows from before the refresh are freed as soon as
//! the last of them has moved. Staging the new table on each session instead would hash it once
//! per viewer and leave every page that sat through a refresh pinning a copy of the old data.
//!
//! **A schedule runs while somebody is looking.** Under `dagpane host` that is the difference
//! between polling every source of every resident app — most of a fleet, most of the time, is
//! idle — and polling the ones on a screen. It is also what keeps a page on a wall current
//! after its app has gone idle and been evicted: the schedule follows the viewers, not the
//! residency map. The first viewer after a quiet spell is shown what was last read and checks
//! at once, so a stale page is corrected within one read rather than within one interval.
//!
//! **A refresh changes rows, never their shape**, and a refusal keeps the rows the app had —
//! see `dagpane_app::refresh::read`. Every refusal and every failure is printed: a dashboard
//! that has quietly stopped refreshing is the failure this module most needs to make visible.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use dagpane_app::refresh::{read, RefreshOutcome};
use dagpane_app::{App, Version};
use dagpane_auth::Viewer;
use dagpane_host::{AppKey, Footprint, Host, SharedBaseline};
use tokio::sync::watch;
use tokio::time::Instant;

/// One app's data as it stands now: what a connection opens on, and what it moves onto when the
/// data is refreshed.
#[derive(Debug)]
pub(crate) struct Generation {
    /// The app over the newest rows read.
    pub(crate) app: Arc<App>,
    /// What a host lease needs to move with its session. `None` outside `dagpane host`.
    pub(crate) lease: Option<Basis>,
}

/// One connection's place on its app's schedule: the generations it is moved onto, and the
/// token that bounds how long it may be.
///
/// **A connection is admitted once**, when it opens, and a token is good until its `exp`.
/// Before schedules, that also bounded everything a connection could see: new data meant a
/// restart, and a restart meant connecting again with a token that was valid then. New rows
/// are new data, so a connection that has outlived its token is closed rather than moved onto
/// them — when it opens ([`Feed::opening`]) and before each new generation after that
/// ([`Feed::expired`]) — and its page asks the viewer to sign in again. What the connection
/// already holds it keeps until then.
#[derive(Debug)]
pub(crate) struct Feed {
    pub(crate) generations: watch::Receiver<Arc<Generation>>,
    /// Who the front door let in. `None` with no front door, where nothing expires.
    viewer: Option<Viewer>,
    /// Which app, for the line an operator reads when a connection is closed.
    pub(crate) scope: Scope,
}

impl Feed {
    /// The viewer whose token has run out by `at`, if this connection has one.
    pub(crate) fn expired(&self, at: SystemTime) -> Option<&Viewer> {
        self.viewer.as_ref().filter(|v| v.expired(at))
    }

    /// What a connection that was handed `admitted` at the door opens on, once its socket is up.
    ///
    /// A refresh can land between the two, and a viewer who opened on the rows it replaced
    /// would be shown them until the one after — so the newest rows win. Admission was only a
    /// moment ago, but a moment is enough for a token to run out, and new rows go only to a
    /// token that still holds: the same rule every later refresh follows.
    pub(crate) fn opening(&mut self, admitted: &Arc<App>, at: SystemTime) -> Opening {
        let newest = Arc::clone(&self.generations.borrow_and_update());
        if Arc::ptr_eq(&newest.app, admitted) {
            return Opening::Admitted;
        }
        match self.expired(at) {
            Some(viewer) => Opening::Expired(viewer.subject.clone()),
            None => Opening::Newer(newest),
        }
    }
}

/// What a connection opens on — see [`Feed::opening`].
#[derive(Debug)]
pub(crate) enum Opening {
    /// The app the door handed it: nothing has been read since.
    Admitted,
    /// Rows a refresh read between the door and now, which it opens on instead.
    Newer(Arc<Generation>),
    /// Rows a refresh read between the door and now, and the token it was admitted on has run
    /// out since: it is closed instead. The subject, for the line an operator reads.
    Expired(String),
}

/// What every session of a fleet app shares, and what that weighs — for
/// `SessionLease::rebase`, so a viewer who moves onto new rows is not billed for them.
#[derive(Debug)]
pub(crate) struct Basis {
    pub(crate) baseline: Arc<SharedBaseline>,
    pub(crate) source_bytes: u64,
}

/// Which app a schedule keeps current.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Scope {
    /// The one app `dagpane run` and `dagpane serve` hold, by the name a token is checked
    /// against.
    Single(String),
    /// One admission of one app under `dagpane host`. By admission and not by key, for the
    /// reason `Host::sessions` gives: an app evicted and opened again is a second copy of its
    /// rows, and the viewers of each copy are kept current separately.
    Admission(AppKey, u64),
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scope::Single(name) if name.is_empty() => f.write_str("the app"),
            Scope::Single(name) => f.write_str(name),
            Scope::Admission(key, _) => write!(f, "{key}"),
        }
    }
}

#[derive(Debug)]
struct Live {
    tx: Arc<watch::Sender<Arc<Generation>>>,
    /// Whether a schedule task is running for it. A schedule stops when its last viewer leaves
    /// and the next viewer starts it again.
    running: bool,
    /// Kept when nobody is watching. `run` and `serve` hold their one app here, so the entry is
    /// the app; under `host` the slot is, and an entry nobody watches is dropped — which is
    /// also what lets an evicted admission's rows go once its last viewer has.
    keep: bool,
}

/// Every app being kept current, and the schedule for each.
#[derive(Debug)]
pub(crate) struct Schedules {
    live: Mutex<HashMap<Scope, Live>>,
    /// The fleet, under `dagpane host`: a refreshed admission replaces its resident slot so that
    /// the next viewer opens on the new rows and the budget is spent against them.
    host: Option<Arc<Host>>,
}

impl Schedules {
    /// For `run` and `serve`: one app, held here for the life of the process.
    pub(crate) fn single(app: Arc<App>, name: &str) -> Arc<Schedules> {
        let (tx, _) = watch::channel(Arc::new(Generation { app, lease: None }));
        let live = Live {
            tx: Arc::new(tx),
            running: false,
            keep: true,
        };
        Arc::new(Schedules {
            live: Mutex::new(HashMap::from([(Scope::Single(name.to_string()), live)])),
            host: None,
        })
    }

    /// For `host`: the fleet's apps, kept current while somebody is viewing them.
    pub(crate) fn hosted(host: Arc<Host>) -> Arc<Schedules> {
        Arc::new(Schedules {
            live: Mutex::new(HashMap::new()),
            host: Some(host),
        })
    }

    /// The newest generation of `run`'s or `serve`'s one app.
    pub(crate) fn current(&self, scope: &Scope) -> Option<Arc<Generation>> {
        let live = self.live.lock().expect("schedules");
        live.get(scope).map(|l| Arc::clone(&l.tx.borrow()))
    }

    /// Subscribe a connection that has just opened `opened` under `scope`, starting the
    /// schedule if it is not running. `viewer` is whoever the front door admitted, whose token
    /// bounds how long the connection is moved onto new rows — see [`Feed`].
    ///
    /// `None` for an app none of whose sources has a `refresh_secs`: there is nothing to keep
    /// current, so there is no channel to wait on and no task to run.
    ///
    /// The receiver's current value is the newest generation, which may be **newer than
    /// `opened`**: a refresh can land between a connection opening its app and subscribing
    /// here. The connection opens on the channel's generation whenever the two differ — see
    /// `connection` — which is right because the channel is never the one behind: a schedule
    /// tells its viewers before it replaces the resident slot. So a viewer arriving mid-refresh
    /// never starts on the rows the refresh replaced.
    pub(crate) fn subscribe(
        self: &Arc<Self>,
        scope: Scope,
        opened: &Arc<App>,
        viewer: Option<Viewer>,
    ) -> Option<Feed> {
        if !opened.sources.iter().any(|s| s.refresh_secs.is_some()) {
            return None;
        }
        let mut live = self.live.lock().expect("schedules");
        let entry = live.entry(scope.clone()).or_insert_with(|| {
            // Under `host`, the generation a schedule starts from says what its sessions share
            // as well, like every generation after it. A connection that subscribes late can
            // find a newer generation than the app it opened, and it moves its lease onto
            // whichever it opens on — so every generation has to be able to say.
            let lease = self.host.as_ref().map(|_| Basis {
                baseline: Arc::new(SharedBaseline::of(opened)),
                source_bytes: Footprint::of(opened).source_bytes,
            });
            let (tx, _) = watch::channel(Arc::new(Generation {
                app: Arc::clone(opened),
                lease,
            }));
            Live {
                tx: Arc::new(tx),
                running: false,
                keep: false,
            }
        });
        let generations = entry.tx.subscribe();
        if !entry.running {
            entry.running = true;
            tokio::spawn(schedule(
                Arc::clone(self),
                scope.clone(),
                Arc::clone(&entry.tx),
            ));
        }
        Some(Feed {
            generations,
            viewer,
            scope,
        })
    }

    /// Called by a schedule whose last viewer has gone. Stops it unless a viewer arrived in
    /// the meantime, and drops the entry unless it is the one app `run` holds.
    ///
    /// Under the same lock [`Schedules::subscribe`] takes, which is what makes the two agree:
    /// a viewer who subscribes after this has decided either finds the schedule still running
    /// or starts a new one.
    fn unwatched(&self, scope: &Scope) -> bool {
        let mut live = self.live.lock().expect("schedules");
        let Some(entry) = live.get_mut(scope) else {
            return true;
        };
        if entry.tx.receiver_count() > 0 {
            return false;
        }
        if entry.keep {
            entry.running = false;
        } else {
            live.remove(scope);
        }
        true
    }
}

/// One app's schedule, until nobody is watching it.
async fn schedule(
    schedules: Arc<Schedules>,
    scope: Scope,
    tx: Arc<watch::Sender<Arc<Generation>>>,
) {
    let start = Arc::clone(&tx.borrow().app);
    // What each source was last read as. Starts from the generation being shown, so the first
    // check compares against the rows on screen rather than against the compile's.
    let mut seen: BTreeMap<String, Version> = start
        .sources
        .iter()
        .map(|b| (b.cell.clone(), b.loaded_version))
        .collect();
    // Each source on its own interval, and every one due at once: whoever started this
    // schedule is looking at rows that may be as old as the last time anybody was.
    let now = Instant::now();
    let mut due: BTreeMap<String, (Duration, Instant)> = start
        .sources
        .iter()
        .filter_map(|b| {
            // A zero interval would be a busy loop against a database. One second is the
            // finest a manifest can mean, and the floor.
            b.refresh_secs
                .map(|s| (b.cell.clone(), (Duration::from_secs(s.max(1)), now)))
        })
        .collect();
    drop(start);

    loop {
        let Some(wake) = due.values().map(|(_, at)| *at).min() else {
            return;
        };
        tokio::select! {
            _ = tokio::time::sleep_until(wake) => {}
            _ = tx.closed() => {
                if schedules.unwatched(&scope) {
                    return;
                }
                continue;
            }
        }

        let now = Instant::now();
        let ready: BTreeSet<String> = due
            .iter()
            .filter(|(_, (_, at))| *at <= now)
            .map(|(cell, _)| cell.clone())
            .collect();
        for cell in &ready {
            if let Some((every, at)) = due.get_mut(cell) {
                *at = now + *every;
            }
        }

        let app = Arc::clone(&tx.borrow().app);
        // The ceiling a refreshed fleet app has to fit under on its own, as an admission does.
        let budget = schedules.host.as_ref().map(|h| h.budget().max_bytes);
        let previous = seen.clone();
        // Off the async workers: a read is a file, an HTTP request or a query, and building the
        // new graph hashes every row that came back. A connection's pass must not wait behind
        // either.
        let outcome = tokio::task::spawn_blocking(move || {
            let reading = read(
                &app,
                &previous,
                RefreshOutcome::IfChanged,
                |b| ready.contains(&b.cell),
                dagpane_app::source_builder,
            );
            let next = match app.with_sources(&reading) {
                Ok(Some(next)) => Arc::new(next),
                Ok(None) => return (reading, Ok(None)),
                Err(e) => return (reading, Err(e.to_string())),
            };
            // Under `host`, what a lease moving with its session needs, weighed here, off the
            // workers. Refused before anybody is told, if it could not fit the budget even alone.
            let lease = match budget {
                Some(max_bytes) => {
                    let source_bytes = Footprint::of(&next).source_bytes;
                    if source_bytes > max_bytes {
                        return (
                            reading,
                            Err(format!(
                                "its sources would hold {source_bytes} bytes and the budget is \
                                 {max_bytes}"
                            )),
                        );
                    }
                    Some(Basis {
                        baseline: Arc::new(SharedBaseline::of(&next)),
                        source_bytes,
                    })
                }
                None => None,
            };
            (reading, Ok(Some(Generation { app: next, lease })))
        })
        .await;
        let Ok((reading, next)) = outcome else {
            // The read panicked. Nothing was applied, and the next tick tries again.
            eprintln!("dagpane: refresh of {scope} failed unexpectedly; keeping the rows it had");
            continue;
        };

        // What was read counts as seen only if it was taken: applied, or identical to what is
        // already held. A generation refused — over the budget — keeps the versions it had, so
        // the next tick reads it, refuses it and says so again, exactly as a refusal inside
        // `read` does. Recording it would make one refusal the last anybody heard of it.
        if next.is_ok() {
            seen.extend(reading.versions.clone());
        }
        for (cell, error) in reading.failed() {
            eprintln!("dagpane: refresh of {scope}: {cell} — {error}; keeping the rows it had");
        }
        match next {
            Ok(Some(generation)) => {
                let read: Vec<String> = reading
                    .sources
                    .iter()
                    .filter(|(_, o)| o.reloaded())
                    .map(|(cell, o)| format!("{cell} {o}"))
                    .collect();
                println!(
                    "dagpane: refreshed {scope} — {}; {} viewer(s) moved onto it",
                    read.join(", "),
                    tx.receiver_count()
                );
                let generation = Arc::new(generation);
                // The viewers first and the resident slot second, and the order is the point. A
                // viewer who opens the app between the two finds the slot behind the channel,
                // and a connection moves forward onto the channel's generation; the other order
                // would leave a window in which the slot was ahead, and a viewer opening in it
                // would be moved back onto the rows the refresh replaced.
                tx.send_replace(Arc::clone(&generation));
                if let (Some(host), Scope::Admission(key, admission)) = (&schedules.host, &scope) {
                    if let Err(e) = host.reload(key, *admission, Arc::clone(&generation.app)) {
                        eprintln!(
                            "dagpane: refresh of {scope} reached its viewers and not the resident \
                             copy: {e}"
                        );
                    }
                }
            }
            // Read, and every row the same: nothing to tell anyone.
            Ok(None) => {}
            Err(reason) => {
                eprintln!("dagpane: refresh of {scope} refused: {reason}; keeping the rows it had");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use dagpane_app::{BytesSource, FileFormat, Source, SourceError, Sources};
    use dagpane_core::frame::{Frame, FrameBuilder};
    use dagpane_core::ColumnType;
    use dagpane_host::{AppId, Budget, MemAppSource};

    use super::*;

    const MANIFEST: &str = r#"
[app]
title = "growing"

[[source]]
name = "sales"
csv = "sales.csv"
refresh_secs = 1

[[cell]]
name = "rows"
from = "sales"
[[cell.step]]
count = true

[[pane]]
cell = "rows"
metric = { label = "Rows" }
"#;

    fn csv(rows: usize) -> String {
        let mut text = String::from("region,amount\n");
        for i in 0..rows {
            text.push_str(&format!("r{},{}.0\n", i % 4, i * 10));
        }
        text
    }

    /// A few rows until it grows, then more than the host's budget holds — and every read
    /// counted, which is the only way to see a schedule decide not to read.
    #[derive(Debug)]
    struct Growing {
        small: BytesSource,
        big: BytesSource,
        grown: AtomicBool,
        loads: AtomicUsize,
    }

    impl Growing {
        fn now(&self) -> &BytesSource {
            if self.grown.load(Ordering::SeqCst) {
                &self.big
            } else {
                &self.small
            }
        }
    }

    impl Source for Growing {
        fn describe(&self) -> String {
            "growing rows".to_string()
        }
        fn schema(&self) -> Result<Vec<(String, ColumnType)>, SourceError> {
            self.now().schema()
        }
        fn load(&self, into: Box<dyn FrameBuilder>) -> Result<Arc<dyn Frame>, SourceError> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            self.now().load(into)
        }
        fn version(&self) -> Result<Version, SourceError> {
            self.now().version()
        }
    }

    #[tokio::test]
    async fn a_refresh_refused_for_the_budget_is_read_and_refused_again_every_interval() {
        let growing = Arc::new(Growing {
            small: BytesSource::new("sales.csv", FileFormat::Csv, csv(10)),
            big: BytesSource::new("sales.csv", FileFormat::Csv, csv(20_000)),
            grown: AtomicBool::new(false),
            loads: AtomicUsize::new(0),
        });
        let sources: BTreeMap<String, Arc<dyn Source>> =
            BTreeMap::from([("sales".to_string(), Arc::clone(&growing) as Arc<dyn Source>)]);
        let manifest = dagpane_app::manifest::parse(MANIFEST).unwrap();
        let app = Arc::new(
            dagpane_app::compile_with(
                &manifest,
                Sources::Bound {
                    sources: &sources,
                    renderers: &BTreeMap::new(),
                },
            )
            .unwrap(),
        );
        // Room for the ten rows the app compiled with, and nowhere near twenty thousand.
        let small = Footprint::of(&app).source_bytes;
        let host = Arc::new(Host::new(
            Arc::new(MemAppSource::new()),
            Budget::new(small * 4, Duration::from_secs(3600)),
        ));
        let schedules = Schedules::hosted(host);

        growing.grown.store(true, Ordering::SeqCst);
        let before = growing.loads.load(Ordering::SeqCst);
        let key = AppKey::new(
            AppId::parse("sales").unwrap(),
            dagpane_host::manifest_digest(MANIFEST),
        );
        let viewer = schedules
            .subscribe(Scope::Admission(key, 0), &app, None)
            .expect("a scheduled source");
        // Ticks at about 0, 1 and 2 seconds: the first checks at once, as a first viewer's
        // schedule does, and each is refused.
        tokio::time::sleep(Duration::from_millis(2_800)).await;

        let reads = growing.loads.load(Ordering::SeqCst) - before;
        assert!(
            reads >= 3,
            "{reads} read(s) in three intervals: a refused refresh was recorded as seen, so \
             the schedule stopped reading it — and stopped saying it had refused it"
        );
        assert!(
            Arc::ptr_eq(&viewer.generations.borrow().app, &app),
            "a viewer was moved onto rows the budget refused"
        );
    }

    /// The app over ten rows, compiled again for each call: two calls are two generations.
    fn compiled() -> Arc<App> {
        let sources: BTreeMap<String, Arc<dyn Source>> = BTreeMap::from([(
            "sales".to_string(),
            Arc::new(BytesSource::new("sales.csv", FileFormat::Csv, csv(10))) as Arc<dyn Source>,
        )]);
        let manifest = dagpane_app::manifest::parse(MANIFEST).unwrap();
        Arc::new(
            dagpane_app::compile_with(
                &manifest,
                Sources::Bound {
                    sources: &sources,
                    renderers: &BTreeMap::new(),
                },
            )
            .unwrap(),
        )
    }

    /// Somebody the door let in on a token good for five minutes.
    fn alice() -> Viewer {
        use base64::Engine;
        use dagpane_auth::{Access, Algorithm, Jwks, Policy};
        use ring::rand::SystemRandom;
        use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};

        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        let point = pair.public_key().as_ref();
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let jwks = Jwks::parse(&format!(
            r#"{{"keys":[{{"kty":"EC","crv":"P-256","use":"sig","kid":"k1","x":"{}","y":"{}"}}]}}"#,
            b64.encode(&point[1..33]),
            b64.encode(&point[33..65]),
        ))
        .unwrap();
        let exp = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 300;
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some("k1".into());
        let token = jsonwebtoken::encode(
            &header,
            &serde_json::json!({
                "sub": "alice", "iss": "https://idp.test", "aud": "dagpane", "exp": exp,
            }),
            &jsonwebtoken::EncodingKey::from_ec_der(pkcs8.as_ref()),
        )
        .unwrap();
        Policy {
            issuer: "https://idp.test".into(),
            audience: "dagpane".into(),
            algorithms: BTreeSet::from([Algorithm::ES256]),
            leeway_seconds: 0,
            access: Access::AnyApp,
        }
        .admit(&jwks, &token, "door")
        .unwrap()
    }

    #[tokio::test]
    async fn rows_read_between_the_door_and_the_socket_go_only_to_a_token_that_still_holds() {
        // The window is the upgrade — milliseconds — so no socket test can aim at it; this is
        // the decision itself. Nothing below awaits, so the schedule `subscribe` starts never
        // runs, and every generation here is one the test published.
        let app = compiled();
        let schedules = Schedules::single(Arc::clone(&app), "door");
        let scope = Scope::Single("door".to_string());
        let mut feed = schedules
            .subscribe(scope.clone(), &app, Some(alice()))
            .expect("a scheduled source");
        let later = SystemTime::now() + Duration::from_secs(3_600);

        // Nothing read since the door: the connection opens on what it was handed, and a token
        // that has run out since changes nothing — what it was admitted to, it may see.
        assert!(matches!(
            feed.opening(&app, SystemTime::now()),
            Opening::Admitted
        ));
        assert!(matches!(feed.opening(&app, later), Opening::Admitted));

        // A refresh lands between the door and the socket being up.
        let newer = compiled();
        schedules.live.lock().unwrap()[&scope]
            .tx
            .send_replace(Arc::new(Generation {
                app: Arc::clone(&newer),
                lease: None,
            }));
        match feed.opening(&app, SystemTime::now()) {
            Opening::Newer(generation) => assert!(Arc::ptr_eq(&generation.app, &newer)),
            other => panic!("a token that holds opens on the newest rows, not {other:?}"),
        }
        match feed.opening(&app, later) {
            Opening::Expired(subject) => assert_eq!(subject, "alice"),
            other => panic!("a token that has run out was opened on new rows: {other:?}"),
        }

        // With no front door there is nothing to run out.
        let mut open = schedules.subscribe(scope, &app, None).unwrap();
        assert!(matches!(open.opening(&app, later), Opening::Newer(_)));
    }
}
