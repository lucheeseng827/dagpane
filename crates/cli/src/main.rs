//! `dagpane` — the binary.
//!
//! Four commands, and the ordering between them is deliberate. `check` and `graph` answer
//! questions about an app that has not run. `explain` answers the one question this project
//! exists to answer, in a terminal, with no browser in the loop. `run` is the demo.
//!
//! Nothing is decided in this crate: it parses arguments, calls `dagpane-app`, and formats.
//! A number printed here is a number some test in `dagpane-core` already asserted.

#![forbid(unsafe_code)]

mod args;
mod explain;
mod graph;

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use dagpane_app::App;

use crate::args::{Cli, Command};

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
            println!("  ok");
            Ok(())
        }

        Command::Graph { app, format } => {
            let compiled = load(&app)?;
            print!("{}", graph::render(&compiled, format));
            Ok(())
        }

        Command::Explain { app, set, json } => {
            let compiled = Arc::new(load(&app)?);
            let report = explain::run(compiled, &set)?;
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

        Command::Run { app, port, host } => {
            let compiled = Arc::new(load(&app)?);
            let addr: std::net::SocketAddr = format!("{host}:{port}")
                .parse()
                .map_err(|e| format!("{host}:{port} is not an address to bind: {e}"))?;

            if !addr.ip().is_loopback() {
                eprintln!(
                    "dagpane: warning — binding {addr}, which is reachable from outside this \
                     machine.\n         There is no authentication in this version: anyone who \
                     can reach that\n         address can read every pane of this app. See \
                     SECURITY.md."
                );
            }

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
            runtime
                .block_on(dagpane_serve::serve(compiled, addr))
                .map_err(|e| e.to_string())
        }
    }
}

fn load(path: &Path) -> Result<App, String> {
    dagpane_app::load(path).map_err(|e| format!("{}: {e}", path.display()))
}
