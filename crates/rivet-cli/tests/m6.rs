//! M6 acceptance tests: foreign adapters end-to-end (CMake), system
//! probing errors, git+adapter combos. CMake tests skip when `cmake` is
//! absent; the real-zlib test additionally needs `RIVET_TEST_NETWORK=1`.

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

fn cmake_available() -> bool {
    std::process::Command::new("cmake")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn network_opted_in() -> bool {
    std::env::var("RIVET_TEST_NETWORK").is_ok()
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
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

fn git(cwd: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-c")
        .arg("protocol.file.allow=always")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("git binary must exist for M6 tests");
    assert!(status.success(), "git {args:?} failed in {}", cwd.display());
}

fn file_url(p: &Path) -> String {
    let s = p.display().to_string().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

#[test]
fn cmake_path_dep_end_to_end() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    if !cmake_available() {
        eprintln!("SKIP: cmake not on PATH");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    // Hermetic mini cmake lib (no rivet.json — foreign source).
    let mini = dir.path().join("mini");
    write(
        &mini.join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.16)\nproject(mini CXX)\nadd_library(mini STATIC src/mini.cpp)\ntarget_include_directories(mini PUBLIC include)\n",
    );
    write(
        &mini.join("include").join("mini").join("mini.h"),
        "#pragma once\nnamespace mini { int add(int a, int b); }\n",
    );
    write(
        &mini.join("src").join("mini.cpp"),
        "#include \"mini/mini.h\"\nnamespace mini { int add(int a, int b) { return a + b; } }\n",
    );
    let app = dir.path().join("app");
    write(
        &app.join("rivet.json"),
        "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"},\"dependencies\":{\"mini\":{\"path\":\"../mini\",\"adapter\":{\"tool\":\"cmake\",\"targets\":[\"mini\"]},\"package\":{\"name\":\"mini\",\"version\":\"0.3.0\"}}}}",
    );
    write(
        &app.join("src").join("main.cpp"),
        "#include <cstdio>\n#include \"mini/mini.h\"\nint main() { std::printf(\"%d\\n\", mini::add(40, 2)); }\n",
    );

    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success();
    rivet(&home)
        .current_dir(&app)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));

    // Rebuild is Fresh (adapter fingerprint gate); editing the foreign
    // source rebuilds through the adapter.
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success()
        .stdout(predicate::str::contains("Fresh"));
    write(
        &mini.join("src").join("mini.cpp"),
        "#include \"mini/mini.h\"\nnamespace mini { int add(int a, int b) { return a + b; } } // v2\n",
    );
    rivet(&home)
        .current_dir(&app)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));
}

#[test]
fn cmake_mini_example_builds() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    if !cmake_available() {
        eprintln!("SKIP: cmake not on PATH");
        return;
    }
    // Copy (never build inside the repo checkout).
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let example = manifest
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples")
        .join("cmake-mini");
    assert!(example.is_dir(), "examples/cmake-mini must exist");
    let dir = TempDir::new().unwrap();
    let work = dir.path().join("cmake-mini");
    copy_dir(&example, &work);
    let home = dir.path().join("home");
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
        .stdout(predicate::str::contains("mini add: 5"));
}

#[test]
fn git_cmake_dep_end_to_end() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    if !cmake_available() {
        eprintln!("SKIP: cmake not on PATH");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    // Foreign cmake lib in a git repo (no rivet.json).
    let bare = dir.path().join("foreign.git");
    let work = dir.path().join("foreign-work");
    git(
        dir.path(),
        &["init", "--bare", "--", &bare.to_string_lossy()],
    );
    git(
        dir.path(),
        &[
            "clone",
            "--",
            &bare.to_string_lossy(),
            &work.to_string_lossy(),
        ],
    );
    write(
        &work.join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.16)\nproject(foreign CXX)\nadd_library(foreign STATIC src/foreign.cpp)\ntarget_include_directories(foreign PUBLIC include)\n",
    );
    write(
        &work.join("include").join("foreign.h"),
        "#pragma once\nint fanswer();\n",
    );
    write(
        &work.join("src").join("foreign.cpp"),
        "#include \"foreign.h\"\nint fanswer() { return 7; }\n",
    );
    git(&work, &["add", "-A"]);
    git(
        &work,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "init",
        ],
    );
    git(&work, &["tag", "v1"]);
    let branch = {
        let out = std::process::Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(&work)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    git(&work, &["push", "origin", &branch, "--tags"]);

    let app = dir.path().join("app");
    write(
        &app.join("rivet.json"),
        &format!(
            "{{\"package\":{{\"name\":\"app\",\"version\":\"0.1.0\"}},\"dependencies\":{{\"foreign\":{{\"git\":\"{}\",\"tag\":\"v1\",\"adapter\":{{\"tool\":\"cmake\"}},\"package\":{{\"name\":\"foreign\",\"version\":\"1.0.0\"}}}}}}}}",
            file_url(&bare)
        ),
    );
    write(
        &app.join("src").join("main.cpp"),
        "#include <iostream>\n#include \"foreign.h\"\nint main() { std::cout << fanswer() << '\\n'; }\n",
    );
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success();
    assert!(app.join("rivet.lock.json").is_file());
    rivet(&home)
        .current_dir(&app)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains('7'));
}

#[test]
fn missing_system_lib_errors_actionably() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let app = dir.path().join("app");
    write(
        &app.join("rivet.json"),
        "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"},\"dependencies\":{\"nosuchlib\":{\"system\":true}}}",
    );
    write(
        &app.join("src").join("main.cpp"),
        "int main() { return 0; }\n",
    );
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .failure()
        .stderr(predicate::str::contains("nosuchlib").or(predicate::str::contains("not found")));
}

#[test]
fn zlib_via_git_cmake() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    if !cmake_available() {
        eprintln!("SKIP: cmake not on PATH");
        return;
    }
    if !network_opted_in() {
        eprintln!("SKIP: set RIVET_TEST_NETWORK=1 to run the live-zlib test");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let app = dir.path().join("app");
    write(
        &app.join("rivet.json"),
        "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"},\"dependencies\":{\"zlib\":{\"git\":\"https://github.com/madler/zlib.git\",\"tag\":\"v1.3.1\",\"adapter\":{\"tool\":\"cmake\",\"options\":{\"BUILD_SHARED_LIBS\":\"OFF\"}},\"package\":{\"name\":\"zlib\",\"version\":\"1.3.1\"}}}}",
    );
    write(
        &app.join("src").join("main.cpp"),
        "#include <cstdio>\n#include <cstring>\n#include \"zlib.h\"\nint main() { std::printf(\"%s\\n\", zlibVersion()); }\n",
    );
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success();
    rivet(&home)
        .current_dir(&app)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("1.3.1"));
}
