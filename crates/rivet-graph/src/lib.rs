//! Build graph of compile/link/archive steps.
//!
//! M1 scope: minimal single-step compile+link for binary targets
//! (`src/main.cpp` convention or explicit `bin` entries). The output layout
//! already matches the full design — `target/<profile>/{deps,build,
//! incremental}` — so M2 (per-TU compiles, depfiles, content-hash
//! incremental rebuilds, parallel scheduler) slots in without moving files.
//!
//! [`plan`] resolves what to build; [`build`] executes it with the detected
//! [`rivet_toolchain::CxxToolchain`]. Compiler diagnostics pass through
//! unmodified on the inherited stdio handles.

use std::path::{Path, PathBuf};
use std::time::Instant;

use rivet_manifest::{effective_bins, effective_standard, Manifest};
use rivet_toolchain::{CompileOptions, CxxToolchain};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Build errors. Always actionable: what failed, where, how to fix.
#[derive(Debug, Error)]
pub enum Error {
    /// No binary targets found.
    #[error("no binary targets found in {dir}. Fix: create src/main.cpp or add a `bin` entry to rivet.json")]
    NoTargets {
        /// Package directory searched.
        dir: String,
    },
    /// A source file is missing.
    #[error("source file not found: {path}. Fix: create it or fix the `bin` path in rivet.json")]
    MissingSource {
        /// Missing path as written.
        path: String,
    },
    /// Unknown profile name.
    #[error("unknown profile `{profile}`. Fix: use `dev` or `release` (or `build --release` for release)")]
    UnknownProfile {
        /// Requested profile.
        profile: String,
    },
    /// Compiler exited non-zero (diagnostics already printed unmodified).
    #[error("build failed: {what}")]
    CompileFailed {
        /// What failed (e.g. `cl src/main.cpp`).
        what: String,
    },
    /// IO failure.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// Manifest error.
    #[error("{0}")]
    Manifest(#[from] rivet_manifest::Error),
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

/// One binary to build.
#[derive(Debug, Clone)]
pub struct BinBuild {
    /// Binary name.
    pub name: String,
    /// Source files (M1: exactly one entry point; M2: full TU list).
    pub sources: Vec<PathBuf>,
    /// Linked executable path.
    pub output: PathBuf,
}

/// What [`build`] will do.
#[derive(Debug, Clone)]
pub struct BuildPlan {
    /// Profile name (`dev` or `release`).
    pub profile: String,
    /// Binaries to build.
    pub bins: Vec<BinBuild>,
    /// Target directory (`<pkg>/target/debug` or `<pkg>/target/release`).
    pub target_dir: PathBuf,
}

/// Output directory name for a profile, mirroring Cargo: the `dev` profile
/// builds into `target/debug/`, `release` into `target/release/`.
pub fn profile_dir(profile: &str) -> &str {
    match profile {
        "dev" => "debug",
        "release" => "release",
        _ => profile,
    }
}

/// Summary returned after a successful build.
#[derive(Debug, Clone)]
pub struct BuildSummary {
    /// Profile built.
    pub profile: String,
    /// Executables produced.
    pub outputs: Vec<PathBuf>,
    /// Wall time.
    pub elapsed: std::time::Duration,
}

/// Resolve the build plan for `profile` (`dev` or `release`).
pub fn plan(manifest: &Manifest, profile: &str) -> Result<BuildPlan, Error> {
    if profile != "dev" && profile != "release" {
        return Err(Error::UnknownProfile {
            profile: profile.to_string(),
        });
    }
    let bins = effective_bins(manifest);
    if bins.is_empty() {
        return Err(Error::NoTargets {
            dir: manifest.dir.display().to_string(),
        });
    }
    let target_dir = manifest.dir.join("target").join(profile_dir(profile));
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let mut builds = Vec::new();
    for b in bins {
        let entry = manifest.dir.join(&b.path);
        let sources = collect_sources(&entry).ok_or_else(|| Error::MissingSource {
            path: entry.display().to_string(),
        })?;
        builds.push(BinBuild {
            name: b.name.clone(),
            sources,
            output: target_dir.join(format!("{}{suffix}", b.name)),
        });
    }
    Ok(BuildPlan {
        profile: profile.to_string(),
        bins: builds,
        target_dir,
    })
}

/// Collect source files for a `bin` entry: a file → itself; a directory →
/// all `*.cpp`/`*.cc`/`*.cxx` files under it (recursive, M2-ready).
pub fn collect_sources(entry: &Path) -> Option<Vec<PathBuf>> {
    if entry.is_file() {
        return Some(vec![entry.to_path_buf()]);
    }
    if entry.is_dir() {
        let mut out: Vec<PathBuf> = walkdir::WalkDir::new(entry)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.path().to_path_buf())
            .filter(|p| {
                p.extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| matches!(x, "cpp" | "cc" | "cxx" | "C"))
            })
            .collect();
        out.sort();
        if out.is_empty() {
            return None;
        }
        return Some(out);
    }
    None
}

/// Compile options derived from the manifest + profile.
pub fn compile_options(manifest: &Manifest, profile: &str) -> CompileOptions {
    let prof = if profile == "release" {
        &manifest.profile.release
    } else {
        &manifest.profile.dev
    };
    CompileOptions {
        std: effective_standard(manifest).to_string(),
        opt: prof.opt,
        debug: prof.debug,
        defines: Vec::new(),
        include_dirs: {
            let inc = manifest.dir.join("include");
            if inc.is_dir() {
                vec![inc]
            } else {
                Vec::new()
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

/// Options controlling build execution.
#[derive(Debug, Clone, Default)]
pub struct BuildOptions {
    /// Print the full compiler command before running it.
    pub verbose: bool,
}

/// Execute a [`BuildPlan`] with `toolchain`. Compiler stdout/stderr are
/// inherited so diagnostics pass through unmodified.
pub fn build(
    plan: &BuildPlan,
    manifest: &Manifest,
    toolchain: &CxxToolchain,
    opts: &BuildOptions,
) -> Result<BuildSummary, Error> {
    let started = Instant::now();
    let compile_opts = compile_options(manifest, &plan.profile);

    // Forward-compatible layout (M2 owns deps/build/incremental contents).
    for sub in ["deps", "build", "incremental"] {
        std::fs::create_dir_all(plan.target_dir.join(sub))?;
    }

    let mut outputs = Vec::new();
    for bin in &plan.bins {
        // M1 single-step compile+link.
        let mut cmd = toolchain.build_exe_command(&bin.sources, &bin.output, &compile_opts);
        // Run from the package dir so relative `bin` paths resolve.
        cmd.current_dir(&manifest.dir);
        if opts.verbose {
            eprintln!("       Running `{cmd:?}`");
        }
        let status = cmd.status().map_err(|e| Error::CompileFailed {
            what: format!(
                "could not execute {} ({e}). Fix: check `rivet toolchains` output",
                toolchain.cxx.display()
            ),
        })?;
        if !status.success() {
            return Err(Error::CompileFailed {
                what: format!(
                    "{} failed with exit {status} (diagnostics above, unmodified)",
                    toolchain.cxx.display()
                ),
            });
        }
        outputs.push(bin.output.clone());
    }

    Ok(BuildSummary {
        profile: plan.profile.clone(),
        outputs,
        elapsed: started.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_in(dir: &Path, json: &str) -> Manifest {
        std::fs::write(dir.join("rivet.json"), json).unwrap();
        let mut m = rivet_manifest::parse_str(json).unwrap();
        m.dir = dir.to_path_buf();
        m
    }

    #[test]
    fn unknown_profile_errors_actionably() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"}}",
        );
        let err = plan(&m, "fast").unwrap_err().to_string();
        assert!(err.contains("unknown profile"), "got: {err}");
    }

    #[test]
    fn no_targets_without_main_or_bin() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"}}",
        );
        let err = plan(&m, "dev").unwrap_err().to_string();
        assert!(err.contains("no binary targets"), "got: {err}");
        assert!(err.contains("src/main.cpp"), "got: {err}");
    }

    #[test]
    fn convention_bin_resolves() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.cpp"), "int main(){}\n").unwrap();
        let m = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"hello\",\"version\":\"0.1.0\"}}",
        );
        let p = plan(&m, "dev").unwrap();
        assert_eq!(p.bins.len(), 1);
        assert_eq!(p.bins[0].name, "hello");
        assert!(p.target_dir.ends_with(Path::new("target").join("debug")));
    }

    #[test]
    fn profile_dir_mirrors_cargo() {
        assert_eq!(profile_dir("dev"), "debug");
        assert_eq!(profile_dir("release"), "release");
    }

    #[test]
    fn collect_sources_dir_uses_walkdir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src").join("sub")).unwrap();
        std::fs::write(dir.path().join("src/a.cpp"), "").unwrap();
        std::fs::write(dir.path().join("src/sub/b.cc"), "").unwrap();
        std::fs::write(dir.path().join("src/note.txt"), "").unwrap();
        let got = collect_sources(&dir.path().join("src")).unwrap();
        assert_eq!(got.len(), 2, "got: {got:?}");
    }
}
