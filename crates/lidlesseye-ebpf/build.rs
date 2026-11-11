//! Compiles the eBPF object, falling back to the checked-in prebuilt object
//! when clang is not available.
//!
//! A prebuilt `bpf/profile.bpf.o` is committed so that `cargo install
//! lidlesseye` and downstream crates work without a BPF toolchain. Set
//! `LIDLESSEYE_BUILD_BPF=1` to force a rebuild (needed when editing the C
//! source), or `LIDLESSEYE_PREBUILT_BPF=1` to always use the prebuilt object.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let src = manifest_dir.join("bpf/profile.bpf.c");
    let prebuilt = manifest_dir.join("bpf/profile.bpf.o");
    let target = out_dir.join("profile.bpf.o");

    println!("cargo:rerun-if-changed=bpf/profile.bpf.c");
    println!("cargo:rerun-if-env-changed=LIDLESSEYE_BUILD_BPF");
    println!("cargo:rerun-if-env-changed=LIDLESSEYE_PREBUILT_BPF");
    println!("cargo:rerun-if-env-changed=CC");

    // eBPF only exists on Linux; other targets never compile the collector.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        std::fs::write(&target, []).unwrap();
        return;
    }

    let force_prebuilt = std::env::var("LIDLESSEYE_PREBUILT_BPF").as_deref() == Ok("1");
    let clang = std::env::var("CC").unwrap_or_else(|_| "clang".to_string());

    if !force_prebuilt {
        let status = Command::new(&clang)
            .args([
                "-O2",
                "-g",
                "-Wall",
                "-Werror",
                "-target",
                "bpf",
                "-fno-omit-frame-pointer",
                "-c",
            ])
            .arg(&src)
            .arg("-o")
            .arg(&target)
            .status();

        match status {
            Ok(status) if status.success() => {
                // Keep the checked-in object in sync so that users without
                // clang get an up to date binary.
                if let Err(err) = std::fs::copy(&target, &prebuilt) {
                    println!("cargo:warning=could not refresh bpf/profile.bpf.o: {err}");
                }
                println!("cargo:warning=compiled eBPF program from source");
                return;
            }
            Ok(status) => panic!(
                "clang failed to compile {} ({status}). Fix the eBPF source or build with \
                 LIDLESSEYE_PREBUILT_BPF=1.",
                src.display()
            ),
            Err(err) => {
                println!(
                    "cargo:warning=clang not usable ({err}); falling back to the prebuilt eBPF object"
                );
            }
        }
    }

    copy_prebuilt(&prebuilt, &target);
}

fn copy_prebuilt(prebuilt: &Path, target: &Path) {
    match std::fs::metadata(prebuilt) {
        Ok(meta) if meta.len() > 0 => {
            std::fs::copy(prebuilt, target).unwrap_or_else(|err| {
                panic!("failed to copy {}: {err}", prebuilt.display());
            });
        }
        _ => panic!(
            "no prebuilt eBPF object at {}.\n\
             Install clang (apt install clang) and rebuild, or commit a prebuilt object.",
            prebuilt.display()
        ),
    }
}
