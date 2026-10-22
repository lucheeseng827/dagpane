//! Every crate is the workspace's version, and every path dependency pins it.
//!
//! The release gates read one line: `[workspace.package].version` in the root `Cargo.toml`,
//! which the release's tag gate compares with the tag and the image job with the
//! image's tag. Nothing inherits that line — every crate writes its own version, and every
//! path dependency pins one, because `deny.toml` refuses a wildcard — so a bump that missed a
//! crate passed both gates and shipped a binary whose `--version` named the release before.
//!
//! A test rather than a CI step so that it runs wherever `cargo test` does: on every pull
//! request, on the staged cut the tag-time sync builds and tests *before* it pushes anything,
//! and on the mirror. A check that ran only in the mirror's CI would report a bad release after
//! it had been published.

use std::path::{Path, PathBuf};

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn manifest(path: &Path) -> toml::Table {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    text.parse()
        .unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()))
}

/// The path dependencies a manifest declares, in every table cargo reads them from.
fn path_pins(manifest: &toml::Table) -> Vec<(String, Option<String>)> {
    let mut tables: Vec<&toml::Table> = Vec::new();
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(table) = manifest.get(section).and_then(toml::Value::as_table) {
            tables.push(table);
        }
    }
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for target in targets.values().filter_map(toml::Value::as_table) {
            for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(table) = target.get(section).and_then(toml::Value::as_table) {
                    tables.push(table);
                }
            }
        }
    }
    let mut pins = Vec::new();
    for table in tables {
        for (name, spec) in table {
            if let Some(spec) = spec.as_table().filter(|spec| spec.contains_key("path")) {
                let version = spec
                    .get("version")
                    .and_then(toml::Value::as_str)
                    .map(str::to_string);
                pins.push((name.clone(), version));
            }
        }
    }
    pins
}

#[test]
fn every_crate_and_every_path_pin_is_the_workspace_version() {
    let root = manifest(&workspace().join("Cargo.toml"));
    let workspace_table = root["workspace"].as_table().expect("[workspace]");
    let want = workspace_table["package"]["version"]
        .as_str()
        .expect("[workspace.package].version");
    let mut wrong = Vec::new();

    if let Some(deps) = workspace_table
        .get("dependencies")
        .and_then(toml::Value::as_table)
    {
        for (name, spec) in deps {
            let Some(spec) = spec.as_table().filter(|spec| spec.contains_key("path")) else {
                continue;
            };
            let got = spec.get("version").and_then(toml::Value::as_str);
            if got != Some(want) {
                wrong.push(format!("[workspace.dependencies] {name}: pins {got:?}"));
            }
        }
    }

    let members = workspace_table["members"].as_array().expect("members");
    for member in members.iter().filter_map(toml::Value::as_str) {
        let crate_manifest = manifest(&workspace().join(member).join("Cargo.toml"));
        let got = crate_manifest["package"]["version"].as_str();
        if got != Some(want) {
            wrong.push(format!("{member}: version {got:?}"));
        }
        for (name, got) in path_pins(&crate_manifest) {
            if got.as_deref() != Some(want) {
                wrong.push(format!("{member}: {name} pins {got:?}"));
            }
        }
    }

    assert!(
        wrong.is_empty(),
        "the workspace is {want}, and these are not:\n{}",
        wrong.join("\n")
    );
}
