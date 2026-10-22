//! The map, its lifetime rules, and the one property everything else here serves: **one
//! compiled graph per [`AppKey`], however many viewers, and never one shared slot vector.**

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Instant;

use dagpane_app::{manifest, App};
use dagpane_core::Session;

use crate::budget::{Budget, Footprint, SessionFootprint, SharedBaseline};
use crate::error::HostError;
use crate::key::{manifest_digest, AppKey, Domain};
use crate::source::{normalise_host, AppSource};

/// A compiled app and what the host knows about it.
///
/// `app` is an `Arc` and every session over this app holds a clone of it. That is the memory
/// story stated as a field: a hundred viewers of a 600-row app are a hundred slot vectors
/// over one table, and `Arc::ptr_eq` is what proves it. It has since been weighed too, and the
/// weighing is the part a budget needs: the sources are shared, the frames a session
/// **computes** are not, and [`Budget::max_bytes`] counts only the first of those. A
/// row-keeping app costs about its source again per viewer. `BENCHMARKS.md`. The second is
/// reported rather than budgeted: [`Host::sessions`].
#[derive(Debug)]
pub struct AppSlot {
    /// Which app and which manifest.
    pub key: AppKey,
    /// The compiled app. Shared with every session; never mutated.
    pub app: Arc<App>,
    /// What it costs to hold.
    pub footprint: Footprint,
    /// What every session of it shares — its sources' allocations — so that a session's own
    /// cost can be stated as what it holds *beyond* this. Computed once, at admission.
    pub baseline: Arc<SharedBaseline>,
    /// Which admission this is: an identity, unique over the host's life, and not a count.
    ///
    /// The key alone does not say which copy of an app a viewer is holding. A key evicted —
    /// idle, over budget, or deployed over and then rolled back to — and opened again is
    /// compiled again, with a second copy of its sources, while a viewer who connected before
    /// the eviction is still holding the first. [`LiveSession::admission`] is matched against
    /// this, so that viewer is not booked against the copy that happens to be resident now.
    pub admission: u64,
    /// When it was compiled.
    pub loaded_at: Instant,
    /// Milliseconds since the host started, at the last open. An integer rather than an
    /// `Instant` so a slot can be touched under a *read* lock on the map: the hot path is a
    /// hit, and taking a write lock to record that a hit happened would make the hot path
    /// the contended one.
    last_opened_ms: AtomicU64,
}

impl AppSlot {
    /// How long since this app was last opened, in milliseconds.
    pub fn idle_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.last_opened_ms.load(Ordering::Relaxed))
    }
}

/// Why an app stopped being resident.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvictReason {
    /// A new manifest was published for this app. The old key goes immediately and is not
    /// left to be found: a viewer who reconnects after a deploy must not be served the graph
    /// that was deployed over.
    Redeployed,
    /// The budget was exceeded and this was the least recently opened app.
    OverBudget,
    /// Nobody opened it within [`Budget::idle_after`], and a sweep ran.
    Idle,
}

impl std::fmt::Display for EvictReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            EvictReason::Redeployed => "redeployed",
            EvictReason::OverBudget => "over budget",
            EvictReason::Idle => "idle",
        })
    }
}

/// One eviction, as it is recorded.
///
/// "The budget is configuration and the eviction is logged" is a design decision that is only
/// true if there is something to read. This is it — a bounded in-memory log the process can
/// print, and the thing a test asserts on instead of inferring an eviction from a cache miss.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Eviction {
    /// Which app version left.
    pub key: AppKey,
    /// Why.
    pub reason: EvictReason,
    /// What it was holding.
    pub bytes: u64,
    /// How long it had gone unopened, in milliseconds.
    pub idle_ms: u64,
}

/// How many evictions the log keeps. Past this the oldest are dropped: an unbounded log in a
/// long-lived process is a memory leak with a good excuse.
const EVICTION_LOG_CAP: usize = 256;

/// Many apps, one process.
///
/// ```text
///   host header ──normalise──> AppSource::resolve ──> AppKey ──> hit?  ──> Arc<App>
///                                                                └─miss─> fetch, check the
///                                                                         digest, compile,
///                                                                         admit, evict
/// ```
///
/// # What this type promises
///
/// * **One `Arc<App>` per [`AppKey`].** Two viewers of one app, two tenants of one app, a
///   viewer and a scheduled refresh — all of them get the same pointer.
/// * **A session is per-connection and is never shared.** This type hands out `Arc<App>` and
///   [`Host::session`] builds a fresh [`Session`] over it. There is no API here that returns
///   a shared session, because a shared session is not a cache — it is a viewer reading
///   another viewer's inputs.
/// * **A redeploy is a new key, and the old one is evicted at once.** Not left to expire.
/// * **Nothing is admitted that the source cannot prove.** The manifest bytes are digested
///   and compared against the key before anything is compiled.
#[derive(Debug)]
pub struct Host {
    apps: RwLock<HashMap<AppKey, Arc<AppSlot>>>,
    source: Arc<dyn AppSource>,
    budget: Budget,
    started: Instant,
    evictions: Mutex<Vec<Eviction>>,
    /// Every session a connection holds, as a `Weak` to what it last reported. Weak so that a
    /// connection ending is the whole of the bookkeeping: its lease drops, the entry stops
    /// upgrading, and the next snapshot does not count it.
    leases: Mutex<Vec<Weak<LeaseCell>>>,
    /// The next [`AppSlot::admission`].
    admissions: AtomicU64,
}

impl Host {
    /// A host serving whatever `source` publishes, holding at most `budget`.
    pub fn new(source: Arc<dyn AppSource>, budget: Budget) -> Host {
        Host {
            apps: RwLock::new(HashMap::new()),
            source,
            budget,
            started: Instant::now(),
            evictions: Mutex::new(Vec::new()),
            leases: Mutex::new(Vec::new()),
            admissions: AtomicU64::new(0),
        }
    }

    /// The budget this host was built with.
    pub fn budget(&self) -> Budget {
        self.budget
    }

    /// What [`AppSource::domains`] says its source answers one label under: for explaining a
    /// refusal, never for routing.
    pub fn domains(&self) -> Vec<Domain> {
        self.source.domains()
    }

    /// The app a request for this host header wants, compiling it if it is not resident.
    ///
    /// The returned `Arc` is the *only* copy of this app in the process. Clone it, hold it
    /// for as long as the connection lives, and build a [`Session`] over it per connection.
    ///
    /// # Errors
    ///
    /// Any [`HostError`]: nothing routed, nothing to fetch, bytes that do not match the key,
    /// a manifest that does not compile, or an app larger than the whole budget.
    pub fn open(&self, host_header: &str) -> Result<Arc<App>, HostError> {
        let host = normalise_host(host_header);
        let key = self.source.resolve(&host)?;
        Ok(self.open_key(&key)?.app.clone())
    }

    /// A fresh session over the app routed at this host header, and the lease that tells the
    /// host it exists.
    ///
    /// The whole per-connection story in one call: resolve, share the graph, own the slots.
    /// The lease is returned rather than kept because the host cannot hold the session — it
    /// is the connection's — and so cannot weigh it by looking; see [`SessionLease`].
    ///
    /// # Errors
    ///
    /// Any [`HostError`] [`Host::open`] can return.
    pub fn session(&self, host_header: &str) -> Result<(Session, SessionLease), HostError> {
        let (app, lease) = self.open_session(host_header)?;
        Ok((Session::new(app.graph.clone()), lease))
    }

    /// The app routed at this host header, and a lease for one session over it, for a caller
    /// that builds the session itself — which is what `crates/serve` does, because its session
    /// type carries the app's panes as well as its slots.
    ///
    /// There is no way to get a session from this type without a lease, on purpose. A path
    /// that skipped it would be a viewer the process holds memory for and cannot see, which is
    /// the gap the lease was added to close.
    ///
    /// # Errors
    ///
    /// Any [`HostError`] [`Host::open`] can return.
    pub fn open_session(&self, host_header: &str) -> Result<(Arc<App>, SessionLease), HostError> {
        let host = normalise_host(host_header);
        let key = self.source.resolve(&host)?;
        let slot = self.open_key(&key)?;
        Ok((slot.app.clone(), self.lease(&slot)))
    }

    fn lease(&self, slot: &AppSlot) -> SessionLease {
        let cell = Arc::new(LeaseCell {
            key: slot.key.clone(),
            admission: slot.admission,
            source_bytes: AtomicU64::new(slot.footprint.source_bytes),
            bytes: AtomicU64::new(0),
        });
        let mut leases = self.leases.lock().expect("session leases");
        // Pruned on the way in, so the list is bounded by the sessions alive at the latest
        // connect rather than by every session that has ever lived.
        leases.retain(|weak| weak.strong_count() > 0);
        leases.push(Arc::downgrade(&cell));
        SessionLease {
            cell,
            baseline: Arc::clone(&slot.baseline),
        }
    }

    /// The slot for a key, loading it if it is not resident. The whole miss path lives here.
    fn open_key(&self, key: &AppKey) -> Result<Arc<AppSlot>, HostError> {
        let now = self.now_ms();

        // Hit path: a read lock, a hash lookup, an `Arc` clone, and a relaxed store. No
        // writer is woken and no allocation happens.
        if let Some(slot) = self.apps.read().expect("host lock").get(key) {
            slot.last_opened_ms.store(now, Ordering::Relaxed);
            return Ok(slot.clone());
        }

        // Miss path. Fetch and compile OUTSIDE the lock: compiling reads the app's CSVs, and
        // holding a write lock across a disk read would stall every other app's hit path for
        // the duration. The cost of doing it this way is that two simultaneous first opens of
        // one app can both compile; the loser's work is dropped below, which is a wasted
        // compile and never two resident graphs.
        let bytes = self.source.fetch(key)?;
        let found = manifest_digest(&bytes.text);
        if found != key.manifest_digest {
            return Err(HostError::DigestMismatch {
                key: key.clone(),
                found,
            });
        }
        let parsed = manifest::parse(&bytes.text).map_err(|e| HostError::Manifest {
            key: key.clone(),
            reason: e.to_string(),
        })?;
        let app = manifest::compile(&parsed, &bytes.base_dir).map_err(|e| HostError::Manifest {
            key: key.clone(),
            reason: e.to_string(),
        })?;
        let footprint = Footprint::of(&app);

        if footprint.source_bytes > self.budget.max_bytes {
            return Err(HostError::OverBudget {
                key: key.clone(),
                bytes: footprint.source_bytes,
                budget: self.budget.max_bytes,
            });
        }

        let baseline = Arc::new(SharedBaseline::of(&app));
        let slot = Arc::new(AppSlot {
            key: key.clone(),
            app: Arc::new(app),
            footprint,
            baseline,
            // Taken by every compile, including one that loses the race below and is dropped:
            // a number nobody ends up holding is harmless in an identity and would be wrong in
            // a count, which is why this is the former.
            admission: self.admissions.fetch_add(1, Ordering::Relaxed),
            loaded_at: Instant::now(),
            last_opened_ms: AtomicU64::new(now),
        });

        let mut evicted = Vec::new();
        let admitted = {
            let mut apps = self.apps.write().expect("host lock");

            // Somebody else won the race while we compiled. Theirs is already the one every
            // other session holds, so ours is dropped — one graph per key, always.
            if let Some(existing) = apps.get(key) {
                existing.last_opened_ms.store(now, Ordering::Relaxed);
                existing.clone()
            } else {
                // A different version of the same app is a redeploy: it goes now, before the
                // new one is admitted, so the budget never has to hold both.
                let stale: Vec<AppKey> = apps
                    .keys()
                    .filter(|k| k.app_id == key.app_id && k.manifest_digest != key.manifest_digest)
                    .cloned()
                    .collect();
                for k in stale {
                    if let Some(old) = apps.remove(&k) {
                        evicted.push(record(&old, EvictReason::Redeployed, now));
                    }
                }

                apps.insert(key.clone(), slot.clone());
                evict_to_budget(&mut apps, self.budget.max_bytes, key, now, &mut evicted);
                slot
            }
        };

        self.log(evicted);
        Ok(admitted)
    }

    /// Replace an admission's data with refreshed data, and say what its sessions now share.
    ///
    /// `app` is the admission's app over new rows — [`App::with_sources`] — and the caller is
    /// whatever owns the schedule. If that admission is still resident its slot is replaced,
    /// under the same key and the same admission, so the next viewer opens on the new rows and
    /// the budget is spent against what the app now holds rather than what it held when it was
    /// compiled; growing past the budget evicts other apps, least recently opened first, as an
    /// admission would. If it is not — evicted while somebody was still looking at it — there
    /// is no slot to replace, and the answer is still what the viewers who hold it need in
    /// order to keep their own figures straight.
    ///
    /// [`App::with_sources`]: dagpane_app::App::with_sources
    ///
    /// # Errors
    ///
    /// [`HostError::OverBudget`] when the refreshed sources would not fit in the whole budget
    /// on their own. Nothing is replaced: the admission keeps the rows it had, and so should
    /// its viewers — admitting the new ones would evict every other app and still not fit,
    /// which is the outage [`Host::open`] refuses for the same reason.
    pub fn reload(&self, key: &AppKey, admission: u64, app: Arc<App>) -> Result<Reload, HostError> {
        // Weighed before the lock: both walk the app's sources, and the hit path of every
        // other app waits on this lock.
        let footprint = Footprint::of(&app);
        let baseline = Arc::new(SharedBaseline::of(&app));
        let resident = {
            let mut apps = self.apps.write().expect("host lock");
            match apps.get(key) {
                Some(slot) if slot.admission == admission => {
                    if footprint.source_bytes > self.budget.max_bytes {
                        return Err(HostError::OverBudget {
                            key: key.clone(),
                            bytes: footprint.source_bytes,
                            budget: self.budget.max_bytes,
                        });
                    }
                    let now = self.now_ms();
                    let replaced = Arc::new(AppSlot {
                        key: key.clone(),
                        app: Arc::clone(&app),
                        footprint,
                        baseline: Arc::clone(&baseline),
                        admission,
                        loaded_at: slot.loaded_at,
                        last_opened_ms: AtomicU64::new(slot.last_opened_ms.load(Ordering::Relaxed)),
                    });
                    apps.insert(key.clone(), replaced);
                    let mut evicted = Vec::new();
                    evict_to_budget(&mut apps, self.budget.max_bytes, key, now, &mut evicted);
                    drop(apps);
                    self.log(evicted);
                    true
                }
                _ => false,
            }
        };
        Ok(Reload {
            app,
            baseline,
            source_bytes: footprint.source_bytes,
            resident,
        })
    }

    /// Drop every app nobody has opened within [`Budget::idle_after`]. Returns what went.
    ///
    /// Explicit rather than automatic, and never on the request path: the cost of evicting
    /// an app one request too early is a user waiting for a recompile, so the decision
    /// belongs to whatever owns the process's idle time and not to a viewer.
    pub fn sweep_idle(&self) -> Vec<Eviction> {
        let now = self.now_ms();
        let idle_ms = self.budget.idle_after.as_millis().min(u64::MAX as u128) as u64;

        let mut evicted = Vec::new();
        {
            let mut apps = self.apps.write().expect("host lock");
            let stale: Vec<AppKey> = apps
                .iter()
                .filter(|(_, slot)| slot.idle_ms(now) >= idle_ms)
                .map(|(k, _)| k.clone())
                .collect();
            for k in stale {
                if let Some(old) = apps.remove(&k) {
                    evicted.push(record(&old, EvictReason::Idle, now));
                }
            }
        }
        self.log(evicted.clone());
        evicted
    }

    /// Every resident app version, in no particular order.
    pub fn resident(&self) -> Vec<Arc<AppSlot>> {
        self.apps
            .read()
            .expect("host lock")
            .values()
            .cloned()
            .collect()
    }

    /// The sum of every resident app's source bytes. What the budget is spent against.
    pub fn resident_bytes(&self) -> u64 {
        self.apps
            .read()
            .expect("host lock")
            .values()
            .map(|s| s.footprint.source_bytes)
            .sum()
    }

    /// Every live session, in one snapshot.
    ///
    /// Includes sessions over apps that are no longer resident, and that is the most useful
    /// thing in it. Evicting an app removes it from the map and from [`Host::resident_bytes`],
    /// but a viewer still connected holds the app's graph and therefore its sources, so the
    /// memory the budget just counted as freed is still held until the last of them leaves.
    /// [`LiveSession::source_bytes`] is how much. That stays true when the same key is opened
    /// again and resident once more: the viewer is still on the earlier admission, and
    /// [`LiveSession::admission`] is how to tell.
    pub fn sessions(&self) -> Vec<LiveSession> {
        self.leases
            .lock()
            .expect("session leases")
            .iter()
            .filter_map(Weak::upgrade)
            .map(|cell| LiveSession {
                key: cell.key.clone(),
                admission: cell.admission,
                source_bytes: cell.source_bytes.load(Ordering::Relaxed),
                bytes: cell.bytes.load(Ordering::Relaxed),
            })
            .collect()
    }

    /// What every live session holds over its app, summed. Reported beside
    /// [`Host::resident_bytes`] and never added to it by the budget — see
    /// [`SessionFootprint`] for why.
    pub fn session_bytes(&self) -> u64 {
        self.sessions().iter().map(|s| s.bytes).sum()
    }

    /// The eviction log, oldest first, capped at the most recent 256.
    pub fn evictions(&self) -> Vec<Eviction> {
        self.evictions.lock().expect("eviction log").clone()
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis().min(u64::MAX as u128) as u64
    }

    fn log(&self, mut evicted: Vec<Eviction>) {
        if evicted.is_empty() {
            return;
        }
        let mut log = self.evictions.lock().expect("eviction log");
        log.append(&mut evicted);
        let overflow = log.len().saturating_sub(EVICTION_LOG_CAP);
        if overflow > 0 {
            log.drain(..overflow);
        }
    }
}

/// What a lease shares with the host: which app, what its sources weigh, and what the
/// session held at its last pass.
#[derive(Debug)]
struct LeaseCell {
    key: AppKey,
    admission: u64,
    /// What the admission's sources weigh. Moves when a refresh replaces them — see
    /// [`SessionLease::rebase`] — which is why it is atomic and not a plain number.
    source_bytes: AtomicU64,
    bytes: AtomicU64,
}

/// Held by a connection for as long as its session lives, and the only way the host learns
/// that the session exists.
///
/// The host hands sessions out and never holds one — a session is mutated on its
/// connection's task, and nothing else may touch it — so the host cannot weigh a session by
/// looking at it. The connection reports instead: [`SessionLease::record`] after each pass
/// stores what the session holds now, an atomic store with no lock on the interaction path.
/// Ending the connection drops the lease, and that is the whole of the bookkeeping; the host
/// holds only a `Weak`, so a lease nobody holds stops being counted without anybody having to
/// remember to say so.
#[derive(Debug)]
pub struct SessionLease {
    cell: Arc<LeaseCell>,
    baseline: Arc<SharedBaseline>,
}

impl SessionLease {
    /// Weigh `session` and store the figure where the host reads it.
    ///
    /// Call it after every pass. A refresh that runs no pass changes nothing the session
    /// holds and needs no call; a rejected batch stages nothing and needs none either.
    pub fn record(&self, session: &Session) -> SessionFootprint {
        let footprint = SessionFootprint::of(session, &self.baseline);
        self.cell.bytes.store(footprint.bytes, Ordering::Relaxed);
        footprint
    }

    /// Tell the lease its session moved onto refreshed data: what is shared is now `baseline` —
    /// [`Reload::baseline`], or [`SharedBaseline::of`] the new app — and the admission's sources
    /// weigh `source_bytes`. The session is weighed against those from here on.
    ///
    /// Without it, a session that moved onto new rows would be weighed against the baseline of
    /// the rows it left — and every viewer would be billed for the refreshed source as though
    /// it were theirs alone, which is the over-count this ledger exists not to make. Call it
    /// before the [`SessionLease::record`] that follows the move.
    pub fn rebase(&mut self, baseline: Arc<SharedBaseline>, source_bytes: u64) {
        self.baseline = baseline;
        self.cell
            .source_bytes
            .store(source_bytes, Ordering::Relaxed);
    }

    /// Which admission of [`SessionLease::key`] this session is over — what a schedule keyed
    /// by admission subscribes it under.
    pub fn admission(&self) -> u64 {
        self.cell.admission
    }

    /// What the session held at its last [`SessionLease::record`], or zero before the first.
    pub fn bytes(&self) -> u64 {
        self.cell.bytes.load(Ordering::Relaxed)
    }

    /// The app version this session is over.
    pub fn key(&self) -> &AppKey {
        &self.cell.key
    }
}

/// An admission over refreshed data, as [`Host::reload`] leaves it.
#[derive(Clone, Debug)]
pub struct Reload {
    /// The app over the new rows. Every session of the admission moves onto this one.
    pub app: Arc<App>,
    /// What its sessions now share — its sources' allocations — for [`SessionLease::rebase`].
    pub baseline: Arc<SharedBaseline>,
    /// What its sources weigh now.
    pub source_bytes: u64,
    /// Whether the admission was resident and its slot was replaced. `false` for one evicted
    /// while viewers held it: they still move onto the new rows, and the next viewer to open
    /// the app compiles a fresh admission as before.
    pub resident: bool,
}

/// One live session, as [`Host::sessions`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveSession {
    /// Which app version the session is over.
    pub key: AppKey,
    /// Which admission of that version: the [`AppSlot::admission`] it was opened on. Resident
    /// only while a slot under `key` carries the same number.
    pub admission: u64,
    /// What that admission's sources weigh. Counted by the budget while it is resident and not
    /// after — though a session still connected keeps those sources alive regardless.
    pub source_bytes: u64,
    /// What the session held over its app at its last pass.
    pub bytes: u64,
}

fn record(slot: &AppSlot, reason: EvictReason, now_ms: u64) -> Eviction {
    Eviction {
        key: slot.key.clone(),
        reason,
        bytes: slot.footprint.source_bytes,
        idle_ms: slot.idle_ms(now_ms),
    }
}

/// Evict least-recently-opened apps until the map fits, never touching `keep`.
///
/// `keep` is the app the caller is opening right now. Evicting it to make room for itself is
/// the degenerate case that would turn a single oversized app into an infinite reload loop;
/// [`Host::open_key`] has already refused anything that cannot fit on its own, so what is
/// left here always terminates with `keep` resident.
fn evict_to_budget(
    apps: &mut HashMap<AppKey, Arc<AppSlot>>,
    max_bytes: u64,
    keep: &AppKey,
    now_ms: u64,
    evicted: &mut Vec<Eviction>,
) {
    let mut total: u64 = apps.values().map(|s| s.footprint.source_bytes).sum();
    while total > max_bytes {
        let victim = apps
            .iter()
            .filter(|(k, _)| *k != keep)
            .max_by_key(|(_, s)| s.idle_ms(now_ms))
            .map(|(k, _)| k.clone());

        let Some(victim) = victim else { return };
        if let Some(old) = apps.remove(&victim) {
            total = total.saturating_sub(old.footprint.source_bytes);
            evicted.push(record(&old, EvictReason::OverBudget, now_ms));
        }
    }
}
