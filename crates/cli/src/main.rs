//! `dagpane` — the binary.
//!
//! Eight commands, and the ordering between them is deliberate. `check` and `graph` answer
//! questions about an app that has not run. `explain` answers the one question this project
//! exists to answer, in a terminal, with no browser in the loop. `refresh` answers the same
//! question about *data* moving rather than a control. `export` writes the app out with no
//! server at all.
//!
//! The last three are one app deployed three ways, and they are separate commands because
//! their defaults disagree about everything: `run` is the demo — loopback, a printed URL, and
//! a person to stop it; `serve` is one replica of however many — every interface, probes on a
//! listener of their own, and a drain that goes unready before it stops accepting; `host` is a
//! directory of apps on one port, routed by the `Host` header.
//!
//! Nothing is decided in this crate: it parses arguments, calls `dagpane-app`, and formats.
//! A number printed here is a number some test in `dagpane-core` already asserted.

#![forbid(unsafe_code)]

mod args;
mod auth;
mod explain;
mod export;
mod graph;
mod origin;
mod refresh;

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use dagpane_app::App;
use dagpane_host::{Budget, DirAppSource, Host};

use crate::args::{Cli, Command};

/// What the cut costs and what it buys, for an app that declared one.
///
/// Silent for an undivided app, which is most of them — a line saying "everything runs on the
/// server" on every `check` would be noise, and the absence of these lines is already the
/// answer.
///
/// The two numbers are the ones a deployment decision turns on: **the frontier** is what has
/// to be serialised whenever any of it moves, and **the controls answered without the
/// network** are what the split was for. An app with a wide frontier and no local controls
/// has been split the wrong way, and this is where that shows up rather than in a flame graph.
fn report_placement(app: &dagpane_app::App) {
    let cut = &app.cut;
    if !cut.is_split() {
        return;
    }
    let graph = &app.graph;
    println!(
        "  placement: {} cell(s) in the page, {} on the server",
        cut.client_cells(),
        graph.len() - cut.client_cells()
    );
    let frontier: Vec<&str> = cut.boundary().iter().map(|&id| graph.name(id)).collect();
    println!(
        "  frontier: {} cell(s) cross the wire{}{}",
        frontier.len(),
        if frontier.is_empty() { "" } else { " — " },
        frontier.join(", ")
    );

    let local: Vec<&str> = app
        .widgets
        .iter()
        .filter(|w| graph.id(&w.cell).is_some_and(|id| cut.is_local(graph, id)))
        .map(|w| w.cell.as_str())
        .collect();
    if local.is_empty() {
        // Worth saying out loud. A split whose every control still needs the server has
        // taken on a protocol and bought nothing with it.
        println!("  no control would be answered without the network");
    } else {
        println!(
            "  {} of {} control(s) would be answered without the network — {}",
            local.len(),
            app.widgets.len(),
            local.join(", ")
        );
    }
    // Said on every placed app, because the numbers above are about a cut that nothing
    // serves yet: `dagpane run` evaluates the whole graph here whatever `place` says. The
    // conditional mood two lines up is doing the same work, and this says why.
    println!("  note: the cut is checked and reported, not yet served — `run`, `serve` and");
    println!("        `export` evaluate every cell on this side. ROADMAP.md §4.");
}

/// Whether two listeners would fight over the same port.
///
/// Equality is not the test. `--bind 127.0.0.1 --port 8787 --admin-bind 0.0.0.0 --admin-port
/// 8787` is two *different* socket addresses that cannot both be bound: a wildcard covers every
/// address on its port, so the second `bind(2)` fails with `EADDRINUSE` — after the start-up
/// banner has already told an operator where both listeners are.
fn would_collide(a: std::net::SocketAddr, b: std::net::SocketAddr) -> bool {
    if a.port() != b.port() {
        return false;
    }
    // Same port: they collide unless they are on genuinely different addresses. A wildcard
    // covers all of them, and an address always covers itself.
    a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified()
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("dagpane: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Check { app } => {
            let compiled = load(&app)?;
            println!(
                "dagpane: {} — {} cells ({} inputs, {} computed), {} panes",
                compiled.title,
                compiled.graph.len(),
                compiled.widgets.len(),
                compiled.graph.len() - compiled.graph.sources().count(),
                compiled.panes.len()
            );
            // A source is a cell too, and a reader counting the first line will notice if
            // the arithmetic does not close. Say where the difference went.
            let data_sources = compiled.graph.sources().count() - compiled.widgets.len();
            if data_sources > 0 {
                println!(
                    "  {data_sources} data source(s) loaded at start-up and shared by every session"
                );
            }
            report_placement(&compiled);
            println!("  ok");
            Ok(())
        }

        Command::Graph { app, format } => {
            let compiled = load(&app)?;
            print!("{}", graph::render(&compiled, format));
            Ok(())
        }

        Command::Explain {
            app,
            set,
            change_column,
            json,
        } => {
            let compiled = Arc::new(load(&app)?);
            let report = explain::run(compiled, &set, change_column.as_deref())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
                );
            } else {
                print!("{}", report.render());
            }
            Ok(())
        }

        Command::Export { app, out, wasm } => {
            let compiled = load(&app)?;
            let written = export::write(&compiled, &app, &out, wasm.as_deref())?;
            println!("dagpane: {} — {}", compiled.title, out.display());
            for (path, bytes) in &written.files {
                println!("  {:>9}  {}", export::human(*bytes), path.display());
            }
            println!("  {:>9}  total", export::human(written.total()));
            // The number that decides whether this was a good idea. An exported app carries
            // its data, so the page is as big as the data — which is fine for the tens of
            // thousands of rows BENCHMARKS.md calls interactive and absurd past that.
            println!(
                "\n  Serve the whole directory over http — `cd {} && python3 -m http.server`\n  — or put it behind any static host. A browser will not run it from file://.",
                out.display()
            );
            Ok(())
        }

        Command::Run {
            app,
            port,
            host,
            auth: auth_args,
        } => {
            let compiled = Arc::new(load(&app)?);
            let addr: std::net::SocketAddr = format!("{host}:{port}")
                .parse()
                .map_err(|e| format!("{host}:{port} is not an address to bind: {e}"))?;

            let door = auth::front_door(&auth_args)?;
            auth::describe(&addr, door.as_deref(), "this app");

            println!(
                "dagpane: {} — {} cells, {} panes",
                compiled.title,
                compiled.graph.len(),
                compiled.panes.len()
            );
            println!("dagpane: http://{addr}");

            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            // The app's name for a token's per-app claim: the manifest's file stem, which
            // is the same rule `dagpane host` routes by — so one claim works in both modes.
            let app_name = app
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("app")
                .to_string();
            runtime
                .block_on(dagpane_serve::serve_with(
                    compiled,
                    addr,
                    dagpane_serve::Options {
                        app_name,
                        auth: door,
                        // Loopback by default, so the allowlist derived from the bound
                        // address is the right one. `serve` is where an operator says
                        // otherwise.
                        origins: Vec::new(),
                        ..Default::default()
                    },
                ))
                .map_err(|e| e.to_string())
        }

        Command::Serve {
            app,
            port,
            bind,
            admin_port,
            admin_bind,
            origins,
            drain_seconds,
            heartbeat_seconds,
            auth: auth_args,
        } => {
            // Read once and compile from the bytes read, rather than `load`ing the path and
            // reading it again for the digest: a fingerprint that describes a *different*
            // read of the file is a fingerprint that can be wrong, and the whole point of
            // reporting one is that a rollout can trust it.
            let text =
                std::fs::read_to_string(&app).map_err(|e| format!("{}: {e}", app.display()))?;
            let manifest = dagpane_app::manifest::parse(&text)
                .map_err(|e| format!("{}: {e}", app.display()))?;
            let base = app.parent().unwrap_or(Path::new("."));
            let compiled = Arc::new(
                dagpane_app::compile(&manifest, base)
                    .map_err(|e| format!("{}: {e}", app.display()))?,
            );

            let addr: std::net::SocketAddr = format!("{bind}:{port}")
                .parse()
                .map_err(|e| format!("{bind}:{port} is not an address to bind: {e}"))?;
            let admin_addr: std::net::SocketAddr = format!("{admin_bind}:{admin_port}")
                .parse()
                .map_err(|e| format!("{admin_bind}:{admin_port} is not an address to bind: {e}"))?;
            if would_collide(addr, admin_addr) {
                return Err(format!(
                    "the app would bind {addr} and the probes {admin_addr}, which cannot both \
                     be bound — a wildcard covers every address on its port. They are separate \
                     listeners on purpose: give --admin-port a port of its own, and do not \
                     publish it"
                ));
            }

            let origins: Vec<String> = origins
                .iter()
                .map(|o| origin::parse(o))
                .collect::<Result<_, _>>()?;

            let app_name = app
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("app")
                .to_string();

            let door = auth::front_door(&auth_args)?;
            auth::describe(&addr, door.as_deref(), "this app");

            let heartbeat = (heartbeat_seconds > 0).then(|| Duration::from_secs(heartbeat_seconds));

            let identity = dagpane_serve::Identity::of(&app_name, &text, &compiled);
            println!(
                "dagpane: {} — {} cells, {} panes",
                compiled.title, identity.cells, identity.panes
            );
            println!("dagpane: app       http://{addr}");
            println!("dagpane: probes    http://{admin_addr}/healthz, /readyz — do not publish this port");
            // The line a rollout greps. Two digests across a fleet is a rollout that half
            // happened: two apps under one name, with no symptom but viewers disagreeing.
            println!(
                "dagpane: manifest  {} — every replica of this app must report this digest",
                identity.fingerprint
            );
            println!(
                "dagpane: drain     {drain_seconds}s unready before the listener closes; \
                 SIGTERM and SIGINT both take that path"
            );
            match heartbeat {
                Some(_) => println!(
                    "dagpane: heartbeat {heartbeat_seconds}s — keep the idle timeout in front \
                     of this above that"
                ),
                // Worth a line rather than silence: the symptom of getting this wrong is a
                // page that renders and then stops answering, which looks like anything.
                None => println!(
                    "dagpane: heartbeat off — a proxy that idles connections out will reap a \
                     viewer who is only reading"
                ),
            }
            if origins.is_empty() {
                println!("dagpane: origins   the bound address — see the warning below");
            } else {
                println!("dagpane: origins   {}", origins.join(", "));
            }

            // The single most likely way to deploy this and have it do nothing. The default
            // bind is a wildcard, so the default path is the broken one unless an operator
            // says where the app is published — and the symptom is a page that loads and
            // then sits there, with nothing in the log. Saying it at start-up is the only
            // place it can be said before somebody is handed a dead link.
            if origins.is_empty() && !addr.ip().is_loopback() {
                eprintln!(
                    "dagpane: warning — bound {addr} with no --origin, so a browser may open \
                     the socket only\n         from `http://{addr}` or `https://{addr}`, which \
                     is not an origin any browser\n         sends. The page will load and the \
                     controls will do nothing. Pass\n         --origin https://<the name you \
                     publish this at>, once per name."
                );
            }

            let lifecycle = Arc::new(dagpane_serve::Lifecycle::new(
                identity,
                Duration::from_secs(drain_seconds),
            ));

            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            runtime.block_on(async move {
                // Bound before either is served, so a port already in use is a start-up
                // failure and not a replica that passes its probes while serving nothing.
                let listener = tokio::net::TcpListener::bind(addr)
                    .await
                    .map_err(|e| format!("{addr}: {e}"))?;
                let admin = tokio::net::TcpListener::bind(admin_addr)
                    .await
                    .map_err(|e| format!("{admin_addr}: {e}"))?;
                dagpane_serve::serve_replica(
                    compiled,
                    listener,
                    admin,
                    dagpane_serve::Options {
                        app_name,
                        auth: door,
                        origins,
                        heartbeat,
                    },
                    lifecycle,
                )
                .await
                .map_err(|e| e.to_string())
            })
        }

        Command::Refresh {
            app,
            force,
            watch,
            json,
        } => {
            let compiled = Arc::new(load(&app)?);
            match watch {
                Some(secs) => refresh::watch(compiled, Duration::from_secs(secs.max(1)), json),
                None => {
                    let report = refresh::run(compiled, force);
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&report.json())
                                .map_err(|e| e.to_string())?
                        );
                    } else {
                        print!("{}", report.render());
                    }
                    // A source that could not be read is a non-zero exit: this command is
                    // the thing a cron job runs, and a cron job that cannot tell a failed
                    // refresh from a successful one is one nobody notices has stopped.
                    if report.exit_code() != 0 {
                        return Err("one or more sources could not be read".to_string());
                    }
                    Ok(())
                }
            }
        }

        Command::Host {
            dir,
            port,
            bind,
            budget_mb,
            idle_minutes,
            admin_port,
            admin_bind,
            drain_seconds,
            heartbeat_seconds,
            auth: auth_args,
        } => {
            let addr: std::net::SocketAddr = format!("{bind}:{port}")
                .parse()
                .map_err(|e| format!("{bind}:{port} is not an address to bind: {e}"))?;

            let admin_addr: std::net::SocketAddr = format!("{admin_bind}:{admin_port}")
                .parse()
                .map_err(|e| format!("{admin_bind}:{admin_port} is not an address to bind: {e}"))?;
            if would_collide(addr, admin_addr) {
                return Err(format!(
                    "the apps would bind {addr} and the probes {admin_addr}, which cannot both \
                     be bound — a wildcard covers every address on its port. They are separate \
                     listeners on purpose: give --admin-port a port of its own, and do not \
                     publish it"
                ));
            }

            if !dir.is_dir() {
                return Err(format!("{} is not a directory", dir.display()));
            }

            let source = Arc::new(DirAppSource::new(&dir));
            let apps = source.apps();
            if apps.is_empty() {
                return Err(format!(
                    "{} holds no app: a manifest here is named for the app it serves, so \
                     `sales.toml` is the app `sales`",
                    dir.display()
                ));
            }
            for skipped in source.skipped() {
                eprintln!(
                    "dagpane: warning — skipping {skipped}.toml: an app name is a DNS label \
                     (lowercase letters, digits and `-`), because it is routed on"
                );
            }

            let door = auth::front_door(&auth_args)?;
            auth::describe(&addr, door.as_deref(), "every app below");

            let budget = Budget::new(
                budget_mb.saturating_mul(1 << 20),
                Duration::from_secs(idle_minutes.saturating_mul(60)),
            );
            println!(
                "dagpane: {} app(s) from {}, {budget_mb} MB of source data, idle after \
                 {idle_minutes} min",
                apps.len(),
                dir.display()
            );
            for app in &apps {
                // Not compiled yet, deliberately: the host compiles on the first request, so
                // a broken manifest takes down its own app and not the process. `dagpane
                // check` is how you find out before anybody asks.
                println!("  http://{app}.localhost:{port}/   ({app}.toml)");
            }

            let heartbeat = (heartbeat_seconds > 0).then(|| Duration::from_secs(heartbeat_seconds));

            let host = Arc::new(Host::new(source, budget));
            // The fleet describes itself PER REQUEST rather than once here: apps arrive on
            // first request and leave under budget pressure or the sweep below, so a snapshot
            // taken now would report a fleet this process stops holding within the minute.
            let lifecycle = Arc::new(dagpane_serve::Lifecycle::describing(
                Arc::new(dagpane_serve::Fleet::new(Arc::clone(&host))),
                Duration::from_secs(drain_seconds),
            ));

            println!("dagpane: probes    http://{admin_addr}/healthz, /readyz — do not publish this port");
            match heartbeat {
                Some(_) => println!(
                    "dagpane: heartbeat {heartbeat_seconds}s — keep the idle timeout in front \
                     of this above that"
                ),
                None => println!(
                    "dagpane: heartbeat off — a proxy that idles connections out will reap a \
                     viewer who is only reading"
                ),
            }
            println!(
                "dagpane: drain     {drain_seconds}s unready before the listener closes; \
                 SIGTERM and SIGINT both take that path"
            );
            // Half the idle window, so an app leaves somewhere between `idle_minutes` and one
            // and a half times it rather than at an unbounded time after it.
            let sweep = (idle_minutes > 0).then(|| {
                Duration::from_secs(idle_minutes.saturating_mul(60) / 2).max(Duration::from_secs(1))
            });
            match sweep {
                Some(every) => println!(
                    "dagpane: sweep     every {} s, dropping apps unopened for {idle_minutes} min",
                    every.as_secs()
                ),
                // Worth saying, because this was the shipped behaviour for every release
                // before this one whatever --idle-minutes said.
                None => println!(
                    "dagpane: sweep     off — an app leaves only on a redeploy or under budget \
                     pressure"
                ),
            }

            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind(addr)
                    .await
                    .map_err(|e| format!("{addr}: {e}"))?;
                let admin = tokio::net::TcpListener::bind(admin_addr)
                    .await
                    .map_err(|e| format!("{admin_addr}: {e}"))?;

                if let Some(every) = sweep {
                    tokio::spawn(dagpane_serve::sweep_idle(
                        Arc::clone(&host),
                        every,
                        Arc::clone(&lifecycle),
                    ));
                }
                dagpane_serve::serve_host_replica(host, listener, admin, door, heartbeat, lifecycle)
                    .await
                    .map_err(|e| e.to_string())
            })
        }
    }
}

fn load(path: &Path) -> Result<App, String> {
    dagpane_app::load(path).map_err(|e| format!("{}: {e}", path.display()))
}
