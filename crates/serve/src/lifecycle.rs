//! Being one replica of however many: the states a supervisor can see, and the order they
//! change in.
//!
//! `dagpane run` is a person and a laptop — the process stops when the person stops it, and
//! nothing but that person is watching. A replica behind a load balancer is the other shape:
//! something else decides when it starts, when it stops, and whether traffic goes to it, and
//! it decides those by asking. This module is what it asks.
//!
//! # Why a drain is a sequence and not a flag
//!
//! The mistake that makes a rolling update visible to viewers is stopping in the wrong order.
//! A replica that closes its listener the moment it is signalled is still in the load
//! balancer's pool — the balancer finds out at its own pace, and every connection it routes
//! in the meantime is a reset. So the order here is:
//!
//! 1. **Go unready.** [`State::Draining`], and `/readyz` starts answering `503`. The listener
//!    is still open and in-flight work is untouched.
//! 2. **Wait.** Long enough for whatever is in front to have asked and been told. That is a
//!    property of the balancer, not of this process, so it is a number the operator sets.
//! 3. **Stop accepting**, and let what is in flight finish. This is the part `axum` already
//!    does; everything above is what has to happen before it.
//!
//! Skipping step 2 is the whole bug, and it is invisible in a test that stops one process.
//!
//! # Liveness is not readiness, and conflating them kills the pod
//!
//! [`State::Draining`] fails `/readyz` and **passes** `/healthz`. A liveness probe that went
//! red during a drain would have the supervisor conclude the process had hung and `SIGKILL`
//! it — turning a graceful stop into exactly the ungraceful one this module exists to avoid.
//! Liveness answers *is this process alive*; readiness answers *should it be sent work*.
//!
//! # Which signal
//!
//! Every supervisor that matters sends `SIGTERM`: `systemd` by default, Kubernetes on every
//! pod deletion, Docker on `stop`. This waits for that one **and** `SIGINT`, so Ctrl-C at a
//! terminal and a rolling update take the same path.
//!
//! That fixes a measured hang rather than a hypothetical one. The published image is
//! `scratch` plus a static binary, so the binary is **PID 1** — and PID 1 has no default
//! disposition to apply, so an unhandled `SIGTERM` is not a termination, it is *ignored*. A
//! pod that ignores `SIGTERM` runs until its termination grace period expires and `SIGKILL`
//! lands. `OPERATIONS.md` measured 11 s for a `podman stop` that should have taken
//! milliseconds, paid once per replica per rollout.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

/// Where a process is in its life, as a probe sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Accepting, and asking to be sent work.
    Serving,
    /// Asking **not** to be sent more, while still accepting and still finishing what it has.
    /// The window in which a load balancer is expected to notice.
    Draining,
    /// The app listener is closed and the process is on its way out.
    Stopped,
}

impl State {
    /// Whether a load balancer should send this replica a new connection.
    pub fn ready(self) -> bool {
        matches!(self, State::Serving)
    }
}

/// What a process says it is, when a probe asks.
///
/// A trait rather than a struct because the two shapes answer differently in kind. A single
/// app is **fixed at start-up** — it compiled one manifest and will serve that one until it
/// stops. A hosting process is not: apps arrive on first request, leave under budget pressure
/// or an idle sweep, and a manifest edited on disk replaces the one before it. Describing that
/// from a snapshot taken at start-up would report a fleet the process stopped holding.
///
/// So this is asked **per request**, and what it returns is what the process holds now.
pub trait Health: Send + Sync + std::fmt::Debug {
    /// A JSON object. `state` and `ready` are added by the probe and must not be set here.
    fn describe(&self) -> serde_json::Value;
}

/// What a replica says it is, so a rollout can check that its replicas agree.
///
/// `OPERATIONS.md` states the failure this exists to make visible: *a manifest's bytes are its
/// identity, so two replicas holding different bytes are serving two different apps under one
/// name.* Nothing in the process could previously be asked which bytes it held, so a rollout
/// that half-succeeded — some pods on the new manifest, some on the old — served two apps
/// under one hostname with no symptom but viewers disagreeing with each other.
///
/// [`Identity::fingerprint`] is [`dagpane_host::manifest_digest`], the same identity
/// `dagpane host` already routes and evicts by. One digest across every replica is the check;
/// two is a rollout that is not finished, or one that is stuck.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Identity {
    /// The app's name — the manifest's file stem, the same string a token's per-app claim
    /// names and `dagpane host` routes on.
    pub app: String,
    /// The digest of the manifest's bytes.
    pub fingerprint: String,
    /// How many cells the compiled graph has.
    pub cells: usize,
    /// How many panes it draws.
    pub panes: usize,
    /// The binary's version.
    pub version: &'static str,
}

impl Identity {
    /// Describe a compiled app. `app` is its name; `manifest` is the manifest's own bytes.
    pub fn of(app_name: impl Into<String>, manifest: &str, app: &dagpane_app::App) -> Identity {
        Identity {
            app: app_name.into(),
            fingerprint: dagpane_host::manifest_digest(manifest).to_string(),
            cells: app.graph.len(),
            panes: app.panes.len(),
            version: env!("CARGO_PKG_VERSION"),
        }
    }
}

impl Health for Identity {
    fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            // Which shape is answering. A probe pointed at the wrong one should be able to
            // tell, rather than finding the field it wanted missing.
            "kind": "app",
            "app": self.app,
            "fingerprint": self.fingerprint,
            "cells": self.cells,
            "panes": self.panes,
            "version": self.version,
        })
    }
}

/// What a **hosting** process says it is: the apps it is holding, right now.
///
/// The interesting field is `apps`, and it is interesting because it changes. Each entry is an
/// app id and the digest of the manifest bytes behind it — the same pair `dagpane host` routes
/// and evicts by — so the fleet question a rollout asks of one app ("do the replicas agree?")
/// can be asked of every app at once.
///
/// `resident_bytes` against `budget_bytes` is the other number worth watching, because it is
/// the one that decides what gets evicted next.
#[derive(Debug)]
pub struct Fleet {
    host: Arc<dagpane_host::Host>,
    version: &'static str,
}

impl Fleet {
    /// Describe this host.
    pub fn new(host: Arc<dagpane_host::Host>) -> Fleet {
        Fleet {
            host,
            version: env!("CARGO_PKG_VERSION"),
        }
    }
}

impl Health for Fleet {
    fn describe(&self) -> serde_json::Value {
        // ONE snapshot. `resident()` drops its read lock before returning and
        // `resident_bytes()` takes a fresh one, so asking both would let an admission or an
        // eviction land between them — and the probe would report a list of apps beside a
        // total that does not add up to it. Nothing is corrupted by that; it is just a payload
        // nobody can trust, which for a probe is the whole of its job.
        let resident = self.host.resident();
        let resident_bytes: u64 = resident.iter().map(|s| s.footprint.source_bytes).sum();
        let mut apps: Vec<serde_json::Value> = resident
            .iter()
            .map(|slot| {
                serde_json::json!({
                    "app": slot.key.app_id.as_str(),
                    "manifest": slot.key.manifest_digest.to_string(),
                    "bytes": slot.footprint.source_bytes,
                })
            })
            .collect();
        // Residency is a map, so its order is whatever the hash gave. Sorted, because a probe
        // output that reshuffles between two identical requests is one nobody can diff.
        apps.sort_by(|a, b| {
            (a["app"].as_str(), a["manifest"].as_str())
                .cmp(&(b["app"].as_str(), b["manifest"].as_str()))
        });
        serde_json::json!({
            "kind": "host",
            "apps": apps,
            "resident_bytes": resident_bytes,
            "budget_bytes": self.host.budget().max_bytes,
            "version": self.version,
        })
    }
}

/// The replica's state, and the drain that moves it.
///
/// One of these is shared by the app listener and the probe listener, which is what makes the
/// two agree: the probes report the same value the shutdown sequence is setting, rather than
/// a second copy of it that could be stale.
#[derive(Debug)]
pub struct Lifecycle {
    state: watch::Sender<State>,
    stop: watch::Sender<bool>,
    drain: Duration,
    health: Arc<dyn Health>,
}

impl Lifecycle {
    /// A replica that will pause `drain` between going unready and closing its listener.
    ///
    /// `drain` should exceed the load balancer's own interval times its unhealthy threshold —
    /// that product is how long the balancer may take to notice, and this is the window it has
    /// to notice in. Too short and the last few connections are reset; too long and a rollout
    /// is slow. Neither is silent, which is why it is a number and not a guess.
    pub fn new(identity: Identity, drain: Duration) -> Lifecycle {
        Lifecycle::describing(Arc::new(identity), drain)
    }

    /// [`Lifecycle::new`], for a process whose description changes while it runs — a
    /// [`Fleet`], or anything else implementing [`Health`].
    pub fn describing(health: Arc<dyn Health>, drain: Duration) -> Lifecycle {
        Lifecycle {
            state: watch::channel(State::Serving).0,
            stop: watch::channel(false).0,
            drain,
            health,
        }
    }

    /// What this process says it is, asked now.
    pub fn describe(&self) -> serde_json::Value {
        self.health.describe()
    }

    /// How long this replica will answer `/readyz` with `503` before it stops accepting.
    pub fn drain_for(&self) -> Duration {
        self.drain
    }

    /// Where it is now.
    pub fn state(&self) -> State {
        *self.state.borrow()
    }

    /// The future to hand [`axum::serve`]'s graceful shutdown: wait for a stop signal, then
    /// run the drain.
    pub async fn on_signal(&self) {
        tokio::select! {
            _ = stop_signal() => {}
            _ = self.requested() => {}
        }
        self.drain().await;
    }

    /// Ask this replica to stop, as a signal would.
    ///
    /// A signal is not the only thing that can decide a replica should go away, and a caller
    /// that has decided so takes **this** path rather than a shorter one of its own: the
    /// ordering in this module's docs is the part that matters, and a second way to stop
    /// would be a second place for it to be wrong.
    ///
    /// It is also how the ordering is tested. `kill(2)` is not the property worth asserting;
    /// what `/readyz` says while the listener is still open is.
    pub fn request_stop(&self) {
        self.stop.send_replace(true);
    }

    /// Resolve once [`Lifecycle::request_stop`] has been called — including if it already was
    /// before this was awaited, which is why it is a watch and not a notification.
    async fn requested(&self) {
        let mut rx = self.stop.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// The drain itself, from the moment something asked to stop.
    ///
    /// Separate from [`Lifecycle::on_signal`] so a test can drive the real sequence without a
    /// real signal — the ordering property in this module's docs is the part worth testing,
    /// and `kill(2)` is not.
    ///
    /// Returning is the caller's cue to stop accepting. It does **not** set
    /// [`State::Stopped`]: the listener is still open until `axum` says otherwise, and a probe
    /// answered before then should say `draining`, which is true, rather than `stopped`, which
    /// is not yet.
    pub async fn drain(&self) {
        // Unready first, and only then the wait. A replica that closed its listener here
        // would still be in the balancer's pool, and every connection routed to it between
        // now and the balancer's next health check would be a reset.
        self.state.send_if_modified(|s| {
            if *s == State::Serving {
                *s = State::Draining;
                true
            } else {
                // Only ever forwards. This is public, so "something asked to stop" can arrive
                // after the listeners have already closed — a supervisor sending a second
                // `SIGTERM` because the first appeared to do nothing, which is what a drain
                // looks like from outside. A replica that answered `draining` again after it
                // had stopped would have that supervisor waiting on a drain that was over.
                //
                // It is not what keeps the drain window to one window: `on_signal` calls this
                // exactly once, and a second signal arrives at a future nobody is awaiting.
                false
            }
        });
        tokio::time::sleep(self.drain).await;
    }

    /// Record that the app listener has closed.
    pub fn stopped(&self) {
        self.state.send_replace(State::Stopped);
    }

    /// Resolve once [`Lifecycle::stopped`] has been called.
    ///
    /// The probe listener shuts down on this rather than on the signal, because a supervisor
    /// has to be able to *see* the drain it asked for. Probes that died with the signal would
    /// make every drain indistinguishable from a crash.
    pub async fn until_stopped(&self) {
        let mut rx = self.state.subscribe();
        while *rx.borrow_and_update() != State::Stopped {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Wait for whichever of `SIGTERM` and `SIGINT` arrives first.
///
/// On a platform with no `SIGTERM` this is `SIGINT` alone, which is the behaviour every mode
/// of this binary had before there was a reason to care.
pub(crate) async fn stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            // Registering a handler can fail; losing the graceful stop entirely because of it
            // would be worse than losing one of the two signals.
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// The probe listener's two endpoints, and nothing else.
///
/// **A separate listener on purpose.** The app's port has four endpoints and no others, and
/// that is a claim worth keeping — a health surface on the same port is a fifth, it collides
/// with the renderer route's namespace, and it goes out through whatever ingress publishes
/// the app. The supervisor is not the public, so it gets its own door.
///
/// Bind it on an address the supervisor can reach and leave it out of the Service, the target
/// group, or the upstream. It carries no row of anybody's data — an app name, a manifest
/// digest, two counts and a state — but it is still not a page to publish.
pub fn admin_router(lifecycle: Arc<Lifecycle>) -> axum::Router {
    use axum::routing::get;
    axum::Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(lifecycle)
}

/// Liveness, and the replica's identity.
///
/// `200` for as long as the process is running, **including while it drains** — see this
/// module's docs on why a liveness probe that fails during a drain ends the drain with a
/// `SIGKILL`.
async fn healthz(
    axum::extract::State(lifecycle): axum::extract::State<Arc<Lifecycle>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let state = lifecycle.state();
    let mut body = lifecycle.describe();
    // Added here rather than by the describer, so that every shape reports its state the same
    // way and no implementation can disagree with the readiness the probe beside it reports.
    if let Some(fields) = body.as_object_mut() {
        fields.insert("state".into(), serde_json::json!(state));
        fields.insert("ready".into(), serde_json::json!(state.ready()));
    }
    axum::Json(body).into_response()
}

/// Readiness: whether to send this replica a new connection.
async fn readyz(
    axum::extract::State(lifecycle): axum::extract::State<Arc<Lifecycle>>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    let state = lifecycle.state();
    let status = if state.ready() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        axum::Json(serde_json::json!({ "state": state, "ready": state.ready() })),
    )
        .into_response()
}
