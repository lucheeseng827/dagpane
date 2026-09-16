//! Printing the graph.
//!
//! `dagpane graph` exists because a declared graph *can* be printed. A runtime that
//! discovers its edges while evaluating has no structure to show until it is already
//! running, which means no diff in review and nothing for CI to assert on. This command is
//! the visible half of that trade.

use dagpane_app::App;
use dagpane_core::CellId;

use crate::args::GraphFormat;

pub fn render(app: &App, format: GraphFormat) -> String {
    match format {
        GraphFormat::Text => text(app),
        GraphFormat::Mermaid => mermaid(app),
        GraphFormat::Json => json(app),
    }
}

fn kind_of(app: &App, id: CellId) -> &'static str {
    if !app.graph.is_source(id) {
        "cell"
    } else if app.widget(app.graph.name(id)).is_some() {
        "input"
    } else {
        "data"
    }
}

fn text(app: &App) -> String {
    let mut out = String::new();
    out.push_str(&format!("{} — {} cells\n\n", app.title, app.graph.len()));
    // Evaluation order, which is height order — so a reader scanning down the page is
    // reading the order the runtime will use, not the order the file was written in.
    for &id in app.graph.order() {
        let name = app.graph.name(id);
        let inputs = app.graph.inputs_of(id);
        let panes: Vec<&str> = app.panes_for(name).map(|p| p.id.as_str()).collect();
        out.push_str(&format!(
            "{:>2}  {:<6} {:<18}",
            app.graph.height(id),
            kind_of(app, id),
            name
        ));
        if !inputs.is_empty() {
            let names: Vec<&str> = inputs.iter().map(|i| app.graph.name(*i)).collect();
            out.push_str(&format!(" ← {}", names.join(", ")));
        }
        if !panes.is_empty() {
            out.push_str(&format!("   [pane {}]", panes.join(", ")));
        }
        out.push('\n');
    }
    out.push_str("\nthe left column is height: a pass evaluates in ascending order, which is\n");
    out.push_str("what makes it impossible for a cell to see a mix of old and new inputs.\n");
    out
}

/// A cell name is author-supplied, so it is never emitted as a Mermaid node id or spliced
/// unescaped into a label: a name containing `"`, `[` or `]` would produce a broken diagram —
/// or, in a manifest somebody else wrote, a deliberately shaped one. Ids are synthetic and
/// positional; labels are quoted with the characters Mermaid treats as syntax replaced.
fn mermaid_id(index: usize) -> String {
    format!("c{index}")
}

fn mermaid_label(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '"' => '\'',
            '[' | ']' | '(' | ')' | '{' | '}' | '<' | '>' | '|' => ' ',
            '\n' | '\r' => ' ',
            other => other,
        })
        .collect()
}

fn mermaid(app: &App) -> String {
    let index_of = |wanted: dagpane_core::CellId| {
        app.graph
            .order()
            .iter()
            .position(|&id| id == wanted)
            .expect("every cell is in the graph's own order")
    };

    let mut out = String::from("flowchart TD\n");
    for (i, &id) in app.graph.order().iter().enumerate() {
        let label = mermaid_label(app.graph.name(id));
        let node = mermaid_id(i);
        let shape = match kind_of(app, id) {
            "input" => format!("{node}([\"{label}\"])"),
            "data" => format!("{node}[(\"{label}\")]"),
            _ => format!("{node}[\"{label}\"]"),
        };
        out.push_str(&format!("    {shape}\n"));
    }
    for (i, &id) in app.graph.order().iter().enumerate() {
        for &input in app.graph.inputs_of(id) {
            out.push_str(&format!(
                "    {} --> {}\n",
                mermaid_id(index_of(input)),
                mermaid_id(i)
            ));
        }
    }
    out
}

fn json(app: &App) -> String {
    let cells: Vec<serde_json::Value> = app
        .graph
        .order()
        .iter()
        .map(|&id| {
            serde_json::json!({
                "name": app.graph.name(id),
                "kind": kind_of(app, id),
                "height": app.graph.height(id),
                "inputs": app.graph.inputs_of(id).iter()
                    .map(|i| app.graph.name(*i)).collect::<Vec<_>>(),
                "dependents": app.graph.dependents_of(id).iter()
                    .map(|i| app.graph.name(*i)).collect::<Vec<_>>(),
                "panes": app.panes_for(app.graph.name(id))
                    .map(|p| p.id.as_str()).collect::<Vec<_>>(),
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "title": app.title,
        "cells": cells,
    }))
    .expect("a graph description always serialises")
}
