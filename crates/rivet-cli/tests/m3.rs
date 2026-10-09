//! M3 acceptance tests: profiles (opt/lto/sanitizer flags + fingerprint),
//! features (contract C1), `rivet test`, and `rivet compile-commands`.
//! All tests run a real C++ compiler in temp dirs.

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

fn write_pkg(dir: &Path, json: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("rivet.json"), json).unwrap();
}

fn write_src(pkg: &Path, rel: &str, content: &str) {
    let path = pkg.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn test_exe(pkg: &Path, profile: &str, stem: &str) -> PathBuf {
    let exe = if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    };
    pkg.join("target").join(profile).join("tests").join(exe)
}

// ---------------------------------------------------------------------------
// rivet test
// ---------------------------------------------------------------------------

#[test]
fn passing_suite_runs_and_reports() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let pkg = dir.path().join("calc");
    write_pkg(
        &pkg,
        "{\"package\":{\"name\":\"calc\",\"version\":\"0.1.0\"}}",
    );
    write_src(&pkg, "include/calc.h", "int add(int a, int b);\n");
    write_src(
        &pkg,
        "src/lib.cpp",
        "#include \"calc.h\"\nint add(int a, int b) { return a + b; }\n",
    );
    write_src(
        &pkg,
        "src/main.cpp",
        "#include <iostream>\n#include \"calc.h\"\nint main() { std::cout << add(1, 2) << '\\n'; }\n",
    );
    write_src(
        &pkg,
        "tests/add.cpp",
        "#include \"calc.h\"\nint main() { return add(40, 2) == 42 ? 0 : 1; }\n",
    );
    write_src(&pkg, "tests/trivial.cpp", "int main() { return 0; }\n");

    rivet()
        .current_dir(&pkg)
        .arg("test")
        .assert()
        .success()
        .stdout(predicate::str::contains("test add ... ok"))
        .stdout(predicate::str::contains("test trivial ... ok"))
        .stdout(predicate::str::contains(
            "test result: ok. 2 passed; 0 failed",
        ));

    // Test binaries land where the contract says.
    assert!(test_exe(&pkg, "debug", "add").is_file());
    assert!(test_exe(&pkg, "debug", "trivial").is_file());

    // Second run is fresh but still runs the tests.
    rivet()
        .current_dir(&pkg)
        .arg("test")
        .assert()
        .success()
        .stdout(predicate::str::contains("(tests)"))
        .stdout(predicate::str::contains("2 passed; 0 failed"));
}

#[test]
fn failing_suite_reports_and_exits_nonzero() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let pkg = dir.path().join("bad");
    write_pkg(
        &pkg,
        "{\"package\":{\"name\":\"bad\",\"version\":\"0.1.0\"}}",
    );
    write_src(&pkg, "src/main.cpp", "int main() { return 0; }\n");
    write_src(
        &pkg,
        "tests/fails.cpp",
        "#include <cstdio>\nint main() { std::puts(\"boom-output\"); return 3; }\n",
    );
    write_src(&pkg, "tests/passes.cpp", "int main() { return 0; }\n");

    rivet()
        .current_dir(&pkg)
        .arg("test")
        .assert()
        .failure()
        .stdout(predicate::str::contains("test fails ... FAILED"))
        .stdout(predicate::str::contains("boom-output"))
        .stdout(predicate::str::contains("test passes ... ok"))
        .stdout(predicate::str::contains("1 passed; 1 failed"));
}

#[test]
fn test_selects_package_and_profile() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let ws = dir.path().join("ws");
    write_pkg(&ws, "{\"workspace\":{\"members\":[\"crates/*\"]}}");
    let a = ws.join("crates").join("a");
    write_pkg(&a, "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"}}");
    write_src(&a, "src/main.cpp", "int main() { return 0; }\n");
    write_src(&a, "tests/a.cpp", "int main() { return 0; }\n");
    let b = ws.join("crates").join("b");
    write_pkg(&b, "{\"package\":{\"name\":\"b\",\"version\":\"0.1.0\"}}");
    write_src(&b, "src/main.cpp", "int main() { return 0; }\n");
    write_src(&b, "tests/b.cpp", "int main() { return 0; }\n");

    // -p narrows to one package's tests.
    rivet()
        .current_dir(&ws)
        .arg("test")
        .arg("-p")
        .arg("a")
        .assert()
        .success()
        .stdout(predicate::str::contains("test a ... ok"))
        .stdout(predicate::str::contains("test b ... ok").not());
    assert!(test_exe(&a, "debug", "a").is_file());
    assert!(!test_exe(&b, "debug", "b").is_file());

    // --release lands in target/release.
    rivet()
        .current_dir(&ws)
        .arg("test")
        .arg("-p")
        .arg("b")
        .arg("--release")
        .assert()
        .success()
        .stdout(predicate::str::contains("test b ... ok"));
    assert!(test_exe(&b, "release", "b").is_file());
}

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

#[test]
fn release_and_dev_profiles_build_separately() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let pkg = dir.path().join("prof");
    write_pkg(
        &pkg,
        "{\"package\":{\"name\":\"prof\",\"version\":\"0.1.0\"},\"profile\":{\"release\":{\"opt\":3}}}",
    );
    write_src(&pkg, "src/main.cpp", "int main() { return 0; }\n");

    rivet().current_dir(&pkg).arg("build").assert().success();
    assert!(pkg.join("target").join("debug").is_dir());
    rivet()
        .current_dir(&pkg)
        .arg("build")
        .arg("--release")
        .assert()
        .success();
    assert!(pkg.join("target").join("release").is_dir());
}

#[test]
fn lto_profile_builds_and_runs() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let pkg = dir.path().join("lto");
    write_pkg(
        &pkg,
        "{\"package\":{\"name\":\"lto\",\"version\":\"0.1.0\"},\"profile\":{\"dev\":{\"opt\":2,\"lto\":true}}}",
    );
    write_src(
        &pkg,
        "src/main.cpp",
        "#include <iostream>\nint main() { std::cout << 42 << '\\n'; }\n",
    );

    // Proves the /GL+/LTCG (-flto) mapping end to end on the local toolchain.
    rivet()
        .current_dir(&pkg)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));
}

#[test]
fn msvc_rejects_non_address_sanitizer() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    if !rivet_toolchain::detect().is_ok_and(|t| t.is_msvc_like()) {
        eprintln!("SKIP: MSVC-only validation test");
        return;
    }
    let dir = TempDir::new().unwrap();
    let pkg = dir.path().join("san");
    write_pkg(
        &pkg,
        "{\"package\":{\"name\":\"san\",\"version\":\"0.1.0\"},\"profile\":{\"dev\":{\"sanitizers\":[\"thread\"]}}}",
    );
    write_src(&pkg, "src/main.cpp", "int main() { return 0; }\n");

    rivet()
        .current_dir(&pkg)
        .arg("build")
        .assert()
        .failure()
        .stderr(predicate::str::contains("thread"))
        .stderr(predicate::str::contains("address"));
}

#[test]
fn address_sanitizer_builds_and_runs_on_linux() {
    if !cfg!(target_os = "linux") {
        eprintln!("SKIP: ASan run coverage is Linux-gated (MSVC needs the ASan DLL dir on PATH; see DECISIONS.md)");
        return;
    }
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let pkg = dir.path().join("asan");
    write_pkg(
        &pkg,
        "{\"package\":{\"name\":\"asan\",\"version\":\"0.1.0\"},\"profile\":{\"dev\":{\"sanitizers\":[\"address\"]}}}",
    );
    write_src(
        &pkg,
        "src/main.cpp",
        "#include <iostream>\nint main() { std::cout << \"asan-ok\\n\"; }\n",
    );

    rivet()
        .current_dir(&pkg)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("asan-ok"));
}

// ---------------------------------------------------------------------------
// Features (contract C1)
// ---------------------------------------------------------------------------

fn features_corpus(dir: &TempDir) -> PathBuf {
    let app = dir.path().join("app");
    write_pkg(
        &app,
        "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"},\"dependencies\":{\"extra\":{\"path\":\"../extra\",\"optional\":true}},\"features\":{\"default\":[],\"extra\":[\"dep:extra\"]}}",
    );
    write_src(
        &app,
        "src/main.cpp",
        "#include <iostream>\nint main() { std::cout << \"app\\n\"; }\n",
    );
    let extra = dir.path().join("extra");
    write_pkg(
        &extra,
        "{\"package\":{\"name\":\"extra\",\"version\":\"0.1.0\"}}",
    );
    write_src(&extra, "include/extra.h", "int extra_value();\n");
    write_src(
        &extra,
        "src/lib.cpp",
        "#include \"extra.h\"\nint extra_value() { return 7; }\n",
    );
    app
}

#[test]
fn optional_dep_excluded_by_default_included_on_request() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let app = features_corpus(&dir);
    let extra_target = || dir.path().join("extra").join("target");

    // Default: the optional dep never builds.
    rivet()
        .current_dir(&app)
        .arg("build")
        .assert()
        .success()
        .stdout(predicate::str::contains("Compiling extra").not());
    assert!(
        !extra_target().exists(),
        "optional dep must not build by default"
    );

    // Requested: the optional dep builds first, app rebuilds (fingerprint).
    rivet()
        .current_dir(&app)
        .arg("build")
        .arg("--features")
        .arg("extra")
        .assert()
        .success()
        .stdout(predicate::str::contains("Compiling extra"));
    assert!(extra_target().is_dir());

    // Comma-separated + all-features spellings agree.
    rivet()
        .current_dir(&app)
        .arg("build")
        .arg("--all-features")
        .assert()
        .success();
    assert!(extra_target().is_dir());

    // Unknown features fail actionably.
    rivet()
        .current_dir(&app)
        .arg("build")
        .arg("--features")
        .arg("ghost")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown feature"))
        .stderr(predicate::str::contains("ghost"));
}

#[test]
fn no_default_features_skips_default_deps() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let app = dir.path().join("app");
    write_pkg(
        &app,
        "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"},\"dependencies\":{\"extra\":{\"path\":\"../extra\",\"optional\":true}},\"features\":{\"default\":[\"dep:extra\"]}}",
    );
    write_src(
        &app,
        "src/main.cpp",
        "#include <iostream>\nint main() { std::cout << \"app\\n\"; }\n",
    );
    let extra = dir.path().join("extra");
    write_pkg(
        &extra,
        "{\"package\":{\"name\":\"extra\",\"version\":\"0.1.0\"}}",
    );
    write_src(&extra, "src/lib.cpp", "int extra_value() { return 7; }\n");

    // Default pulls the optional dep in via `default = ["dep:extra"]`.
    rivet().current_dir(&app).arg("build").assert().success();
    assert!(dir.path().join("extra").join("target").is_dir());

    rivet()
        .current_dir(&app)
        .arg("clean")
        .arg("-p")
        .arg("app")
        .assert()
        .success();
    std::fs::remove_dir_all(dir.path().join("extra").join("target")).unwrap();

    // --no-default-features leaves it out.
    rivet()
        .current_dir(&app)
        .arg("build")
        .arg("--no-default-features")
        .assert()
        .success()
        .stdout(predicate::str::contains("Compiling extra").not());
    assert!(!dir.path().join("extra").join("target").exists());
}

// ---------------------------------------------------------------------------
// compile-commands
// ---------------------------------------------------------------------------

#[test]
fn compile_commands_emits_valid_json() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let pkg = dir.path().join("cc");
    write_pkg(
        &pkg,
        "{\"package\":{\"name\":\"cc\",\"version\":\"0.1.0\"}}",
    );
    write_src(&pkg, "include/cc.h", "int f();\n");
    write_src(
        &pkg,
        "src/lib.cpp",
        "#include \"cc.h\"\nint f() { return 1; }\n",
    );
    write_src(
        &pkg,
        "src/main.cpp",
        "#include <iostream>\n#include \"cc.h\"\nint main() { std::cout << f() << '\\n'; }\n",
    );
    write_src(&pkg, "tests/t.cpp", "int main() { return 0; }\n");

    rivet()
        .current_dir(&pkg)
        .arg("compile-commands")
        .assert()
        .success();

    let db = pkg
        .join("target")
        .join("debug")
        .join("compile_commands.json");
    assert!(db.is_file(), "expected {}", db.display());
    let text = std::fs::read_to_string(&db).unwrap();
    let entries: serde_json::Value = serde_json::from_str(&text).unwrap();
    let entries = entries.as_array().unwrap();
    // lib + bin + test TUs.
    assert_eq!(entries.len(), 3, "got: {text}");
    for entry in entries {
        assert!(entry.get("directory").is_some(), "got: {entry}");
        assert!(entry.get("arguments").is_some(), "got: {entry}");
        assert!(entry.get("file").is_some(), "got: {entry}");
        let args = entry["arguments"].as_array().unwrap();
        assert!(!args.is_empty(), "got: {entry}");
    }
    assert!(
        text.contains("main.cpp"),
        "mentions a source file, got: {text}"
    );
    assert!(text.contains("t.cpp"), "includes tests, got: {text}");
}
