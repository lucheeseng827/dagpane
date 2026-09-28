//! `dagpane export` — an app as a directory of files, with no server behind it.
//!
//! # What this produces, and why it is small
//!
//! ```text
//! dist/
//!   index.html          the same client `dagpane run` serves, plus a config block
//!   dagpane.js          the glue, from `dagpane_wasm::GLUE_JS`
//!   dagpane.wasm        the engine, built for wasm32
//!   renderers/…         whatever `[app] renderers` declared
//! ```
//!
//! The manifest and every source's rows are written **into the page**, which is why the CSVs
//! do not appear in that listing. Two reasons, and the second is the one that matters. A
//! second fetch is a second thing to get wrong on a static host — a MIME type, a redirect, a
//! path that works locally and not under a prefix. And an app whose data is in the page is a
//! file somebody can email; an app whose data is beside it is a directory somebody has to
//! deploy correctly.
//!
//! The cost is stated at the end of the command's own output rather than here: an export is
//! as large as the data in it, and a page carrying a 40 MB CSV is a page nobody should ship.
//! `BENCHMARKS.md` has where the ceiling actually is.
//!
//! # What it is not
//!
//! Not a deployment. It writes files; where they go is not this command's business. Not a
//! substitute for `dagpane run` either — an exported app has no front door, no refresh and no
//! server-side sources, because it has no server. `ROADMAP.md` §4 is where the boundary is
//! argued; this module only draws it.

use std::path::{Path, PathBuf};

use dagpane_app::App;

/// Where the wasm module is expected to be when nobody says otherwise.
///
/// A path rather than an embedded copy, deliberately. Embedding it would make every `dagpane`
/// binary a megabyte larger for a command most runs never use, and it would put a build of
/// one crate inside the build of another — which is a circular dependency with a cargo
/// feature painted over it.
///
/// **It is relative, so it resolves against the caller's working directory**, and it is
/// therefore a convenience for one situation only: a shell sitting at this workspace's root
/// just after `cargo build -p dagpane-wasm --target wasm32-unknown-unknown --release`. An
/// installed `dagpane`, a shell in a subdirectory, and a `CARGO_TARGET_DIR` pointing
/// elsewhere all need `--wasm`, and none of them is a misuse.
///
/// Guessing harder would not fix that. Climbing to find a workspace root is wrong under
/// `CARGO_TARGET_DIR`; looking beside the executable is wrong for every distribution that
/// does not ship a `.wasm` next to its binary, which is all of them. So the default stays
/// narrow and honest, and [`missing_module`] makes the failure say which of those it was
/// rather than leaving a relative path on screen for the reader to resolve.
const DEFAULT_WASM: &str = "target/wasm32-unknown-unknown/release/dagpane_wasm.wasm";

/// What to say when the module is not where we looked.
///
/// The failing path is printed **absolute**. A bare
/// `target/wasm32-unknown-unknown/release/dagpane_wasm.wasm: No such file or directory` is
/// the one message that cannot be acted on, because the whole question is *relative to
/// what* — and the answer, the working directory, is the one thing the reader cannot see.
fn missing_module(path: &Path, defaulted: bool, e: std::io::Error) -> String {
    let shown = std::fs::canonicalize(path)
        .or_else(|_| std::env::current_dir().map(|cwd| cwd.join(path)))
        .unwrap_or_else(|_| path.to_path_buf());
    let mut message = format!("no wasm module at {}: {e}\n", shown.display());
    // Only when we guessed. A caller who passed `--wasm` knows where they pointed, and
    // telling them an installed binary needs the flag they just used reads as an insult.
    if defaulted {
        message.push_str(
            "\nThat path is the default, resolved against the current directory \u{2014} \
             nothing was passed to --wasm. It finds a module only in a dagpane workspace \
             that has just built one. An installed `dagpane`, a shell below the workspace \
             root, and a CARGO_TARGET_DIR pointing elsewhere all need --wasm.\n",
        );
    }
    message.push_str(
        "\nBuild one:\n  \
         cargo build -p dagpane-wasm --target wasm32-unknown-unknown --release\n\
         \nor point at one you already have:\n  \
         dagpane export \u{2026} --wasm path/to/dagpane_wasm.wasm\n",
    );
    message
}

/// What was written, for the command to report.
#[derive(Debug)]
pub struct Written {
    /// Each file, with its size in bytes, in the order it was written.
    pub files: Vec<(PathBuf, u64)>,
}

impl Written {
    /// The total, which is the number worth looking at: an exported app is as big as its data.
    pub fn total(&self) -> u64 {
        self.files.iter().map(|(_, n)| n).sum()
    }
}

/// Write `app` to `out` as a static bundle.
///
/// `manifest_path` is the manifest itself — its text goes into the page, and its directory is
/// where the sources were read from. `wasm` overrides where the module is looked for.
///
/// # Errors
///
/// A string for the terminal: a missing wasm module, a source this command cannot inline, or
/// anything the filesystem refuses.
pub fn write(
    app: &App,
    manifest_path: &Path,
    out: &Path,
    wasm: Option<&Path>,
) -> Result<Written, String> {
    let wasm_path = wasm
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_WASM));
    let module =
        std::fs::read(&wasm_path).map_err(|e| missing_module(&wasm_path, wasm.is_none(), e))?;

    let manifest = std::fs::read_to_string(manifest_path)
        .map_err(|e| format!("{}: {e}", manifest_path.display()))?;

    // The rows, as the bytes they were read from. Read again rather than re-encoded out of
    // the compiled frame: a CSV round-tripped through the engine's own types would differ
    // from the file in quoting, in trailing newlines and in how a null prints, and the
    // exported app would then be computing from something subtly other than the app that was
    // checked. The same file, byte for byte, is the only version of this that is honest.
    //
    // The paths come from the manifest and are resolved the way `compile` resolves them —
    // against the manifest's own directory. Recovering them from a `Source`'s `describe()`
    // would be parsing a sentence written for a log line, which is a different thing that
    // happens to look the same.
    let base = manifest_path.parent().unwrap_or(Path::new("."));
    let parsed = dagpane_app::manifest::parse(&manifest).map_err(|e| e.to_string())?;
    let mut sources = serde_json::Map::new();
    for spec in &parsed.source {
        let path = spec
            .csv
            .clone()
            .or_else(|| spec.file.as_ref().map(|f| f.path.clone()))
            .ok_or_else(|| {
                format!(
                    "source `{}` does not come from a file, and an export has no server to \
                     read it from. Export an app whose sources are files.",
                    spec.name
                )
            })?;
        let full = base.join(&path);
        let text = std::fs::read_to_string(&full)
            .map_err(|e| format!("source `{}`: {}: {e}", spec.name, full.display()))?;
        sources.insert(spec.name.clone(), serde_json::Value::String(text));
    }

    let mut renderers = serde_json::Map::new();
    for r in &app.renderers {
        renderers.insert(r.path.clone(), serde_json::Value::String(r.source.clone()));
    }

    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let mut files = Vec::new();
    let mut put = |name: &str, bytes: &[u8]| -> Result<(), String> {
        let path = out.join(name);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        files.push((path, bytes.len() as u64));
        Ok(())
    };

    put("dagpane.wasm", &module)?;
    put("dagpane.js", dagpane_wasm::GLUE_JS.as_bytes())?;
    // Written beside the page as well as inlined into it. The inline copy is what a running
    // app uses; the files are what a renderer author edits and reloads without re-exporting,
    // and having both costs a few kilobytes against a debugging session.
    for r in &app.renderers {
        put(&r.path, r.source.as_bytes())?;
    }
    put(
        "index.html",
        page(&manifest, &sources, &renderers)?.as_bytes(),
    )?;

    Ok(Written { files })
}

/// The exported page: the client, plus the block that tells it to run locally.
fn page(
    manifest: &str,
    sources: &serde_json::Map<String, serde_json::Value>,
    renderers: &serde_json::Map<String, serde_json::Value>,
) -> Result<String, String> {
    let config = serde_json::json!({
        "wasm": "./dagpane.wasm",
        "glue": "./dagpane.js",
        "manifest": manifest,
        "sources": sources,
        "renderers": renderers,
    });
    let json = serde_json::to_string(&config).map_err(|e| e.to_string())?;

    // EVERY `<`, not just the ones starting a closing tag. `\u003c` is inert to a JSON parser
    // and round-trips to `<`, so the data is unchanged; what it removes is the page's ability
    // to be reparsed by its own contents.
    //
    // The narrower `replace("</", "<\\/")` this used to do was not wrong about `</script>` —
    // `</` contains no letters, so it catches `</ScRiPt>` exactly as it catches the lowercase
    // form — but it missed a second exit that does not look like one. Inside a `<script>`,
    // `<!--` puts the tokenizer into the escaped state and a following `<script` into the
    // *double*-escaped state, where the element needs two `</script>` to close rather than
    // one. A CSV cell containing `<!--<script>` therefore swallows the rest of the document:
    // verified in Chromium against an exported bundle, where `window.DAGPANE_LOCAL` came back
    // `undefined` and the page sat at "connecting…" forever.
    //
    // That is a denial of render rather than an injection — the attacker cannot close the
    // element either, since their `</` is escaped too — but an app that blanks on a row of its
    // own data is not a bundle anybody should ship. Escaping every `<` closes the state
    // machine off entirely, which is cheaper to reason about than enumerating its exits.
    let json = json.replace('<', "\\u003c");

    let injected = format!("<script>window.DAGPANE_LOCAL = {json};</script>\n<script>");
    let client = dagpane_serve::CLIENT;
    // The client has exactly one `<script>` and the config has to precede it, because the
    // client reads `window.DAGPANE_LOCAL` while it is evaluating.
    match client.find("<script>") {
        Some(_) => Ok(client.replacen("<script>", &injected, 1)),
        None => Err("the embedded client has no <script> to inject into".to_string()),
    }
}

/// Bytes, for a person. Kibibytes because that is what a file manager shows.
pub fn human(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KIB {
        format!("{bytes} B")
    } else if b < KIB * KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{:.1} MiB", b / (KIB * KIB))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_the_way_a_file_manager_prints_them() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(2048), "2.0 KiB");
        assert_eq!(human(3 * 1024 * 1024), "3.0 MiB");
    }

    #[test]
    fn the_config_cannot_close_the_script_it_is_written_into() {
        // An exported page is a file that gets emailed around, and a cell of somebody's data
        // that silently ends the script produces a blank page nobody can explain.
        //
        // Every string below is a DIFFERENT exit from the script data state, and the third is
        // the one a narrower escape misses:
        //
        //   * `</script>`   — the classic.
        //   * `</ScRiPt>`   — the same exit; the tokenizer matches the tag name case
        //                     insensitively. (An escape keyed on `</` catches this already:
        //                     `</` has no letters. Kept because a future escape that *is*
        //                     keyed on the tag name would not, and would look correct.)
        //   * `<!--<script` — not a closing tag at all. It puts the tokenizer into the
        //                     double-escaped state, where the element needs TWO `</script>`
        //                     to close. Since every `</` here is escaped, it never closes:
        //                     the rest of the document is swallowed and the page never boots.
        //                     Verified in Chromium, where it left `window.DAGPANE_LOCAL`
        //                     undefined and the page at "connecting…".
        let hostile = "</script>|</ScRiPt>|<!--<script>|<b>oops</b>";
        let manifest = format!("[app]\ntitle = \"{hostile}\"\n");
        let html = page(&manifest, &Default::default(), &Default::default()).unwrap();

        let after = html
            .split_once("window.DAGPANE_LOCAL = ")
            .expect("the config block")
            .1;
        let payload = after.split_once("</script>").expect("the block closes").0;

        // One assertion covering all of them, because enumerating exits is the losing game
        // this escape exists to stop playing: a `<` that reaches the page as a `<` is a state
        // machine the data can still drive.
        assert!(
            !payload.contains('<'),
            "a raw `<` reached the page and can reparse it: {payload}"
        );
        assert!(
            payload.contains("\\u003c"),
            "escaped rather than dropped: {payload}"
        );

        // And the data is unchanged — `<` round-trips, so this is an encoding and not a
        // sanitiser. A bundle that quietly rewrote somebody's rows would be worse than one
        // that failed to open.
        let json: serde_json::Value =
            serde_json::from_str(payload.trim_end_matches(';')).expect("still valid JSON");
        assert!(
            json["manifest"]
                .as_str()
                .expect("a manifest")
                .contains(hostile),
            "the payload was altered rather than encoded: {json:?}"
        );
    }

    #[test]
    fn the_config_precedes_the_client_that_reads_it() {
        let html = page(
            "[app]\ntitle = \"t\"\n",
            &Default::default(),
            &Default::default(),
        )
        .unwrap();
        let config = html.find("DAGPANE_LOCAL").expect("the config block");
        let reads = html
            .rfind("window.DAGPANE_LOCAL")
            .expect("the client's own read");
        assert!(
            config < reads,
            "the client evaluates the config while booting"
        );
    }

    #[test]
    fn a_missing_module_is_reported_at_a_path_the_reader_can_resolve() {
        let e = || std::io::Error::new(std::io::ErrorKind::NotFound, "No such file or directory");
        let defaulted = missing_module(Path::new(DEFAULT_WASM), true, e());

        // The whole point. A relative path in this message asks the reader "relative to
        // what?", and the working directory is the one thing they cannot see from it.
        assert!(
            !defaulted.contains(&format!("at {DEFAULT_WASM}")),
            "the default must not be echoed back relative: {defaulted}"
        );
        let cwd = std::env::current_dir().unwrap();
        assert!(
            defaulted.contains(&cwd.display().to_string()),
            "it resolved against {}, so say so: {defaulted}",
            cwd.display()
        );
        assert!(
            defaulted.contains("nothing was passed to --wasm"),
            "the reader has to learn this path was a guess: {defaulted}"
        );
        assert!(
            defaulted.contains("An installed `dagpane`"),
            "the case this default cannot serve is the one worth naming: {defaulted}"
        );

        // A path the caller chose is a different failure: they know where they pointed, so
        // the sentences explaining where the default came from would be answering nobody.
        let asked = missing_module(Path::new("elsewhere/dagpane_wasm.wasm"), false, e());
        assert!(!asked.contains("nothing was passed"), "{asked}");
        assert!(!asked.contains("An installed `dagpane`"), "{asked}");
        assert!(
            asked.contains("elsewhere/dagpane_wasm.wasm"),
            "the path they gave is still the subject: {asked}"
        );
        assert!(
            asked.contains("cargo build -p dagpane-wasm"),
            "how to produce one is useful either way: {asked}"
        );
    }
}
