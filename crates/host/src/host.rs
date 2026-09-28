//! The map, its lifetime rules, and the one property everything else here serves: **one
//! compiled graph per [`AppKey`], however many viewers, and never one shared slot vector.**

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use dagpane_app::{manifest, App};
use dagpane_core::Session;

use crate::budget::{Budget, Footprint};
use crate::error::HostError;
use crate::key::{manifest_digest, AppKey};
use crate::source::{normalise_host, AppSource};

/// A compiled app and what the host knows about it.
///
/// `app` is an `Arc` and every session over this app holds a clone of it. That is the memory
/// story stated as a field: a hundred viewers of a 600-row app are a hundred slot vectors
/// over one table, and `Arc::ptr_eq` is what proves it. It has since been weighed too, and the
/// weighing is the part a budget needs: the sources are shared, the frames a session
/// **computes** are not, and [`Budget::max_bytes`] counts only the first of those. A
/// row-keeping app costs about its source again per viewer. `BENCHMARKS.md`.
#[derive(Debug)]
pub struct AppSlot {
    /// Which app and which manifest.
    pub key: AppKey,
    /// The compiled app. Shared with every session; never mutated.
    pub app: Arc<App>,
    /// What it costs to hold.
    pub footprint: Footprint,
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
        }
    }

    /// The budget this host was built with.
    pub fn budget(&self) -> Budget {
        self.budget
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

    /// A fresh session over the app routed at this host header.
    ///
    /// The whole per-connection story in one call: resolve, share the graph, own the slots.
    ///
    /// # Errors
    ///
    /// Any [`HostError`] [`Host::open`] can return.
    pub fn session(&self, host_header: &str) -> Result<Session, HostError> {
        let app = self.open(host_header)?;
        Ok(Session::new(app.graph.clone()))
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

        let slot = Arc::new(AppSlot {
            key: key.clone(),
            app: Arc::new(app),
            footprint,
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
