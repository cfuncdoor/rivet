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

/// Resolve the build scope for `root` (the nearest manifest to the CWD).
///
/// - Workspace root → members (+ the root package itself when present).
/// - Plain package → just it.
/// - `only` (`-p`) narrows seeds to one package by name.
/// - The transitive `path`-dependency closure is always pulled in and the
///   result is topologically sorted (dependencies first).
pub fn resolve_units(
    root: &Manifest,
    profile: &str,
    only: Option<&str>,
) -> Result<Vec<Unit>, Error> {
    super::check_profile(profile)?;
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
    // carry `workspace`); path deps must be plain packages.
    let mut units: BTreeMap<PathBuf, Unit> = BTreeMap::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    for seed in seeds {
        let dir = canonical(&seed.dir)?;
        if units.contains_key(&dir) {
            continue;
        }
        let mut man = seed;
        man.dir = dir.clone();
        let mut dep_dirs = direct_dep_dirs(&man)?;
        dep_dirs.sort();
        dep_dirs.dedup();
        queue.extend(dep_dirs.clone());
        units.insert(
            dir.clone(),
            Unit {
                manifest: man,
                dir,
                deps: dep_dirs,
                from_workspace,
            },
        );
    }
    while let Some(dep_dir) = queue.pop_front() {
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
        let mut dep_dirs = direct_dep_dirs(&dep_man)?;
        dep_dirs.sort();
        dep_dirs.dedup();
        queue.extend(dep_dirs.clone());
        units.insert(
            dep_dir.clone(),
            Unit {
                manifest: dep_man,
                dir: dep_dir,
                deps: dep_dirs,
                from_workspace: false,
            },
        );
    }
    // 3. Topological order (dependencies first); report cycles.
    topo_sort(units)
}

/// Direct path-dependency dirs of `man` (canonical), validating that every
/// dep exists with a manifest and that non-path deps fail with milestone
/// pointers (registry → M5, git → M4).
fn direct_dep_dirs(man: &Manifest) -> Result<Vec<PathBuf>, Error> {
    let mut dep_dirs = Vec::new();
    for (dep_name, dep_dir) in m::path_dependencies(man) {
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
        dep_dirs.push(c);
    }
    // Non-path deps cannot build yet: actionable milestone pointers.
    for (dep_name, dep) in &man.dependencies {
        if dep.is_git() {
            return Err(Error::GitDep {
                name: dep_name.clone(),
            });
        }
        if dep.is_registry() {
            return Err(Error::RegistryDep {
                name: dep_name.clone(),
            });
        }
    }
    Ok(dep_dirs)
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
        let target = dep
            .dir
            .join("target")
            .join(super::profile_dir(profile))
            .join("build");
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
}
