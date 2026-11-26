//! The BCPack format tables are constants of the format, not this project's
//! work, and are never committed (see `docs/asset-policy.md`). They are taken
//! at build time from `AURA_BCPACK_TABLES` (a file path) or from
//! `local/bcpack-tables.bin` under the runtime root (ignored by git); without
//! either the build goes on and BCPack streams cannot be decoded
//! (`Tables::builtin` says so).

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=AURA_BCPACK_TABLES");
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let local = manifest.join("../../local/bcpack-tables.bin");
    println!("cargo:rerun-if-changed={}", local.display());
    let source = std::env::var_os("AURA_BCPACK_TABLES").map(PathBuf::from).unwrap_or(local);
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets it")).join("bcpack-tables.bin");
    let bytes = std::fs::read(&source).unwrap_or_else(|_| {
        println!(
            "cargo:warning=no BCPack tables at {} (set AURA_BCPACK_TABLES): compressed textures will not decode",
            source.display()
        );
        Vec::new()
    });
    std::fs::write(&out, bytes).expect("OUT_DIR is writable");
}
