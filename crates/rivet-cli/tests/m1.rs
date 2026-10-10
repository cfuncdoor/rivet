//! Acceptance tests: `rivet new hello && rivet build && rivet run`
//! against a real C++ compiler in temp dirs.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use assert_cmd::cargo::cargo_bin;
use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn rivet() -> Command {
    Command::new(cargo_bin("rivet"))
}

fn toolchain_available() -> bool {
    rivet_toolchain::detect().is_ok()
}

fn new_project(dir: &TempDir, name: &str) {
    rivet()
        .current_dir(dir.path())
        .arg("new")
        .arg(name)
        .assert()
        .success()
        .stdout(predicate::str::contains("Created"));
}

#[test]
fn new_build_run_single_file_bin() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    new_project(&dir, "hello");

    let pkg: PathBuf = dir.path().join("hello");
    assert!(pkg.join("rivet.json").is_file());
    assert!(pkg.join("src").join("main.cpp").is_file());

    // Build.
    rivet()
        .current_dir(&pkg)
        .arg("build")
        .assert()
        .success()
        .stdout(predicate::str::contains("Compiling").and(predicate::str::contains("Finished")));

    let exe = if cfg!(windows) {
        pkg.join("target").join("debug").join("hello.exe")
    } else {
        pkg.join("target").join("debug").join("hello")
    };
    assert!(exe.is_file(), "expected exe at {}", exe.display());

    // Run (binary output passes through).
    rivet()
        .current_dir(&pkg)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("Hello, world!"));
}

#[test]
fn build_release_profile() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    new_project(&dir, "hello");
    let pkg: PathBuf = dir.path().join("hello");

    rivet()
        .current_dir(&pkg)
        .arg("build")
        .arg("--release")
        .assert()
        .success()
        .stdout(predicate::str::contains("release"));

    let exe = if cfg!(windows) {
        pkg.join("target").join("release").join("hello.exe")
    } else {
        pkg.join("target").join("release").join("hello")
    };
    assert!(exe.is_file(), "expected exe at {}", exe.display());
}

#[test]
fn run_forwards_binary_args() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    new_project(&dir, "hello");
    let pkg: PathBuf = dir.path().join("hello");
    // Binary echoes argv (overwrite the hello template).
    std::fs::write(
        pkg.join("src").join("main.cpp"),
        "#include <iostream>\nint main(int argc, char** argv) {\n  for (int i = 1; i < argc; ++i) std::cout << argv[i] << '\\n';\n  return 0;\n}\n",
    )
    .unwrap();

    rivet()
        .current_dir(&pkg)
        .arg("run")
        .arg("--")
        .arg("ping123")
        .assert()
        .success()
        .stdout(predicate::str::contains("ping123"));
}

#[test]
fn compiler_errors_reported() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    new_project(&dir, "hello");
    let pkg: PathBuf = dir.path().join("hello");
    std::fs::write(pkg.join("src").join("main.cpp"), "this is not c++\n").unwrap();

    rivet().current_dir(&pkg).arg("build").assert().failure();
}

#[test]
fn manifest_errors_are_actionable() {
    let dir = TempDir::new().unwrap();
    new_project(&dir, "hello");
    let pkg: PathBuf = dir.path().join("hello");
    std::fs::write(pkg.join("rivet.json"), "{ not json").unwrap();

    rivet()
        .current_dir(&pkg)
        .arg("build")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("invalid JSON").or(predicate::str::contains("rivet.json")),
        );
}

#[test]
fn toolchains_lists_default() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    rivet()
        .arg("toolchains")
        .assert()
        .success()
        .stdout(predicate::str::contains("Toolchain").and(predicate::str::contains("default")));
}

#[test]
fn clean_removes_target() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    new_project(&dir, "hello");
    let pkg: PathBuf = dir.path().join("hello");
    rivet().current_dir(&pkg).arg("build").assert().success();
    assert!(pkg.join("target").is_dir());
    rivet().current_dir(&pkg).arg("clean").assert().success();
    assert!(!pkg.join("target").exists());
}

#[test]
fn json_message_format_emits_json_lines() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    new_project(&dir, "hello");
    let pkg: PathBuf = dir.path().join("hello");
    rivet()
        .current_dir(&pkg)
        .arg("--message-format=json")
        .arg("build")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("\"type\":\"status\"")
                .or(predicate::str::contains("\"type\": \"status\"")),
        );
}

#[test]
fn future_commands_fail_actionably() {
    // Inside a project scope so `publish` reaches its real handler.
    let dir = TempDir::new().unwrap();
    rivet()
        .current_dir(dir.path())
        .arg("new")
        .arg("hello")
        .assert()
        .success();
    // Publish is implemented: without a registry it fails actionably;
    // --dry-run shows what would upload.
    rivet()
        .current_dir(dir.path().join("hello"))
        .arg("publish")
        .arg("--dry-run")
        .arg("--registry")
        .arg("https://example.com")
        .assert()
        .success()
        .stdout(predicate::str::contains("would upload"));
    let _ = OsString::from("keep-import");
}

#[test]
fn rivet_cxx_bogus_override_fails_actionably() {
    // No toolchain needed: the override fails before any probing.
    let dir = TempDir::new().unwrap();
    new_project(&dir, "hello");
    let pkg: PathBuf = dir.path().join("hello");
    rivet()
        .current_dir(&pkg)
        .env("RIVET_CXX", "/nonexistent/rivet-test-driver-xyz")
        .arg("build")
        .assert()
        .failure()
        .stderr(predicate::str::contains("RIVET_CXX"));
}

fn find_archive(pkg: &Path) -> Option<PathBuf> {
    let mut found = None;
    let mut stack = vec![pkg.join("target")];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "lib" || e == "a") {
                // Skip import libraries next to a DLL of the same stem.
                if path.extension().is_some_and(|e| e == "lib")
                    && path.with_extension("dll").is_file()
                {
                    continue;
                }
                found = Some(path);
            }
        }
    }
    found
}

#[test]
fn new_lib_scaffolds_and_builds() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    rivet()
        .current_dir(dir.path())
        .arg("new")
        .arg("--lib")
        .arg("mylib")
        .assert()
        .success()
        .stdout(predicate::str::contains("library package"));
    let pkg: PathBuf = dir.path().join("mylib");
    assert!(pkg.join("src").join("lib.cpp").is_file());
    assert!(pkg.join("include").join("mylib.h").is_file());
    assert!(!pkg.join("src").join("main.cpp").exists());
    let manifest = std::fs::read_to_string(pkg.join("rivet.json")).unwrap();
    assert!(manifest.contains("\"lib\""), "got: {manifest}");
    rivet()
        .current_dir(&pkg)
        .arg("build")
        .assert()
        .success()
        .stdout(predicate::str::contains("Finished"));
    let archive = find_archive(&pkg).expect("static archive must be produced");
    assert!(
        archive.extension().is_some_and(|e| e == "lib" || e == "a"),
        "got: {}",
        archive.display()
    );
}

#[test]
fn new_shared_and_init_lib() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    rivet()
        .current_dir(dir.path())
        .arg("new")
        .arg("--shared")
        .arg("shlib")
        .assert()
        .success()
        .stdout(predicate::str::contains("shared-library package"));
    let pkg: PathBuf = dir.path().join("shlib");
    let manifest = std::fs::read_to_string(pkg.join("rivet.json")).unwrap();
    assert!(manifest.contains("\"shared\""), "got: {manifest}");
    // Shared linking needs the export/SONAME story (tracked): the build
    // must fail actionably, not silently.
    rivet()
        .current_dir(&pkg)
        .arg("build")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not supported yet"));

    // `init --lib` scaffolds in the current directory.
    let init_dir = dir.path().join("initlib");
    std::fs::create_dir_all(&init_dir).unwrap();
    rivet()
        .current_dir(&init_dir)
        .arg("init")
        .arg("--lib")
        .assert()
        .success();
    assert!(init_dir.join("src").join("lib.cpp").is_file());
}

#[test]
fn new_lib_and_shared_conflict() {
    let dir = TempDir::new().unwrap();
    rivet()
        .current_dir(dir.path())
        .arg("new")
        .arg("--lib")
        .arg("--shared")
        .arg("both")
        .assert()
        .failure();
}
