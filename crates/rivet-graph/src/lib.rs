//! Build graph of compile/link/archive steps.
//!
//! Build graph of compile/link/archive steps for multi-file packages
//! with content-hash incremental rebuilds.
//!
//! Layout (per package, per profile):
//!
//! ```text
//! target/debug|release/
//!   build/<tag>/*.o[bj]   per-target objects (`bin-<name>`, `lib-<name>`)
//!   build/<lib>.a|.lib    static archives
//!   deps/<tag>/*.d         GCC/Clang depfiles (`-MMD -MF`)
//!   incremental/rivet-state.json  fingerprints + source/dep hashes + digests
//!   <bin>[.exe]            linked executables
//! ```
//!
//! [`plan`] resolves a package into a [`PackagePlan`] (lib archive + bins).
//! [`is_fresh_unit`] probes freshness without building; [`build_unit`]
//! compiles stale TUs in parallel ([`schedule`]), archives, links, records
//! state, and returns a [`UnitSummary`]. Workspace orchestration (members,
//! path deps, topo order) lives in [`workspace`]; the CLI drives one unit at
//! a time in that order.
//!
//! Compiler diagnostics are captured per task and replayed byte-for-byte on
//! failure, so errors still pass through unmodified without interleaving.

pub mod compile_commands;
pub mod depfile;
pub mod features;
pub mod schedule;
pub mod script;
pub mod state;
pub mod test;
pub mod workspace;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};

use rivet_manifest as m;
use rivet_manifest::Manifest;
use rivet_toolchain::{CompileOptions, CxxToolchain, DepStyle};
use thiserror::Error;

pub use features::{
    activated_features, activated_features_lenient, active_optional_deps, FeatureFlags,
};
pub use test::{discover_tests, DiscoveredTest};

use workspace::UnitInputs;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Build errors. Always actionable: what failed, where, how to fix.
#[derive(Debug, Error)]
pub enum Error {
    /// No buildable targets in the package.
    #[error("no targets found in {dir}. Fix: create src/main.cpp (bin), src/lib.cpp (lib), or add `bin`/`lib` to rivet.json")]
    NoTargets {
        /// Package directory searched.
        dir: String,
    },
    /// A source file is missing.
    #[error(
        "source file not found: {path}. Fix: create it or fix the `bin`/`lib` path in rivet.json"
    )]
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
    /// Compiler/linker/archiver exited non-zero (diagnostics replayed above).
    #[error("build failed: {what}")]
    CompileFailed {
        /// What failed.
        what: String,
    },
    /// A path dependency directory is missing or unreadable.
    #[error("path dependency `{name}` not found: {path}. Fix: check the `path` in rivet.json (relative to the package dir)")]
    DependencyDir {
        /// Dependency name as written.
        name: String,
        /// Directory that failed.
        path: String,
    },
    /// A path dependency has no (valid) manifest.
    #[error(
        "path dependency `{name}` at {path} has no valid rivet.json. Fix: run `rivet init` there"
    )]
    DependencyManifest {
        /// Dependency name as written.
        name: String,
        /// Dependency directory.
        path: String,
    },
    /// A git dependency was encountered outside any lock flow.
    #[error("git dependency `{name}` is not supported here (no lockfile covers it). Fix: run `rivet update` first")]
    GitDep {
        /// Dependency name.
        name: String,
    },
    /// A registry dependency was encountered outside any lock flow.
    #[error("registry dependency `{name}` is not supported here (no lockfile covers it). Fix: run `rivet update` first")]
    RegistryDep {
        /// Dependency name.
        name: String,
    },
    /// A `patch` entry uses a non-path source (only local paths supported).
    #[error("patch for `{name}` uses a non-path source. Fix: point it at a local `path`")]
    PatchSource {
        /// Dependency key.
        name: String,
    },
    /// A path dependency points at a workspace root instead of a package.
    #[error("path dependency at {path} is a workspace root. Fix: depend on a member package directory instead")]
    WorkspaceDep {
        /// Dependency directory.
        path: String,
    },
    /// A locked dependency is missing or stale in `rivet.lock.json`.
    #[error("dependency `{name}` is not covered by rivet.lock.json ({reason}). Fix: run `rivet update` (or check `patch` overrides)")]
    NotLocked {
        /// Dependency key.
        name: String,
        /// Why coverage failed.
        reason: String,
    },
    /// A locked registry package cannot be fetched by this materializer.
    #[error("registry package `{name}` cannot be fetched by this materializer")]
    RegistryFetch {
        /// Dependency key.
        name: String,
    },
    /// A locked source could not be materialized.
    #[error("could not materialize locked source for `{name}`: {message}")]
    Materialize {
        /// Dependency key.
        name: String,
        /// What failed.
        message: String,
    },
    /// A foreign adapter spec or run failed.
    #[error("adapter for `{name}` failed: {message}")]
    Adapter {
        /// Dependency key.
        name: String,
        /// What failed.
        message: String,
    },
    /// Path dependencies form a cycle.
    #[error("dependency cycle detected: {cycle}. Fix: remove one of the `path` edges")]
    DependencyCycle {
        /// Cycle rendering.
        cycle: String,
    },
    /// `-p` selected an unknown package.
    #[error("package `{name}` not found in this workspace. Available: {available}. Fix: use `-p <name>` with one of these")]
    UnknownPackage {
        /// Requested name.
        name: String,
        /// Available names, comma-joined.
        available: String,
    },
    /// A shared library target was encountered (needs an export/SONAME story).
    #[error("shared library `{name}` is not supported yet (needs an export/SONAME story). Fix: use `\"type\": \"static\"` or `\"header-only\"` for now")]
    SharedLib {
        /// Library name.
        name: String,
    },
    /// `--features` named a feature the manifest does not define.
    #[error("unknown feature `{name}`. Available: {available}. Fix: use `--features` with one of these or add it to the `features` table in rivet.json")]
    UnknownFeature {
        /// Requested name.
        name: String,
        /// Available names, comma-joined.
        available: String,
    },
    /// A `dep:<name>` feature entry names a dependency that does not exist.
    #[error("feature `{feature}` enables `dep:{name}`, but no such dependency exists. Fix: add `\"{name}\": {{ \"path\": \"...\", \"optional\": true }}` to `dependencies` in rivet.json")]
    UnknownFeatureDep {
        /// Feature carrying the entry.
        feature: String,
        /// Missing dependency name.
        name: String,
    },
    /// A `dep:<name>` feature entry names a dependency that is not optional.
    #[error("feature `{feature}` enables `dep:{name}`, but `{name}` is not optional. Fix: mark it `\"optional\": true` in `dependencies` in rivet.json")]
    NonOptionalFeatureDep {
        /// Feature carrying the entry.
        feature: String,
        /// Non-optional dependency name.
        name: String,
    },
    /// Two `tests/` files share a binary stem.
    #[error("duplicate test name `{name}`: two files in tests/ share the stem `{name}`. Fix: rename one of them")]
    DuplicateTestName {
        /// Colliding stem.
        name: String,
    },
    /// IO failure.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// Manifest error.
    #[error("{0}")]
    Manifest(#[from] m::Error),
    /// Incremental-state error.
    #[error("{0}")]
    State(#[from] state::Error),
    /// Toolchain error.
    #[error("{0}")]
    Toolchain(#[from] rivet_toolchain::Error),
    /// Build-script failure.
    #[error("{0}")]
    Buildscript(#[from] rivet_buildscript::Error),
    /// Artifact-cache failure.
    #[error("{0}")]
    Cache(#[from] rivet_cache::Error),
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

/// What a [`TargetBuild`] produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    /// Linked executable.
    Bin,
    /// Static archive.
    StaticLib,
}

/// One translation-unit job: source → object (+ optional depfile).
#[derive(Debug, Clone)]
pub struct TuJob {
    /// Absolute source path.
    pub src_abs: PathBuf,
    /// Source path relative to the package dir, `/` separators (state key).
    pub src_rel: String,
    /// Absolute object path.
    pub obj_abs: PathBuf,
    /// Object path relative to the target dir (state record).
    pub obj_rel: String,
    /// Absolute depfile path (GCC/Clang only).
    pub depfile_abs: Option<PathBuf>,
    /// Depfile path relative to the target dir.
    pub depfile_rel: Option<String>,
}

/// One binary or static-library target.
#[derive(Debug, Clone)]
pub struct TargetBuild {
    /// Bin or static lib.
    pub kind: TargetKind,
    /// Target name.
    pub name: String,
    /// State-unit tag (`bin-<name>` / `lib-<name>`).
    pub tag: String,
    /// Translation units.
    pub jobs: Vec<TuJob>,
    /// Object directory (`target/<prof>/build/<tag>`).
    pub objects_dir: PathBuf,
    /// Depfile directory (`target/<prof>/deps/<tag>`).
    pub depfiles_dir: PathBuf,
    /// Linked output (exe or archive).
    pub output: PathBuf,
}

/// What [`build_unit`] will do for one package.
#[derive(Debug, Clone)]
pub struct PackagePlan {
    /// Profile name (`dev` or `release`).
    pub profile: String,
    /// Binaries to build.
    pub bins: Vec<TargetBuild>,
    /// Static library to build, if any.
    pub lib: Option<TargetBuild>,
    /// `true` when the package also carries a header-only lib (no archive;
    /// contributes only `include/` to dependents).
    pub header_only: bool,
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

/// Reject unknown profiles.
fn check_profile(profile: &str) -> Result<(), Error> {
    if profile != "dev" && profile != "release" {
        return Err(Error::UnknownProfile {
            profile: profile.to_string(),
        });
    }
    Ok(())
}

/// Object-file stem for a source: package-relative when possible
/// (readable), else the full absolute path, with separators replaced.
/// Collisions within a target get a path-hash suffix.
fn obj_stem(source_abs: &Path, pkg_dir: &Path, taken: &mut BTreeSet<String>) -> String {
    let base = match source_abs.strip_prefix(pkg_dir) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => source_abs.display().to_string(),
    };
    let mut stem: String = base
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == ':' {
                '_'
            } else {
                c
            }
        })
        .collect();
    if taken.contains(&stem) {
        stem.push('_');
        stem.push_str(&state::hash_bytes(source_abs.display().to_string().as_bytes())[..8]);
        // In the absurd case of a double collision, keep extending.
        let mut n = 0u32;
        while taken.contains(&stem) {
            n += 1;
            stem.push_str(&n.to_string());
        }
    }
    taken.insert(stem.clone());
    stem
}

/// `src_rel` key for a source (package-relative, `/` separators).
fn src_rel_key(source_abs: &Path, pkg_dir: &Path) -> String {
    match source_abs.strip_prefix(pkg_dir) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => source_abs.display().to_string(),
    }
}

/// Build one [`TargetBuild`] from resolved sources.
fn make_target(
    kind: TargetKind,
    name: &str,
    sources: Vec<PathBuf>,
    manifest: &Manifest,
    target_dir: &Path,
    toolchain: &CxxToolchain,
    output: PathBuf,
) -> TargetBuild {
    let tag = match kind {
        TargetKind::Bin => format!("bin-{name}"),
        TargetKind::StaticLib => format!("lib-{name}"),
    };
    let objects_dir = target_dir.join("build").join(&tag);
    let depfiles_dir = target_dir.join("deps").join(&tag);
    let ext = toolchain.obj_extension();
    let want_depfile = toolchain.dep_style() == DepStyle::GccDepfile;
    let mut taken = BTreeSet::new();
    let jobs = sources
        .into_iter()
        .map(|src_abs| {
            let stem = obj_stem(&src_abs, &manifest.dir, &mut taken);
            let obj_abs = objects_dir.join(format!("{stem}.{ext}"));
            let obj_rel = obj_abs
                .strip_prefix(target_dir)
                .unwrap_or(&obj_abs)
                .display()
                .to_string();
            let (depfile_abs, depfile_rel) = if want_depfile {
                let abs = depfiles_dir.join(format!("{stem}.d"));
                let rel = abs
                    .strip_prefix(target_dir)
                    .unwrap_or(&abs)
                    .display()
                    .to_string();
                (Some(abs), Some(rel))
            } else {
                (None, None)
            };
            TuJob {
                src_rel: src_rel_key(&src_abs, &manifest.dir),
                src_abs,
                obj_abs,
                obj_rel,
                depfile_abs,
                depfile_rel,
            }
        })
        .collect();
    TargetBuild {
        kind,
        name: name.to_string(),
        tag,
        jobs,
        objects_dir,
        depfiles_dir,
        output,
    }
}

/// Default target dir for a package (`<pkg>/target/debug|release`).
pub fn default_target_dir(pkg_dir: &Path, profile: &str) -> PathBuf {
    pkg_dir.join("target").join(profile_dir(profile))
}

/// Resolve the build plan for `profile` (`dev` or `release`).
///
/// Needs a toolchain only for file extensions, not for detection: pass any
/// toolchain of the right family (the CLI passes the detected one).
pub fn plan_with(
    manifest: &Manifest,
    profile: &str,
    toolchain: &CxxToolchain,
) -> Result<PackagePlan, Error> {
    let target_dir = default_target_dir(&manifest.dir, profile);
    plan_with_target(manifest, profile, toolchain, &target_dir)
}

/// Resolve the build plan into an explicit target dir.
///
/// Locked-materialized units (immutable checkouts, vendor copies) build here
/// instead of the package default so the cache/vendor trees stay pristine.
pub fn plan_with_target(
    manifest: &Manifest,
    profile: &str,
    toolchain: &CxxToolchain,
    target_dir: &Path,
) -> Result<PackagePlan, Error> {
    check_profile(profile)?;
    let resolved_lib = m::resolve_lib(manifest)?;
    if let Some(lib) = &resolved_lib {
        if lib.lib_type == "shared" {
            return Err(Error::SharedLib {
                name: lib.name.clone(),
            });
        }
        if lib.lib_type == "header-only" && m::lib_has_sources(lib) {
            return Err(m::Error::path_error(
                "lib",
                "header-only lib must not list sources. Fix: remove `lib.path`/`src/lib.cpp` or use `\"type\": \"static\"`",
            )
            .into());
        }
    }
    let bins = m::resolve_bins(manifest);
    let compiled_lib = resolved_lib.as_ref().filter(|l| m::lib_has_sources(l));
    let header_only = resolved_lib
        .as_ref()
        .is_some_and(|l| !m::lib_has_sources(l));
    if bins.is_empty() && compiled_lib.is_none() && !header_only {
        return Err(Error::NoTargets {
            dir: manifest.dir.display().to_string(),
        });
    }
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let mut plan_bins = Vec::new();
    for b in &bins {
        let sources = m::bin_full_sources(manifest, b, &resolved_lib)?;
        let output = target_dir.join(format!("{}{suffix}", b.name));
        plan_bins.push(make_target(
            TargetKind::Bin,
            &b.name,
            sources,
            manifest,
            target_dir,
            toolchain,
            output,
        ));
    }
    let plan_lib = match compiled_lib {
        Some(lib) => {
            let sources = m::lib_full_sources(manifest, lib);
            if sources.is_empty() {
                return Err(Error::MissingSource {
                    path: "lib sources (src/lib.cpp or `lib.path`)".to_string(),
                });
            }
            let output = target_dir
                .join("build")
                .join(toolchain.staticlib_filename(&lib.name));
            Some(make_target(
                TargetKind::StaticLib,
                &lib.name,
                sources,
                manifest,
                target_dir,
                toolchain,
                output,
            ))
        }
        None => None,
    };
    Ok(PackagePlan {
        profile: profile.to_string(),
        bins: plan_bins,
        lib: plan_lib,
        header_only,
        target_dir: target_dir.to_path_buf(),
    })
}

/// Compile options for a package: own `include/` first, then dependency
/// include dirs (deduped, order-preserving — `-I` order is semantics).
/// Profile `opt`/`debug`/`lto`/`sanitizers` come from the manifest profile;
/// `features` (sorted, enabled) joins the fingerprint. `extra_defines`
/// carries adapter/probe/script defines.
pub fn compile_options(
    manifest: &Manifest,
    profile: &str,
    extra_includes: &[PathBuf],
    features: &BTreeSet<String>,
    extra_defines: &[String],
) -> CompileOptions {
    let prof = if profile == "release" {
        &manifest.profile.release
    } else {
        &manifest.profile.dev
    };
    let mut include_dirs = Vec::new();
    let own = manifest.dir.join("include");
    if own.is_dir() {
        include_dirs.push(own);
    }
    for inc in extra_includes {
        if !include_dirs.contains(inc) {
            include_dirs.push(inc.clone());
        }
    }
    CompileOptions {
        std: m::effective_standard(manifest).to_string(),
        opt: prof.opt,
        debug: prof.debug,
        defines: extra_defines.to_vec(),
        include_dirs,
        lto: prof.lto,
        sanitizers: prof.sanitizers.clone(),
        features: features.iter().cloned().collect(),
    }
}

/// Full compile options: base options plus adapter/probe defines and the
/// build script's defines/includes (deduped, order-preserving).
fn full_compile_options(
    manifest: &Manifest,
    profile: &str,
    inputs: &UnitInputs,
    features: &BTreeSet<String>,
    script: &script::ScriptUse,
) -> CompileOptions {
    let mut opts = compile_options(
        manifest,
        profile,
        &inputs.include_dirs,
        features,
        &inputs.defines,
    );
    for d in &script.defines {
        if !opts.defines.contains(d) {
            opts.defines.push(d.clone());
        }
    }
    for inc in &script.includes {
        if !opts.include_dirs.contains(inc) {
            opts.include_dirs.push(inc.clone());
        }
    }
    opts
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

/// Options controlling build execution.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// Print each executed command before running it.
    pub verbose: bool,
    /// Parallel compile jobs (`0` = auto = cores).
    pub jobs: usize,
    /// Stop scheduling new compiles after the first failure.
    pub fail_fast: bool,
    /// Artifact-cache root (`~/.rivet` layout). `Some` enables
    /// whole-package cache lookup/store around the build; `None` disables.
    pub cache_dir: Option<PathBuf>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            verbose: false,
            jobs: 0,
            fail_fast: true,
            cache_dir: None,
        }
    }
}

/// What was rebuilt for one package.
#[derive(Debug, Clone, Default)]
pub struct UnitSummary {
    /// Package name.
    pub package: String,
    /// All outputs (exes + archives).
    pub outputs: Vec<PathBuf>,
    /// `(target tag, src rel)` pairs recompiled.
    pub recompiled: Vec<(String, String)>,
    /// Outputs relinked/re-archived.
    pub relinked: Vec<PathBuf>,
    /// Verbose log lines (`Running …`, header counts).
    pub log: Vec<String>,
    /// `true` when nothing needed doing.
    pub fresh: bool,
    /// `true` when outputs were restored from the artifact cache instead
    /// of compiling (CLI prints `Cached`).
    pub cached: bool,
}

/// Fingerprint for a package build.
///
/// Covers toolchain + resolved flags + features + link-only inputs.
/// `script` contributes its defines/includes through the options;
/// `lib_dirs`/`sys_libs` (link-only, invisible to the compile fingerprint)
/// join as a content hash so probe changes rebuild.
fn fingerprint_for(
    manifest: &Manifest,
    profile: &str,
    inputs: &UnitInputs,
    toolchain: &CxxToolchain,
    features: &BTreeSet<String>,
    script: &script::ScriptUse,
) -> String {
    let opts = full_compile_options(manifest, profile, inputs, features, script);
    let mut sys = String::new();
    for dir in inputs.lib_dirs.iter().chain(script.lib_dirs.iter()) {
        sys.push_str(&dir.display().to_string());
        sys.push('\0');
    }
    for name in inputs.sys_libs.iter().chain(script.sys_libs.iter()) {
        sys.push_str(name);
        sys.push('\0');
    }
    format!(
        "{}|syslibs={}",
        toolchain.fingerprint(&opts),
        state::hash_bytes(sys.as_bytes())
    )
}

/// One stale translation unit awaiting compilation.
struct StaleTu {
    tag: String,
    job: TuJob,
    src_hash: String,
}

/// Freshness assessment: stale TUs plus stale link/archive outputs.
struct Assessment {
    stale_tus: Vec<StaleTu>,
    /// `(output id, output path)` needing a link/archive step.
    stale_links: Vec<(String, PathBuf)>,
    /// Object `(path, hash)` snapshot per output id behind the
    /// assess-time link digest. The build reuses these for targets it
    /// did not recompile and only re-hashes the (few) archives live, so
    /// dependency rebuilds or swapped archives are still caught while
    /// hundreds of objects are never read twice.
    link_objs: BTreeMap<String, LinkSnapshot>,
}

/// Assess freshness, pruning artifacts orphaned by removed sources/targets.
/// Pruned files are deleted (missing files ignored); state is only saved by
/// [`build_unit`], so a probe never persists anything.
fn assess(
    plan: &PackagePlan,
    inputs: &UnitInputs,
    script: &script::ScriptUse,
    fingerprint: &str,
) -> Result<(state::BuildState, Assessment), Error> {
    let (mut st, usable) = state::BuildState::load(&plan.target_dir, fingerprint)?;
    // Live set: tags -> src rels, plus link ids.
    let mut live_tags: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut live_links: BTreeSet<String> = BTreeSet::new();
    let mut targets: Vec<&TargetBuild> = plan.bins.iter().collect();
    if let Some(lib) = &plan.lib {
        targets.push(lib);
    }
    for t in &targets {
        live_tags.insert(
            t.tag.clone(),
            t.jobs.iter().map(|j| j.src_rel.clone()).collect(),
        );
        live_links.insert(link_id(t));
    }
    // Prune dead units / sources (delete orphaned artifacts quietly).
    for unit in st.units() {
        if !live_tags.contains_key(&unit) {
            for (obj_rel, dep_rel) in st.remove_unit(&unit) {
                remove_quiet(&plan.target_dir.join(obj_rel));
                if let Some(d) = dep_rel {
                    remove_quiet(&plan.target_dir.join(d));
                }
            }
        }
    }
    for (tag, live_srcs) in &live_tags {
        for src in st.sources_of(tag) {
            if !live_srcs.contains(&src) {
                if let Some((obj_rel, dep_rel)) = st.remove_source(tag, &src) {
                    remove_quiet(&plan.target_dir.join(obj_rel));
                    if let Some(d) = dep_rel {
                        remove_quiet(&plan.target_dir.join(d));
                    }
                }
            }
        }
    }
    for id in st.link_ids() {
        if !live_links.contains(&id) {
            st.remove_link(&id);
        }
    }
    // Stale TUs. Content hashes are memoized per assessment: hundreds
    // of TUs typically share the same SDK headers, and re-hashing those
    // per TU dominates no-op rebuilds.
    let mut hashes: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut stale_tus = Vec::new();
    for t in &targets {
        for job in &t.jobs {
            let src_hash =
                cached_hash(&mut hashes, &job.src_abs)?.ok_or_else(|| Error::MissingSource {
                    path: job.src_abs.display().to_string(),
                })?;
            let stale = if !usable {
                true
            } else {
                match st.tu(&t.tag, &job.src_rel) {
                    None => true,
                    Some(rec) => {
                        !job.obj_abs.is_file()
                            || rec.hash != src_hash
                            || deps_changed(&rec.deps, &rec.dep_hashes, &mut hashes)?
                    }
                }
            };
            if stale {
                stale_tus.push(StaleTu {
                    tag: t.tag.clone(),
                    job: job.clone(),
                    src_hash,
                });
            }
        }
    }
    // Stale links: missing output or digest mismatch over current inputs.
    // Object hashes behind each digest are snapshotted so the build can
    // reuse them for targets it does not recompile (archives stay live).
    let mut stale_links = Vec::new();
    let mut link_objs = BTreeMap::new();
    for t in &targets {
        let (digest, objs) =
            current_link_digest(plan, t, inputs, script, fingerprint, &mut hashes)?;
        let stale = match &digest {
            None => true,
            Some(d) => st.link(&link_id(t)).map(|s| s != d).unwrap_or(true),
        };
        if stale {
            stale_links.push((link_id(t), t.output.clone()));
        }
        if digest.is_some() {
            link_objs.insert(link_id(t), objs);
        }
    }
    Ok((
        st,
        Assessment {
            stale_tus,
            stale_links,
            link_objs,
        },
    ))
}

/// Content hash with per-assessment memoization (shared headers hash once).
fn cached_hash(
    cache: &mut HashMap<PathBuf, Option<String>>,
    path: &Path,
) -> Result<Option<String>, Error> {
    if let Some(hit) = cache.get(path) {
        return Ok(hit.clone());
    }
    let hash = state::hash_file(path)?;
    cache.insert(path.to_path_buf(), hash.clone());
    Ok(hash)
}

/// `true` when any recorded dep hash differs from disk (missing counts).
fn deps_changed(
    deps: &[String],
    hashes: &BTreeMap<String, String>,
    cache: &mut HashMap<PathBuf, Option<String>>,
) -> Result<bool, Error> {
    for dep in deps {
        let current = cached_hash(cache, Path::new(dep))?;
        if current.as_ref() != hashes.get(dep) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Link-output id for a target.
fn link_id(t: &TargetBuild) -> String {
    match t.kind {
        TargetKind::Bin => format!("bin:{}", t.name),
        TargetKind::StaticLib => format!("lib:{}", t.name),
    }
}

/// Digest over current link inputs for an output, or `None` when any input
/// is missing (object or archive not built yet), plus the object
/// [`LinkSnapshot`] behind the digest. Paths join the digest
/// alongside contents so a moved-but-identical input still relinks; link
/// search dirs and bare names join as flag strings. Content hashes come
/// from the shared memo map: shared headers and archives hash once.
fn current_link_digest(
    plan: &PackagePlan,
    t: &TargetBuild,
    inputs: &UnitInputs,
    script: &script::ScriptUse,
    fingerprint: &str,
    hashes: &mut HashMap<PathBuf, Option<String>>,
) -> Result<(Option<String>, LinkSnapshot), Error> {
    let mut objs = Vec::new();
    for job in &t.jobs {
        match cached_hash(hashes, &job.obj_abs)? {
            Some(h) => objs.push((job.obj_abs.clone(), h)),
            None => return Ok((None, Vec::new())),
        }
    }
    let (archives, lib_dirs, sys_libs) = full_link_inputs(plan, t, inputs, script);
    let mut archive_hashes = Vec::new();
    for archive in &archives {
        match cached_hash(hashes, archive)? {
            Some(h) => archive_hashes.push((archive.clone(), h)),
            None => return Ok((None, Vec::new())),
        }
    }
    let digest = link_digest_parts(fingerprint, &objs, &archive_hashes, &lib_dirs, &sys_libs);
    Ok((Some(digest), objs))
}

/// Object `(path, hash)` snapshot behind an assess-time link digest.
/// The build reuses these for targets it did not recompile and only
/// re-hashes the (few) archives live.
type LinkSnapshot = Vec<(PathBuf, String)>;

/// Link digest reusing an assess-time object snapshot: objects are
/// provably untouched (the target was not recompiled), so only the (few)
/// archives are hashed live. Falls back to `None` when any input is
/// missing, exactly like [`current_link_digest`].
fn reuse_link_digest(
    plan: &PackagePlan,
    t: &TargetBuild,
    inputs: &UnitInputs,
    script: &script::ScriptUse,
    fingerprint: &str,
    objs: &LinkSnapshot,
    hashes: &mut HashMap<PathBuf, Option<String>>,
) -> Result<Option<String>, Error> {
    if objs.len() != t.jobs.len()
        || objs
            .iter()
            .zip(t.jobs.iter())
            .any(|((path, _), job)| path != &job.obj_abs)
    {
        return Ok(None);
    }
    let (archives, lib_dirs, sys_libs) = full_link_inputs(plan, t, inputs, script);
    let mut archive_hashes = Vec::new();
    for archive in &archives {
        match cached_hash(hashes, archive)? {
            Some(h) => archive_hashes.push((archive.clone(), h)),
            None => return Ok(None),
        }
    }
    Ok(Some(link_digest_parts(
        fingerprint,
        objs,
        &archive_hashes,
        &lib_dirs,
        &sys_libs,
    )))
}

/// Join link digest parts: object/archive path=hash pairs plus search dirs
/// and bare lib names as flag strings.
fn link_digest_parts(
    fingerprint: &str,
    objs: &[(PathBuf, String)],
    archives: &[(PathBuf, String)],
    lib_dirs: &[PathBuf],
    sys_libs: &[String],
) -> String {
    let mut parts = Vec::new();
    for (path, h) in objs {
        parts.push(format!("{}={h}", path.display()));
    }
    for (archive, h) in archives {
        parts.push(format!("{}={h}", archive.display()));
    }
    for dir in lib_dirs {
        parts.push(format!("L{}", dir.display()));
    }
    for name in sys_libs {
        parts.push(format!("l{name}"));
    }
    state::link_digest(fingerprint, &parts)
}

/// Full link inputs for a target: archives (own lib, script files, dep
/// archives in order) plus search dirs and bare names.
fn full_link_inputs(
    plan: &PackagePlan,
    t: &TargetBuild,
    inputs: &UnitInputs,
    script: &script::ScriptUse,
) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<String>) {
    let mut archives = link_archives(plan, t, inputs);
    for a in &script.archives {
        if !archives.contains(a) {
            archives.push(a.clone());
        }
    }
    let mut lib_dirs = inputs.lib_dirs.clone();
    for d in &script.lib_dirs {
        if !lib_dirs.contains(d) {
            lib_dirs.push(d.clone());
        }
    }
    let mut sys_libs = inputs.sys_libs.clone();
    for s in &script.sys_libs {
        if !sys_libs.contains(s) {
            sys_libs.push(s.clone());
        }
    }
    (archives, lib_dirs, sys_libs)
}

/// Archives on a target's link line: own lib archive (bins only) plus
/// dependency archives, in link order.
fn link_archives(plan: &PackagePlan, t: &TargetBuild, inputs: &UnitInputs) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if t.kind == TargetKind::Bin {
        if let Some(lib) = &plan.lib {
            out.push(lib.output.clone());
        }
    }
    out.extend(inputs.archives.iter().cloned());
    out
}

/// Delete a file, ignoring not-found.
fn remove_quiet(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {}
    }
}

/// Probe freshness without building anything (no dirs created, no state
/// saved). Used by the CLI to print `Fresh` vs `Compiling`. `features` is
/// the package's activated set (joins the fingerprint); sanitizer options
/// are validated so misconfiguration fails fast even on a fresh tree.
pub fn is_fresh_unit(
    manifest: &Manifest,
    plan: &PackagePlan,
    toolchain: &CxxToolchain,
    inputs: &UnitInputs,
    features: &BTreeSet<String>,
) -> Result<bool, Error> {
    let (script, script_needs_run) = script::peek_script(manifest, &plan.target_dir)?;
    if script_needs_run {
        return Ok(false);
    }
    let fingerprint = fingerprint_for(
        manifest,
        &plan.profile,
        inputs,
        toolchain,
        features,
        &script,
    );
    let opts = full_compile_options(manifest, &plan.profile, inputs, features, &script);
    toolchain.validate_options(&opts)?;
    let (_st, assessment) = assess(plan, inputs, &script, &fingerprint)?;
    Ok(assessment.stale_tus.is_empty() && assessment.stale_links.is_empty())
}

/// Build one package: compile stale TUs in parallel, archive, link, record
/// state. Returns what was rebuilt. `features` is the package's activated
/// set (fingerprint input).
pub fn build_unit(
    manifest: &Manifest,
    plan: &PackagePlan,
    toolchain: &CxxToolchain,
    inputs: &UnitInputs,
    opts: &BuildOptions,
    features: &BTreeSet<String>,
) -> Result<UnitSummary, Error> {
    let package = m::package_name(manifest).unwrap_or("<unknown>").to_string();
    for sub in ["deps", "build", "incremental"] {
        std::fs::create_dir_all(plan.target_dir.join(sub))?;
    }
    // Build script first: its outputs feed flags, links, fingerprint, and
    // the cache key. The script compiles against base options (it cannot
    // depend on its own output).
    let script_out_dir = plan.target_dir.join("build").join("script");
    let base_opts = full_compile_options(
        manifest,
        &plan.profile,
        inputs,
        features,
        &script::ScriptUse::default(),
    );
    let script = script::run_script_if_needed(
        manifest,
        toolchain,
        &base_opts,
        &plan.target_dir,
        &script_out_dir,
        opts.verbose,
    )?;
    let fingerprint = fingerprint_for(
        manifest,
        &plan.profile,
        inputs,
        toolchain,
        features,
        &script,
    );
    let compile_opts = full_compile_options(manifest, &plan.profile, inputs, features, &script);
    toolchain.validate_options(&compile_opts)?;
    let mut targets: Vec<&TargetBuild> = plan.bins.iter().collect();
    if let Some(lib) = &plan.lib {
        targets.push(lib);
    }
    for t in &targets {
        std::fs::create_dir_all(&t.objects_dir)?;
        std::fs::create_dir_all(&t.depfiles_dir)?;
        // Outputs may live in subdirs of the target dir (`tests/`).
        if let Some(parent) = t.output.parent() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let (mut st, assessment) = assess(plan, inputs, &script, &fingerprint)?;
    let mut summary = UnitSummary {
        package,
        outputs: all_outputs(plan),
        ..Default::default()
    };
    if script.ran {
        summary.log.push("ran build.rivet.cpp".to_string());
    }
    for line in &script.log {
        summary.log.push(line.clone());
    }

    // Artifact cache: on a hit, restore outputs + state and skip the build.
    if !assessment.stale_tus.is_empty() || !assessment.stale_links.is_empty() {
        if let Some(root) = &opts.cache_dir {
            let key = cache_key_for(manifest, plan, &fingerprint, features)?;
            if try_cache_restore(root, &key, plan, &mut st, &mut summary)? {
                summary.cached = true;
                return Ok(summary);
            }
        }
    }

    // 1. Parallel compiles.
    if !assessment.stale_tus.is_empty() {
        let tasks: Vec<schedule::Task> = assessment
            .stale_tus
            .iter()
            .map(|stale| compile_task(manifest, toolchain, &compile_opts, stale))
            .collect();
        let outcomes = schedule::run(tasks, opts.jobs, opts.fail_fast);
        let mut failures: Vec<String> = Vec::new();
        for outcome in &outcomes {
            if opts.verbose {
                summary.log.push(format!("Running `{}`", outcome.display));
                for note in &outcome
                    .result
                    .as_ref()
                    .map(|s| s.notes.clone())
                    .unwrap_or_default()
                {
                    summary.log.push(note.clone());
                }
            }
            match &outcome.result {
                Ok(_) => {
                    let (tag, rel) = split_task_name(&outcome.name);
                    summary.recompiled.push((tag, rel));
                }
                Err(e) => {
                    // Show the failed command too: with parallel captured
                    // output there is no other place it appears.
                    eprintln!("       Running `{}`", outcome.display);
                    // Replay captured output verbatim (stdout→stdout,
                    // stderr→stderr) so diagnostics pass through unmodified.
                    let stdout = std::io::stdout();
                    let mut out = stdout.lock();
                    let _ = out.write_all(&outcome.stdout);
                    let _ = out.flush();
                    let stderr = std::io::stderr();
                    let mut err = stderr.lock();
                    let _ = err.write_all(&outcome.stderr);
                    let _ = err.flush();
                    failures.push(format!("{} ({e})", outcome.name));
                }
            }
        }
        if !failures.is_empty() {
            failures.sort();
            return Err(Error::CompileFailed {
                what: format!(
                    "compile failed for {} (diagnostics above, unmodified)",
                    failures.join(", ")
                ),
            });
        }
        // 2. Record fresh dep info for every recompiled TU (single-threaded;
        // outcome stdout is reused for MSVC-like notes).
        let by_name: BTreeMap<&str, &schedule::TaskOutcome> =
            outcomes.iter().map(|o| (o.name.as_str(), o)).collect();
        for stale in &assessment.stale_tus {
            let name = task_name(&stale.tag, &stale.job.src_rel);
            let outcome = by_name.get(name.as_str()).unwrap();
            let deps = stale_deps(manifest, plan, toolchain, stale, outcome)?;
            let mut dep_hashes = BTreeMap::new();
            for dep in &deps {
                // A header vanishing between compile and record is
                // pathological; the empty sentinel forces a rebuild (which
                // then fails loudly at the compiler) instead of silence.
                let h = state::hash_file(Path::new(dep))?.unwrap_or_default();
                dep_hashes.insert(dep.clone(), h);
            }
            st.record_tu(
                &stale.tag,
                &stale.job.src_rel,
                state::TuRecord {
                    hash: stale.src_hash.clone(),
                    object_rel: stale.job.obj_rel.clone(),
                    depfile_rel: stale.job.depfile_rel.clone(),
                    deps,
                    dep_hashes,
                },
            );
        }
    }

    // 3. Archive the static lib (if its TUs were rebuilt or it is missing).
    // One memo map serves the lib and bin digests below so shared
    // archives hash once.
    let mut build_hashes: HashMap<PathBuf, Option<String>> = HashMap::new();
    if let Some(lib) = &plan.lib {
        let lib_rebuilt = summary.recompiled.iter().any(|(tag, _)| tag == &lib.tag);
        if lib_rebuilt || !lib.output.is_file() {
            let mut cmd = toolchain.archive_command(
                &lib.jobs
                    .iter()
                    .map(|j| j.obj_abs.clone())
                    .collect::<Vec<_>>(),
                &lib.output,
            )?;
            cmd.current_dir(&manifest.dir);
            if opts.verbose {
                summary.log.push(format!("Running `{cmd:?}`"));
            }
            let out = cmd.output().map_err(|e| Error::CompileFailed {
                what: format!(
                    "could not execute {} ({e}). Fix: check `rivet toolchains` output",
                    toolchain
                        .ar
                        .as_ref()
                        .map(|a| a.display().to_string())
                        .unwrap_or_else(|| "<archiver>".to_string())
                ),
            })?;
            if !out.status.success() {
                let _ = std::io::stdout().lock().write_all(&out.stdout);
                let _ = std::io::stderr().lock().write_all(&out.stderr);
                return Err(Error::CompileFailed {
                    what: format!(
                        "archive failed for {} with exit {} (diagnostics above, unmodified)",
                        lib.name, out.status
                    ),
                });
            }
            summary.relinked.push(lib.output.clone());
        }
        // Recompiled objects changed on disk: hash everything fresh.
        // Otherwise the assess-time object snapshot is still exact and
        // only archives hash live, so a dependency re-archive still
        // re-archives here instead of going stale.
        let digest = if lib_rebuilt {
            current_link_digest(plan, lib, inputs, &script, &fingerprint, &mut build_hashes)?.0
        } else {
            match assessment.link_objs.get(&link_id(lib)) {
                Some(objs) => reuse_link_digest(
                    plan,
                    lib,
                    inputs,
                    &script,
                    &fingerprint,
                    objs,
                    &mut build_hashes,
                )?,
                None => {
                    current_link_digest(
                        plan,
                        lib,
                        inputs,
                        &script,
                        &fingerprint,
                        &mut build_hashes,
                    )?
                    .0
                }
            }
        };
        if let Some(digest) = digest {
            st.record_link(&link_id(lib), digest);
        }
    }

    // 4. Link bins whose inputs changed. Fresh bins reuse the assess-time
    // object snapshot (only archives hash live); linking never modifies
    // inputs, so the pre-link digest doubles as the post-link record and
    // no second hashing pass runs.
    for bin in &plan.bins {
        let bin_rebuilt = summary.recompiled.iter().any(|(tag, _)| tag == &bin.tag);
        let digest = if bin_rebuilt {
            current_link_digest(plan, bin, inputs, &script, &fingerprint, &mut build_hashes)?.0
        } else {
            match assessment.link_objs.get(&link_id(bin)) {
                Some(objs) => reuse_link_digest(
                    plan,
                    bin,
                    inputs,
                    &script,
                    &fingerprint,
                    objs,
                    &mut build_hashes,
                )?,
                None => {
                    current_link_digest(
                        plan,
                        bin,
                        inputs,
                        &script,
                        &fingerprint,
                        &mut build_hashes,
                    )?
                    .0
                }
            }
        };
        let needs =
            bin_rebuilt || !bin.output.is_file() || digest.as_ref() != st.link(&link_id(bin));
        if !needs {
            continue;
        }
        let objects: Vec<PathBuf> = bin.jobs.iter().map(|j| j.obj_abs.clone()).collect();
        let (archives, lib_dirs, sys_libs) = full_link_inputs(plan, bin, inputs, &script);
        let mut cmd = toolchain.link_exe_command(
            &objects,
            &archives,
            &lib_dirs,
            &sys_libs,
            &bin.output,
            &compile_opts,
        );
        cmd.current_dir(&manifest.dir);
        if opts.verbose {
            summary.log.push(format!("Running `{cmd:?}`"));
        }
        let out = cmd.output().map_err(|e| Error::CompileFailed {
            what: format!(
                "could not execute {} ({e}). Fix: check `rivet toolchains` output",
                toolchain.cxx.display()
            ),
        })?;
        if !out.status.success() {
            let _ = std::io::stdout().lock().write_all(&out.stdout);
            let _ = std::io::stderr().lock().write_all(&out.stderr);
            return Err(Error::CompileFailed {
                what: format!(
                    "link failed for {} with exit {} (diagnostics above, unmodified)",
                    bin.name, out.status
                ),
            });
        }
        summary.relinked.push(bin.output.clone());
        if let Some(digest) = digest {
            st.record_link(&link_id(bin), digest);
        }
    }

    st.save(&plan.target_dir)?;
    // Store fresh outputs in the artifact cache. Best-effort: a store
    // failure is logged, never fatal to an otherwise good build.
    if let Some(root) = &opts.cache_dir {
        let key = cache_key_for(manifest, plan, &fingerprint, features)?;
        if let Err(e) = cache_store(root, &key, plan, &st) {
            summary
                .log
                .push(format!("artifact cache store skipped: {e}"));
        }
    }
    summary.fresh = summary.recompiled.is_empty() && summary.relinked.is_empty();
    Ok(summary)
}

/// All plan outputs (exes + archives).
fn all_outputs(plan: &PackagePlan) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = plan.bins.iter().map(|b| b.output.clone()).collect();
    if let Some(lib) = &plan.lib {
        out.push(lib.output.clone());
    }
    out
}

/// Sidecar recording what a cache entry restores: output mapping plus a
/// state snapshot (the cache manifest only tracks sizes, not TU records).
/// Lives inside the artifact dir as `build-state.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CacheSidecar {
    key: String,
    outputs: Vec<SidecarOutput>,
    tus: BTreeMap<String, BTreeMap<String, state::TuState>>,
    links: BTreeMap<String, String>,
}

/// One restored output: original absolute path + stored basename.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SidecarOutput {
    original: PathBuf,
    stored: String,
}

const SIDECAR_NAME: &str = "build-state.json";

/// Cache key inputs: every source file, current content. Recorded headers
/// are NOT part of the key (they may be unknown after a `target/` wipe);
/// instead a lookup verifies stored dep hashes against current files and
/// treats any drift as a miss (then the normal build runs).
fn cache_inputs_hash(plan: &PackagePlan) -> Result<String, Error> {
    let mut files = BTreeSet::new();
    for t in plan.bins.iter().chain(plan.lib.iter()) {
        for job in &t.jobs {
            files.insert(job.src_abs.clone());
        }
    }
    let existing: Vec<PathBuf> = files.into_iter().filter(|p| p.is_file()).collect();
    Ok(rivet_cache::inputs_hash(&existing)?)
}

/// Full artifact-cache key for a package build.
fn cache_key_for(
    manifest: &Manifest,
    plan: &PackagePlan,
    fingerprint: &str,
    features: &BTreeSet<String>,
) -> Result<String, Error> {
    let name = m::package_name(manifest).unwrap_or("<unknown>");
    let version = m::package_version(manifest).unwrap_or("0.0.0");
    let features: Vec<String> = features.iter().cloned().collect();
    let inputs_hash = cache_inputs_hash(plan)?;
    // Flags ride inside the fingerprint already; pass none separately.
    Ok(rivet_cache::artifact_key(
        name,
        version,
        &features,
        fingerprint,
        &plan.profile,
        &[],
        &inputs_hash,
    ))
}

/// Try a cache restore: verify recorded dep hashes against current files,
/// copy outputs back, repopulate state, save. Returns `true` on a hit
/// (caller returns the summary as cached); any drift is a miss and the
/// normal build runs.
fn try_cache_restore(
    cache_root: &Path,
    key: &str,
    plan: &PackagePlan,
    st: &mut state::BuildState,
    summary: &mut UnitSummary,
) -> Result<bool, Error> {
    let Some(stored) = rivet_cache::lookup(cache_root, key) else {
        return Ok(false);
    };
    let sidecar_path = stored.dir.join(SIDECAR_NAME);
    let text = match std::fs::read_to_string(&sidecar_path) {
        Ok(t) => t,
        Err(_) => return Ok(false),
    };
    let sidecar: CacheSidecar = match serde_json::from_str(&text) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };
    if sidecar.key != key {
        return Ok(false);
    }
    // Verify recorded dep hashes against current files first: the key
    // covers sources only, so header drift must fail the hit (then the
    // normal build runs and recompiles precisely).
    for sources in sidecar.tus.values() {
        for rec in sources.values() {
            for dep in &rec.deps {
                let current = state::hash_file(Path::new(dep))?;
                if current.as_ref() != rec.dep_hashes.get(dep) {
                    return Ok(false);
                }
            }
        }
    }
    // Map stored basenames to cache paths; every recorded output must
    // resolve, else treat as a miss and rebuild normally.
    let mut by_name: BTreeMap<String, PathBuf> = BTreeMap::new();
    for file in &stored.files {
        if let Some(name) = file.file_name().and_then(|n| n.to_str()) {
            by_name.insert(name.to_string(), file.clone());
        }
    }
    let mut restored: Vec<PathBuf> = Vec::new();
    for output in &sidecar.outputs {
        let Some(cached) = by_name.get(&output.stored) else {
            return Ok(false);
        };
        if let Some(parent) = output.original.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(cached, &output.original)?;
        restored.push(output.original.clone());
    }
    // Repopulate incremental state from the snapshot so the next build
    // probes Fresh instead of rebuilding.
    for (unit, sources) in &sidecar.tus {
        for (src_rel, rec) in sources {
            st.record_tu(
                unit,
                src_rel,
                state::TuRecord {
                    hash: rec.hash.clone(),
                    object_rel: rec.object.clone(),
                    depfile_rel: rec.depfile.clone(),
                    deps: rec.deps.clone(),
                    dep_hashes: rec.dep_hashes.clone(),
                },
            );
        }
    }
    for (id, digest) in &sidecar.links {
        st.record_link(id, digest.clone());
    }
    st.save(&plan.target_dir)?;
    summary.outputs = restored;
    summary
        .log
        .push("restored outputs from artifact cache".to_string());
    Ok(true)
}

/// Store a fresh build in the artifact cache with its state sidecar.
fn cache_store(
    cache_root: &Path,
    key: &str,
    plan: &PackagePlan,
    st: &state::BuildState,
) -> Result<(), Error> {
    let outputs = all_outputs(plan);
    if outputs.iter().any(|o| !o.is_file()) {
        return Ok(());
    }
    let stored = rivet_cache::store(cache_root, key, &outputs)?;
    let mut by_original: BTreeMap<PathBuf, String> = BTreeMap::new();
    for (original, cached) in outputs.iter().zip(stored.files.iter()) {
        if let Some(name) = cached.file_name().and_then(|n| n.to_str()) {
            by_original.insert(original.clone(), name.to_string());
        }
    }
    let mut sidecar_outputs = Vec::new();
    for original in &outputs {
        let Some(stored) = by_original.get(original) else {
            return Ok(());
        };
        sidecar_outputs.push(SidecarOutput {
            original: original.clone(),
            stored: stored.clone(),
        });
    }
    let mut tus = BTreeMap::new();
    for unit in st.units() {
        let mut sources = BTreeMap::new();
        for src in st.sources_of(&unit) {
            if let Some(rec) = st.tu(&unit, &src) {
                sources.insert(src.clone(), rec.clone());
            }
        }
        tus.insert(unit.clone(), sources);
    }
    let mut links = BTreeMap::new();
    for id in st.link_ids() {
        if let Some(digest) = st.link(&id) {
            links.insert(id.clone(), digest.clone());
        }
    }
    let sidecar = CacheSidecar {
        key: key.to_string(),
        outputs: sidecar_outputs,
        tus,
        links,
    };
    let text = serde_json::to_string_pretty(&sidecar).map_err(|e| state::Error::Corrupt {
        path: SIDECAR_NAME.to_string(),
        message: e.to_string(),
    })?;
    std::fs::write(stored.dir.join(SIDECAR_NAME), format!("{text}\n"))?;
    Ok(())
}

/// Task name joining unit tag + source rel (replay sorts by this).
fn task_name(tag: &str, src_rel: &str) -> String {
    format!("{tag} {src_rel}")
}

/// Split a task name back into `(tag, src rel)`.
fn split_task_name(name: &str) -> (String, String) {
    match name.find(' ') {
        Some(i) => (name[..i].to_string(), name[i + 1..].to_string()),
        None => (name.to_string(), String::new()),
    }
}

/// Build one compile task: captured stdout/stderr, dep tracking on.
fn compile_task(
    manifest: &Manifest,
    toolchain: &CxxToolchain,
    opts: &CompileOptions,
    stale: &StaleTu,
) -> schedule::Task {
    let mut cmd = toolchain.compile_command(
        &stale.job.src_abs,
        &stale.job.obj_abs,
        stale.job.depfile_abs.as_deref(),
        opts,
    );
    cmd.current_dir(&manifest.dir);
    let display = format!("{cmd:?}");
    let name = task_name(&stale.tag, &stale.job.src_rel);
    let is_msvc = toolchain.dep_style() == DepStyle::MsvcShowIncludes;
    schedule::Task {
        name: name.clone(),
        run: Box::new(move || {
            let out = match cmd.output() {
                Ok(o) => o,
                Err(e) => {
                    return schedule::TaskOutcome {
                        name,
                        display,
                        result: Err(format!("could not execute compiler ({e})")),
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    };
                }
            };
            if !out.status.success() {
                return schedule::TaskOutcome {
                    name,
                    display,
                    result: Err(format!("exit {}", out.status)),
                    stdout: out.stdout,
                    stderr: out.stderr,
                };
            }
            let mut notes = Vec::new();
            if is_msvc {
                let count = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter(|l| l.starts_with("Note: including file:"))
                    .count();
                notes.push(format!("tracked {count} headers"));
            }
            schedule::TaskOutcome {
                name,
                display,
                result: Ok(schedule::TaskSuccess { notes }),
                stdout: out.stdout,
                stderr: out.stderr,
            }
        }),
    }
}

/// Header list for a freshly compiled TU: GCC reads its depfile, MSVC-like
/// reuses the captured `/showIncludes` stdout.
fn stale_deps(
    manifest: &Manifest,
    _plan: &PackagePlan,
    toolchain: &CxxToolchain,
    stale: &StaleTu,
    outcome: &schedule::TaskOutcome,
) -> Result<Vec<String>, Error> {
    let deps = match toolchain.dep_style() {
        DepStyle::GccDepfile => {
            let path = stale
                .job
                .depfile_abs
                .as_ref()
                .ok_or_else(|| Error::CompileFailed {
                    what: "depfile tracking misconfigured (no depfile for GCC-style compile)"
                        .to_string(),
                })?;
            match std::fs::read_to_string(path) {
                Ok(text) => depfile::parse_gcc_depfile(&text, &manifest.dir),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(e) => return Err(Error::Io(e)),
            }
        }
        DepStyle::MsvcShowIncludes => {
            depfile::parse_show_includes(&String::from_utf8_lossy(&outcome.stdout), &manifest.dir)
        }
    };
    Ok(deps.into_iter().map(|p| p.display().to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivet_toolchain::ToolchainKind;

    fn fake_toolchain() -> CxxToolchain {
        CxxToolchain {
            kind: ToolchainKind::Gcc,
            cxx: PathBuf::from("g++"),
            version: "test".to_string(),
            msvc_env: None,
            ar: None,
            target: None,
            found_via: "test".to_string(),
        }
    }

    fn manifest_in(dir: &Path, json: &str) -> Manifest {
        std::fs::write(dir.join("rivet.json"), json).unwrap();
        let mut m = m::parse_str(json).unwrap();
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
        let err = plan_with(&m, "fast", &fake_toolchain())
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown profile"), "got: {err}");
    }

    #[test]
    fn no_targets_without_main_or_lib() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"}}",
        );
        let err = plan_with(&m, "dev", &fake_toolchain())
            .unwrap_err()
            .to_string();
        assert!(err.contains("no targets"), "got: {err}");
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
        let p = plan_with(&m, "dev", &fake_toolchain()).unwrap();
        assert_eq!(p.bins.len(), 1);
        assert_eq!(p.bins[0].name, "hello");
        assert_eq!(p.bins[0].tag, "bin-hello");
        assert!(p.target_dir.ends_with(Path::new("target").join("debug")));
        assert!(p.lib.is_none());
    }

    #[test]
    fn profile_dir_mirrors_cargo() {
        assert_eq!(profile_dir("dev"), "debug");
        assert_eq!(profile_dir("release"), "release");
    }

    #[test]
    fn lib_and_bin_plan_tags_and_outputs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        for f in ["main.cpp", "lib.cpp", "util.cpp"] {
            std::fs::write(dir.path().join("src").join(f), "").unwrap();
        }
        let m = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"}}",
        );
        let tc = fake_toolchain();
        let p = plan_with(&m, "dev", &tc).unwrap();
        let lib = p.lib.as_ref().unwrap();
        assert_eq!(lib.tag, "lib-a");
        assert_eq!(lib.jobs.len(), 2, "lib owns lib.cpp + helper");
        assert_eq!(p.bins[0].jobs.len(), 1, "bin is entry-only with a lib");
        assert!(lib.output.to_string_lossy().ends_with("liba.a"));
        // Distinct object dirs: no bin/lib collision.
        assert_ne!(lib.objects_dir, p.bins[0].objects_dir);
    }

    #[test]
    fn shared_lib_is_an_m7_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.cpp"), "").unwrap();
        let m = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"},\"lib\":{\"type\":\"shared\"}}",
        );
        let err = plan_with(&m, "dev", &fake_toolchain())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not supported"), "got: {err}");
    }

    #[test]
    fn header_only_package_plans_without_archive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("include")).unwrap();
        std::fs::write(dir.path().join("include").join("a.h"), "").unwrap();
        let m = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"}}",
        );
        let p = plan_with(&m, "dev", &fake_toolchain()).unwrap();
        assert!(p.bins.is_empty());
        assert!(p.lib.is_none());
        assert!(p.header_only);
    }

    #[test]
    fn obj_stem_collisions_get_hash_suffix() {
        let pkg = Path::new("/pkg");
        let mut taken = BTreeSet::new();
        let a = obj_stem(Path::new("/pkg/src/a.cpp"), pkg, &mut taken);
        // Same stem through a different absolute path must not collide.
        taken.insert("other_src_a.cpp".to_string());
        let b = obj_stem(Path::new("/other/src/a.cpp"), pkg, &mut taken);
        assert_ne!(a, b, "collision must be disambiguated");
        assert!(a.starts_with("src_a.cpp"));
        // Exact duplicate stem forces the hash suffix path.
        let mut taken2 = BTreeSet::new();
        taken2.insert("src_a.cpp".to_string());
        let c = obj_stem(Path::new("/pkg/src/a.cpp"), pkg, &mut taken2);
        assert_ne!(c, "src_a.cpp");
        assert!(c.starts_with("src_a.cpp_"));
    }

    #[test]
    fn link_archives_order_own_lib_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        for f in ["main.cpp", "lib.cpp"] {
            std::fs::write(dir.path().join("src").join(f), "").unwrap();
        }
        let m = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"}}",
        );
        let p = plan_with(&m, "dev", &fake_toolchain()).unwrap();
        let inputs = UnitInputs {
            archives: vec![PathBuf::from("/dep/libd.a")],
            include_dirs: Vec::new(),
            lib_dirs: Vec::new(),
            sys_libs: Vec::new(),
            defines: Vec::new(),
        };
        let archives = link_archives(&p, &p.bins[0], &inputs);
        assert_eq!(archives.len(), 2);
        assert_eq!(archives[0], p.lib.as_ref().unwrap().output);
        assert_eq!(archives[1], PathBuf::from("/dep/libd.a"));
    }
}
