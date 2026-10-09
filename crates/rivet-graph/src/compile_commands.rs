//! `compile_commands.json` generation (Clangd-style compilation database).
//!
//! One entry per translation unit: `{directory, arguments, file}`, where
//! `arguments` is the compiler driver plus the full compile flags plus the
//! source. Entries cover bins, the lib, and tests (when present).

use std::collections::BTreeSet;
use std::path::Path;

use rivet_manifest::Manifest;
use rivet_toolchain::CxxToolchain;
use serde::Serialize;

use super::workspace::UnitInputs;
use super::{Error, PackagePlan, TargetBuild};

/// One compilation-database entry.
#[derive(Debug, Clone, Serialize)]
pub struct CompileEntry {
    /// Package directory the command runs in (absolute).
    pub directory: String,
    /// Driver + full flags + source.
    pub arguments: Vec<String>,
    /// Absolute source path.
    pub file: String,
}

/// Entries for every TU in `plan` (bins + lib) under `manifest`'s flags.
pub fn compile_commands_for(
    manifest: &Manifest,
    plan: &PackagePlan,
    toolchain: &CxxToolchain,
    inputs: &UnitInputs,
    features: &BTreeSet<String>,
) -> Result<Vec<CompileEntry>, Error> {
    let opts = super::compile_options(manifest, &plan.profile, &inputs.include_dirs, features);
    toolchain.validate_options(&opts)?;
    let mut targets: Vec<&TargetBuild> = plan.bins.iter().collect();
    if let Some(lib) = &plan.lib {
        targets.push(lib);
    }
    let mut out = Vec::new();
    for target in targets {
        out.extend(entries_for_target(manifest, toolchain, &opts, target));
    }
    // Tests are cheap to include (no build needed — compile-only entries).
    for test in super::test::discover_tests(manifest)? {
        let target = super::test::make_test_target(&test, manifest, plan, toolchain);
        out.extend(entries_for_target(manifest, toolchain, &opts, &target));
    }
    Ok(out)
}

fn entries_for_target(
    manifest: &Manifest,
    toolchain: &CxxToolchain,
    opts: &rivet_toolchain::CompileOptions,
    target: &TargetBuild,
) -> Vec<CompileEntry> {
    let mut out = Vec::new();
    for job in &target.jobs {
        let mut arguments = vec![toolchain.cxx.display().to_string()];
        arguments.extend(
            toolchain
                .compile_only_args(&job.src_abs, &job.obj_abs, opts)
                .iter()
                .map(|a| a.to_string_lossy().into_owned()),
        );
        out.push(CompileEntry {
            directory: manifest.dir.display().to_string(),
            arguments,
            file: job.src_abs.display().to_string(),
        });
    }
    out
}

/// Write `entries` as pretty JSON to `path` (parent dirs created).
pub fn write_compile_commands(path: &Path, entries: &[CompileEntry]) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(entries).map_err(|e| Error::CompileFailed {
        what: format!("could not serialize compile_commands.json ({e})"),
    })?;
    std::fs::write(path, text)?;
    Ok(())
}
