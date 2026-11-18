//! Materialises the dashboard inside the crate before rust-embed expands its
//! `#[folder]` attribute.
//!
//! The export itself is produced outside the crate by `make ui` (or the
//! Dockerfile's Node stage) into `web/dist`, which is a build artefact and not
//! committed. rust-embed needs a directory that exists at compile time and that
//! travels with the package, so `ui/` is that directory and this script mirrors
//! `web/dist` into it:
//!
//! * in a checkout, `ui/` is a mirror of the export, refreshed whenever the
//!   export changes (and ignored by git);
//! * in a published crate, `ui/` was packed by `cargo package` and there is no
//!   `web/dist`, so the packed copy is what gets embedded;
//! * in a bare checkout with no Node, neither exists and an empty `ui/` keeps
//!   `cargo build` working — the server then answers with its "UI is not
//!   embedded in this build" placeholder, which is a visible symptom instead of
//!   a compile error in an unrelated crate.
//!
//! Only changed files are rewritten: touching every file on every build would
//! make rust-embed re-expand the folder and rebuild the crate each time.

use std::io;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set"),
    );
    let export = manifest.join("../../web/dist");
    let ui = manifest.join("ui");

    // Re-expand the embedded files when the dashboard is rebuilt.
    println!("cargo:rerun-if-changed=../../web/dist");

    let has_export = export.join("index.html").is_file();
    let has_ui = ui.join("index.html").is_file();

    if has_export {
        match mirror(&export, &ui) {
            Ok(changed) => {
                if changed {
                    println!("cargo:warning=refreshed ui/ from web/dist");
                }
            }
            Err(err) => println!(
                "cargo:warning=could not mirror {} into {}: {err}",
                export.display(),
                ui.display()
            ),
        }
        return;
    }

    if has_ui {
        // Packaged crate: the export is already inside the package.
        return;
    }

    if let Err(err) = std::fs::create_dir_all(&ui) {
        println!("cargo:warning=could not create {}: {err}", ui.display());
    }
    println!(
        "cargo:warning=ui/ has no dashboard; run `make ui` (needs Node) or this binary will serve a placeholder"
    );
}

/// Copy `src` over `dst`, writing only files whose bytes differ and dropping
/// files that no longer exist in `src`. Returns whether anything changed.
fn mirror(src: &Path, dst: &Path) -> io::Result<bool> {
    let mut changed = false;
    let mut keep: Vec<PathBuf> = Vec::new();
    collect(src, src, &mut keep)?;

    for rel in &keep {
        let from = src.join(rel);
        let to = dst.join(rel);
        let same = match (std::fs::read(&from), std::fs::read(&to)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if same {
            continue;
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&from, &to)?;
        changed = true;
    }

    // Prune files the export no longer contains: a removed route would
    // otherwise stay in the binary forever.
    let mut stale: Vec<PathBuf> = Vec::new();
    collect(dst, dst, &mut stale)?;
    for rel in stale {
        if !keep.contains(&rel) {
            std::fs::remove_file(dst.join(&rel))?;
            changed = true;
        }
    }

    Ok(changed)
}

/// Every file under `dir`, as paths relative to `base`. Missing directories
/// yield nothing, which is what makes the first run work.
fn collect(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect(base, &path, out)?;
        } else if let Ok(rel) = path.strip_prefix(base) {
            out.push(rel.to_path_buf());
        }
    }
    Ok(())
}
