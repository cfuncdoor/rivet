//! Acceptance tests: C sources (`src/main.c`, helpers, `src/lib.c`,
//! `tests/*.c`, mixed C/C++) against a real compiler in temp dirs.

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

fn exe_in(pkg: &std::path::Path, profile: &str, name: &str) -> PathBuf {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    pkg.join("target").join(profile).join(exe)
}

fn write_manifest(pkg: &std::path::Path, body: &str) {
    std::fs::write(pkg.join("rivet.json"), body).unwrap();
}

const MANIFEST: &str = r#"{
  "$schema": "https://raw.githubusercontent.com/cfuncdoor/rivet/main/rivet.schema.json",
  "package": { "name": "capp", "version": "0.1.0" }
}
"#;

#[test]
fn c_bin_with_helpers_builds_and_runs() {
    if !toolchain_available() {
        eprintln!("SKIP: no toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let pkg = dir.path().join("capp");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    write_manifest(&pkg, MANIFEST);
    std::fs::write(
        pkg.join("src").join("main.c"),
        "#include <stdio.h>\nint helper(void);\nint main(void) { printf(\"c says %d\\n\", helper()); return 0; }\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("src").join("helper.c"),
        "int helper(void) { return 41 + 1; }\n",
    )
    .unwrap();

    rivet()
        .current_dir(&pkg)
        .arg("build")
        .assert()
        .success()
        .stdout(predicate::str::contains("Finished"));
    assert!(exe_in(&pkg, "debug", "capp").is_file());

    rivet()
        .current_dir(&pkg)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("c says 42"));
}

#[test]
fn mixed_c_and_cpp_link() {
    if !toolchain_available() {
        eprintln!("SKIP: no toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let pkg = dir.path().join("capp");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    write_manifest(&pkg, MANIFEST);
    std::fs::write(
        pkg.join("src").join("main.cpp"),
        "#include <cstdio>\nextern \"C\" int helper(void);\nint main() { std::printf(\"mixed %d\\n\", helper()); return 0; }\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("src").join("helper.c"),
        "int helper(void) { return 7; }\n",
    )
    .unwrap();

    rivet().current_dir(&pkg).arg("build").assert().success();
    rivet()
        .current_dir(&pkg)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("mixed 7"));
}

#[test]
fn c_lib_convention_links_into_bin() {
    if !toolchain_available() {
        eprintln!("SKIP: no toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let pkg = dir.path().join("capp");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    write_manifest(&pkg, MANIFEST);
    std::fs::write(
        pkg.join("src").join("main.c"),
        "#include <stdio.h>\nint answer(void);\nint main(void) { printf(\"answer %d\\n\", answer()); return 0; }\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("src").join("lib.c"),
        "int answer(void) { return 42; }\n",
    )
    .unwrap();

    rivet().current_dir(&pkg).arg("build").assert().success();
    rivet()
        .current_dir(&pkg)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("answer 42"));
}

#[test]
fn c_test_file_runs() {
    if !toolchain_available() {
        eprintln!("SKIP: no toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let pkg = dir.path().join("capp");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    std::fs::create_dir_all(pkg.join("tests")).unwrap();
    write_manifest(&pkg, MANIFEST);
    std::fs::write(
        pkg.join("src").join("main.c"),
        "int main(void) { return 0; }\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("tests").join("sums.c"),
        "int main(void) { return (1 + 1 == 2) ? 0 : 1; }\n",
    )
    .unwrap();

    rivet()
        .current_dir(&pkg)
        .arg("test")
        .assert()
        .success()
        .stdout(predicate::str::contains("sums").or(predicate::str::contains("passed")));
}

#[test]
fn c_standard_manifest_builds() {
    if !toolchain_available() {
        eprintln!("SKIP: no toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let pkg = dir.path().join("capp");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    write_manifest(
        &pkg,
        r#"{
  "$schema": "https://raw.githubusercontent.com/cfuncdoor/rivet/main/rivet.schema.json",
  "package": { "name": "capp", "version": "0.1.0", "standard": "c17" }
}
"#,
    );
    std::fs::write(
        pkg.join("src").join("main.c"),
        "int main(void) { return 0; }\n",
    )
    .unwrap();

    rivet().current_dir(&pkg).arg("build").assert().success();
    assert!(exe_in(&pkg, "debug", "capp").is_file());
}

#[test]
fn c_shared_lib_links_and_runs() {
    if !toolchain_available() {
        eprintln!("SKIP: no toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let lib = dir.path().join("shlib");
    std::fs::create_dir_all(lib.join("src")).unwrap();
    std::fs::create_dir_all(lib.join("include")).unwrap();
    write_manifest(
        &lib,
        r#"{
  "$schema": "https://raw.githubusercontent.com/cfuncdoor/rivet/main/rivet.schema.json",
  "package": { "name": "shlib", "version": "0.1.0" },
  "lib": { "type": "shared" }
}
"#,
    );
    std::fs::write(
        lib.join("include").join("shlib.h"),
        "#pragma once\n#if defined(_WIN32)\n#if defined(shlib_EXPORTS)\n#define SHLIB_API __declspec(dllexport)\n#else\n#define SHLIB_API __declspec(dllimport)\n#endif\n#else\n#define SHLIB_API __attribute__((visibility(\"default\")))\n#endif\nint SHLIB_API answer(void);\n",
    )
    .unwrap();
    std::fs::write(
        lib.join("src").join("lib.c"),
        "#include \"shlib.h\"\nint SHLIB_API answer(void) { return 42; }\n",
    )
    .unwrap();

    let app = dir.path().join("app");
    std::fs::create_dir_all(app.join("src")).unwrap();
    write_manifest(
        &app,
        r#"{
  "$schema": "https://raw.githubusercontent.com/cfuncdoor/rivet/main/rivet.schema.json",
  "package": { "name": "app", "version": "0.1.0" },
  "dependencies": { "shlib": { "path": "../shlib" } }
}
"#,
    );
    std::fs::write(
        app.join("src").join("main.c"),
        "#include <stdio.h>\n#include \"shlib.h\"\nint main(void) { printf(\"%d\\n\", answer()); return 0; }\n",
    )
    .unwrap();

    rivet().current_dir(&app).arg("build").assert().success();
    rivet()
        .current_dir(&app)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));
}

#[test]
fn both_c_and_cpp_entries_are_an_error() {
    // Fails during planning, before toolchain detection.
    let dir = TempDir::new().unwrap();
    let pkg = dir.path().join("capp");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    write_manifest(&pkg, MANIFEST);
    std::fs::write(
        pkg.join("src").join("main.c"),
        "int main(void) { return 0; }\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("src").join("main.cpp"),
        "int main() { return 0; }\n",
    )
    .unwrap();

    rivet()
        .current_dir(&pkg)
        .arg("build")
        .assert()
        .failure()
        .stderr(predicate::str::contains("main.cpp").and(predicate::str::contains("main.c")));
}
