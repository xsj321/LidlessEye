//! Prepares `web/dist` before rust-embed expands its `#[folder]` attribute.
//!
//! The dashboard is a build artefact now: `make ui` (or the Dockerfile's Node
//! stage) produces `web/dist`, and it is not committed. rust-embed resolves the
//! folder at compile time and fails when the path is missing, so a fresh
//! checkout would not build at all. Creating the directory here keeps
//! `cargo build` working: an empty one makes the server answer with its
//! "UI is not embedded in this build" placeholder, which is a visible symptom
//! instead of a compile error in an unrelated crate.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set"),
    );
    let dist = manifest.join("../../web/dist");

    if !dist.is_dir()
        && let Err(err) = std::fs::create_dir_all(&dist)
    {
        println!("cargo:warning=could not create {}: {err}", dist.display());
    }

    if !dist.join("index.html").is_file() {
        println!(
            "cargo:warning=web/dist has no dashboard build; run `make ui` (needs Node) or this binary will serve a placeholder"
        );
    }

    // Re-expand the embedded files when the dashboard is rebuilt.
    println!("cargo:rerun-if-changed=../../web/dist");
}
