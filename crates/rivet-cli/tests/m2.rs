//! Acceptance tests: multi-file builds, content-hash incrementality,
//! depfile header tracking, static lib + bin, path deps, workspaces.
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

fn exe_path(pkg: &Path, profile: &str, name: &str) -> PathBuf {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    pkg.join("target").join(profile).join(exe)
}

fn archive_path(pkg: &Path, profile: &str, lib: &str) -> PathBuf {
    let file = if cfg!(windows) {
        format!("{lib}.lib")
    } else {
        format!("lib{lib}.a")
    };
    pkg.join("target").join(profile).join("build").join(file)
}

/// `Running` lines that are compile steps (contain a compile-only flag).
fn compile_lines(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|l| l.contains("Running") && (l.contains("\"/c\"") || l.contains("\"-c\"")))
        .collect()
}

#[test]
fn multi_file_bin_builds_and_runs() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let pkg = dir.path().join("app");
    write_pkg(
        &pkg,
        "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"}}",
    );
    write_src(&pkg, "include/util.h", "int util();\n");
    write_src(
        &pkg,
        "src/util.cpp",
        "#include \"util.h\"\nint util() { return 40 + 2; }\n",
    );
    write_src(
        &pkg,
        "src/main.cpp",
        "#include <iostream>\n#include \"util.h\"\nint main() { std::cout << util() << '\\n'; }\n",
    );

    rivet().current_dir(&pkg).arg("build").assert().success();
    let exe = exe_path(&pkg, "debug", "app");
    assert!(exe.is_file(), "expected exe at {}", exe.display());

    // Both TUs got objects (filtering PDBs/side files); depfiles/state exist.
    let objs: Vec<_> = std::fs::read_dir(
        pkg.join("target")
            .join("debug")
            .join("build")
            .join("bin-app"),
    )
    .unwrap()
    .map(|e| e.unwrap().path())
    .filter(|p| {
        p.extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x == "obj" || x == "o")
    })
    .collect();
    assert_eq!(objs.len(), 2, "two objects expected");
    assert!(pkg
        .join("target")
        .join("debug")
        .join("incremental")
        .join("rivet-state.json")
        .is_file());

    rivet()
        .current_dir(&pkg)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));
}

#[test]
fn header_touch_rebuilds_only_dependents() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let pkg = dir.path().join("app");
    write_pkg(
        &pkg,
        "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"}}",
    );
    write_src(&pkg, "include/a.h", "int av();\n");
    write_src(&pkg, "include/b.h", "int bv();\n");
    write_src(
        &pkg,
        "src/a.cpp",
        "#include \"a.h\"\nint av() { return 1; }\n",
    );
    write_src(
        &pkg,
        "src/b.cpp",
        "#include \"b.h\"\nint bv() { return 2; }\n",
    );
    write_src(
        &pkg,
        "src/main.cpp",
        "#include <iostream>\n#include \"a.h\"\n#include \"b.h\"\nint main() { std::cout << av() + bv() << '\\n'; }\n",
    );

    // First build compiles all three TUs.
    let first = rivet()
        .current_dir(&pkg)
        .arg("build")
        .arg("-v")
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    let first = String::from_utf8(first).unwrap();
    assert_eq!(
        compile_lines(&first).len(),
        3,
        "all TUs compile first\n{first}"
    );

    let obj_dir = pkg
        .join("target")
        .join("debug")
        .join("build")
        .join("bin-app");
    let mtime = |stem_part: &str| {
        std::fs::read_dir(&obj_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.file_name().unwrap().to_string_lossy().contains(stem_part))
            .map(|p| std::fs::metadata(p).unwrap().modified().unwrap())
    };
    let main_before = mtime("main").unwrap();
    let b_before = mtime("b.cpp").unwrap();

    // Modify ONLY a.h (dependents: main.cpp + a.cpp; b.cpp untouched).
    write_src(&pkg, "include/a.h", "int av(); // v2\n");

    let second = rivet()
        .current_dir(&pkg)
        .arg("build")
        .arg("-v")
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    let second = String::from_utf8(second).unwrap();
    let recompiled = compile_lines(&second);
    assert_eq!(
        recompiled.len(),
        2,
        "only a.cpp + main.cpp recompile\n{second}"
    );
    assert!(
        recompiled.iter().any(|l| l.contains("a.cpp")),
        "a.cpp recompiled\n{second}"
    );
    assert!(
        recompiled.iter().any(|l| l.contains("main.cpp")),
        "main.cpp recompiled\n{second}"
    );
    assert!(
        !recompiled.iter().any(|l| l.contains("b.cpp")),
        "b.cpp untouched\n{second}"
    );

    // Object mtimes agree: main rewritten, b.cpp's object untouched.
    assert!(mtime("main").unwrap() > main_before);
    assert_eq!(mtime("b.cpp").unwrap(), b_before);

    rivet()
        .current_dir(&pkg)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains('3'));
}

#[test]
fn mtime_only_touch_without_content_change_is_fresh() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let pkg = dir.path().join("app");
    write_pkg(
        &pkg,
        "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"}}",
    );
    write_src(&pkg, "include/a.h", "int av();\n");
    write_src(
        &pkg,
        "src/a.cpp",
        "#include \"a.h\"\nint av() { return 1; }\n",
    );
    write_src(
        &pkg,
        "src/main.cpp",
        "#include <iostream>\n#include \"a.h\"\nint main() { std::cout << av() << '\\n'; }\n",
    );

    rivet().current_dir(&pkg).arg("build").assert().success();
    // Rewrite the header with identical content (mtime changes, hash same).
    let h = pkg.join("include").join("a.h");
    let content = std::fs::read(&h).unwrap();
    std::fs::write(&h, content).unwrap();

    rivet()
        .current_dir(&pkg)
        .arg("build")
        .assert()
        .success()
        .stdout(predicate::str::contains("Fresh"));
}

#[test]
fn noop_rebuild_is_fresh() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let pkg = dir.path().join("app");
    write_pkg(
        &pkg,
        "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"}}",
    );
    write_src(&pkg, "src/main.cpp", "int main() { return 0; }\n");

    rivet().current_dir(&pkg).arg("build").assert().success();
    rivet()
        .current_dir(&pkg)
        .arg("build")
        .arg("-v")
        .assert()
        .success()
        .stdout(predicate::str::contains("Fresh"))
        .stderr(predicate::str::contains("Running").not());
}

#[test]
fn lib_and_bin_share_helpers() {
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
        "src/util.cpp",
        "#include \"calc.h\"\nint triple(int x) { return add(x, add(x, x)); }\n",
    );
    write_src(
        &pkg,
        "src/main.cpp",
        "#include <iostream>\n#include \"calc.h\"\nint triple(int);\nint main() { std::cout << triple(14) << '\\n'; }\n",
    );

    rivet().current_dir(&pkg).arg("build").assert().success();
    let archive = archive_path(&pkg, "debug", "calc");
    assert!(
        archive.is_file(),
        "expected archive at {}",
        archive.display()
    );
    assert!(exe_path(&pkg, "debug", "calc").is_file());

    rivet()
        .current_dir(&pkg)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));
}

#[test]
fn path_dependency_links_transitively() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let lib = dir.path().join("mylib");
    write_pkg(
        &lib,
        "{\"package\":{\"name\":\"mylib\",\"version\":\"0.1.0\"}}",
    );
    write_src(&lib, "include/add.h", "int add(int a, int b);\n");
    write_src(
        &lib,
        "src/lib.cpp",
        "#include \"add.h\"\nint add(int a, int b) { return a + b; }\n",
    );
    let app = dir.path().join("app");
    write_pkg(
        &app,
        "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"},\"dependencies\":{\"mylib\":{\"path\":\"../mylib\"}}}",
    );
    write_src(
        &app,
        "src/main.cpp",
        "#include <iostream>\n#include \"add.h\"\nint main() { std::cout << add(40, 2) << '\\n'; }\n",
    );

    rivet().current_dir(&app).arg("build").assert().success();
    rivet()
        .current_dir(&app)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));

    // Changing the lib header rebuilds both packages (dep archive + app TU).
    write_src(&lib, "include/add.h", "int add(int a, int b); // v2\n");
    let out = rivet()
        .current_dir(&app)
        .arg("build")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("Compiling mylib"), "mylib rebuilds\n{out}");
    assert!(out.contains("Compiling app"), "app relinks\n{out}");
    rivet()
        .current_dir(&app)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));
}

#[test]
fn workspace_build_select_and_run() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let ws = dir.path().join("ws");
    write_pkg(&ws, "{\"workspace\":{\"members\":[\"crates/*\"]}}");
    let greet = ws.join("crates").join("greet");
    write_pkg(
        &greet,
        "{\"package\":{\"name\":\"greet\",\"version\":\"0.1.0\"}}",
    );
    write_src(&greet, "include/greet.h", "const char* greet();\n");
    write_src(
        &greet,
        "src/lib.cpp",
        "#include \"greet.h\"\nconst char* greet() { return \"hi\"; }\n",
    );
    let hello = ws.join("crates").join("hello");
    write_pkg(
        &hello,
        "{\"package\":{\"name\":\"hello\",\"version\":\"0.1.0\"},\"dependencies\":{\"greet\":{\"path\":\"../greet\"}}}",
    );
    write_src(
        &hello,
        "src/main.cpp",
        "#include <iostream>\n#include \"greet.h\"\nint main() { std::cout << greet() << \" from hello\\n\"; }\n",
    );
    let tool = ws.join("crates").join("tool");
    write_pkg(
        &tool,
        "{\"package\":{\"name\":\"tool\",\"version\":\"0.1.0\"}}",
    );
    write_src(
        &tool,
        "src/main.cpp",
        "#include <iostream>\nint main() { std::cout << \"tool\\n\"; }\n",
    );

    // Root build covers the whole workspace in dependency order.
    let out = rivet()
        .current_dir(&ws)
        .arg("build")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    for name in ["greet", "hello", "tool"] {
        assert!(
            out.contains(&format!("Compiling {name}")),
            "builds {name}\n{out}"
        );
    }
    assert!(
        out.find("Compiling greet").unwrap() < out.find("Compiling hello").unwrap(),
        "deps first\n{out}"
    );

    // -p narrows to one package.
    rivet()
        .current_dir(&ws)
        .arg("build")
        .arg("-p")
        .arg("greet")
        .assert()
        .success()
        .stdout(predicate::str::contains("hello").not());

    // run -p selects the package binary.
    rivet()
        .current_dir(&ws)
        .arg("run")
        .arg("-p")
        .arg("hello")
        .assert()
        .success()
        .stdout(predicate::str::contains("hi from hello"));

    // run --bin selects across the workspace.
    rivet()
        .current_dir(&ws)
        .arg("run")
        .arg("--bin")
        .arg("tool")
        .assert()
        .success()
        .stdout(predicate::str::contains("tool"));

    // Ambiguous bare run lists candidates.
    rivet()
        .current_dir(&ws)
        .arg("run")
        .assert()
        .failure()
        .stderr(predicate::str::contains("multiple binaries"));

    // clean -p removes one target dir only.
    rivet()
        .current_dir(&ws)
        .arg("clean")
        .arg("-p")
        .arg("hello")
        .assert()
        .success();
    assert!(!hello.join("target").exists());
    assert!(greet.join("target").is_dir());
}

#[test]
fn keep_going_reports_all_failures() {
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
    write_src(&pkg, "src/main.cpp", "this is not c++\n");
    write_src(&pkg, "src/other.cpp", "neither is this\n");

    // Fail-fast (-j1): only the first TU is attempted.
    let err = rivet()
        .current_dir(&pkg)
        .arg("build")
        .arg("-j1")
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(err).unwrap();
    assert!(err.contains("main.cpp"), "got: {err}");
    assert!(
        !err.contains("other.cpp"),
        "fail-fast skips the rest, got: {err}"
    );

    // --keep-going: both failures surface.
    let err = rivet()
        .current_dir(&pkg)
        .arg("build")
        .arg("--keep-going")
        .arg("-j1")
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(err).unwrap();
    assert!(err.contains("main.cpp"), "got: {err}");
    assert!(err.contains("other.cpp"), "got: {err}");
}

#[test]
fn unindexed_deps_fail_actionably() {
    // No compiler needed: fails during planning, before detection.
    let dir = TempDir::new().unwrap();
    let pkg2 = dir.path().join("app2");
    write_pkg(
        &pkg2,
        "{\"package\":{\"name\":\"app2\",\"version\":\"0.1.0\"},\"dependencies\":{\"fmt\":\"11\"}}",
    );
    write_src(&pkg2, "src/main.cpp", "int main() { return 0; }\n");
    rivet()
        .current_dir(&pkg2)
        .arg("build")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not in the local index copy"));
}

fn find_built(pkg: &Path, stem: &str, exts: &[&str]) -> Option<PathBuf> {
    let mut stack = vec![pkg.join("target")];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_stem().and_then(|s| s.to_str()) == Some(stem)
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| exts.contains(&e))
            {
                return Some(path);
            }
        }
    }
    None
}

#[test]
fn shared_lib_links_runs_and_rebuilds() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let lib = dir.path().join("shlib");
    write_pkg(
        &lib,
        "{\"package\":{\"name\":\"shlib\",\"version\":\"0.1.0\"},\"lib\":{\"type\":\"shared\"}}",
    );
    // CMake-style export macro: rivet defines `shlib_EXPORTS` when
    // compiling the lib itself, so one header serves both sides.
    write_src(
        &lib,
        "include/shlib.h",
        "#pragma once\n#if defined(_WIN32)\n#if defined(shlib_EXPORTS)\n#define SHLIB_API __declspec(dllexport)\n#else\n#define SHLIB_API __declspec(dllimport)\n#endif\n#else\n#define SHLIB_API __attribute__((visibility(\"default\")))\n#endif\nint SHLIB_API answer();\n",
    );
    write_src(
        &lib,
        "src/lib.cpp",
        "#include \"shlib.h\"\nint SHLIB_API answer() { return 42; }\n",
    );
    let app = dir.path().join("app");
    write_pkg(
        &app,
        "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"},\"dependencies\":{\"shlib\":{\"path\":\"../shlib\"}}}",
    );
    write_src(
        &app,
        "src/main.cpp",
        "#include <iostream>\n#include \"shlib.h\"\nint main() { std::cout << answer() << '\\n'; }\n",
    );

    // Full build: DLL/.so + import lib exist, the binary runs (proves the
    // dep linked AND loads, with its runtime file staged beside the exe).
    rivet()
        .current_dir(&app)
        .arg("build")
        .assert()
        .success()
        .stdout(predicate::str::contains("Finished"));
    let dll_exts: &[&str] = if cfg!(windows) {
        &["dll", "lib"]
    } else if cfg!(target_os = "macos") {
        &["dylib"]
    } else {
        &["so"]
    };
    let dynamic = find_built(&lib, "shlib", dll_exts).expect("shared output must exist");
    assert!(
        dynamic.exists(),
        "shared output missing: {}",
        dynamic.display()
    );
    if cfg!(windows) {
        assert!(
            find_built(&lib, "shlib", &["lib"]).is_some(),
            "import library must exist beside the DLL"
        );
        assert!(
            find_built(&app, "shlib", &["dll"]).is_some(),
            "DLL must be staged next to the binary"
        );
    }
    rivet()
        .current_dir(&app)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));

    // Touching the lib rebuilds + relinks both, still correct.
    write_src(
        &lib,
        "src/lib.cpp",
        "#include \"shlib.h\"\nint SHLIB_API answer() { return 43; }\n",
    );
    rivet()
        .current_dir(&app)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("43"));
}
