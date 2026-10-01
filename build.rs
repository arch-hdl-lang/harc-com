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
    println!("cargo:rerun-if-changed=src");
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
    println!("cargo:rustc-env=HARC_GRAPH_GENERATOR_ID={hash:016x}");
    Ok(())
}
