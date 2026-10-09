//! Acceptance tests: git deps, lockfile, update, offline, vendor, dev,
//! prune — all against local bare repos (`file://`), no network.

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

fn toolchain_available() -> bool {
    rivet_toolchain::detect().is_ok()
}

/// Run git with local-friendly config in `cwd`.
fn git(cwd: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .arg("-c")
        .arg("protocol.file.allow=always")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("git binary must exist for these tests");
    assert!(status.success(), "git {args:?} failed in {}", cwd.display());
}

fn git_out(cwd: &Path, args: &[&str]) -> String {
    let out = StdCommand::new("git")
        .arg("-c")
        .arg("protocol.file.allow=always")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git binary must exist for these tests");
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        cwd.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
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

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

/// A seed repo: `origin.git` (bare) + `work` clone with one commit.
/// Returns `(origin_bare, work_dir, branch)`.
fn make_origin(dir: &Path, name: &str) -> (PathBuf, PathBuf, String) {
    let bare = dir.join(format!("{name}.git"));
    let work = dir.join(format!("{name}-work"));
    git(dir, &["init", "--bare", "--", &bare.to_string_lossy()]);
    git(
        dir,
        &[
            "clone",
            "--",
            &bare.to_string_lossy(),
            &work.to_string_lossy(),
        ],
    );
    (
        bare,
        work.clone(),
        git_out(&work, &["branch", "--show-current"]),
    )
}

fn commit_all(work: &Path, msg: &str) {
    git(work, &["add", "-A"]);
    git(
        work,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            msg,
        ],
    );
}

fn push_all(work: &Path, branch: &str) {
    git(work, &["push", "origin", branch, "--tags"]);
}

/// Minimal C++ lib package sources in `dir` (package `name`, function
/// `answer()` returning `value`).
fn write_lib(dir: &Path, name: &str, value: i32) {
    write(
        &dir.join("rivet.json"),
        &format!("{{\"package\":{{\"name\":\"{name}\",\"version\":\"0.1.0\"}}}}"),
    );
    write(
        &dir.join("include").join(format!("{name}.h")),
        "int answer();\n",
    );
    write(
        &dir.join("src").join("lib.cpp"),
        &format!("#include \"{name}.h\"\nint answer() {{ return {value}; }}\n"),
    );
}

fn lock_sha(app: &Path, dep: &str) -> String {
    let text = std::fs::read_to_string(app.join("rivet.lock.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    v["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == dep)
        .unwrap()["source"]["sha"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn git_tag_dep_end_to_end() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let (bare, work, branch) = make_origin(dir.path(), "zlib");
    write_lib(&work, "zlib", 131);
    commit_all(&work, "init");
    git(&work, &["tag", "v1.3.1"]);
    push_all(&work, &branch);

    let app = dir.path().join("app");
    write(
        &app.join("rivet.json"),
        &format!(
            "{{\"package\":{{\"name\":\"app\",\"version\":\"0.1.0\"}},\"dependencies\":{{\"zlib\":{{\"git\":\"{}\",\"tag\":\"v1.3.1\"}}}}}}",
            file_url(&bare)
        ),
    );
    write(
        &app.join("src").join("main.cpp"),
        "#include <iostream>\n#include \"zlib.h\"\nint main() { std::cout << answer() << '\\n'; }\n",
    );

    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success();
    let lock = app.join("rivet.lock.json");
    assert!(lock.is_file(), "lockfile must be written");
    assert_eq!(lock_sha(&app, "zlib").len(), 40);

    rivet(&home)
        .current_dir(&app)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("131"));

    // Second build reuses the lock (bytes identical, Fresh).
    let before = std::fs::read(&lock).unwrap();
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success()
        .stdout(predicate::str::contains("Fresh"));
    assert_eq!(std::fs::read(&lock).unwrap(), before);
}

#[test]
fn update_moves_branch() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let (bare, work, branch) = make_origin(dir.path(), "lib");
    write_lib(&work, "lib", 1);
    commit_all(&work, "one");
    push_all(&work, &branch);

    let app = dir.path().join("app");
    write(
        &app.join("rivet.json"),
        &format!(
            "{{\"package\":{{\"name\":\"app\",\"version\":\"0.1.0\"}},\"dependencies\":{{\"lib\":{{\"git\":\"{}\",\"branch\":\"{branch}\"}}}}}}",
            file_url(&bare)
        ),
    );
    write(
        &app.join("src").join("main.cpp"),
        "#include <iostream>\n#include \"lib.h\"\nint main() { std::cout << answer() << '\\n'; }\n",
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
        .stdout(predicate::str::contains('1'));
    let old_sha = lock_sha(&app, "lib");

    // New upstream commit.
    write_lib(&work, "lib", 2);
    commit_all(&work, "two");
    push_all(&work, &branch);

    rivet(&home)
        .current_dir(&app)
        .arg("update")
        .assert()
        .success()
        .stdout(predicate::str::contains("lib").and(predicate::str::contains("->")));
    let new_sha = lock_sha(&app, "lib");
    assert_ne!(old_sha, new_sha);
    assert!(new_sha.len() == 40);

    rivet(&home)
        .current_dir(&app)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains('2'));
}

#[test]
fn precise_pins_commit() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let (bare, work, branch) = make_origin(dir.path(), "lib");
    write_lib(&work, "lib", 7);
    commit_all(&work, "one");
    let pinned = git_out(&work, &["rev-parse", "HEAD"]);
    write_lib(&work, "lib", 8);
    commit_all(&work, "two");
    push_all(&work, &branch);

    let app = dir.path().join("app");
    write(
        &app.join("rivet.json"),
        &format!(
            "{{\"package\":{{\"name\":\"app\",\"version\":\"0.1.0\"}},\"dependencies\":{{\"lib\":{{\"git\":\"{}\",\"branch\":\"{branch}\"}}}}}}",
            file_url(&bare)
        ),
    );
    write(
        &app.join("src").join("main.cpp"),
        "#include <iostream>\n#include \"lib.h\"\nint main() { std::cout << answer() << '\\n'; }\n",
    );

    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success();
    rivet(&home)
        .current_dir(&app)
        .arg("update")
        .arg("-p")
        .arg("lib")
        .arg("--precise")
        .arg(&pinned)
        .assert()
        .success();
    assert_eq!(lock_sha(&app, "lib"), pinned);
    rivet(&home)
        .current_dir(&app)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains('7'));
}

#[test]
fn moved_tag_is_error_on_update() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let (bare, work, branch) = make_origin(dir.path(), "lib");
    write_lib(&work, "lib", 1);
    commit_all(&work, "one");
    git(&work, &["tag", "v1"]);
    push_all(&work, &branch);

    let app = dir.path().join("app");
    write(
        &app.join("rivet.json"),
        &format!(
            "{{\"package\":{{\"name\":\"app\",\"version\":\"0.1.0\"}},\"dependencies\":{{\"lib\":{{\"git\":\"{}\",\"tag\":\"v1\"}}}}}}",
            file_url(&bare)
        ),
    );
    write(
        &app.join("src").join("main.cpp"),
        "int main() { return 0; }\n",
    );
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success();

    // Force-move the tag upstream.
    write_lib(&work, "lib", 2);
    commit_all(&work, "two");
    git(&work, &["tag", "-f", "v1"]);
    git(&work, &["push", "-f", "origin", "v1"]);

    rivet(&home)
        .current_dir(&app)
        .arg("update")
        .assert()
        .failure()
        .stderr(predicate::str::contains("moved"));
}

#[test]
fn locked_flag_errors_on_change() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let (bare, work, branch) = make_origin(dir.path(), "lib");
    write_lib(&work, "lib", 1);
    commit_all(&work, "one");
    git(&work, &["tag", "v1"]);
    push_all(&work, &branch);

    let app = dir.path().join("app");
    let manifest = format!(
        "{{\"package\":{{\"name\":\"app\",\"version\":\"0.1.0\"}},\"dependencies\":{{\"lib\":{{\"git\":\"{}\",\"tag\":\"v1\"}}}}}}",
        file_url(&bare)
    );
    write(&app.join("rivet.json"), &manifest);
    write(
        &app.join("src").join("main.cpp"),
        "int main() { return 0; }\n",
    );
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success();

    // Change the requested ref: --locked must fail, plain build rewrites.
    write(
        &app.join("rivet.json"),
        &format!(
            "{{\"package\":{{\"name\":\"app\",\"version\":\"0.1.0\"}},\"dependencies\":{{\"lib\":{{\"git\":\"{}\",\"branch\":\"{branch}\"}}}}}}",
            file_url(&bare)
        ),
    );
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .arg("--locked")
        .assert()
        .failure()
        .stderr(predicate::str::contains("lock"));
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success();
}

#[test]
fn offline_rebuild_from_vendor() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let (bare, work, branch) = make_origin(dir.path(), "lib");
    write_lib(&work, "lib", 42);
    commit_all(&work, "one");
    git(&work, &["tag", "v1"]);
    push_all(&work, &branch);

    let app = dir.path().join("app");
    write(
        &app.join("rivet.json"),
        &format!(
            "{{\"package\":{{\"name\":\"app\",\"version\":\"0.1.0\"}},\"dependencies\":{{\"lib\":{{\"git\":\"{}\",\"tag\":\"v1\"}}}}}}",
            file_url(&bare)
        ),
    );
    write(
        &app.join("src").join("main.cpp"),
        "#include <iostream>\n#include \"lib.h\"\nint main() { std::cout << answer() << '\\n'; }\n",
    );

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
    assert!(app.join("vendor").join("lib").is_dir());
    assert!(app.join("vendor").join("rivet-vendor.json").is_file());

    // Fresh empty cache + --offline: vendor supplies the sources.
    let cold = dir.path().join("cold-home");
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
fn subdir_dep_builds() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let (bare, work, branch) = make_origin(dir.path(), "engine");
    let sub = work.join("libs").join("core");
    write_lib(&sub, "core", 9);
    commit_all(&work, "init");
    git(&work, &["tag", "v1"]);
    push_all(&work, &branch);

    let app = dir.path().join("app");
    write(
        &app.join("rivet.json"),
        &format!(
            "{{\"package\":{{\"name\":\"app\",\"version\":\"0.1.0\"}},\"dependencies\":{{\"core\":{{\"git\":\"{}\",\"tag\":\"v1\",\"subdir\":\"libs/core\"}}}}}}",
            file_url(&bare)
        ),
    );
    write(
        &app.join("src").join("main.cpp"),
        "#include <iostream>\n#include \"core.h\"\nint main() { std::cout << answer() << '\\n'; }\n",
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
        .stdout(predicate::str::contains('9'));
}

#[test]
fn submodule_dep_builds() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    // Inner lib repo.
    let (inner_bare, inner_work, inner_branch) = make_origin(dir.path(), "inner");
    write_lib(&inner_work, "inner", 5);
    commit_all(&inner_work, "init");
    push_all(&inner_work, &inner_branch);
    // Outer repo with the inner as submodule + a rivet package using it.
    let (outer_bare, outer_work, outer_branch) = make_origin(dir.path(), "outer");
    git(
        &outer_work,
        &["submodule", "add", &file_url(&inner_bare), "third/inner"],
    );
    write_lib(&outer_work, "outer", 6);
    // outer's lib uses the submodule header via a relative include: this
    // compiles only when the submodule was actually initialized.
    write(
        &outer_work.join("src").join("lib.cpp"),
        "#include \"outer.h\"\n#include \"../third/inner/include/inner.h\"\nint answer() { return 6; }\n",
    );
    commit_all(&outer_work, "with submodule");
    push_all(&outer_work, &outer_branch);

    let app = dir.path().join("app");
    write(
        &app.join("rivet.json"),
        &format!(
            "{{\"package\":{{\"name\":\"app\",\"version\":\"0.1.0\"}},\"dependencies\":{{\"outer\":{{\"git\":\"{}\",\"branch\":\"{outer_branch}\",\"submodules\":true}}}}}}",
            file_url(&outer_bare)
        ),
    );
    write(
        &app.join("src").join("main.cpp"),
        "#include <iostream>\n#include \"outer.h\"\nint main() { std::cout << answer() << '\\n'; }\n",
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
        .stdout(predicate::str::contains('6'));
}

#[test]
fn prune_lists_and_deletes() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let (bare_a, work_a, branch_a) = make_origin(dir.path(), "a");
    write_lib(&work_a, "a", 1);
    commit_all(&work_a, "one");
    git(&work_a, &["tag", "v1"]);
    push_all(&work_a, &branch_a);
    let (bare_b, work_b, branch_b) = make_origin(dir.path(), "b");
    write_lib(&work_b, "b", 2);
    commit_all(&work_b, "one");
    git(&work_b, &["tag", "v1"]);
    push_all(&work_b, &branch_b);

    let app = dir.path().join("app");
    let manifest_with = |dep: &str, url: &PathBuf| {
        format!(
            "{{\"package\":{{\"name\":\"app\",\"version\":\"0.1.0\"}},\"dependencies\":{{\"{dep}\":{{\"git\":\"{}\",\"tag\":\"v1\"}}}}}}",
            file_url(url)
        )
    };
    write(&app.join("rivet.json"), &manifest_with("a", &bare_a));
    write(
        &app.join("src").join("main.cpp"),
        "int main() { return 0; }\n",
    );
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success();
    rivet(&home)
        .current_dir(&app)
        .arg("git")
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("Checkout").and(predicate::str::contains("used by a")));

    // Switch to dep b + update: a's checkout becomes unreferenced.
    write(&app.join("rivet.json"), &manifest_with("b", &bare_b));
    rivet(&home)
        .current_dir(&app)
        .arg("update")
        .assert()
        .success();
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success();
    rivet(&home)
        .current_dir(&app)
        .arg("git")
        .arg("prune")
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(predicate::str::contains("Would delete"));
    rivet(&home)
        .current_dir(&app)
        .arg("git")
        .arg("prune")
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleted"));
}

#[test]
fn patch_override_and_dev() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let (bare, work, branch) = make_origin(dir.path(), "lib");
    write_lib(&work, "lib", 11);
    commit_all(&work, "one");
    git(&work, &["tag", "v1"]);
    push_all(&work, &branch);

    let app = dir.path().join("app");
    write(
        &app.join("rivet.json"),
        &format!(
            "{{\"package\":{{\"name\":\"app\",\"version\":\"0.1.0\"}},\"dependencies\":{{\"lib\":{{\"git\":\"{}\",\"tag\":\"v1\"}}}}}}",
            file_url(&bare)
        ),
    );
    write(
        &app.join("src").join("main.cpp"),
        "#include <iostream>\n#include \"lib.h\"\nint main() { std::cout << answer() << '\\n'; }\n",
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
        .stdout(predicate::str::contains("11"));

    // dev checks out a writable clone and auto-adds the patch entry.
    rivet(&home)
        .current_dir(&app)
        .arg("dev")
        .arg("lib")
        .assert()
        .success()
        .stdout(predicate::str::contains("Created"));
    let dev_lib = app.join("deps-dev").join("lib");
    assert!(dev_lib.join(".git").is_dir());
    let manifest = std::fs::read_to_string(app.join("rivet.json")).unwrap();
    assert!(manifest.contains("deps-dev/lib"), "patch entry added");

    // Editing the dev copy changes the build (patch wins over the lock).
    write(
        &dev_lib.join("src").join("lib.cpp"),
        "#include \"lib.h\"\nint answer() { return 12; }\n",
    );
    rivet(&home)
        .current_dir(&app)
        .arg("run")
        .assert()
        .success()
        .stdout(predicate::str::contains("12"));
    // The lock still records the base SHA.
    assert_eq!(lock_sha(&app, "lib").len(), 40);
}

#[test]
fn git_fetch_warms_cache() {
    if !toolchain_available() {
        eprintln!("SKIP: no C++ toolchain detected");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let (bare, work, branch) = make_origin(dir.path(), "lib");
    write_lib(&work, "lib", 3);
    commit_all(&work, "one");
    git(&work, &["tag", "v1"]);
    push_all(&work, &branch);

    let app = dir.path().join("app");
    write(
        &app.join("rivet.json"),
        &format!(
            "{{\"package\":{{\"name\":\"app\",\"version\":\"0.1.0\"}},\"dependencies\":{{\"lib\":{{\"git\":\"{}\",\"tag\":\"v1\"}}}}}}",
            file_url(&bare)
        ),
    );
    write(
        &app.join("src").join("main.cpp"),
        "int main() { return 0; }\n",
    );
    rivet(&home)
        .current_dir(&app)
        .arg("build")
        .assert()
        .success();

    // Fresh cache: fetch warms mirrors, then offline build works.
    let cold = dir.path().join("cold");
    rivet(&cold)
        .current_dir(&app)
        .arg("git")
        .arg("fetch")
        .assert()
        .success()
        .stdout(predicate::str::contains("Fetched"));
    rivet(&cold)
        .current_dir(&app)
        .arg("build")
        .arg("--offline")
        .assert()
        .success();
}
