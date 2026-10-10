//! Acceptance tests: `rivet add` / `rivet remove` — all against local
//! `file://` git repos and file registries, no network (except the
//! explicitly gated shorthand test).

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::cargo::cargo_bin;
use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// This binary + hermetic `RIVET_HOME`.
fn rivet(home: &Path) -> Command {
    let mut cmd = Command::new(cargo_bin("rivet"));
    cmd.env("RIVET_HOME", home);
    // Never prompt, even in test setup gaps.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd
}

/// Run git with local-friendly config in `cwd`.
fn git(cwd: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .arg("-c")
        .arg("protocol.file.allow=always")
        .arg("-c")
        .arg("user.email=t@t")
        .arg("-c")
        .arg("user.name=t")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("git binary must exist for these tests");
    assert!(status.success(), "git {args:?} failed in {}", cwd.display());
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

/// `file://` URL for a local path (forward slashes).
fn file_url(p: &Path) -> String {
    let s = p.display().to_string().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

/// Bare `origin.git` + `work` clone with one commit on `main`.
fn make_origin(dir: &Path, name: &str) -> (PathBuf, PathBuf) {
    let bare = dir.join(format!("{name}.git"));
    let work = dir.join(format!("{name}-work"));
    git(
        dir,
        &[
            "-c",
            "init.defaultBranch=main",
            "init",
            "--bare",
            "--",
            &bare.to_string_lossy(),
        ],
    );
    git(
        dir,
        &[
            "clone",
            "--",
            &bare.to_string_lossy(),
            &work.to_string_lossy(),
        ],
    );
    (bare, work)
}

fn commit_all(work: &Path, msg: &str) {
    git(work, &["add", "-A"]);
    git(work, &["commit", "-m", msg]);
}

fn push_all(work: &Path) {
    git(work, &["push", "origin", "main", "--tags"]);
}

fn new_project(home: &Path, dir: &TempDir, name: &str) -> PathBuf {
    rivet(home)
        .current_dir(dir.path())
        .arg("new")
        .arg(name)
        .assert()
        .success();
    dir.path().join(name)
}

fn manifest_dep(pkg: &Path, key: &str) -> serde_json::Value {
    let text = std::fs::read_to_string(pkg.join("rivet.json")).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    doc["dependencies"][key].clone()
}

fn lock_package(pkg: &Path, name: &str) -> serde_json::Value {
    let text = std::fs::read_to_string(pkg.join("rivet.lock.json")).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    doc["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == name)
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

#[test]
fn add_git_manifest_repo_exact_tag() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let (bare, work) = make_origin(dir.path(), "libx");
    write(
        &work.join("rivet.json"),
        "{\"package\":{\"name\":\"libx\",\"version\":\"1.0.0\"}}",
    );
    write(&work.join("src").join("lib.cpp"), "int x() { return 1; }\n");
    commit_all(&work, "libx 1.0.0");
    git(&work, &["tag", "v1.0.0"]);
    push_all(&work);

    let pkg = new_project(&home, &dir, "app");
    let spec = format!("{}@v1.0.0", file_url(&bare));
    rivet(&home)
        .current_dir(&pkg)
        .arg("add")
        .arg(&spec)
        .assert()
        .success()
        .stdout(predicate::str::contains("Added").and(predicate::str::contains("libx")));

    let dep = manifest_dep(&pkg, "libx");
    assert_eq!(dep["tag"], serde_json::Value::String("v1.0.0".to_string()));
    assert_eq!(dep["git"], serde_json::Value::String(file_url(&bare)));
    // Manifest-carrying upstream: no inline package, no adapter.
    assert!(dep.get("package").is_none(), "got: {dep}");
    assert!(dep.get("adapter").is_none(), "got: {dep}");
    let locked = lock_package(&pkg, "libx");
    assert_eq!(
        locked["source"]["ref"]["kind"],
        serde_json::Value::String("tag".to_string())
    );
    assert_eq!(
        locked["source"]["ref"]["value"],
        serde_json::Value::String("v1.0.0".to_string())
    );
    assert!(
        locked["source"]["sha"]
            .as_str()
            .is_some_and(|s| s.len() == 40),
        "got: {locked}"
    );
}

#[test]
fn add_git_v_prefix_and_default_branch() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let (bare, work) = make_origin(dir.path(), "liby");
    write(
        &work.join("rivet.json"),
        "{\"package\":{\"name\":\"liby\",\"version\":\"2.5.0\"}}",
    );
    commit_all(&work, "liby 2.5.0");
    git(&work, &["tag", "v2.5.0"]);
    push_all(&work);

    // `@2.5.0` resolves the `v2.5.0` tag.
    let pkg = new_project(&home, &dir, "app1");
    let spec = format!("{}@2.5.0", file_url(&bare));
    rivet(&home)
        .current_dir(&pkg)
        .arg("add")
        .arg(&spec)
        .assert()
        .success();
    assert_eq!(
        manifest_dep(&pkg, "liby")["tag"],
        serde_json::Value::String("v2.5.0".to_string())
    );

    // No ref tracks the default branch as `HEAD`.
    let pkg2 = new_project(&home, &dir, "app2");
    rivet(&home)
        .current_dir(&pkg2)
        .arg("add")
        .arg(file_url(&bare))
        .assert()
        .success();
    assert_eq!(
        manifest_dep(&pkg2, "liby")["branch"],
        serde_json::Value::String("HEAD".to_string())
    );
}

#[test]
fn add_git_cmake_adapter_detected() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let (bare, work) = make_origin(dir.path(), "mini");
    write(
        &work.join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.16)\nproject(mini)\n",
    );
    write(
        &work.join("src").join("mini.cpp"),
        "int m() { return 1; }\n",
    );
    commit_all(&work, "mini");
    git(&work, &["tag", "v0.2.0"]);
    push_all(&work);

    let pkg = new_project(&home, &dir, "app");
    let spec = format!("{}@v0.2.0", file_url(&bare));
    rivet(&home)
        .current_dir(&pkg)
        .arg("add")
        .arg(&spec)
        .assert()
        .success();
    let dep = manifest_dep(&pkg, "mini");
    assert_eq!(dep["tag"], serde_json::Value::String("v0.2.0".to_string()));
    assert_eq!(
        dep["adapter"]["tool"],
        serde_json::Value::String("cmake".to_string())
    );
    assert_eq!(
        dep["package"]["name"],
        serde_json::Value::String("mini".to_string())
    );
    assert_eq!(
        dep["package"]["version"],
        serde_json::Value::String("0.2.0".to_string())
    );
}

#[test]
fn add_git_sources_adapter_for_bare_tree() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let (bare, work) = make_origin(dir.path(), "stray");
    write(&work.join("stray.cpp"), "int s() { return 1; }\n");
    write(&work.join("stray.h"), "int s();\n");
    commit_all(&work, "stray");
    git(&work, &["tag", "v1.0.0"]);
    push_all(&work);

    let pkg = new_project(&home, &dir, "app");
    let spec = format!("{}@v1.0.0", file_url(&bare));
    rivet(&home)
        .current_dir(&pkg)
        .arg("add")
        .arg(&spec)
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "top-level sources compile directly",
        ));
    let dep = manifest_dep(&pkg, "stray");
    assert_eq!(
        dep["adapter"]["tool"],
        serde_json::Value::String("sources".to_string())
    );
    assert_eq!(
        dep["package"]["name"],
        serde_json::Value::String("stray".to_string())
    );
    assert!(!lock_package(&pkg, "stray").is_null());
}

#[test]
fn add_git_bare_empty_warns_but_resolves() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let (bare, work) = make_origin(dir.path(), "docsy");
    write(&work.join("README.md"), "docs only\n");
    commit_all(&work, "docsy");
    git(&work, &["tag", "v1.0.0"]);
    push_all(&work);

    let pkg = new_project(&home, &dir, "app");
    let spec = format!("{}@v1.0.0", file_url(&bare));
    rivet(&home)
        .current_dir(&pkg)
        .arg("add")
        .arg(&spec)
        .assert()
        .success()
        .stderr(predicate::str::contains("no recognized build"));
    let dep = manifest_dep(&pkg, "docsy");
    assert!(dep.get("adapter").is_none(), "got: {dep}");
    assert_eq!(
        dep["package"]["name"],
        serde_json::Value::String("docsy".to_string())
    );
    assert!(!lock_package(&pkg, "docsy").is_null());
}

#[test]
fn add_git_unknown_ref_fails_cleanly() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let (bare, work) = make_origin(dir.path(), "libz");
    write(
        &work.join("rivet.json"),
        "{\"package\":{\"name\":\"libz\",\"version\":\"1.0.0\"}}",
    );
    commit_all(&work, "libz");
    push_all(&work);

    let pkg = new_project(&home, &dir, "app");
    let before = std::fs::read_to_string(pkg.join("rivet.json")).unwrap();
    let spec = format!("{}@nope-9.9", file_url(&bare));
    rivet(&home)
        .current_dir(&pkg)
        .arg("add")
        .arg(&spec)
        .assert()
        .failure()
        .stderr(predicate::str::contains("could not resolve"));
    // Failed add leaves the manifest untouched, no lockfile written.
    assert_eq!(
        std::fs::read_to_string(pkg.join("rivet.json")).unwrap(),
        before
    );
    assert!(!pkg.join("rivet.lock.json").exists());
}

#[test]
fn add_duplicate_and_bad_specs_fail() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let (bare, work) = make_origin(dir.path(), "libd");
    write(
        &work.join("rivet.json"),
        "{\"package\":{\"name\":\"libd\",\"version\":\"1.0.0\"}}",
    );
    commit_all(&work, "libd");
    git(&work, &["tag", "v1.0.0"]);
    push_all(&work);

    let pkg = new_project(&home, &dir, "app");
    let spec = format!("{}@v1.0.0", file_url(&bare));
    rivet(&home)
        .current_dir(&pkg)
        .arg("add")
        .arg(&spec)
        .assert()
        .success();
    // Twice is an error naming the fix.
    rivet(&home)
        .current_dir(&pkg)
        .arg("add")
        .arg(&spec)
        .assert()
        .failure()
        .stderr(predicate::str::contains("already depends"));
    // Unparseable specs fail before any network.
    for bad in ["a/b/c", "owner/", "/repo", "foo@", ""] {
        rivet(&home)
            .current_dir(&pkg)
            .arg("add")
            .arg(bad)
            .assert()
            .failure();
    }
}

/// Package `pkg` into a local file registry; returns the registry URL.
fn publish_local(reg_root: &Path, pkg: &Path, name: &str, version: &str) -> String {
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
        "{{\"version\":\"{version}\",\"checksum\":\"{}\",\"yanked\":false,\"deps\":[]}}",
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
    std::fs::write(&index_file, prior).unwrap();
    file_url(reg_root)
}

fn make_widget(dir: &Path, version: &str) -> PathBuf {
    let pkg = dir.join(format!("widget-{version}"));
    write(
        &pkg.join("rivet.json"),
        &format!("{{\"package\":{{\"name\":\"widget\",\"version\":\"{version}\"}}}}"),
    );
    write(&pkg.join("include").join("widget.h"), "int w();\n");
    write(
        &pkg.join("src").join("lib.cpp"),
        "#include \"widget.h\"\nint w() { return 1; }\n",
    );
    pkg
}

#[test]
fn add_registry_versioned_and_latest() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let reg = dir.path().join("registry");
    let w1 = make_widget(dir.path(), "1.0.0");
    let w2 = make_widget(dir.path(), "2.0.0");
    let url = publish_local(&reg, &w1, "widget", "1.0.0");
    publish_local(&reg, &w2, "widget", "2.0.0");

    // `widget@1` pins the 1.x line.
    let pkg = new_project(&home, &dir, "app1");
    rivet(&home)
        .current_dir(&pkg)
        .env("RIVET_REGISTRY", &url)
        .arg("add")
        .arg("widget@1")
        .assert()
        .success()
        .stdout(predicate::str::contains("Added"));
    assert_eq!(
        manifest_dep(&pkg, "widget"),
        serde_json::Value::String("1".to_string())
    );
    assert_eq!(
        lock_package(&pkg, "widget")["version"],
        serde_json::Value::String("1.0.0".to_string())
    );

    // Bare `widget` resolves `*` (latest).
    let pkg2 = new_project(&home, &dir, "app2");
    rivet(&home)
        .current_dir(&pkg2)
        .env("RIVET_REGISTRY", &url)
        .arg("add")
        .arg("widget")
        .assert()
        .success();
    assert_eq!(
        manifest_dep(&pkg2, "widget"),
        serde_json::Value::String("*".to_string())
    );
    assert_eq!(
        lock_package(&pkg2, "widget")["version"],
        serde_json::Value::String("2.0.0".to_string())
    );
}

#[test]
fn remove_flow_prunes_manifest_and_lock() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let reg = dir.path().join("registry");
    let w1 = make_widget(dir.path(), "1.0.0");
    let url = publish_local(&reg, &w1, "widget", "1.0.0");

    let pkg = new_project(&home, &dir, "app");
    rivet(&home)
        .current_dir(&pkg)
        .env("RIVET_REGISTRY", &url)
        .arg("add")
        .arg("widget@1")
        .assert()
        .success();
    assert!(!lock_package(&pkg, "widget").is_null());

    rivet(&home)
        .current_dir(&pkg)
        .arg("remove")
        .arg("widget")
        .assert()
        .success()
        .stdout(predicate::str::contains("Removed"));
    assert!(manifest_dep(&pkg, "widget").is_null());
    assert!(lock_package(&pkg, "widget").is_null());

    // Removing again names the available keys.
    rivet(&home)
        .current_dir(&pkg)
        .arg("remove")
        .arg("widget")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found in dependencies"));
}

#[test]
fn add_sources_dep_builds_with_the_program() {
    if rivet_toolchain::detect().is_err() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    // Foreign lib: bare sources, no manifest, no build system.
    let (bare, work) = make_origin(dir.path(), "calc");
    write(&work.join("calc.h"), "int calc();\n");
    write(
        &work.join("calc.cpp"),
        "#include \"calc.h\"\nint calc() { return 41; }\n",
    );
    commit_all(&work, "calc");
    git(&work, &["tag", "v1.0.0"]);
    push_all(&work);

    let pkg = new_project(&home, &dir, "app");
    let spec = format!("{}@v1.0.0", file_url(&bare));
    rivet(&home)
        .current_dir(&pkg)
        .arg("add")
        .arg(&spec)
        .assert()
        .success();
    // The app uses the foreign header; the dep compiles + links with it.
    write(
        &pkg.join("src").join("main.cpp"),
        "#include <iostream>\n#include \"calc.h\"\nint main() { std::cout << calc() + 1 << '\\n'; }\n",
    );
    rivet(&home)
        .current_dir(&pkg)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));
}

#[test]
fn add_github_shorthand_live() {
    if std::env::var("RIVET_TEST_NETWORK").as_deref() != Ok("1") {
        eprintln!("SKIP: needs network (RIVET_TEST_NETWORK=1)");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let pkg = new_project(&home, &dir, "app");
    rivet(&home)
        .current_dir(&pkg)
        .arg("add")
        .arg("ocornut/imgui@1.91.4")
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "top-level sources compile directly",
        ));
    let dep = manifest_dep(&pkg, "imgui");
    assert_eq!(dep["tag"], serde_json::Value::String("v1.91.4".to_string()));
    assert_eq!(
        dep["adapter"]["tool"],
        serde_json::Value::String("sources".to_string())
    );
    assert_eq!(
        dep["package"]["version"],
        serde_json::Value::String("1.91.4".to_string())
    );
    assert!(!lock_package(&pkg, "imgui").is_null());
}
