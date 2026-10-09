//! Acceptance tests: registry deps end-to-end against a local `file://`
//! registry (index + tarballs crafted in tempdirs), publish/login/search,
//! checksums, yank, offline, install. No network.

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

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn file_url(p: &Path) -> String {
    let s = p.display().to_string().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

/// A minimal rivet lib package on disk (returned dir has rivet.json).
fn make_lib_pkg(dir: &Path, name: &str, version: &str, value: i32) -> PathBuf {
    let pkg = dir.join(format!("{name}-{version}"));
    write(
        &pkg.join("rivet.json"),
        &format!("{{\"package\":{{\"name\":\"{name}\",\"version\":\"{version}\"}}}}"),
    );
    write(
        &pkg.join("include").join(format!("{name}.h")),
        "int answer();\n",
    );
    write(
        &pkg.join("src").join("lib.cpp"),
        &format!("#include \"{name}.h\"\nint answer() {{ return {value}; }}\n"),
    );
    pkg
}

/// Package `pkg` into a local file registry; returns (registry_url, checksum).
fn publish_local(reg_root: &Path, pkg: &Path, name: &str, version: &str, yanked: bool) -> String {
    let meta = rivet_registry::PublishMeta {
        name: name.to_string(),
        version: version.to_string(),
        description: None,
        deps: Vec::new(),
    };
    let packaged = rivet_registry::package_source_dir(pkg, &meta).unwrap();
    let tarballs = reg_root.join("tarballs");
    std::fs::create_dir_all(&tarballs).unwrap();
    std::fs::write(
        tarballs.join(format!("{name}-{version}.tar.gz")),
        &packaged.bytes,
    )
    .unwrap();
    let index_line = format!(
        "{{\"version\":\"{version}\",\"checksum\":\"{}\",\"yanked\":{yanked},\"deps\":[]}}",
        packaged.checksum
    );
    let index_dir = reg_root.join("index");
    std::fs::create_dir_all(&index_dir).unwrap();
    let index_file = index_dir.join(name);
    let mut prior = std::fs::read_to_string(&index_file).unwrap_or_default();
    if !prior.is_empty() && !prior.ends_with('\n') {
        prior.push('\n');
    }
    prior.push_str(&index_line);
    prior.push('\n');
    std::fs::write(&index_file, &prior).unwrap();
    packaged.checksum
}

fn app_with_registry_dep(app: &Path, dep: &str, req: &str, registry_url: &str) {
    write(
        &app.join("rivet.json"),
        &format!(
            "{{\"package\":{{\"name\":\"app\",\"version\":\"0.1.0\"}},\"dependencies\":{{\"{dep}\":{{\"version\":\"{req}\",\"registry\":\"{registry_url}\"}}}}}}"
        ),
    );
    write(
        &app.join("src").join("main.cpp"),
        &format!(
            "#include <iostream>\n#include \"{dep}.h\"\nint main() {{ std::cout << answer() << '\\n'; }}\n"
        ),
    );
}

fn lock_checksum(app: &Path, dep: &str) -> String {
    let text = std::fs::read_to_string(app.join("rivet.lock.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    v["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == dep)
        .unwrap()["checksum"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn registry_dep_end_to_end() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let reg = dir.path().join("reg");
    let pkg = make_lib_pkg(dir.path(), "calc", "1.2.0", 42);
    let checksum = publish_local(&reg, &pkg, "calc", "1.2.0", false);

    let app = dir.path().join("app");
    app_with_registry_dep(&app, "calc", "^1", &file_url(&reg));
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success();
    assert_eq!(lock_checksum(&app, "calc"), checksum);
    rivet(&home)
        .current_dir(&app)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));

    // Rebuild reuses the lock (Fresh).
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success()
        .stdout(predicate::str::contains("Fresh"));
}

#[test]
fn checksum_mismatch_is_hard_error() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let reg = dir.path().join("reg");
    let pkg = make_lib_pkg(dir.path(), "calc", "1.2.0", 42);
    publish_local(&reg, &pkg, "calc", "1.2.0", false);

    let app = dir.path().join("app");
    app_with_registry_dep(&app, "calc", "^1", &file_url(&reg));
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success();

    // Poison the tarball and drop the extracted cache: rebuild must fail
    // on checksum, not silently use anything. (RIVET_HOME *is* the cache
    // root, so the registry cache lives directly under it.)
    std::fs::write(reg.join("tarballs").join("calc-1.2.0.tar.gz"), b"tampered").unwrap();
    let cache_pkg = home.join("registry");
    let _ = std::fs::remove_dir_all(&cache_pkg);
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Checksum").or(predicate::str::contains("checksum")));
}

#[test]
fn yanked_versions_skipped() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let reg = dir.path().join("reg");
    let v1 = make_lib_pkg(dir.path(), "calc", "1.0.0", 1);
    publish_local(&reg, &v1, "calc", "1.0.0", false);
    let v2 = make_lib_pkg(dir.path(), "calc", "2.0.0", 2);
    publish_local(&reg, &v2, "calc", "2.0.0", true);

    let app = dir.path().join("app");
    app_with_registry_dep(&app, "calc", ">=1", &file_url(&reg));
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success();
    // Yanked 2.0.0 skipped: locked version is 1.0.0.
    let text = std::fs::read_to_string(app.join("rivet.lock.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let ver = v["packages"].as_array().unwrap()[0]["version"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(ver, "1.0.0", "yanked version must be skipped");
}

#[test]
fn publish_dry_run_and_login_search() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let pkg = make_lib_pkg(dir.path(), "calc", "0.1.0", 1);

    // Dry run needs no network or token.
    rivet(&home)
        .current_dir(&pkg)
        .arg("publish")
        .arg("--dry-run")
        .arg("--registry")
        .arg("https://example.com")
        .assert()
        .success()
        .stdout(predicate::str::contains("would upload").and(predicate::str::contains("sha256:")));

    // Login stores a token under the hermetic home.
    rivet(&home)
        .arg("login")
        .arg("--registry")
        .arg("https://example.com")
        .arg("--token")
        .arg("sekret")
        .assert()
        .success()
        .stdout(predicate::str::contains("Logged in"));

    // Search over a local file registry.
    let reg = dir.path().join("reg");
    publish_local(&reg, &pkg, "calc", "0.1.0", false);
    rivet(&home)
        .arg("search")
        .arg("cal")
        .arg("--registry")
        .arg(file_url(&reg))
        .assert()
        .success()
        .stdout(predicate::str::contains("calc"));
}

#[test]
fn offline_rebuild_from_vendor_registry() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let reg = dir.path().join("reg");
    let pkg = make_lib_pkg(dir.path(), "calc", "1.2.0", 42);
    publish_local(&reg, &pkg, "calc", "1.2.0", false);

    let app = dir.path().join("app");
    app_with_registry_dep(&app, "calc", "^1", &file_url(&reg));
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success();
    rivet(&home)
        .current_dir(&app)
        .arg("vendor")
        .assert()
        .success();
    assert!(app.join("vendor").join("calc").is_dir());

    let cold = dir.path().join("cold");
    rivet(&cold)
        .current_dir(&app)
        .arg("build")
        .arg("--offline")
        .assert()
        .success();
    rivet(&cold)
        .current_dir(&app)
        .arg("run")
        .arg("--offline")
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));
}

#[test]
fn install_copies_bins() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let app = dir.path().join("app");
    write(
        &app.join("rivet.json"),
        "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"}}",
    );
    write(
        &app.join("src").join("main.cpp"),
        "int main() { return 0; }\n",
    );
    rivet(&home)
        .current_dir(&app)
        .arg("install")
        .assert()
        .success()
        .stdout(predicate::str::contains("Installed"));
    let exe = if cfg!(windows) { "app.exe" } else { "app" };
    assert!(home.join("bin").join(exe).is_file());
}
