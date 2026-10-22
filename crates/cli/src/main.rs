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
mod domain;
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
use dagpane_host::{AppId, Budget, DirAppSource, Domain, Host};

use crate::args::{Cli, Command, LivenessArgs};

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

impl LivenessArgs {
    /// What the flags ask of the server, as the one value it takes: `0` turns each off.
    fn liveness(&self) -> dagpane_serve::Liveness {
        let seconds = |n: u64| (n > 0).then(|| Duration::from_secs(n));
        dagpane_serve::Liveness {
            heartbeat: seconds(self.heartbeat_seconds),
            write_timeout: seconds(self.write_timeout_seconds),
            ping_timeout: seconds(self.ping_timeout_seconds),
        }
    }
}

/// The start-up lines for the liveness flags, which `run`, `serve` and `host` all print.
fn describe_liveness(liveness: &dagpane_serve::Liveness) {
    for line in liveness_lines(liveness) {
        println!("{line}");
    }
}

/// One line per setting, saying what it is or that it is off. Apart from the printing so that
/// every state can be tested: a line that says one thing while the server does another is the
/// failure the flags exist to prevent.
fn liveness_lines(liveness: &dagpane_serve::Liveness) -> [String; 3] {
    let heartbeat = match liveness.heartbeat {
        Some(every) => format!(
            "dagpane: heartbeat {}s — keep the idle timeout in front of this above that",
            every.as_secs()
        ),
        // Worth a line rather than silence: the symptom of getting this wrong is a page that
        // renders and then stops answering, which looks like anything.
        None => "dagpane: heartbeat off — a proxy that idles connections out will reap a viewer \
                 who is only reading"
            .to_string(),
    };
    let writes = match liveness.write_timeout {
        Some(limit) => format!(
            "dagpane: writes    {}s per frame beyond what a 128 kbit/s link needs — a client \
             that stops reading, or falls that far behind, is dropped",
            limit.as_secs()
        ),
        // The symptom of this being off is memory that does not come back, which looks like
        // anything; a line here is the one place it is written down.
        None => "dagpane: writes    no deadline — a client that stops reading keeps its \
                 connection, and what it holds, for as long as it likes"
            .to_string(),
    };
    let answers = match (liveness.ping_timeout, liveness.heartbeat) {
        (Some(limit), Some(_)) => format!(
            "dagpane: answers   {}s per ping beyond what a 128 kbit/s link needs — a client \
             that stops reading, or goes, is dropped",
            limit.as_secs()
        ),
        // A deadline with nothing to answer is no deadline, and saying so beats a line that
        // promises one.
        (Some(_), None) => "dagpane: answers   no pings to answer with the heartbeat off — a \
                            client that stops reading on a quiet connection, or goes, keeps it"
            .to_string(),
        (None, _) => "dagpane: answers   no deadline — a client that stops reading on a quiet \
                      connection, or goes, keeps its connection and what it holds until TCP \
                      gives up, or for good"
            .to_string(),
    };
    [heartbeat, writes, answers]
}

/// How `run` and `serve` serve their one app.
///
/// Every field named and the liveness taken apart in full, with no `..Default::default()`: a
/// setting added to either struct fails to compile here, rather than quietly keeping its
/// default while the flag that was meant to set it parses and does nothing.
fn serving(
    app_name: String,
    auth: Option<Arc<dagpane_serve::Auth>>,
    origins: Vec<String>,
    liveness: dagpane_serve::Liveness,
) -> dagpane_serve::Options {
    let dagpane_serve::Liveness {
        heartbeat,
        write_timeout,
        ping_timeout,
    } = liveness;
    dagpane_serve::Options {
        app_name,
        auth,
        origins,
        heartbeat,
        write_timeout,
        ping_timeout,
    }
}

/// Whether two listeners would fight over the same port.
///
/// Equality is not the test. `--bind 127.0.0.1 --port 8787 --admin-bind 0.0.0.0 --admin-port
/// 8787` is two *different* socket addresses that cannot both be bound: a wildcard covers every
/// address on its port, so the second `bind(2)` would fail with `EADDRINUSE` — a message about
/// a port in use, where this one can say why.
///
/// Port 0 is never a collision, not even with another port 0: it asks the kernel for a free
/// port, and the kernel picks one per listener — and [`bind_both`] binds a fixed port before a
/// port 0, so the kernel cannot hand the fixed one to the other listener first.
fn would_collide(a: std::net::SocketAddr, b: std::net::SocketAddr) -> bool {
    if a.port() != b.port() || a.port() == 0 {
        return false;
    }
    // Same port: they collide unless they are on genuinely different addresses. A wildcard
    // covers all of them, and an address always covers itself.
    a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified()
}

/// The app's listener and the probes', both bound, or the address that could not be.
///
/// A listener on a fixed port is bound first. Port 0 takes whatever free port the kernel picks,
/// and bound first it could be handed the other listener's port, when that port lies in the
/// range the kernel picks from: the process would then collide with itself, and nothing would
/// be holding the port by the time anybody looked.
async fn bind_both(
    addr: std::net::SocketAddr,
    admin_addr: std::net::SocketAddr,
) -> Result<(tokio::net::TcpListener, tokio::net::TcpListener), String> {
    async fn bind(at: std::net::SocketAddr) -> Result<tokio::net::TcpListener, String> {
        tokio::net::TcpListener::bind(at)
            .await
            .map_err(|e| format!("{at}: {e}"))
    }
    if addr.port() == 0 && admin_addr.port() != 0 {
        let admin = bind(admin_addr).await?;
        Ok((bind(addr).await?, admin))
    } else {
        let listener = bind(addr).await?;
        Ok((listener, bind(admin_addr).await?))
    }
}

/// The warning for `serve` bound beyond loopback with no `--origin`, naming the address it bound,
/// or nothing.
///
/// The single most likely way to deploy this and have it do nothing. The default bind is a
/// wildcard, so the default path is the broken one unless an operator says where the app is
/// published — and the symptom is a page that loads and then sits there, with nothing in the
/// log. Saying it at start-up is the only place it can be said before somebody is handed a dead
/// link.
///
/// A concrete address is an origin a browser sends — a page opened at `http://192.168.1.10:8787`
/// works — so there the warning says what does not work rather than that nothing does.
fn origins_warning(bound: std::net::SocketAddr, origins: &[String]) -> Option<String> {
    (origins.is_empty() && !bound.ip().is_loopback()).then(|| {
        if bound.ip().is_unspecified() {
            format!(
                "dagpane: warning — bound {bound} with no --origin, so a browser may open the \
                 socket only\n         from `http://{bound}` or `https://{bound}`, which is not \
                 an origin any browser\n         sends. The page will load and the controls will \
                 do nothing. Pass\n         --origin https://<the name you publish this at>, once \
                 per name."
            )
        } else {
            format!(
                "dagpane: warning — bound {bound} with no --origin, so a browser may open the \
                 socket only\n         from `http://{bound}` or `https://{bound}`. A page opened \
                 at any other name — a\n         hostname, a proxy's URL — will load and its \
                 controls will do nothing. Pass\n         --origin https://<the name you publish \
                 this at>, once per name; it replaces\n         the address, so name that too if \
                 it is still used."
            )
        }
    })
}

/// The warning for `host` bound beyond loopback with no `--domain` outside localhost, or
/// nothing.
///
/// About names, not about who can connect — that is `auth::open_warning`'s. A wide bind is
/// usually a proxy or a container in front, and the commonest way to have that do nothing is
/// to forget that a forwarded hostname has to be declared. Docker's `-p` and a port-forward
/// still work at `<app>.localhost`, so the warning says what is refused rather than claiming
/// the apps are unreachable.
fn domains_warning(bound: std::net::SocketAddr, domains: &[Domain]) -> Option<String> {
    // `--domain localhost`, or one under it, declares nothing another machine can use.
    let beyond_localhost = domains.iter().any(|d| {
        let d = d.as_str();
        d != "localhost" && !d.ends_with(".localhost")
    });
    (!beyond_localhost && !bound.ip().is_loopback()).then(|| {
        // Not "no --domain" when one was given: `--domain dev.localhost` is answered, and a
        // warning that denied it would be the first thing an operator stopped believing.
        let (declared, name) = if domains.is_empty() {
            ("no --domain", "<app>.localhost")
        } else {
            ("every --domain under localhost", "a name under localhost")
        };
        format!(
            "dagpane: warning — bound {bound} with {declared}, so each app answers only as\n         \
             {name} and every other name gets 404. A browser on another machine cannot\n         \
             open that name; any client that reaches this port can still send it. If these apps\n         \
             are published under a domain, pass --domain <that domain>."
        )
    })
}

/// The start-up line that says which names reach an app, with one to try.
fn names_line(domains: &[Domain], apps: &[AppId]) -> String {
    let mut shown: Vec<&str> = vec!["localhost"];
    for domain in domains {
        if !shown.contains(&domain.as_str()) {
            shown.push(domain.as_str());
        }
    }
    let patterns = shown
        .iter()
        .map(|d| format!("<app>.{d}"))
        .collect::<Vec<_>>()
        .join(", ");
    let example_domain = shown.get(1).copied().unwrap_or("localhost");
    let example = apps
        .first()
        .map(|app| format!(" — e.g. {app}.{example_domain}"))
        .unwrap_or_default();
    format!(
        "dagpane: names     {patterns}{example}; any other name, a bare <app> or an address \
         gets 404"
    )
}

/// A `--domain` that starts with one of the apps' names: `--domain sales.example.com` beside
/// `sales.toml`, which is the full hostname of one app where the domain they all sit under
/// was meant — and which makes that app answer at `sales.sales.example.com`.
///
/// Only when what follows the app's label could itself be declared: `--domain acme.com` beside
/// `acme.toml` is an app named after its own apex, not a mistake, and the fix this would offer
/// — `--domain com` — is one the parser refuses.
fn domain_collisions(domains: &[Domain], apps: &[AppId], dir: &Path) -> Vec<String> {
    domains
        .iter()
        .filter_map(|domain| {
            let (label, rest) = domain.as_str().split_once('.')?;
            Domain::parse(rest).ok()?;
            apps.iter().any(|app| app.as_str() == label).then(|| {
                format!(
                    "dagpane: warning — --domain {domain} begins with {label}, which is an app \
                     in {}, so that app answers at {label}.{domain}. --domain names what the \
                     apps sit under: if that is {rest}, pass --domain {rest}",
                    dir.display()
                )
            })
        })
        .collect()
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
            // A schedule is behaviour a running server will have that nothing else on this
            // screen would tell anybody about, so it is said here, before anything runs.
            for bound in &compiled.sources {
                if let Some(secs) = bound.refresh_secs {
                    println!(
                        "  {}: re-read every {} s while anyone is viewing the app",
                        bound.cell,
                        secs.max(1)
                    );
                }
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
            liveness: liveness_args,
            auth: auth_args,
        } => {
            let compiled = Arc::new(load(&app)?);
            let addr: std::net::SocketAddr = format!("{host}:{port}")
                .parse()
                .map_err(|e| format!("{host}:{port} is not an address to bind: {e}"))?;

            let door = auth::front_door(&auth_args)?;
            auth::describe(door.as_deref());

            println!(
                "dagpane: {} — {} cells, {} panes",
                compiled.title,
                compiled.graph.len(),
                compiled.panes.len()
            );
            let liveness = liveness_args.liveness();
            describe_liveness(&liveness);

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
            serve_to_the_end(runtime, async move {
                // Bound before the URL is printed, so a URL on screen is something listening
                // on it: a port already in use stops here with the address in the message,
                // and the port printed for `--port 0` is the one the kernel picked.
                let listener = tokio::net::TcpListener::bind(addr)
                    .await
                    .map_err(|e| format!("{addr}: {e}"))?;
                let bound = listener.local_addr().map_err(|e| e.to_string())?;
                if let Some(warning) = auth::open_warning(bound, door.as_deref(), "this app") {
                    eprintln!("{warning}");
                }
                println!("dagpane: http://{bound}");
                dagpane_serve::serve_on_with(
                    compiled,
                    listener,
                    // No origins: loopback by default, so the allowlist derived from the
                    // bound address is the right one. `serve` is where an operator says
                    // otherwise.
                    serving(app_name, door, Vec::new(), liveness),
                )
                .await
                .map_err(|e| e.to_string())
            })
        }

        Command::Serve {
            app,
            port,
            bind,
            admin_port,
            admin_bind,
            origins,
            drain_seconds,
            liveness: liveness_args,
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
            auth::describe(door.as_deref());

            let liveness = liveness_args.liveness();

            let identity = dagpane_serve::Identity::of(&app_name, &text, &compiled);
            println!(
                "dagpane: {} — {} cells, {} panes",
                compiled.title, identity.cells, identity.panes
            );
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
            describe_liveness(&liveness);
            match origins.is_empty() {
                // The warning comes once the address is bound, and names it.
                true if !addr.ip().is_loopback() => {
                    println!("dagpane: origins   the bound address — see the warning below")
                }
                true => println!("dagpane: origins   the bound address"),
                false => println!("dagpane: origins   {}", origins.join(", ")),
            }

            let lifecycle = Arc::new(dagpane_serve::Lifecycle::new(
                identity,
                Duration::from_secs(drain_seconds),
            ));

            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            serve_to_the_end(runtime, async move {
                // Bound before either is served, so a port already in use is a start-up
                // failure and not a replica that passes its probes while serving nothing —
                // and before either address is printed, so what is printed is listening, on
                // the port the kernel picked when asked for 0.
                let (listener, admin) = bind_both(addr, admin_addr).await?;
                let bound = listener.local_addr().map_err(|e| e.to_string())?;
                let admin_bound = admin.local_addr().map_err(|e| e.to_string())?;
                let warnings = [
                    auth::open_warning(bound, door.as_deref(), "this app"),
                    origins_warning(bound, &origins),
                ];
                for warning in warnings.into_iter().flatten() {
                    eprintln!("{warning}");
                }
                println!("dagpane: app       http://{bound}");
                println!(
                    "dagpane: probes    http://{admin_bound}/healthz, /readyz — do not publish \
                     this port"
                );
                dagpane_serve::serve_replica(
                    compiled,
                    listener,
                    admin,
                    serving(app_name, door, origins, liveness),
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
            domains,
            budget_mb,
            idle_minutes,
            admin_port,
            admin_bind,
            drain_seconds,
            liveness: liveness_args,
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
            // Before anything is bound or any address printed: a domain written wrong is one no
            // request will ever match, so it is refused here rather than found as a page of 404s.
            let domains = domains
                .iter()
                .map(|raw| domain::parse(raw))
                .collect::<Result<Vec<_>, _>>()?;

            let source = Arc::new(DirAppSource::new(&dir).with_domains(domains.clone()));
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
            for warning in domain_collisions(&domains, &apps, &dir) {
                eprintln!("{warning}");
            }

            let door = auth::front_door(&auth_args)?;
            auth::describe(door.as_deref());

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
            // No scheme in it, so a failed bind still prints no `http://` — see `bind_both`.
            println!("{}", names_line(&domains, &apps));

            let liveness = liveness_args.liveness();

            let host = Arc::new(Host::new(source, budget));
            // The fleet describes itself PER REQUEST rather than once here: apps arrive on
            // first request and leave under budget pressure or the sweep below, so a snapshot
            // taken now would report a fleet this process stops holding within the minute.
            let lifecycle = Arc::new(dagpane_serve::Lifecycle::describing(
                Arc::new(dagpane_serve::Fleet::new(Arc::clone(&host))),
                Duration::from_secs(drain_seconds),
            ));

            describe_liveness(&liveness);
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
                // Worth saying, because until the sweep had a caller this was how `host`
                // behaved, whatever --idle-minutes said.
                None => println!(
                    "dagpane: sweep     off — an app leaves only on a redeploy or under budget \
                     pressure"
                ),
            }

            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            serve_to_the_end(runtime, async move {
                // Bound before any address is printed, as `serve` does and for its reasons.
                let (listener, admin) = bind_both(addr, admin_addr).await?;
                let bound = listener.local_addr().map_err(|e| e.to_string())?;
                let admin_bound = admin.local_addr().map_err(|e| e.to_string())?;
                if let Some(warning) = auth::open_warning(bound, door.as_deref(), "every app below")
                {
                    eprintln!("{warning}");
                }
                if let Some(warning) = domains_warning(bound, &domains) {
                    eprintln!("{warning}");
                }
                let bound_port = bound.port();
                for app in &apps {
                    // Not compiled yet, deliberately: the host compiles on the first request,
                    // so a broken manifest takes down its own app and not the process.
                    // `dagpane check` is how you find out before anybody asks.
                    println!("  http://{app}.localhost:{bound_port}/   ({app}.toml)");
                }
                println!(
                    "dagpane: probes    http://{admin_bound}/healthz, /readyz — do not publish \
                     this port"
                );

                if let Some(every) = sweep {
                    tokio::spawn(dagpane_serve::sweep_idle(
                        Arc::clone(&host),
                        every,
                        Arc::clone(&lifecycle),
                    ));
                }
                dagpane_serve::serve_host_replica(host, listener, admin, door, liveness, lifecycle)
                    .await
                    .map_err(|e| e.to_string())
            })
        }
    }
}

/// Run a server on `runtime` to its end, and then let the runtime go without waiting on it.
///
/// Dropping a runtime joins every worker, and a worker in the middle of a viewer's pass finishes
/// the pass first — so a pass still running when the server has finished stopping would hold
/// the process past the grace every socket was given, under a log line that says it stopped
/// without them. By the time the server returns it has waited for everything worth waiting for.
fn serve_to_the_end(
    runtime: tokio::runtime::Runtime,
    serving: impl std::future::Future<Output = Result<(), String>>,
) -> Result<(), String> {
    let served = runtime.block_on(serving);
    runtime.shutdown_background();
    served
}

fn load(path: &Path) -> Result<App, String> {
    dagpane_app::load(path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dagpane_serve::Liveness;

    /// The liveness a command line asks for.
    fn asked(command: &[&str], flags: &[&str]) -> Liveness {
        let args = ["dagpane"].iter().chain(command).chain(flags);
        let cli = Cli::try_parse_from(args).expect("the command line parses");
        match cli.command {
            Command::Run { liveness, .. }
            | Command::Serve { liveness, .. }
            | Command::Host { liveness, .. } => liveness.liveness(),
            _ => unreachable!("not a serving command"),
        }
    }

    const SERVING: [&[&str]; 3] = [
        &["run", "app.toml"],
        &["serve", "app.toml"],
        &["host", "apps"],
    ];

    #[test]
    fn each_serving_command_reads_each_liveness_flag_into_its_own_setting() {
        // Three different numbers, so a flag read into another's setting shows.
        let flags = [
            "--heartbeat-seconds",
            "3",
            "--write-timeout-seconds",
            "5",
            "--ping-timeout-seconds",
            "7",
        ];
        for command in SERVING {
            assert_eq!(
                asked(command, &flags),
                Liveness {
                    heartbeat: Some(Duration::from_secs(3)),
                    write_timeout: Some(Duration::from_secs(5)),
                    ping_timeout: Some(Duration::from_secs(7)),
                },
                "{command:?}"
            );
        }
    }

    #[test]
    fn zero_turns_each_off_and_leaving_them_out_asks_for_the_servers_own_defaults() {
        let off = [
            "--heartbeat-seconds",
            "0",
            "--write-timeout-seconds",
            "0",
            "--ping-timeout-seconds",
            "0",
        ];
        for command in SERVING {
            assert_eq!(
                asked(command, &off),
                Liveness {
                    heartbeat: None,
                    write_timeout: None,
                    ping_timeout: None,
                },
                "{command:?}"
            );
            assert_eq!(asked(command, &[]), Liveness::default(), "{command:?}");
        }
    }

    /// A front door that is never asked to verify a token, only whether it is there.
    fn door() -> dagpane_serve::Auth {
        dagpane_serve::Auth {
            jwks: dagpane_auth::Jwks::parse(r#"{"keys":[{"kty":"RSA","n":"AQAB","e":"AQAB"}]}"#)
                .expect("a key set with one key"),
            policy: dagpane_auth::Policy {
                issuer: "https://issuer.example".into(),
                audience: "dagpane".into(),
                algorithms: [dagpane_auth::Algorithm::RS256].into(),
                leeway_seconds: 0,
                access: dagpane_auth::Access::AnyApp,
            },
            login: None,
        }
    }

    #[test]
    fn serving_carries_every_setting_into_the_options() {
        // A front door dropped here serves the app to anybody, and nothing else would notice.
        let door = Arc::new(door());
        let liveness = Liveness {
            heartbeat: Some(Duration::from_secs(3)),
            write_timeout: Some(Duration::from_secs(5)),
            ping_timeout: Some(Duration::from_secs(7)),
        };
        let options = serving(
            "sales".into(),
            Some(Arc::clone(&door)),
            vec!["https://dash.example".into()],
            liveness,
        );
        assert_eq!(options.app_name, "sales");
        assert!(
            options
                .auth
                .as_ref()
                .is_some_and(|auth| Arc::ptr_eq(auth, &door)),
            "the front door was not passed on"
        );
        assert_eq!(options.origins, ["https://dash.example"]);
        assert_eq!(
            (
                options.heartbeat,
                options.write_timeout,
                options.ping_timeout
            ),
            (
                liveness.heartbeat,
                liveness.write_timeout,
                liveness.ping_timeout
            )
        );
    }

    #[test]
    fn each_warning_names_the_address_bound_and_comes_only_when_it_applies() {
        let at = |text: &str| text.parse::<std::net::SocketAddr>().unwrap();
        // A wildcard with nothing in front and no origin declared: both warnings, each naming the
        // port the listener got, which is not the 0 it was asked for.
        let bound = at("0.0.0.0:45383");
        let open = auth::open_warning(bound, None, "this app").expect("an open-door warning");
        assert!(
            open.contains("binding 0.0.0.0:45383, which is reachable"),
            "{open}"
        );
        let origins = origins_warning(bound, &[]).expect("an origins warning");
        assert!(
            origins.contains("bound 0.0.0.0:45383 with no --origin"),
            "{origins}"
        );
        // On loopback neither applies: nobody else can reach it, and the origin derived from the
        // address bound is one a browser sends.
        let local = at("127.0.0.1:8787");
        assert_eq!(auth::open_warning(local, None, "this app"), None);
        assert_eq!(origins_warning(local, &[]), None);
        // A front door answers the one, and a declared origin the other.
        assert_eq!(auth::open_warning(bound, Some(&door()), "this app"), None);
        assert_eq!(
            origins_warning(bound, &["https://dash.example".to_string()]),
            None
        );
        // A concrete address IS an origin a browser sends: the warning says what does not work,
        // never that nothing does.
        let lan = at("192.168.1.10:8787");
        let warning = origins_warning(lan, &[]).expect("an origins warning");
        assert!(
            warning.contains("bound 192.168.1.10:8787 with no --origin"),
            "{warning}"
        );
        assert!(warning.contains("any other name"), "{warning}");
        assert!(!warning.contains("not an origin"), "{warning}");
        assert!(!warning.contains("The page will load"), "{warning}");
    }

    #[test]
    fn the_domains_warning_comes_only_on_a_wide_bind_with_no_domain_beyond_localhost() {
        let at = |text: &str| text.parse::<std::net::SocketAddr>().unwrap();
        let example = || vec![Domain::parse("example.com").unwrap()];

        let warning = domains_warning(at("0.0.0.0:45383"), &[]).expect("a domains warning");
        assert!(
            warning.contains("bound 0.0.0.0:45383 with no --domain"),
            "{warning}"
        );
        // About names, never a claim that the apps cannot be reached: Docker's -p reaches them.
        assert!(warning.contains("<app>.localhost"), "{warning}");
        assert!(!warning.contains("only this machine"), "{warning}");

        assert_eq!(domains_warning(at("127.0.0.1:8787"), &[]), None);
        assert_eq!(domains_warning(at("0.0.0.0:8787"), &example()), None);
        // Declaring localhost, or a name under it, declares nothing another machine can use —
        // and the warning says that rather than that nothing was declared.
        for local in ["localhost", "dev.localhost"] {
            let declared = [Domain::parse(local).unwrap()];
            let warning = domains_warning(at("0.0.0.0:8787"), &declared).expect(local);
            assert!(
                warning.contains("bound 0.0.0.0:8787 with every --domain under localhost"),
                "{warning}"
            );
            assert!(!warning.contains("no --domain"), "{warning}");
        }
    }

    #[test]
    fn the_names_line_says_which_names_reach_an_app_and_offers_one() {
        let apps = vec![AppId::parse("ops").unwrap(), AppId::parse("sales").unwrap()];
        assert_eq!(
            names_line(&[], &apps),
            "dagpane: names     <app>.localhost — e.g. ops.localhost; any other name, a bare \
             <app> or an address gets 404"
        );
        let declared = vec![
            Domain::parse("example.com").unwrap(),
            Domain::localhost(),
            Domain::parse("internal").unwrap(),
        ];
        let line = names_line(&declared, &apps);
        assert!(
            line.starts_with(
                "dagpane: names     <app>.localhost, <app>.example.com, <app>.internal — e.g. \
                 ops.example.com;"
            ),
            "{line}"
        );
        // A failed bind must print no `http://`, and this line is printed before the bind.
        assert!(!line.contains("http"), "{line}");
    }

    #[test]
    fn a_domain_that_begins_with_an_app_is_named_as_the_likely_mistake() {
        let apps = vec![AppId::parse("sales").unwrap()];
        let dir = Path::new("apps");
        let mistaken =
            domain_collisions(&[Domain::parse("sales.example.com").unwrap()], &apps, dir);
        assert_eq!(mistaken.len(), 1, "{mistaken:?}");
        assert!(
            mistaken[0].contains("pass --domain example.com"),
            "{}",
            mistaken[0]
        );
        assert!(domain_collisions(&[Domain::parse("example.com").unwrap()], &apps, dir).is_empty());
        // An app named after its own apex is not a mistake, and `--domain com` is no fix.
        let acme = vec![AppId::parse("acme").unwrap()];
        assert!(domain_collisions(&[Domain::parse("acme.com").unwrap()], &acme, dir).is_empty());
        assert!(domain_collisions(&[Domain::localhost()], &apps, dir).is_empty());
    }

    #[test]
    fn port_zero_never_collides_and_a_wildcard_on_one_port_always_does() {
        let at = |text: &str| text.parse::<std::net::SocketAddr>().unwrap();
        // Port 0 asks the kernel for a free port, once per listener.
        assert!(!would_collide(at("127.0.0.1:0"), at("127.0.0.1:0")));
        assert!(!would_collide(at("0.0.0.0:0"), at("127.0.0.1:0")));
        assert!(!would_collide(at("127.0.0.1:0"), at("127.0.0.1:8787")));
        // The same port: the same address, or a wildcard over it, cannot both be bound.
        assert!(would_collide(at("127.0.0.1:8787"), at("127.0.0.1:8787")));
        assert!(would_collide(at("127.0.0.1:8787"), at("0.0.0.0:8787")));
        assert!(would_collide(at("0.0.0.0:8787"), at("127.0.0.1:8787")));
        // Different addresses, or different ports, can.
        assert!(!would_collide(at("127.0.0.1:8787"), at("127.0.0.2:8787")));
        assert!(!would_collide(at("127.0.0.1:8787"), at("127.0.0.1:9787")));
    }

    #[test]
    fn each_start_up_line_says_what_its_setting_is_or_that_it_is_off() {
        let lines = |heartbeat: u64, write_timeout: u64, ping_timeout: u64| {
            let seconds = |n: u64| (n > 0).then(|| Duration::from_secs(n));
            liveness_lines(&Liveness {
                heartbeat: seconds(heartbeat),
                write_timeout: seconds(write_timeout),
                ping_timeout: seconds(ping_timeout),
            })
        };
        let says = |lines: &[String; 3], starts: [&str; 3]| {
            for (line, start) in lines.iter().zip(starts) {
                assert!(line.starts_with(start), "{line:?} does not start {start:?}");
            }
        };
        says(
            &lines(3, 5, 7),
            [
                "dagpane: heartbeat 3s ",
                "dagpane: writes    5s per frame",
                "dagpane: answers   7s per ping",
            ],
        );
        says(
            &lines(0, 0, 0),
            [
                "dagpane: heartbeat off",
                "dagpane: writes    no deadline",
                "dagpane: answers   no deadline",
            ],
        );
        // A ping deadline with no pings to answer is none, and the line must not promise one.
        says(
            &lines(0, 5, 7),
            [
                "dagpane: heartbeat off",
                "dagpane: writes    5s per frame",
                "dagpane: answers   no pings to answer with the heartbeat off",
            ],
        );
    }
}
