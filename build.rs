mod build_version;

use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set"));
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_version.rs");
    for path in build_version::watch_paths(&manifest_dir) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!(
        "cargo:rustc-env=HARC_BUILD_VERSION={}",
        build_version::version(&manifest_dir, &env::var("CARGO_PKG_VERSION").unwrap())
    );
}
