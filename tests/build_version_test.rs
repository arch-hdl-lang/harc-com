//! `harc --version` carries build provenance: `0.3.0 (git <commit>[+dirty])`,
//! or `(git unknown)` outside a Git checkout. Same scheme as arch-com.
#[path = "../build_version.rs"]
mod build_version;
use std::{fs, path::Path, process::Command};

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=Version Test",
            "-c",
            "user.email=version@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn provenance_tracks_clean_dirty_staged_restored_and_worktree_heads() {
    let tmp = std::env::temp_dir().join(format!(
        "harc_build_version_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let root = tmp.join("repo");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-b", "main"]);
    fs::write(root.join("source"), "original").unwrap();
    git(&root, &["add", "source"]);
    git(&root, &["commit", "-m", "Initial fixture"]);
    let head = git(&root, &["rev-parse", "--short=12", "HEAD"]);
    assert_eq!(
        build_version::version(&root, "0.3.0"),
        format!("0.3.0 (git {head})")
    );
    fs::write(root.join("source"), "edit").unwrap();
    assert!(build_version::version(&root, "0.3.0").ends_with("+dirty)"));
    git(&root, &["add", "source"]);
    assert!(build_version::version(&root, "0.3.0").ends_with("+dirty)"));
    git(&root, &["reset", "--hard", "HEAD"]);
    assert!(!build_version::version(&root, "0.3.0").contains("dirty"));
    let wt = tmp.join("worktree");
    git(
        &root,
        &["worktree", "add", "--detach", wt.to_str().unwrap(), "HEAD"],
    );
    assert_eq!(
        build_version::version(&wt, "0.3.0"),
        format!("0.3.0 (git {head})")
    );
    let watches = build_version::watch_paths(&wt);
    assert!(watches.contains(&wt.join("source")));
    assert!(watches
        .iter()
        .any(|p| p.ends_with("HEAD") && p.to_string_lossy().contains("worktrees")));
    fs::write(wt.join("source"), "new revision").unwrap();
    git(&wt, &["commit", "-am", "Second fixture"]);
    assert_ne!(
        build_version::version(&wt, "0.3.0"),
        build_version::version(&root, "0.3.0")
    );
    // After `git pack-refs` the branch ref has no loose file: every watched
    // path must still exist, or cargo reruns the build script on every build.
    git(&root, &["pack-refs", "--all"]);
    let watches = build_version::watch_paths(&root);
    for path in &watches {
        assert!(
            path.exists(),
            "watched path does not exist: {}",
            path.display()
        );
    }
    // ...while the packed branch ref's directory is still watched, so the
    // loose ref reappearing (next commit / reset) triggers a rebuild.
    let heads = root.join(".git/refs/heads").canonicalize().unwrap();
    assert!(
        watches
            .iter()
            .any(|p| p.canonicalize().ok().as_deref() == Some(heads.as_path())),
        "packed branch ref: refs/heads not watched in {watches:?}"
    );
    let archive = root.join("source-archive");
    fs::create_dir(&archive).unwrap();
    assert_eq!(
        build_version::version(&archive, "0.3.0"),
        "0.3.0 (git unknown)"
    );
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn harc_version_flag_reports_build_provenance() {
    let out = Command::new(env!("CARGO_BIN_EXE_harc"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let expected_prefix = format!("harc {} (git ", env!("CARGO_PKG_VERSION"));
    assert!(
        text.trim().starts_with(&expected_prefix) && text.trim().ends_with(')'),
        "unexpected --version output: {text:?}"
    );
}
