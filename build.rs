mod build_version;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn source_paths(dir: &Path, paths: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            source_paths(&path, paths)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            paths.push(path);
        }
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
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
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=build.rs");
    let mut paths = Vec::new();
    source_paths(Path::new("src"), &mut paths)?;
    paths.sort();
    let mut hash = 0xcbf29ce484222325u64;
    for path in paths {
        for byte in path
            .to_string_lossy()
            .as_bytes()
            .iter()
            .copied()
            .chain([0])
            .chain(fs::read(&path)?.into_iter().chain([0]))
        {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
    }
    for byte in fs::read("Cargo.lock")?.into_iter().chain([0]) {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    for byte in fs::read("build.rs")?.into_iter().chain([0]) {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    println!("cargo:rustc-env=HARC_GRAPH_GENERATOR_ID={hash:016x}");
    Ok(())
}
