//! The command line. Parsing only — nothing here decides anything.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "dagpane",
    version,
    about = "A reactive runtime for data apps: an interaction recomputes the cells that depend on it, and nothing else."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Compile an app and report what is wrong with it. Exits non-zero on any error.
    Check {
        /// The app manifest. Paths inside it resolve against its own directory.
        app: PathBuf,
    },

    /// Print the dependency graph. The structure the runtime will actually use, before it
    /// runs — which is the point of declaring edges rather than discovering them.
    Graph {
        app: PathBuf,
        #[arg(long, value_enum, default_value_t = GraphFormat::Text)]
        format: GraphFormat,
    },

    /// Run one interaction and print exactly what it cost.
    ///
    /// This is the product claim in a terminal. No socket, no browser, no timing noise —
    /// just which cells ran, which served a cached value, which were never looked at, and
    /// which panes would have gone on the wire.
    Explain {
        app: PathBuf,
        /// `name=value`, repeatable. Everything given is applied in one pass, the way a
        /// client that moved two controls in one gesture would send them.
        #[arg(long = "set", value_name = "NAME=VALUE")]
        set: Vec<String>,
        /// Machine-readable, for a CI gate that asserts on the numbers.
        #[arg(long)]
        json: bool,
    },

    /// Serve the app.
    Run {
        app: PathBuf,
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// The address to bind. Defaults to loopback: there is no authentication in this
        /// version, and binding anything else prints a warning saying so.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum GraphFormat {
    /// Indented, one cell per line, in evaluation order.
    Text,
    /// A `flowchart` block, for pasting into a document that renders one.
    Mermaid,
    /// Cells, edges and heights, for a tool.
    Json,
}
