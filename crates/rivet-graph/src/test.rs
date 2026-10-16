//! `rivet test` support: discovery, test-target planning, execution.
//!
//! Discovery scans `<pkg>/tests/*.{c,cpp}` (top-level files only; no
//! recursion, mirroring the `tests/` convention). Each file becomes one test
//! binary at `target/<prof>/tests/<stem>[.exe]`, compiled with the package
//! flags and linked against the package lib archive (when any) plus
//! dependency archives — reusing [`crate::build_unit`] with synthesized
//! [`TargetBuild`]s (tag `test-<stem>`) so the shared incremental state
//! covers them.
//!
//! `devDependencies` are parsed by the manifest but not wired into test
//! inputs yet; test targets use the package's normal dependency closure.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rivet_manifest::{self as m, Manifest};
use rivet_toolchain::CxxToolchain;

use super::workspace::UnitInputs;
use super::{BuildOptions, Error, PackagePlan, TargetBuild, TargetKind};

/// One discovered test: a `tests/` source file and its binary stem.
#[derive(Debug, Clone)]
pub struct DiscoveredTest {
    /// Binary name (file stem).
    pub name: String,
    /// Absolute source path.
    pub src: PathBuf,
}

/// Discover `tests/*.{c,cpp}` for a package (top-level files only, sorted by
/// stem). A missing `tests/` dir means no tests — not an error.
pub fn discover_tests(manifest: &Manifest) -> Result<Vec<DiscoveredTest>, Error> {
    let dir = manifest.dir.join("tests");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::Io(e)),
    };
    let mut out: Vec<DiscoveredTest> = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file() || !m::is_source(&path) {
            continue;
        }
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        out.push(DiscoveredTest { name, src: path });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.src.cmp(&b.src)));
    // Two files sharing a stem would collide on one output binary.
    for pair in out.windows(2) {
        if pair[0].name == pair[1].name {
            return Err(Error::DuplicateTestName {
                name: pair[0].name.clone(),
            });
        }
    }
    Ok(out)
}

/// Synthesize one test [`TargetBuild`]: objects under `build/test-<stem>`,
/// depfiles under `deps/test-<stem>`, output at `target/<prof>/tests/<stem>`.
pub fn make_test_target(
    test: &DiscoveredTest,
    manifest: &Manifest,
    plan: &PackagePlan,
    toolchain: &CxxToolchain,
) -> TargetBuild {
    let tag = format!("test-{}", test.name);
    let objects_dir = plan.target_dir.join("build").join(&tag);
    let depfiles_dir = plan.target_dir.join("deps").join(&tag);
    let ext = toolchain.obj_extension();
    let want_depfile = toolchain.dep_style() == rivet_toolchain::DepStyle::GccDepfile;
    let obj_abs = objects_dir.join(format!("{}.{ext}", sanitize_stem(&test.name)));
    let obj_rel = obj_abs
        .strip_prefix(&plan.target_dir)
        .unwrap_or(&obj_abs)
        .display()
        .to_string();
    let (depfile_abs, depfile_rel) = if want_depfile {
        let abs = depfiles_dir.join(format!("{}.d", sanitize_stem(&test.name)));
        let rel = abs
            .strip_prefix(&plan.target_dir)
            .unwrap_or(&abs)
            .display()
            .to_string();
        (Some(abs), Some(rel))
    } else {
        (None, None)
    };
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    TargetBuild {
        kind: TargetKind::Bin,
        name: test.name.clone(),
        tag,
        jobs: vec![super::TuJob {
            src_abs: test.src.clone(),
            src_rel: test
                .src
                .strip_prefix(&manifest.dir)
                .unwrap_or(&test.src)
                .to_string_lossy()
                .replace('\\', "/"),
            obj_abs,
            obj_rel,
            depfile_abs,
            depfile_rel,
        }],
        objects_dir,
        depfiles_dir,
        output: plan
            .target_dir
            .join("tests")
            .join(format!("{}{suffix}", test.name)),
    }
}

/// Filename-safe stem (stems come from real files, so this is just `/`
/// hardening for exotic names).
fn sanitize_stem(stem: &str) -> String {
    stem.chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == ':' {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// Build all test binaries for one package: the normal `plan` (lib+bins,
/// already built by the caller) extended with synthesized test targets,
/// driven through [`crate::build_unit`] so incremental state covers them.
///
/// Returns the test targets (in discovery order) plus the build summary.
pub fn build_tests(
    manifest: &Manifest,
    plan: &PackagePlan,
    toolchain: &CxxToolchain,
    inputs: &UnitInputs,
    opts: &BuildOptions,
    features: &BTreeSet<String>,
) -> Result<(Vec<TargetBuild>, super::UnitSummary), Error> {
    let discovered = discover_tests(manifest)?;
    let mut bins = plan.bins.clone();
    let mut tests = Vec::new();
    for test in &discovered {
        let target = make_test_target(test, manifest, plan, toolchain);
        bins.push(target.clone());
        tests.push(target);
    }
    let full = PackagePlan {
        profile: plan.profile.clone(),
        bins,
        lib: plan.lib.clone(),
        header_only: plan.header_only,
        target_dir: plan.target_dir.clone(),
    };
    let summary = super::build_unit(manifest, &full, toolchain, inputs, opts, features)?;
    Ok((tests, summary))
}

/// Outcome of running one test binary.
#[derive(Debug)]
pub struct TestRun {
    /// Test stem.
    pub name: String,
    /// `true` when the binary exited zero.
    pub passed: bool,
    /// Process exit code, when available.
    pub code: Option<i32>,
    /// Captured stdout.
    pub stdout: Vec<u8>,
    /// Captured stderr.
    pub stderr: Vec<u8>,
}

/// Run one test binary (cwd = package dir), capturing output.
pub fn run_test_binary(exe: &Path, name: &str, cwd: &Path) -> Result<TestRun, Error> {
    let out = std::process::Command::new(exe)
        .current_dir(cwd)
        .output()
        .map_err(Error::Io)?;
    Ok(TestRun {
        name: name.to_string(),
        passed: out.status.success(),
        code: out.status.code(),
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_in(dir: &Path, json: &str) -> Manifest {
        std::fs::write(dir.join("rivet.json"), json).unwrap();
        let mut m = m::parse_str(json).unwrap();
        m.dir = dir.to_path_buf();
        m
    }

    #[test]
    fn missing_tests_dir_means_no_tests() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"}}",
        );
        assert!(discover_tests(&m).unwrap().is_empty());
    }

    #[test]
    fn discovers_top_level_cpp_only() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"}}",
        );
        std::fs::create_dir_all(dir.path().join("tests").join("sub")).unwrap();
        std::fs::write(dir.path().join("tests").join("b.cpp"), "").unwrap();
        std::fs::write(dir.path().join("tests").join("a.cpp"), "").unwrap();
        std::fs::write(dir.path().join("tests").join("note.txt"), "").unwrap();
        std::fs::write(dir.path().join("tests").join("sub").join("c.cpp"), "").unwrap();
        let found = discover_tests(&m).unwrap();
        let names: Vec<&str> = found.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn duplicate_stems_are_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"}}",
        );
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(dir.path().join("tests").join("a.cpp"), "").unwrap();
        std::fs::write(dir.path().join("tests").join("a.cxx"), "").unwrap();
        let err = discover_tests(&m).unwrap_err().to_string();
        assert!(err.contains('a'), "got: {err}");
    }
}
