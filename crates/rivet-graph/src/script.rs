//! Build-script orchestration for the graph.
//!
//! Packages may carry `build.rivet.cpp` (auto-detected). This module decides
//! whether the script must (re-)run, runs it through `rivet-buildscript`,
//! persists its outputs, and converts them into build inputs. Freshness
//! persistence lives beside the state file so a no-change rebuild never
//! re-executes the script: `target/<prof>/incremental/build-script.json`.
//!
//! Probes ([`peek_script`]) never run the script (safe for `is_fresh` and
//! `compile-commands`); [`run_script_if_needed`] runs it when the rerun
//! rule fires. A deleted script cleans up its persisted outputs so stale
//! defines/includes can never leak into a later build.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rivet_buildscript as bs;
use serde::{Deserialize, Serialize};

use super::Error;

/// Build inputs contributed by a script run (or its persisted outputs).
#[derive(Debug, Clone, Default)]
pub struct ScriptUse {
    /// Extra include dirs.
    pub includes: Vec<PathBuf>,
    /// Extra defines (`NAME` or `NAME=value`).
    pub defines: Vec<String>,
    /// Extra library search dirs.
    pub lib_dirs: Vec<PathBuf>,
    /// Absolute archive paths for the link line.
    pub archives: Vec<PathBuf>,
    /// Bare `-l` names for the link line.
    pub sys_libs: Vec<String>,
    /// Log lines for `-v` (script passthrough + decisions).
    pub log: Vec<String>,
    /// `true` when the script actually ran (vs reused persisted outputs).
    pub ran: bool,
}

/// Persisted script outputs + freshness snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedScript {
    includes: Vec<PathBuf>,
    libs: Vec<String>,
    lib_dirs: Vec<PathBuf>,
    defines: Vec<String>,
    script_hash: String,
    files: BTreeMap<String, Option<String>>,
    env: BTreeMap<String, Option<String>>,
}

/// Persistence path for a target dir.
fn persist_path(target_dir: &Path) -> PathBuf {
    target_dir.join("incremental").join("build-script.json")
}

/// Convert a script output (fresh or persisted) into build inputs.
/// `rivet:lib` values starting with `-l` become bare names; everything
/// else is an archive path (relative entries join the package dir —
/// the parser already absolutizes, this is belt-and-braces).
fn use_from_output(
    manifest_dir: &Path,
    libs: &[String],
    includes: &[PathBuf],
    lib_dirs: &[PathBuf],
    defines: &[String],
) -> ScriptUse {
    let mut use_ = ScriptUse {
        includes: includes.to_vec(),
        defines: defines.to_vec(),
        lib_dirs: lib_dirs.to_vec(),
        ..Default::default()
    };
    for lib in libs {
        if let Some(name) = lib.strip_prefix("-l") {
            if !name.is_empty() {
                use_.sys_libs.push(name.to_string());
            }
        } else {
            let path = PathBuf::from(lib);
            let abs = if path.is_absolute() {
                path
            } else {
                manifest_dir.join(path)
            };
            use_.archives.push(abs);
        }
    }
    use_
}

/// Load persisted outputs, if any.
fn load_persisted(target_dir: &Path) -> Option<PersistedScript> {
    let text = std::fs::read_to_string(persist_path(target_dir)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Reconstruct a [`bs::ScriptOutput`] from persisted state for
/// [`bs::needs_rerun`] (env values round-trip lossy; rerun decisions only
/// need equality, and non-UTF8 watched vars are vanishingly rare).
fn persisted_to_output(persisted: &PersistedScript) -> bs::ScriptOutput {
    let mut out = bs::ScriptOutput {
        script_hash: persisted.script_hash.clone(),
        ..Default::default()
    };
    for (path, hash) in &persisted.files {
        out.rerun_if_changed.push(PathBuf::from(path));
        out.rerun_file_hashes
            .insert(PathBuf::from(path), hash.clone());
    }
    for (var, value) in &persisted.env {
        out.rerun_if_env.push(var.clone());
        out.env_values
            .insert(var.clone(), value.clone().map(std::ffi::OsString::from));
    }
    out
}

/// Peek at script state without running anything.
///
/// Returns `(use, needs_run)`: persisted outputs (or empty) plus whether a
/// build must re-run the script. A deleted script yields empty outputs and
/// `needs_run = false` here (the build path deletes the stale file); the
/// fingerprint change alone restales dependents.
pub fn peek_script(
    manifest: &super::Manifest,
    target_dir: &Path,
) -> Result<(ScriptUse, bool), Error> {
    if !bs::has_script(&manifest.dir) {
        return Ok((ScriptUse::default(), false));
    }
    let Some(persisted) = load_persisted(target_dir) else {
        return Ok((ScriptUse::default(), true));
    };
    let current_hash = bs::hash_file_opt(&bs::script_path(&manifest.dir))?.unwrap_or_default();
    let previous = persisted_to_output(&persisted);
    if bs::needs_rerun(&manifest.dir, &previous, &current_hash) {
        return Ok((ScriptUse::default(), true));
    }
    let mut use_ = use_from_output(
        &manifest.dir,
        &persisted.libs,
        &persisted.includes,
        &persisted.lib_dirs,
        &persisted.defines,
    );
    use_.log
        .push("(reusing persisted build-script outputs)".to_string());
    Ok((use_, false))
}

/// Run the script when the rerun rule fires, else reuse persisted outputs.
/// Persists fresh outputs; deletes stale persistence when the script is
/// gone. `script_out_dir` holds the compiled script executable.
pub fn run_script_if_needed(
    manifest: &super::Manifest,
    toolchain: &rivet_toolchain::CxxToolchain,
    opts: &rivet_toolchain::CompileOptions,
    target_dir: &Path,
    script_out_dir: &Path,
    verbose: bool,
) -> Result<ScriptUse, Error> {
    if !bs::has_script(&manifest.dir) {
        // Deleted script: drop stale outputs so they can never leak back.
        let path = persist_path(target_dir);
        if path.is_file() {
            let _ = std::fs::remove_file(&path);
        }
        return Ok(ScriptUse::default());
    }
    let (reuse, needs_run) = peek_script(manifest, target_dir)?;
    if !needs_run {
        return Ok(reuse);
    }
    let output = bs::run_package_script(toolchain, &manifest.dir, script_out_dir, opts, verbose)?;
    let Some(output) = output else {
        return Ok(ScriptUse::default());
    };
    let persisted = PersistedScript {
        includes: output.includes.clone(),
        libs: output.libs.clone(),
        lib_dirs: output.lib_dirs.clone(),
        defines: output.defines.clone(),
        script_hash: output.script_hash.clone(),
        files: output
            .rerun_file_hashes
            .iter()
            .map(|(p, h)| (p.display().to_string(), h.clone()))
            .collect(),
        env: output
            .env_values
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.clone().map(|s| s.to_string_lossy().into_owned()),
                )
            })
            .collect(),
    };
    if let Some(parent) = persist_path(target_dir).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text =
        serde_json::to_string_pretty(&persisted).map_err(|e| crate::state::Error::Corrupt {
            path: persist_path(target_dir).display().to_string(),
            message: e.to_string(),
        })?;
    std::fs::write(persist_path(target_dir), format!("{text}\n"))?;
    let mut use_ = use_from_output(
        &manifest.dir,
        &output.libs,
        &output.includes,
        &output.lib_dirs,
        &output.defines,
    );
    use_.ran = true;
    use_.log = output.log.clone();
    Ok(use_)
}
