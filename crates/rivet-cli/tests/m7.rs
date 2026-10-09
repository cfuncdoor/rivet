//! M7 acceptance tests: build scripts, artifact cache, `--target`.
//! Hermetic tempdirs; the buildscript demo is copied out of `examples/`
//! so the repo checkout stays clean.

use std::path::{Path, PathBuf};

use assert_cmd::cargo::cargo_bin;
use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn rivet(home: &Path) -> Command {
    let mut cmd = Command::new(cargo_bin("rivet"));
    cmd.env("RIVET_HOME", home);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd
}

fn toolchain_available() -> bool {
    rivet_toolchain::detect().is_ok()
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    let mut entries: Vec<_> = std::fs::read_dir(src)
        .unwrap()
        .map(|e| e.unwrap())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            if entry.file_name() == "target" {
                continue;
            }
            copy_dir(&from, &to);
        } else {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}

fn example_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples")
        .join(name)
}

#[test]
fn buildscript_demo_end_to_end() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let work = dir.path().join("demo");
    copy_dir(&example_dir("buildscript-demo"), &work);

    rivet(&home)
        .current_dir(&work)
        .arg("build")
        .assert()
        .success();
    rivet(&home)
        .current_dir(&work)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("0.1.0-generated"));

    // No changes: Fresh (the script does not rerun).
    rivet(&home)
        .current_dir(&work)
        .arg("build")
        .assert()
        .success()
        .stdout(predicate::str::contains("Fresh"));

    // Changing the script reruns it (content hash drives the rerun rule).
    // Appending a comment changes nothing downstream, so the main build
    // stays Fresh — but -v shows the script ran.
    let script = work.join("build.rivet.cpp");
    let mut content = std::fs::read(&script).unwrap();
    content.extend_from_slice(b"\n// rebuilt\n");
    std::fs::write(&script, content).unwrap();
    rivet(&home)
        .current_dir(&work)
        .arg("build")
        .arg("-v")
        .assert()
        .success()
        .stderr(predicate::str::contains("build.rivet.cpp"));
}

#[test]
fn artifact_cache_hit_after_wipe() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let app = dir.path().join("app");
    std::fs::create_dir_all(app.join("src")).unwrap();
    std::fs::write(
        app.join("rivet.json"),
        "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"}}",
    )
    .unwrap();
    std::fs::write(
        app.join("src").join("main.cpp"),
        "#include <iostream>\nint main() { std::cout << 7 << '\\n'; }\n",
    )
    .unwrap();

    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success();
    // Wipe all build outputs; the cache must restore them.
    std::fs::remove_dir_all(app.join("target")).unwrap();
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success()
        .stdout(predicate::str::contains("Cached"));
    rivet(&home)
        .current_dir(&app)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains('7'));
}

#[test]
fn target_native_builds_bogus_rejected() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let app = dir.path().join("app");
    std::fs::create_dir_all(app.join("src")).unwrap();
    std::fs::write(
        app.join("rivet.json"),
        "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"}}",
    )
    .unwrap();
    std::fs::write(
        app.join("src").join("main.cpp"),
        "int main() { return 0; }\n",
    )
    .unwrap();

    // Explicit native triple behaves like a default build.
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .arg("--target")
        .arg("native")
        .assert()
        .success();

    // Garbage triples fail actionably (never silently native).
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .arg("--target")
        .arg("not-a-triple")
        .assert()
        .failure()
        .stderr(predicate::str::contains("arch-vendor-os").or(predicate::str::contains("target")));
}
