//! Workspace + path-dependency resolution.
//!
//! [`resolve_units`] turns a root manifest into build-ordered [`Unit`]s:
//! workspace members (or the single package), filtered by `-p`, plus the
//! transitive closure of `path` dependencies. Non-path dependencies fail
//! with milestone pointers (registry → M5, git → M4); cycles fail with the
//! cycle spelled out. Every unit is keyed by its canonical directory so the
//! same package reached twice builds once.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use rivet_manifest::{self as m, Manifest};
use rivet_toolchain::CxxToolchain;

use super::features::{activated_features_lenient, active_optional_deps, FeatureFlags};
use super::Error;

/// One buildable package: its manifest plus canonical dir and direct path
/// dependencies (canonical dirs).
#[derive(Debug, Clone)]
pub struct Unit {
    /// Loaded manifest (`dir` is the canonical package dir).
    pub manifest: Manifest,
    /// Canonical package directory (symlinks resolved).
    pub dir: PathBuf,
    /// Canonical dirs of direct path dependencies.
    pub deps: Vec<PathBuf>,
    /// `true` when reached from a workspace (member or root package).
    pub from_workspace: bool,
    /// Activated features for this package (sorted; contract C1).
    pub features: BTreeSet<String>,
    /// Redirected target dir for locked-materialized units (immutable cache
    /// checkouts and vendor copies must never gain a `target/` tree — their
    /// objects/state/archives live under the scope target dir instead).
    /// `None` means the package default (`<pkg>/target/<prof>`).
    pub target_dir_override: Option<PathBuf>,
}

/// External build inputs for one unit: transitive path-dependency archives
/// (dependents first — correct static link order on GNU ld, harmless on
/// MSVC) plus transitive public include dirs.
#[derive(Debug, Clone, Default)]
pub struct UnitInputs {
    /// Transitive static archives, dependents first.
    pub archives: Vec<PathBuf>,
    /// Transitive `include/` dirs (existence-checked).
    pub include_dirs: Vec<PathBuf>,
}

/// Locked dependency view (the CLI converts `rivet_resolver::Lockfile` into
/// this; the graph stays independent of the resolver crate).
#[derive(Debug, Clone)]
pub struct LockedDep {
    /// Dependency key (= lockfile name).
    pub name: String,
    /// Locked version.
    pub version: String,
    /// Locked source.
    pub source: LockedSource,
}

/// Locked source discriminant (mirrors contract C2).
#[derive(Debug, Clone)]
pub enum LockedSource {
    /// Local path (as written in the manifest).
    Path {
        /// Path as written.
        path: String,
    },
    /// Git checkout.
    Git {
        /// Repo URL (must match the manifest textually).
        url: String,
        /// Requested ref `(kind, value)` (`tag`/`branch`/`rev`).
        git_ref: (String, String),
        /// Locked full SHA.
        sha: String,
        /// Locked `sha^{tree}` (verified at materialization).
        tree: String,
    },
    /// Registry package (fetching arrives with M5).
    Registry {
        /// Registry URL or `"default"`.
        registry: String,
        /// Locked version.
        version: String,
        /// Checksum from the index.
        checksum: String,
    },
}

/// Materialization request for one locked git dep: turn the locked source
/// into a source directory (CLI: global cache checkout, else vendor copy).
#[derive(Debug, Clone)]
pub struct MaterialRequest {
    /// Dependency key (used for cache grouping + vendor lookup).
    pub key: String,
    /// Repo URL.
    pub url: String,
    /// Locked full SHA.
    pub sha: String,
    /// Locked tree hash (verified by the materializer).
    pub tree: String,
    /// Package subdir inside the repo, as written.
    pub subdir: Option<String>,
    /// Whether to initialize submodules.
    pub submodules: bool,
}

/// Turns a locked git dep into a source directory. Implemented by the CLI
/// (which owns the git cache handle); failures are actionable strings the
/// graph wraps in [`Error::Materialize`].
pub trait Materializer {
    /// Materialize `req` (cache checkout or vendor copy), returning the
    /// usable source dir (subdir applied).
    fn materialize(&self, req: &MaterialRequest) -> Result<PathBuf, String>;
}

/// How dependency edges resolve: live manifests only, or via a lockfile.
enum Mode<'a> {
    /// Live resolution (path deps only; git/registry are milestone errors).
    Direct,
    /// Locked resolution (git via materializer, registry is an M5 error).
    Locked {
        /// Lock entries by dep key.
        locked: &'a BTreeMap<String, LockedDep>,
        /// Git materializer.
        materializer: &'a dyn Materializer,
    },
}

/// Queued BFS item: a directory plus, for locked path deps, the expected
/// (key, version) used to detect a stale lockfile at load time, plus for
/// locked-materialized git deps the dep key used for target redirection.
struct Queued {
    dir: PathBuf,
    expect_path_version: Option<(String, String)>,
    redirect_key: Option<String>,
}

/// Sanitize a dep key for use as a directory name.
fn sanitize_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Resolve the build scope for `root` (the nearest manifest to the CWD).
///
/// - Workspace root → members (+ the root package itself when present).
/// - Plain package → just it.
/// - `only` (`-p`) narrows seeds to one package by name.
/// - The transitive `path`-dependency closure is always pulled in and the
///   result is topologically sorted (dependencies first).
///
/// No features are enabled beyond each manifest's `default` (see
/// [`resolve_units_with_features`] for flag-driven activation).
pub fn resolve_units(
    root: &Manifest,
    profile: &str,
    only: Option<&str>,
) -> Result<Vec<Unit>, Error> {
    resolve_units_with_features(
        root,
        profile,
        only,
        &FeatureFlags::default(),
        &BTreeMap::new(),
    )
}

/// Resolve the build scope with feature selection.
///
/// Optional `path` dependencies that no enabled feature activates are
/// filtered out (they never enter the build). Non-path dependencies still
/// fail with milestone pointers (registry → M5, git → M4) even when
/// optional and inactive — the M4 CLI integration wires those flags.
/// `root_patch` maps dep keys to scope-root patch dirs (workspace-wide
/// overrides, applied in both modes).
pub fn resolve_units_with_features(
    root: &Manifest,
    profile: &str,
    only: Option<&str>,
    flags: &FeatureFlags,
    root_patch: &BTreeMap<String, PathBuf>,
) -> Result<Vec<Unit>, Error> {
    resolve_impl(root, profile, only, flags, &Mode::Direct, root_patch)
}

/// Resolve the build scope against a lockfile: git deps materialize from
/// locked SHAs (verified by tree hash), registry deps are an M5 error, and
/// anything the lock does not cover is [`Error::NotLocked`] (run update).
pub fn resolve_locked_units(
    root: &Manifest,
    profile: &str,
    only: Option<&str>,
    flags: &FeatureFlags,
    locked: &BTreeMap<String, LockedDep>,
    materializer: &dyn Materializer,
    root_patch: &BTreeMap<String, PathBuf>,
) -> Result<Vec<Unit>, Error> {
    resolve_impl(
        root,
        profile,
        only,
        flags,
        &Mode::Locked {
            locked,
            materializer,
        },
        root_patch,
    )
}

/// Shared BFS skeleton for [`resolve_units_with_features`] (live) and
/// [`resolve_locked_units`] (locked).
fn resolve_impl(
    root: &Manifest,
    profile: &str,
    only: Option<&str>,
    flags: &FeatureFlags,
    mode: &Mode,
    root_patch: &BTreeMap<String, PathBuf>,
) -> Result<Vec<Unit>, Error> {
    super::check_profile(profile)?;
    // Scope target base: locked-materialized units redirect their builds
    // here (`locked/<dep>/`) instead of into immutable checkouts.
    let scope_target = root.dir.join("target").join(super::profile_dir(profile));
    // 1. Seeds.
    let mut seeds: Vec<Manifest> = Vec::new();
    let from_workspace = root.workspace.is_some();
    if from_workspace {
        let members = root.workspace.as_ref().unwrap().members.clone();
        for dir in m::expand_members(&root.dir, &members)? {
            seeds.push(m::load(&dir)?);
        }
        if root.package.is_some() {
            seeds.push(root.clone());
        }
    } else {
        seeds.push(root.clone());
    }
    if let Some(name) = only {
        seeds.retain(|s| m::package_name(s) == Some(name));
        if seeds.is_empty() {
            // Offer every name we know (seeds + their path deps are unknown
            // yet; members are the actionable scope for `-p`).
            let mut available: Vec<String> = Vec::new();
            if from_workspace {
                let members = root.workspace.as_ref().unwrap().members.clone();
                if let Ok(dirs) = m::expand_members(&root.dir, &members) {
                    for dir in dirs {
                        if let Ok(man) = m::load(&dir) {
                            if let Some(n) = m::package_name(&man) {
                                available.push(n.to_string());
                            }
                        }
                    }
                }
                if let Some(n) = m::package_name(root) {
                    available.push(n.to_string());
                }
            } else if let Some(n) = m::package_name(root) {
                available.push(n.to_string());
            }
            available.sort();
            available.dedup();
            return Err(Error::UnknownPackage {
                name: name.to_string(),
                available: if available.is_empty() {
                    "<none>".to_string()
                } else {
                    available.join(", ")
                },
            });
        }
    }
    // 2. BFS over path deps (canonical dirs dedupe diamonds). Seeds are
    // inserted without the workspace check (a root package may itself
    // carry `workspace`); path deps must be plain packages. Activation is
    // lenient per package (requests apply where defined — union semantics);
    // `known` collects every reached table so requests unknown to the whole
    // scope still error below.
    let mut units: BTreeMap<PathBuf, Unit> = BTreeMap::new();
    let mut known: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<Queued> = VecDeque::new();
    for seed in seeds {
        let dir = canonical(&seed.dir)?;
        if units.contains_key(&dir) {
            continue;
        }
        let mut man = seed;
        man.dir = dir.clone();
        let features = activated_features_lenient(&man, flags)?;
        known.extend(man.features.keys().cloned());
        let active = active_optional_deps(&man, &features);
        let mut dep_dirs = direct_dep_dirs(&man, &active, mode, root_patch)?;
        dep_dirs.sort_by(|a, b| a.dir.cmp(&b.dir));
        dep_dirs.dedup_by(|a, b| a.dir == b.dir);
        queue.extend(dep_dirs.iter().map(|q| Queued {
            dir: q.dir.clone(),
            expect_path_version: q.expect_path_version.clone(),
            redirect_key: q.redirect_key.clone(),
        }));
        let dep_paths: Vec<PathBuf> = dep_dirs.into_iter().map(|q| q.dir).collect();
        units.insert(
            dir.clone(),
            Unit {
                manifest: man,
                dir,
                deps: dep_paths,
                from_workspace,
                features,
                target_dir_override: None,
            },
        );
    }
    while let Some(queued) = queue.pop_front() {
        let dep_dir = queued.dir;
        if units.contains_key(&dep_dir) {
            continue;
        }
        let mut dep_man = m::load(&dep_dir)?;
        dep_man.dir = dep_dir.clone();
        if dep_man.workspace.is_some() {
            return Err(Error::WorkspaceDep {
                path: dep_dir.display().to_string(),
            });
        }
        if let Some((key, version)) = queued.expect_path_version {
            let actual = dep_man
                .package
                .as_ref()
                .map(|p| p.version.clone())
                .unwrap_or_default();
            if actual != version {
                return Err(Error::NotLocked {
                    name: key,
                    reason: format!(
                        "locked version {version} != live version {actual}. Fix: run `rivet update`"
                    ),
                });
            }
        }
        let features = activated_features_lenient(&dep_man, flags)?;
        known.extend(dep_man.features.keys().cloned());
        let active = active_optional_deps(&dep_man, &features);
        let mut dep_dirs = direct_dep_dirs(&dep_man, &active, mode, root_patch)?;
        dep_dirs.sort_by(|a, b| a.dir.cmp(&b.dir));
        dep_dirs.dedup_by(|a, b| a.dir == b.dir);
        queue.extend(dep_dirs.iter().map(|q| Queued {
            dir: q.dir.clone(),
            expect_path_version: q.expect_path_version.clone(),
            redirect_key: q.redirect_key.clone(),
        }));
        let dep_paths: Vec<PathBuf> = dep_dirs.into_iter().map(|q| q.dir).collect();
        let target_dir_override = queued
            .redirect_key
            .map(|key| scope_target.join("locked").join(sanitize_key(&key)));
        units.insert(
            dep_dir.clone(),
            Unit {
                manifest: dep_man,
                dir: dep_dir,
                deps: dep_paths,
                from_workspace: false,
                features,
                target_dir_override,
            },
        );
    }
    // Requests unknown to every reached package are typos, not union members.
    for name in &flags.features {
        if !known.contains(name) {
            let mut available: Vec<String> = known.iter().cloned().collect();
            available.sort();
            return Err(Error::UnknownFeature {
                name: name.clone(),
                available: if available.is_empty() {
                    "<none>".to_string()
                } else {
                    available.join(", ")
                },
            });
        }
    }
    // 3. Topological order (dependencies first); report cycles.
    topo_sort(units)
}

/// Direct dependency dirs of `man`: path deps canonicalized, git deps
/// materialized from the lock (locked mode), registry deps rejected.
///
/// Optional deps outside `active` are skipped (not part of the build) in
/// both modes. In direct mode, active non-path deps fail with milestone
/// pointers (registry → M5, git → M4); in locked mode they resolve against
/// the lockfile (missing/stale coverage is [`Error::NotLocked`], locked
/// registry packages are [`Error::RegistryFetch`]).
fn direct_dep_dirs(
    man: &Manifest,
    active: &BTreeSet<String>,
    mode: &Mode,
    root_patch: &BTreeMap<String, PathBuf>,
) -> Result<Vec<Queued>, Error> {
    let mut out = Vec::new();
    for (dep_name, dep) in &man.dependencies {
        let optional = dep.is_optional();
        if optional && !active.contains(dep_name) {
            continue;
        }
        // Patch overrides (own table first, then the scope root's): local
        // paths only. Patched units build in their own target dir like path
        // deps; the lock (if any) keeps the base entry.
        if let Some(patch_dep) = man.patch.get(dep_name) {
            match patch_dep {
                m::Dependency::Detailed(d) => match &d.path {
                    Some(p) => {
                        let dir = man.dir.join(p);
                        let c = canonical(&dir).map_err(|_| Error::DependencyDir {
                            name: dep_name.clone(),
                            path: dir.display().to_string(),
                        })?;
                        if !c.join("rivet.json").is_file() {
                            return Err(Error::DependencyManifest {
                                name: dep_name.clone(),
                                path: c.display().to_string(),
                            });
                        }
                        out.push(Queued {
                            dir: c,
                            expect_path_version: None,
                            redirect_key: None,
                        });
                        continue;
                    }
                    None => {
                        return Err(Error::PatchSource {
                            name: dep_name.clone(),
                        });
                    }
                },
                m::Dependency::Simple(_) => {
                    return Err(Error::PatchSource {
                        name: dep_name.clone(),
                    });
                }
            }
        }
        if let Some(pdir) = root_patch.get(dep_name) {
            let c = canonical(pdir).map_err(|_| Error::DependencyDir {
                name: dep_name.clone(),
                path: pdir.display().to_string(),
            })?;
            if !c.join("rivet.json").is_file() {
                return Err(Error::DependencyManifest {
                    name: dep_name.clone(),
                    path: c.display().to_string(),
                });
            }
            out.push(Queued {
                dir: c,
                expect_path_version: None,
                redirect_key: None,
            });
            continue;
        }
        if let Some(path) = dep.path() {
            let dep_dir = man.dir.join(path);
            let c = canonical(&dep_dir).map_err(|_| Error::DependencyDir {
                name: dep_name.clone(),
                path: dep_dir.display().to_string(),
            })?;
            if !c.join("rivet.json").is_file() {
                return Err(Error::DependencyManifest {
                    name: dep_name.clone(),
                    path: c.display().to_string(),
                });
            }
            let expect = match mode {
                Mode::Direct => None,
                Mode::Locked { locked, .. } => {
                    let entry = locked.get(dep_name).ok_or_else(|| Error::NotLocked {
                        name: dep_name.clone(),
                        reason: "no lock entry. Fix: run `rivet update`".to_string(),
                    })?;
                    if !matches!(entry.source, LockedSource::Path { .. }) {
                        return Err(Error::NotLocked {
                            name: dep_name.clone(),
                            reason: "locked source kind changed. Fix: run `rivet update`"
                                .to_string(),
                        });
                    }
                    Some((dep_name.clone(), entry.version.clone()))
                }
            };
            out.push(Queued {
                dir: c,
                expect_path_version: expect,
                redirect_key: None,
            });
            continue;
        }
        if dep.is_git() {
            match mode {
                Mode::Direct => {
                    return Err(Error::GitDep {
                        name: dep_name.clone(),
                    });
                }
                Mode::Locked {
                    locked,
                    materializer,
                } => {
                    let entry = locked.get(dep_name).ok_or_else(|| Error::NotLocked {
                        name: dep_name.clone(),
                        reason: "no lock entry. Fix: run `rivet update`".to_string(),
                    })?;
                    let (url, gref, sha, tree) = match &entry.source {
                        LockedSource::Git {
                            url,
                            git_ref,
                            sha,
                            tree,
                        } => (url.clone(), git_ref.clone(), sha.clone(), tree.clone()),
                        _ => {
                            return Err(Error::NotLocked {
                                name: dep_name.clone(),
                                reason: "locked source kind changed. Fix: run `rivet update`"
                                    .to_string(),
                            });
                        }
                    };
                    // The lock must agree with the manifest's requested ref
                    // (textual URL match; bare entries mean the default branch).
                    let detail = match dep {
                        m::Dependency::Detailed(d) => d,
                        m::Dependency::Simple(_) => {
                            return Err(Error::NotLocked {
                                name: dep_name.clone(),
                                reason: "manifest entry changed shape. Fix: run `rivet update`"
                                    .to_string(),
                            });
                        }
                    };
                    let want_url = detail.git.clone().unwrap_or_default();
                    let want_ref = manifest_git_ref(detail);
                    if normalize_url(&want_url) != normalize_url(&url) || want_ref != gref {
                        return Err(Error::NotLocked {
                            name: dep_name.clone(),
                            reason: "requested URL/ref changed. Fix: run `rivet update`"
                                .to_string(),
                        });
                    }
                    let dir = materializer
                        .materialize(&MaterialRequest {
                            key: dep_name.clone(),
                            url,
                            sha,
                            tree,
                            subdir: detail.subdir.clone(),
                            submodules: detail.submodules.unwrap_or(false),
                        })
                        .map_err(|message| Error::Materialize {
                            name: dep_name.clone(),
                            message,
                        })?;
                    // Materialized dirs come from the cache/vendor: canonicalize
                    // (also strips Windows verbatim prefixes for cl.exe).
                    let c = canonical(&dir).map_err(|_| Error::DependencyDir {
                        name: dep_name.clone(),
                        path: dir.display().to_string(),
                    })?;
                    if !c.join("rivet.json").is_file() {
                        return Err(Error::DependencyManifest {
                            name: dep_name.clone(),
                            path: c.display().to_string(),
                        });
                    }
                    out.push(Queued {
                        dir: c,
                        expect_path_version: None,
                        redirect_key: Some(dep_name.clone()),
                    });
                }
            }
            continue;
        }
        if dep.is_registry() {
            match mode {
                Mode::Direct => {
                    return Err(Error::RegistryDep {
                        name: dep_name.clone(),
                    });
                }
                Mode::Locked { locked, .. } => {
                    locked.get(dep_name).ok_or_else(|| Error::NotLocked {
                        name: dep_name.clone(),
                        reason: "no lock entry. Fix: run `rivet update`".to_string(),
                    })?;
                    return Err(Error::RegistryFetch {
                        name: dep_name.clone(),
                    });
                }
            }
        }
    }
    Ok(out)
}

/// Manifest `tag`/`branch`/`rev` as a `(kind, value)` pair, defaulting to
/// the remote default branch exactly like the resolver records it.
fn manifest_git_ref(detail: &m::DependencyDetail) -> (String, String) {
    if let Some(t) = &detail.tag {
        ("tag".to_string(), t.clone())
    } else if let Some(b) = &detail.branch {
        ("branch".to_string(), b.clone())
    } else if let Some(r) = &detail.rev {
        ("rev".to_string(), r.clone())
    } else {
        ("branch".to_string(), "HEAD".to_string())
    }
}

/// Textual URL comparison with trailing slashes ignored.
fn normalize_url(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

/// Kahn's algorithm over `units` keyed by dir. Returns build order or a
/// [`Error::DependencyCycle`] spelling the cycle.
fn topo_sort(units: BTreeMap<PathBuf, Unit>) -> Result<Vec<Unit>, Error> {
    let mut indegree: BTreeMap<PathBuf, usize> = BTreeMap::new();
    let mut dependents: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for (dir, unit) in &units {
        indegree.entry(dir.clone()).or_insert(0);
        for dep in &unit.deps {
            // A dep outside `units` cannot happen (BFS inserts all), but a
            // missing entry means a bug — surface it, don't panic.
            if !units.contains_key(dep) {
                return Err(Error::DependencyDir {
                    name: "<unknown>".to_string(),
                    path: dep.display().to_string(),
                });
            }
            dependents.entry(dep.clone()).or_default().push(dir.clone());
            *indegree.entry(dir.clone()).or_insert(0) += 1;
        }
    }
    let mut ready: VecDeque<PathBuf> = indegree
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(dir, _)| dir.clone())
        .collect();
    let mut order = Vec::new();
    while let Some(dir) = ready.pop_front() {
        if let Some(deps) = dependents.remove(&dir) {
            for dependent in deps {
                let d = indegree.get_mut(&dependent).unwrap();
                *d -= 1;
                if *d == 0 {
                    ready.push_back(dependent);
                }
            }
        }
        order.push(dir);
    }
    if order.len() != units.len() {
        let mut remaining: Vec<String> = units
            .keys()
            .filter(|d| !order.contains(d))
            .map(|d| d.display().to_string())
            .collect();
        remaining.sort();
        return Err(Error::DependencyCycle {
            cycle: remaining.join(" -> "),
        });
    }
    Ok(order
        .into_iter()
        .map(|dir| units.get(&dir).unwrap().clone())
        .collect())
}

/// Canonicalize a package dir (symlinks resolved; must exist).
///
/// On Windows, `std::fs::canonicalize` returns `\\?\`-prefixed verbatim
/// paths, which MSVC tools (`cl.exe`, `lib.exe`) cannot open — `cl` even
/// mangles them into nonsense (`\\main.cpp`). The prefix is stripped (plain
/// disk paths; `\\?\UNC\host\share` becomes `\\host\share`), keeping
/// symlink resolution while staying consumable by every toolchain.
fn canonical(dir: &Path) -> Result<PathBuf, Error> {
    let raw = std::fs::canonicalize(dir).map_err(|e| Error::DependencyDir {
        name: "<package>".to_string(),
        path: format!("{} ({e})", dir.display()),
    })?;
    Ok(strip_verbatim(&raw))
}

/// Strip the Windows verbatim path prefix, if present.
fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.display().to_string();
    if let Some(rest) = text.strip_prefix("\\\\?\\UNC\\") {
        return PathBuf::from(format!("\\\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix("\\\\?\\") {
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

/// External inputs for `unit`: transitive archives (dependents first) and
/// include dirs. `order` is the resolved build order (topo, deps first).
pub fn unit_inputs(
    unit: &Unit,
    order: &[Unit],
    profile: &str,
    toolchain: &CxxToolchain,
) -> Result<UnitInputs, Error> {
    // Transitive closure over dep dirs (BFS from direct deps).
    let by_dir: BTreeMap<&PathBuf, &Unit> = order.iter().map(|u| (&u.dir, u)).collect();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut stack: Vec<PathBuf> = unit.deps.clone();
    while let Some(dir) = stack.pop() {
        if !seen.insert(dir.clone()) {
            continue;
        }
        if let Some(dep) = by_dir.get(&dir) {
            stack.extend(dep.deps.iter().cloned());
        }
    }
    // Topo order restricted to the closure, then reversed so dependents
    // come first on the link line (static-link order for GNU ld).
    let topo: Vec<&Unit> = order.iter().filter(|u| seen.contains(&u.dir)).collect();
    let mut inputs = UnitInputs::default();
    for dep in topo.iter().rev() {
        // Locked-materialized units build under the scope target dir
        // (never inside immutable checkouts); everything else uses the
        // package default. Archive paths must follow the same rule.
        let base = dep
            .target_dir_override
            .clone()
            .unwrap_or_else(|| dep.dir.join("target").join(super::profile_dir(profile)));
        let target = base.join("build");
        if let Ok(Some(lib)) = m::resolve_lib(&dep.manifest) {
            if rivet_manifest::lib_has_sources(&lib) && lib.lib_type == "static" {
                let archive = target.join(toolchain.staticlib_filename(&lib.name));
                // The archive may not exist yet on a fresh `is_fresh` probe;
                // link planning records it regardless, and the build creates
                // deps first (topo order), so it exists at link time.
                inputs.archives.push(archive);
            }
        }
        let inc = dep.dir.join("include");
        if inc.is_dir() && !inputs.include_dirs.contains(&inc) {
            inputs.include_dirs.push(inc);
        }
    }
    Ok(inputs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_pkg(dir: &Path, json: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("rivet.json"), json).unwrap();
    }

    fn load_dir(dir: &Path) -> Manifest {
        m::load(dir).unwrap()
    }

    #[test]
    fn single_package_resolves() {
        let dir = tempfile::tempdir().unwrap();
        write_pkg(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"}}",
        );
        let root = load_dir(dir.path());
        let units = resolve_units(&root, "dev", None).unwrap();
        assert_eq!(units.len(), 1);
    }

    #[test]
    fn path_dep_closure_and_order() {
        let dir = tempfile::tempdir().unwrap();
        write_pkg(
            &dir.path().join("app"),
            "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"},\"dependencies\":{\"lib\":{\"path\":\"../lib\"}}}",
        );
        write_pkg(
            &dir.path().join("lib"),
            "{\"package\":{\"name\":\"lib\",\"version\":\"0.1.0\"}}",
        );
        let root = load_dir(&dir.path().join("app"));
        let units = resolve_units(&root, "dev", None).unwrap();
        assert_eq!(units.len(), 2);
        // Dependencies first.
        let names: Vec<String> = units
            .iter()
            .map(|u| m::package_name(&u.manifest).unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["lib".to_string(), "app".to_string()]);
    }

    #[test]
    fn diamond_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        write_pkg(
            &dir.path().join("app"),
            "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"},\"dependencies\":{\"b\":{\"path\":\"../b\"},\"c\":{\"path\":\"../c\"}}}",
        );
        write_pkg(
            &dir.path().join("b"),
            "{\"package\":{\"name\":\"b\",\"version\":\"0.1.0\"},\"dependencies\":{\"d\":{\"path\":\"../d\"}}}",
        );
        write_pkg(
            &dir.path().join("c"),
            "{\"package\":{\"name\":\"c\",\"version\":\"0.1.0\"},\"dependencies\":{\"d\":{\"path\":\"../d\"}}}",
        );
        write_pkg(
            &dir.path().join("d"),
            "{\"package\":{\"name\":\"d\",\"version\":\"0.1.0\"}}",
        );
        let root = load_dir(&dir.path().join("app"));
        let units = resolve_units(&root, "dev", None).unwrap();
        assert_eq!(units.len(), 4);
        // d before b and c, app last.
        let names: Vec<String> = units
            .iter()
            .map(|u| m::package_name(&u.manifest).unwrap().to_string())
            .collect();
        assert_eq!(names.last().unwrap(), "app");
        assert!(
            names.iter().position(|n| n == "d").unwrap()
                < names.iter().position(|n| n == "b").unwrap()
        );
    }

    #[test]
    fn cycle_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write_pkg(
            &dir.path().join("a"),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"},\"dependencies\":{\"b\":{\"path\":\"../b\"}}}",
        );
        write_pkg(
            &dir.path().join("b"),
            "{\"package\":{\"name\":\"b\",\"version\":\"0.1.0\"},\"dependencies\":{\"a\":{\"path\":\"../a\"}}}",
        );
        let root = load_dir(&dir.path().join("a"));
        let err = resolve_units(&root, "dev", None).unwrap_err().to_string();
        assert!(err.contains("cycle"), "got: {err}");
    }

    #[test]
    fn registry_and_git_deps_point_at_milestones() {
        let dir = tempfile::tempdir().unwrap();
        write_pkg(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"},\"dependencies\":{\"fmt\":\"11\"}}",
        );
        let root = load_dir(dir.path());
        let err = resolve_units(&root, "dev", None).unwrap_err().to_string();
        assert!(err.contains("M5"), "got: {err}");
        write_pkg(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"},\"dependencies\":{\"z\":{\"git\":\"https://example.com/z.git\"}}}",
        );
        let root = load_dir(dir.path());
        let err = resolve_units(&root, "dev", None).unwrap_err().to_string();
        assert!(err.contains("M4"), "got: {err}");
    }

    #[test]
    fn unknown_package_lists_available() {
        let dir = tempfile::tempdir().unwrap();
        write_pkg(
            &dir.path().join("crates").join("app"),
            "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"}}",
        );
        write_pkg(dir.path(), "{\"workspace\":{\"members\":[\"crates/*\"]}}");
        let root = load_dir(dir.path());
        let err = resolve_units(&root, "dev", Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("app"), "got: {err}");
    }

    #[test]
    fn verbatim_prefix_stripped() {
        assert_eq!(
            strip_verbatim(Path::new("\\\\?\\C:\\proj")),
            PathBuf::from("C:\\proj")
        );
        assert_eq!(
            strip_verbatim(Path::new("\\\\?\\UNC\\host\\share")),
            PathBuf::from("\\\\host\\share")
        );
        assert_eq!(strip_verbatim(Path::new("/a/b")), PathBuf::from("/a/b"));
    }

    #[test]
    fn missing_dep_dir_is_actionable() {
        let dir = tempfile::tempdir().unwrap();
        write_pkg(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"},\"dependencies\":{\"m\":{\"path\":\"../gone\"}}}",
        );
        let root = load_dir(dir.path());
        let err = resolve_units(&root, "dev", None).unwrap_err().to_string();
        assert!(err.contains("gone"), "got: {err}");
    }

    fn names(units: &[Unit]) -> Vec<String> {
        units
            .iter()
            .map(|u| m::package_name(&u.manifest).unwrap().to_string())
            .collect()
    }

    #[test]
    fn optional_path_dep_filtered_unless_activated() {
        let dir = tempfile::tempdir().unwrap();
        write_pkg(
            &dir.path().join("app"),
            "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"},\"dependencies\":{\"opt\":{\"path\":\"../opt\",\"optional\":true}},\"features\":{\"default\":[],\"with-opt\":[\"dep:opt\"]}}",
        );
        write_pkg(
            &dir.path().join("opt"),
            "{\"package\":{\"name\":\"opt\",\"version\":\"0.1.0\"}}",
        );
        let root = load_dir(&dir.path().join("app"));

        // Default: optional dep excluded.
        let units = resolve_units(&root, "dev", None).unwrap();
        assert_eq!(names(&units), vec!["app".to_string()]);
        assert!(units[0].features.contains("default"));

        // Activated: optional dep pulled in.
        let units = resolve_units_with_features(
            &root,
            "dev",
            None,
            &FeatureFlags {
                features: vec!["with-opt".to_string()],
                ..Default::default()
            },
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(names(&units), vec!["opt".to_string(), "app".to_string()]);
        assert!(units[1].features.contains("with-opt"));

        // --no-default-features alone still excludes it.
        let units = resolve_units_with_features(
            &root,
            "dev",
            None,
            &FeatureFlags {
                no_default: true,
                ..Default::default()
            },
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(names(&units), vec!["app".to_string()]);
    }

    #[test]
    fn unknown_feature_fails_resolution() {
        let dir = tempfile::tempdir().unwrap();
        write_pkg(
            &dir.path().join("app"),
            "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"},\"features\":{\"default\":[]}}",
        );
        let root = load_dir(&dir.path().join("app"));
        let err = resolve_units_with_features(
            &root,
            "dev",
            None,
            &FeatureFlags {
                features: vec!["ghost".to_string()],
                ..Default::default()
            },
            &BTreeMap::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown feature"), "got: {err}");
    }

    struct FakeMaterializer {
        dir: PathBuf,
    }

    impl Materializer for FakeMaterializer {
        fn materialize(&self, req: &MaterialRequest) -> Result<PathBuf, String> {
            assert_eq!(req.sha, "abc123");
            assert_eq!(req.tree, "tree1");
            Ok(self.dir.clone())
        }
    }

    fn locked_map(entry: LockedDep) -> BTreeMap<String, LockedDep> {
        BTreeMap::from([(entry.name.clone(), entry)])
    }

    #[test]
    fn locked_git_resolves_via_materializer() {
        let dir = tempfile::tempdir().unwrap();
        write_pkg(
            &dir.path().join("app"),
            "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"},\"dependencies\":{\"z\":{\"git\":\"https://example.com/z.git\",\"tag\":\"v1\"}}}",
        );
        write_pkg(
            &dir.path().join("zsrc"),
            "{\"package\":{\"name\":\"z\",\"version\":\"1.0.0\"}}",
        );
        let root = load_dir(&dir.path().join("app"));
        let locked = locked_map(LockedDep {
            name: "z".to_string(),
            version: "1.0.0".to_string(),
            source: LockedSource::Git {
                url: "https://example.com/z.git".to_string(),
                git_ref: ("tag".to_string(), "v1".to_string()),
                sha: "abc123".to_string(),
                tree: "tree1".to_string(),
            },
        });
        let mat = FakeMaterializer {
            dir: dir.path().join("zsrc"),
        };
        let units = resolve_locked_units(
            &root,
            "dev",
            None,
            &FeatureFlags::default(),
            &locked,
            &mat,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(units.len(), 2);
        // Dependencies first; the locked unit redirects its target dir.
        assert!(units[0].target_dir_override.is_some());
        assert!(units[0]
            .target_dir_override
            .as_ref()
            .unwrap()
            .to_string_lossy()
            .contains("locked"));
        assert_eq!(units[1].target_dir_override, None);
    }

    #[test]
    fn locked_missing_entry_is_not_locked() {
        let dir = tempfile::tempdir().unwrap();
        write_pkg(
            &dir.path().join("app"),
            "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"},\"dependencies\":{\"z\":{\"git\":\"https://example.com/z.git\",\"tag\":\"v1\"}}}",
        );
        let root = load_dir(&dir.path().join("app"));
        let mat = FakeMaterializer {
            dir: dir.path().to_path_buf(),
        };
        let err = resolve_locked_units(
            &root,
            "dev",
            None,
            &FeatureFlags::default(),
            &BTreeMap::new(),
            &mat,
            &BTreeMap::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not covered"), "got: {err}");
        assert!(err.contains("rivet update"), "got: {err}");
    }

    #[test]
    fn locked_ref_mismatch_is_not_locked() {
        let dir = tempfile::tempdir().unwrap();
        write_pkg(
            &dir.path().join("app"),
            "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"},\"dependencies\":{\"z\":{\"git\":\"https://example.com/z.git\",\"branch\":\"main\"}}}",
        );
        let root = load_dir(&dir.path().join("app"));
        // Lock says tag v1, manifest says branch main.
        let locked = locked_map(LockedDep {
            name: "z".to_string(),
            version: "1.0.0".to_string(),
            source: LockedSource::Git {
                url: "https://example.com/z.git".to_string(),
                git_ref: ("tag".to_string(), "v1".to_string()),
                sha: "abc123".to_string(),
                tree: "tree1".to_string(),
            },
        });
        let mat = FakeMaterializer {
            dir: dir.path().to_path_buf(),
        };
        let err = resolve_locked_units(
            &root,
            "dev",
            None,
            &FeatureFlags::default(),
            &locked,
            &mat,
            &BTreeMap::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("ref changed"), "got: {err}");
    }

    #[test]
    fn locked_registry_is_m5() {
        let dir = tempfile::tempdir().unwrap();
        write_pkg(
            &dir.path().join("app"),
            "{\"package\":{\"name\":\"app\",\"version\":\"0.1.0\"},\"dependencies\":{\"fmt\":\"11\"}}",
        );
        let root = load_dir(&dir.path().join("app"));
        let locked = locked_map(LockedDep {
            name: "fmt".to_string(),
            version: "11.0.0".to_string(),
            source: LockedSource::Registry {
                registry: "default".to_string(),
                version: "11.0.0".to_string(),
                checksum: "sha256:x".to_string(),
            },
        });
        let mat = FakeMaterializer {
            dir: dir.path().to_path_buf(),
        };
        let err = resolve_locked_units(
            &root,
            "dev",
            None,
            &FeatureFlags::default(),
            &locked,
            &mat,
            &BTreeMap::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("M5"), "got: {err}");
    }
}
