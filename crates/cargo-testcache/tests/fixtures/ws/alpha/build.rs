//! Gives `alpha`'s test binaries what a build script can: a `rustc-env` variable, and a native
//! library directory under `OUT_DIR` that Cargo adds to the dynamic library path. The
//! conformance test checks the direct runner passes both on as `cargo test` does.

use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let native = out_dir.join("native");
    std::fs::create_dir_all(&native).expect("create OUT_DIR/native");
    println!("cargo:rustc-env=ALPHA_BUILD_ENV=1");
    println!("cargo:rustc-link-search=native={}", native.display());
    println!("cargo:rerun-if-changed=build.rs");
}
