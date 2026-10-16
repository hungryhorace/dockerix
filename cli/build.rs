//! Packs the image's build context (src/context.rs) into OUT_DIR, with its
//! sha256, for the binary to embed.

#[path = "src/context.rs"]
mod context;

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn main() {
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let root = manifest.parent().unwrap_or(Path::new("/"));
    // The image's cli-build stage has only cli/: its binary is the agent,
    // which never builds an image, so it embeds an empty context.
    let tar = if root.join("Dockerfile").exists() {
        for rel in context::ROOTS {
            println!("cargo:rerun-if-changed={}", root.join(rel).display());
        }
        let members = context::collect(root).unwrap_or_else(|e| panic!("image context: {e}"));
        context::archive(&members).unwrap_or_else(|e| panic!("image context: {e}"))
    } else {
        println!("cargo:rerun-if-changed=build.rs");
        Vec::new()
    };
    let hash = if tar.is_empty() {
        String::new()
    } else {
        Sha256::digest(&tar).iter().map(|b| format!("{b:02x}")).collect()
    };
    std::fs::write(out.join("context.tar"), &tar).expect("write context.tar");
    std::fs::write(out.join("context.sha256"), hash).expect("write context.sha256");
}
