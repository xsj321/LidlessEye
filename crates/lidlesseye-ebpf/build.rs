//! Compiles the eBPF objects, falling back to the checked-in prebuilt ones
//! when clang is not available.
//!
//! Prebuilt `bpf/*.bpf.o` files are committed so that `cargo install
//! lidlesseye` and downstream crates work without a BPF toolchain. Set
//! `LIDLESSEYE_BUILD_BPF=1` to force a rebuild (needed when editing a `.c`
//! source), or `LIDLESSEYE_PREBUILT_BPF=1` to always use the prebuilt objects.
//!
//! Two objects are built, and they are loaded separately on purpose:
//! `profile.bpf.o` (sampling) and `net.bpf.o` (connection accounting). A kernel
//! that rejects one must not take the other down with it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// `(source, prebuilt, defines)` for each object.
const OBJECTS: &[(&str, &str)] = &[
    ("bpf/profile.bpf.c", "bpf/profile.bpf.o"),
    ("bpf/net.bpf.c", "bpf/net.bpf.o"),
];

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    for (source, _) in OBJECTS {
        println!("cargo:rerun-if-changed={source}");
    }
    println!("cargo:rerun-if-env-changed=LIDLESSEYE_BUILD_BPF");
    println!("cargo:rerun-if-env-changed=LIDLESSEYE_PREBUILT_BPF");
    println!("cargo:rerun-if-env-changed=CC");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let target_endian = std::env::var("CARGO_CFG_TARGET_ENDIAN").unwrap_or_default();

    // The crate embeds these objects with `include_bytes!`, and cargo checks a
    // unit's inputs *before* the build script rewrites them. Without a changing
    // line here, a build that recompiles the object can still reuse the crate's
    // previous rlib and embed the old bytes — which is exactly how a fixed
    // probe kept failing to load from a freshly built image.
    let mut digest = 0xcbf29ce484222325u64;

    for (source, prebuilt) in OBJECTS {
        // The output keeps the prebuilt's name: `include_bytes!` in the crate
        // refers to `net.bpf.o`, not to the `.c` it was compiled from.
        let name = Path::new(prebuilt)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(prebuilt);
        let out = out_dir.join(name);

        // eBPF only exists on Linux; other targets never compile the collector.
        if target_os != "linux" {
            std::fs::write(&out, []).unwrap();
            continue;
        }

        let mut defines: Vec<String> = Vec::new();
        if target_endian == "little" {
            defines.push("-DLIDLESSEYE_LE=1".to_string());
        }
        defines.push(format!("-DLIDLESSEYE_ARCH_{}", target_arch.to_uppercase()));

        build_object(
            &manifest_dir,
            source,
            prebuilt,
            &out,
            &defines,
            &std::env::var("CC").unwrap_or_else(|_| "clang".to_string()),
        );
        if let Ok(bytes) = std::fs::read(&out) {
            for byte in bytes {
                digest ^= u64::from(byte);
                digest = digest.wrapping_mul(0x100000001b3);
            }
        }
    }
    println!("cargo:rustc-env=LIDLESSEYE_BPF_DIGEST={digest:016x}");
}

fn build_object(
    manifest_dir: &Path,
    source: &str,
    prebuilt: &str,
    out: &Path,
    defines: &[String],
    clang: &str,
) {
    let src = manifest_dir.join(source);
    let prebuilt = manifest_dir.join(prebuilt);

    if std::env::var("LIDLESSEYE_PREBUILT_BPF").as_deref() == Ok("1") {
        copy_prebuilt(&prebuilt, out);
        return;
    }

    let status = Command::new(clang)
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
        .args(defines)
        .arg(&src)
        .arg("-o")
        .arg(out)
        .status();

    match status {
        Ok(status) if status.success() => {
            // Keep the checked-in object in sync so that users without clang
            // get an up to date binary.
            if let Err(err) = std::fs::copy(out, &prebuilt) {
                println!(
                    "cargo:warning=could not refresh {}: {err}",
                    prebuilt.display()
                );
            }
            println!("cargo:warning=compiled {source} from source");
        }
        Ok(status) => panic!(
            "clang failed to compile {} ({status}). Fix the eBPF source or build with \
             LIDLESSEYE_PREBUILT_BPF=1.",
            src.display()
        ),
        Err(err) => {
            println!(
                "cargo:warning=clang not usable ({err}); falling back to the prebuilt object for {source}"
            );
            copy_prebuilt(&prebuilt, out);
        }
    }
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
