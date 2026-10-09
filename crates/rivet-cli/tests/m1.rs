//! M1 acceptance tests: `rivet new hello && rivet build && rivet run`
//! against a real C++ compiler in temp dirs.

use std::ffi::OsString;
use std::path::PathBuf;

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
fn future_commands_fail_with_milestone_pointer() {
    rivet()
        .arg("publish")
        .assert()
        .failure()
        .stderr(predicate::str::contains("M5").or(predicate::str::contains("not implemented")));
    let _ = OsString::from("keep-import");
}
