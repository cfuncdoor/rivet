//! Lockfile flow: resolve/write `rivet.lock.json`, materialize locked
//! git sources (cache checkout or vendor copy), and the shared
//! plain-or-locked unit resolution used by build/test/run/compile-commands.
//!
//! Updates, vendor/dev/git-cache commands live here too (part 2 below).
//! The CLI owns all user interaction; `rivet-git`/`rivet-resolver` own the
//! mechanisms.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::Ui;

// ---------------------------------------------------------------------------
// Options + paths
// ---------------------------------------------------------------------------

/// Network/lock strictness flags (`build`, `test`, `run`, `compile-commands`).
#[derive(Debug, Clone, Default)]
pub struct LockOpts {
    /// No network (cache/vendor only).
    pub offline: bool,
    /// Error when the lockfile would change.
    pub locked: bool,
    /// Offline + locked.
    pub frozen: bool,
}

impl LockOpts {
    /// Effective offline mode (`--offline` or `--frozen`).
    pub fn effective_offline(&self) -> bool {
        self.offline || self.frozen
    }
}

/// `rivet.lock.json` next to the scope root manifest.
pub fn lock_path(scope_root: &Path) -> PathBuf {
    scope_root.join("rivet.lock.json")
}

/// Locked versions by dep key (`rivet.lock.json`, empty when absent).
pub fn lock_versions(scope_root: &Path) -> Result<BTreeMap<String, String>> {
    let path = lock_path(scope_root);
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let lock = rivet_resolver::read_lockfile(&path).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(lock
        .packages
        .into_iter()
        .map(|p| (p.name, p.version))
        .collect())
}

/// One-line human summary of a dependency declaration.
pub fn describe_dep(dep: &rivet_manifest::Dependency) -> String {
    match dep {
        rivet_manifest::Dependency::Simple(req) => format!("registry {req}"),
        rivet_manifest::Dependency::Detailed(d) => {
            if let Some(path) = &d.path {
                format!("path {path}")
            } else if let Some(git) = &d.git {
                let r = d
                    .tag
                    .as_ref()
                    .map(|t| format!("tag {t}"))
                    .or_else(|| d.branch.as_ref().map(|b| format!("branch {b}")))
                    .or_else(|| d.rev.as_ref().map(|r| format!("rev {r}")))
                    .unwrap_or_else(|| "default branch".to_string());
                format!("git {git} ({r})")
            } else if let Some(version) = &d.version {
                format!("registry {version}")
            } else if d.system.unwrap_or(false) {
                "system (installed)".to_string()
            } else {
                "unknown source".to_string()
            }
        }
    }
}

/// Git cache handle (requires the `git` binary; honors `RIVET_HOME`).
pub fn git_cache(verbose: bool, offline: bool) -> Result<rivet_git::GitCache> {
    rivet_git::require_git().map_err(|e| anyhow::anyhow!("{e}"))?;
    let root = rivet_cache::cache_root().map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(rivet_git::GitCache::new(&root)
        .with_verbose(verbose)
        .with_offline(offline))
}

/// Local registry index copy (`~/.rivet/registry/index/default`).
fn registry_index_dir() -> Result<PathBuf> {
    let root = rivet_cache::cache_root().map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(root.join("registry").join("index").join("default"))
}

/// Index dir for one registry: the default copy, or a per-URL hash dir.
fn index_dir_for(registry_url: &str) -> Result<PathBuf> {
    if registry_url == "default" {
        return registry_index_dir();
    }
    let root = rivet_cache::cache_root().map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(root
        .join("registry")
        .join("index")
        .join(rivet_registry::registry_hash(registry_url)))
}

/// Default registry URL from `RIVET_REGISTRY` (there is no central
/// rivet.dev registry yet; the env var points at yours).
pub fn default_registry_url() -> Result<String> {
    match std::env::var("RIVET_REGISTRY")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        Some(url) => Ok(url),
        None => bail!(
            "no default registry configured. Fix: set RIVET_REGISTRY=https://... (or give the dep a `registry` URL)"
        ),
    }
}

/// Resolve a locked `registry` value to a concrete URL.
fn concrete_registry_url(locked: &str) -> Result<String> {
    if locked == "default" {
        default_registry_url()
    } else {
        Ok(locked.to_string())
    }
}

/// Collect `(registry_url, dep name)` for registry deps reachable through
/// path edges from `seeds` (git-backed manifests are unknown pre-resolve;
/// their registry deps surface as index errors with actionable hints).
fn scan_registry_deps(seeds: &[rivet_manifest::Manifest]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
    let mut queue: Vec<PathBuf> = seeds.iter().map(|m| m.dir.clone()).collect();
    while let Some(dir) = queue.pop() {
        if !visited.insert(dir.clone()) {
            continue;
        }
        let Ok(man) = rivet_manifest::load(&dir) else {
            continue;
        };
        for (key, dep) in &man.dependencies {
            if dep.is_registry() {
                let url = match dep {
                    rivet_manifest::Dependency::Detailed(d) => {
                        d.registry.clone().unwrap_or_else(|| "default".to_string())
                    }
                    rivet_manifest::Dependency::Simple(_) => "default".to_string(),
                };
                out.entry(url).or_default().push(key.clone());
            } else if let Some(path) = dep.path() {
                queue.push(man.dir.join(path));
            }
        }
    }
    for names in out.values_mut() {
        names.sort();
        names.dedup();
    }
    out
}

/// Ensure local index copies for alternate registries (download missing or
/// stale files when online and `refresh`). The default registry always uses
/// its local copy as-is. Returns the index dir the resolver should read:
/// the single registry's dir, or a deterministic merged temp dir when
/// several registries collide (same package name from two registries in
/// one build is refused).
fn prepare_registry_index(
    seeds: &[rivet_manifest::Manifest],
    offline: bool,
    refresh: bool,
    verbose: bool,
) -> Result<PathBuf> {
    let needed = scan_registry_deps(seeds);
    if needed.is_empty() {
        return registry_index_dir();
    }
    let mut single: Option<PathBuf> = None;
    let mut multi = false;
    for (url, names) in &needed {
        let dir = index_dir_for(url)?;
        let is_default = url == "default";
        for name in names {
            let file = dir.join(name);
            let missing_or_refresh = !file.is_file() || (refresh && !is_default);
            if missing_or_refresh && !offline {
                if is_default {
                    // Default index is user-managed, but when RIVET_REGISTRY
                    // points somewhere the missing/stale file syncs from it
                    // (previously this synced from the literal "default/…",
                    // which always failed).
                    if let Ok(concrete) = default_registry_url() {
                        sync_index_file(&concrete, name, &dir, verbose)?;
                    }
                    continue;
                }
                sync_index_file(url, name, &dir, verbose)?;
            }
        }
        match &single {
            None => single = Some(dir),
            Some(d) if d == &dir => {}
            Some(_) => multi = true,
        }
    }
    let Some(dir) = single else {
        return registry_index_dir();
    };
    if !multi {
        return Ok(dir);
    }
    // Merge several registries into one deterministic scratch dir.
    // Identical bytes merge silently; same name with different bytes is a
    // real two-registry collision and is refused.
    let mut urls: Vec<String> = needed.keys().cloned().collect();
    urls.sort();
    let digest = rivet_registry::sha256_hex(urls.join("\0").as_bytes());
    let merged = std::env::temp_dir().join("rivet-index").join(&digest[..16]);
    std::fs::create_dir_all(&merged)
        .with_context(|| format!("could not create {}", merged.display()))?;
    for (url, names) in &needed {
        let dir = index_dir_for(url)?;
        for name in names {
            let src = dir.join(name);
            let bytes = std::fs::read(&src)
                .with_context(|| format!("could not read {} (sync it first)", src.display()))?;
            let dst = merged.join(name);
            if dst.is_file() {
                let prev = std::fs::read(&dst).unwrap_or_default();
                if prev != bytes {
                    bail!(
                        "package `{name}` comes from two registries in one build, which is not supported. Fix: unify on one registry"
                    );
                }
            } else {
                std::fs::write(&dst, &bytes)
                    .with_context(|| format!("could not write {}", dst.display()))?;
            }
        }
    }
    Ok(merged)
}

/// Download one index file into its local copy.
fn sync_index_file(registry_url: &str, name: &str, dir: &Path, verbose: bool) -> Result<()> {
    let url = format!("{}/index/{}", registry_url.trim_end_matches('/'), name);
    let bytes = rivet_registry::download_bytes(&url, false).map_err(|e| anyhow::anyhow!("{e}"))?;
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    std::fs::write(dir.join(name), &bytes)
        .with_context(|| format!("could not write {}", dir.join(name).display()))?;
    if verbose {
        eprintln!("       synced index {name} from {registry_url}");
    }
    Ok(())
}

/// Vendor dir when the scope carries `vendor/rivet-vendor.json`.
fn vendor_dir(scope_root: &Path) -> Option<PathBuf> {
    let dir = scope_root.join("vendor");
    if dir.join("rivet-vendor.json").is_file() {
        Some(dir)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Seeds + resolve + merge
// ---------------------------------------------------------------------------

/// Seed manifests for resolution: workspace members (+ the root package
/// itself when present), else the single package.
pub fn scope_seeds(root: &rivet_manifest::Manifest) -> Result<Vec<rivet_manifest::Manifest>> {
    if let Some(ws) = &root.workspace {
        let mut seeds = Vec::new();
        for dir in rivet_manifest::expand_members(&root.dir, &ws.members)
            .map_err(|e| anyhow::anyhow!("{e}"))?
        {
            seeds.push(rivet_manifest::load(&dir).map_err(|e| anyhow::anyhow!("{e}"))?);
        }
        if root.package.is_some() {
            seeds.push(root.clone());
        }
        Ok(seeds)
    } else {
        Ok(vec![root.clone()])
    }
}

/// Resolve every seed and merge the locks (same name must agree on version
/// and source; features union). Alternate registry indexes sync on demand
/// (`refresh` forces re-download, e.g. for `update`); the default registry
/// always uses its local copy.
pub fn resolve_lock(
    seeds: &[rivet_manifest::Manifest],
    flags: &rivet_graph::FeatureFlags,
    offline: bool,
    git: &rivet_git::GitCache,
    verbose: bool,
    refresh_indexes: bool,
) -> Result<rivet_resolver::Lockfile> {
    let index_dir = prepare_registry_index(seeds, offline, refresh_indexes, verbose)?;
    let index = rivet_resolver::RegistryIndex::new(index_dir);
    let provider = rivet_resolver::RealProvider::new(git.clone());
    let resolver = rivet_resolver::Resolver::new(index, &provider);
    let opts = rivet_resolver::ResolveOpts {
        features: flags.features.clone(),
        all_features: flags.all,
        no_default_features: flags.no_default,
        offline,
    };
    let mut merged: BTreeMap<String, rivet_resolver::LockedPackage> = BTreeMap::new();
    for seed in seeds {
        let lock = resolver
            .resolve(seed, &opts)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        for pkg in lock.packages {
            if let Some(prev) = merged.get(&pkg.name) {
                if prev.version != pkg.version || prev.source != pkg.source {
                    bail!(
                        "conflicting lock entries for `{}` ({} vs {}). Fix: unify the dependency requests",
                        pkg.name,
                        describe_source(&prev.source),
                        describe_source(&pkg.source)
                    );
                }
                let merged_pkg = merged.get_mut(&pkg.name).unwrap();
                let mut feats: BTreeSet<String> = merged_pkg.features.iter().cloned().collect();
                feats.extend(pkg.features.iter().cloned());
                merged_pkg.features = feats.into_iter().collect();
            } else {
                merged.insert(pkg.name.clone(), pkg);
            }
        }
    }
    Ok(rivet_resolver::Lockfile {
        version: rivet_resolver::LOCK_VERSION,
        packages: merged.into_values().collect(),
    })
}

fn describe_source(source: &rivet_resolver::LockSource) -> String {
    match source {
        rivet_resolver::LockSource::Path { path } => format!("path {path}"),
        rivet_resolver::LockSource::Registry { version, .. } => {
            format!("registry {version}")
        }
        rivet_resolver::LockSource::Git { url, sha, .. } => {
            format!("{url} @ {}", &sha[..sha.len().min(12)])
        }
        rivet_resolver::LockSource::System { version } => {
            format!("system {version}")
        }
    }
}

/// Write the lockfile when its bytes changed. Returns `true` when written.
pub fn write_lock_if_changed(path: &Path, lock: &rivet_resolver::Lockfile) -> Result<bool> {
    let bytes = rivet_resolver::lockfile_to_bytes(lock).map_err(|e| anyhow::anyhow!("{e}"))?;
    if path.is_file() {
        let current =
            std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
        if current == bytes {
            return Ok(false);
        }
    }
    rivet_resolver::write_lockfile(path, lock).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Lock view + materializer
// ---------------------------------------------------------------------------

/// Convert a resolver lockfile into the graph's lock view.
pub fn lock_to_view(
    lock: &rivet_resolver::Lockfile,
) -> BTreeMap<String, rivet_graph::workspace::LockedDep> {
    use rivet_graph::workspace::{LockedDep, LockedSource};
    lock.packages
        .iter()
        .map(|p| {
            let source = match &p.source {
                rivet_resolver::LockSource::Path { path } => {
                    LockedSource::Path { path: path.clone() }
                }
                rivet_resolver::LockSource::Git {
                    url,
                    git_ref,
                    sha,
                    tree,
                } => LockedSource::Git {
                    url: url.clone(),
                    git_ref: (git_ref.kind.clone(), git_ref.value.clone()),
                    sha: sha.clone(),
                    tree: tree.clone(),
                },
                rivet_resolver::LockSource::Registry {
                    registry,
                    version,
                    checksum,
                } => LockedSource::Registry {
                    registry: registry.clone(),
                    version: version.clone(),
                    checksum: checksum.clone(),
                },
                rivet_resolver::LockSource::System { version } => LockedSource::System {
                    version: version.clone(),
                },
            };
            (
                p.name.clone(),
                LockedDep {
                    name: p.name.clone(),
                    version: p.version.clone(),
                    source,
                },
            )
        })
        .collect()
}

/// Vendored source record (`vendor/rivet-vendor.json` entry).
#[derive(Debug, Clone)]
struct VendorEntry {
    tree: String,
}

/// Read the vendor map (`name` → tree hash) when the scope is vendored.
fn read_vendor_map(scope_root: &Path) -> Result<BTreeMap<String, VendorEntry>> {
    let mut map = BTreeMap::new();
    let file = scope_root.join("vendor").join("rivet-vendor.json");
    if !file.is_file() {
        return Ok(map);
    }
    let text = std::fs::read_to_string(&file)
        .with_context(|| format!("could not read {}", file.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text).with_context(|| {
        format!(
            "corrupt {} (delete it and re-run `rivet vendor`)",
            file.display()
        )
    })?;
    if let Some(sources) = value.get("sources").and_then(|s| s.as_object()) {
        for (name, entry) in sources {
            if let Some(tree) = entry.get("tree").and_then(|t| t.as_str()) {
                map.insert(
                    name.clone(),
                    VendorEntry {
                        tree: tree.to_string(),
                    },
                );
            }
        }
    }
    Ok(map)
}

/// Materializes locked git deps from the global cache, falling back to
/// vendored copies (offline-friendly, tree-hash verified).
pub struct CliMaterializer {
    git: rivet_git::GitCache,
    vendor: Option<PathBuf>,
    vendor_map: BTreeMap<String, VendorEntry>,
    verbose: bool,
    offline: bool,
}

impl CliMaterializer {
    /// Build from the scope root (picks up `vendor/` when present).
    pub fn new(
        git: rivet_git::GitCache,
        scope_root: &Path,
        verbose: bool,
        offline: bool,
    ) -> Result<Self> {
        let vendor = vendor_dir(scope_root);
        let vendor_map = read_vendor_map(scope_root)?;
        Ok(Self {
            git,
            vendor,
            vendor_map,
            verbose,
            offline,
        })
    }
}

impl rivet_graph::workspace::Materializer for CliMaterializer {
    fn materialize(
        &self,
        req: &rivet_graph::workspace::MaterialRequest,
    ) -> Result<PathBuf, String> {
        let opts = rivet_git::CheckoutOpts {
            subdir: req.subdir.clone(),
            submodules: req.submodules,
            expected_tree: Some(req.tree.clone()),
        };
        match self
            .git
            .ensure_checkout(&req.key, &req.url, &req.sha, &opts)
        {
            Ok(dir) => Ok(dir),
            Err(cache_err) => {
                // Offline-friendly fallback: a vendored copy whose tree hash
                // matches the lock. Tree equality = byte-identical content,
                // so a mutated vendor tree fails loudly instead of silently.
                if let Some(vendor) = &self.vendor {
                    let vdir = vendor.join(&req.key);
                    let known = self.vendor_map.get(&req.key);
                    if vdir.is_dir() && known.is_some_and(|e| e.tree == req.tree) {
                        match rivet_git::tree_hash_of_checkout(&vdir, self.verbose) {
                            Ok(tree) if tree == req.tree => {
                                let mut out = vdir;
                                if let Some(sub) = &req.subdir {
                                    out = out.join(sub);
                                    if !out.is_dir() {
                                        return Err(format!(
                                            "vendored dep `{}` has no subdir `{sub}`",
                                            req.key
                                        ));
                                    }
                                }
                                return Ok(out);
                            }
                            Ok(tree) => {
                                return Err(format!(
                                    "vendored dep `{}` tree mismatch (lock {}, vendor {tree}). Fix: re-run `rivet vendor`",
                                    req.key, req.tree
                                ));
                            }
                            Err(ve) => {
                                return Err(format!(
                                    "cannot use cache ({cache_err}) nor vendor ({ve}) for `{}`",
                                    req.key
                                ));
                            }
                        }
                    }
                }
                Err(cache_err.to_string())
            }
        }
    }

    fn materialize_registry(
        &self,
        req: &rivet_graph::workspace::RegistryRequest,
    ) -> Result<PathBuf, String> {
        // Vendored copies win when present (copied from verified cache).
        if let Some(vendor) = &self.vendor {
            let vdir = vendor.join(&req.key);
            if vdir.join("rivet.json").is_file() {
                return Ok(vdir);
            }
        }
        let url = concrete_registry_url(&req.registry).map_err(|e| e.to_string())?;
        let cache_root = rivet_cache::cache_root().map_err(|e| e.to_string())?;
        let opts = rivet_registry::MaterializeOpts {
            offline: self.offline,
            verbose: self.verbose,
        };
        let src = rivet_registry::materialize(
            &req.key,
            &req.version,
            &req.checksum,
            &url,
            &cache_root,
            &opts,
        )
        .map_err(|e| e.to_string())?;
        Ok(src.source_dir)
    }
}

// ---------------------------------------------------------------------------
// Shared plain-or-locked resolution
// ---------------------------------------------------------------------------

/// Scope-root patch map (dep key → joined patch dir) for resolution.
/// Non-path patch values are an error (local paths only).
fn root_patch_map(root: &rivet_manifest::Manifest) -> Result<BTreeMap<String, PathBuf>> {
    let mut map = BTreeMap::new();
    for (key, dep) in &root.patch {
        match dep {
            rivet_manifest::Dependency::Detailed(d) => match &d.path {
                Some(p) => {
                    map.insert(key.clone(), root.dir.join(p));
                }
                None => bail!(
                    "patch for `{key}` uses a non-path source (local paths only)"
                ),
            },
            rivet_manifest::Dependency::Simple(_) => bail!(
                "patch for `{key}` must be an object with a `path`. Fix: `\"{key}\": {{\"path\": \"...\"}}`"
            ),
        }
    }
    Ok(map)
}

/// Resolve build units, going through `rivet.lock.json` only when the scope
/// has git/registry deps (plain path-only scopes never touch a lockfile).
pub fn resolve_build_units(
    ui: &Ui,
    root: &rivet_manifest::Manifest,
    profile: &str,
    only: Option<&str>,
    flags: &rivet_graph::FeatureFlags,
    lock_opts: &LockOpts,
    verbose: bool,
) -> Result<Vec<rivet_graph::workspace::Unit>> {
    use rivet_graph::workspace as ws;
    let root_patch = root_patch_map(root)?;
    match ws::resolve_units_with_features(root, profile, only, flags, &root_patch) {
        Ok(units) => Ok(units),
        Err(e @ (rivet_graph::Error::GitDep { .. } | rivet_graph::Error::RegistryDep { .. })) => {
            ui.verbose(&format!(
                "locked deps detected ({e}); using rivet.lock.json flow"
            ));
            resolve_locked_flow(ui, root, profile, only, flags, lock_opts, verbose)
        }
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

/// Locked resolution: reuse the lockfile when it covers the scope,
/// re-resolve (and rewrite, unless `--locked`) when it does not.
fn resolve_locked_flow(
    ui: &Ui,
    root: &rivet_manifest::Manifest,
    profile: &str,
    only: Option<&str>,
    flags: &rivet_graph::FeatureFlags,
    lock_opts: &LockOpts,
    verbose: bool,
) -> Result<Vec<rivet_graph::workspace::Unit>> {
    use rivet_graph::workspace as ws;
    let offline = lock_opts.effective_offline();
    let git = git_cache(verbose, offline)?;
    let seeds = scope_seeds(root)?;
    let path = lock_path(&root.dir);
    let existing: Option<rivet_resolver::Lockfile> = if path.is_file() {
        Some(rivet_resolver::read_lockfile(&path).map_err(|e| anyhow::anyhow!("{e}"))?)
    } else {
        None
    };
    let materialize_with = |lock: &rivet_resolver::Lockfile| -> Result<Vec<ws::Unit>> {
        let view = lock_to_view(lock);
        let mat = CliMaterializer::new(git.clone(), &root.dir, verbose, offline)?;
        let root_patch = root_patch_map(root)?;
        ws::resolve_locked_units(root, profile, only, flags, &view, &mat, &root_patch)
            .map_err(|e| anyhow::anyhow!("{e}"))
    };
    if let Some(existing) = &existing {
        match materialize_with(existing) {
            Ok(units) => return Ok(units),
            Err(e) => {
                let not_locked = e.to_string().contains("not covered by rivet.lock.json");
                if not_locked && (lock_opts.locked || lock_opts.frozen) {
                    // Show the precise diff when possible (best effort:
                    // resolution may need the network); otherwise keep the
                    // actionable NotLocked error.
                    let seeds = scope_seeds(root)?;
                    if let Ok(fresh) = resolve_lock(&seeds, flags, offline, &git, verbose, false) {
                        rivet_resolver::check_locked(existing, &fresh)
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                    }
                }
                if not_locked && !(lock_opts.locked || lock_opts.frozen) {
                    ui.verbose("lockfile stale; re-resolving");
                } else {
                    return Err(e);
                }
            }
        }
    } else if lock_opts.locked || lock_opts.frozen {
        bail!(
            "no rivet.lock.json in {} with --{} set. Fix: run `rivet update` (or build without --{}) first",
            root.dir.display(),
            if lock_opts.frozen { "frozen" } else { "locked" },
            if lock_opts.frozen { "frozen" } else { "locked" }
        );
    }
    // (Re-)resolve and persist, then build from the fresh lock.
    let fresh = resolve_lock(&seeds, flags, offline, &git, verbose, false).map_err(|e| {
        if offline {
            anyhow::anyhow!("{e}\nHint: the lockfile is stale (or missing) and --offline forbids re-resolving; connect once or vendor first")
        } else {
            anyhow::anyhow!("{e}")
        }
    })?;
    if let Some(existing) = &existing {
        if lock_opts.locked || lock_opts.frozen {
            rivet_resolver::check_locked(existing, &fresh).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    }
    if write_lock_if_changed(&path, &fresh)? {
        ui.verbose(&format!("wrote {}", path.display()));
    }
    materialize_with(&fresh)
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

/// Options for `rivet update`.
#[derive(Debug, Clone, Default)]
pub struct UpdateOpts {
    /// Only update this package (others keep their locked entries).
    pub package: Option<String>,
    /// Pin this package to an exact commit SHA (requires `-p`).
    pub precise: Option<String>,
    /// No network (mirrors only).
    pub offline: bool,
}

/// Short SHA for messages (7 chars, Cargo-style).
fn short7(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// Re-resolve, verify tags, write the lock, and report movements.
pub fn update_flow(
    ui: &Ui,
    root: &rivet_manifest::Manifest,
    flags: &rivet_graph::FeatureFlags,
    opts: &UpdateOpts,
    verbose: bool,
) -> Result<()> {
    if opts.precise.is_some() && opts.package.is_none() {
        bail!("`--precise` requires `-p <package>`. Fix: `rivet update -p <name> --precise <sha>`");
    }
    let git = git_cache(verbose, opts.offline)?;
    let mut seeds = scope_seeds(root)?;
    // Patched deps resolve as their base with a warning (the build uses the
    // patch instead; true patch-aware resolution is future work).
    let patched = collect_patched_keys(root, &seeds)?;
    if !patched.is_empty() {
        let mut names: Vec<String> = patched.iter().cloned().collect();
        names.sort();
        ui.warn(&format!(
            "patch overrides active for {}; update resolves the base, the build uses the patch",
            names.join(", ")
        ));
    }
    strip_patched_deps(&mut seeds, &patched);
    let fresh = resolve_lock(&seeds, flags, opts.offline, &git, verbose, true)?;
    let path = lock_path(&root.dir);
    let old: Option<rivet_resolver::Lockfile> = if path.is_file() {
        Some(rivet_resolver::read_lockfile(&path).map_err(|e| anyhow::anyhow!("{e}"))?)
    } else {
        None
    };
    let old_map: BTreeMap<String, rivet_resolver::LockedPackage> = old
        .as_ref()
        .map(|l| {
            l.packages
                .iter()
                .map(|p| (p.name.clone(), p.clone()))
                .collect()
        })
        .unwrap_or_default();
    let mut merged = fresh;
    if let Some(name) = &opts.package {
        // Selective update: keep every other entry pinned.
        let known: BTreeSet<String> = merged
            .packages
            .iter()
            .map(|p| p.name.clone())
            .chain(old_map.keys().cloned())
            .collect();
        if !known.contains(name) {
            let mut available: Vec<String> = known.into_iter().collect();
            available.sort();
            bail!(
                "package `{name}` not found in dependencies. Available: {}",
                if available.is_empty() {
                    "<none>".to_string()
                } else {
                    available.join(", ")
                }
            );
        }
        let mut next: Vec<rivet_resolver::LockedPackage> = Vec::new();
        let mut fresh_by_name: BTreeMap<String, rivet_resolver::LockedPackage> = BTreeMap::new();
        for pkg in merged.packages {
            fresh_by_name.insert(pkg.name.clone(), pkg);
        }
        let mut names: BTreeSet<String> = fresh_by_name.keys().cloned().collect();
        names.extend(old_map.keys().cloned());
        for n in names {
            if n == *name {
                if let Some(pkg) = fresh_by_name.remove(&n) {
                    next.push(pkg);
                }
            } else if let Some(pkg) = fresh_by_name.remove(&n) {
                // A brand-new requirement (not a re-resolution): keep it.
                if old_map.contains_key(&n) {
                    next.push(old_map[&n].clone());
                } else {
                    next.push(pkg);
                }
            } else if let Some(pkg) = old_map.get(&n) {
                next.push(pkg.clone());
            }
        }
        next.sort_by(|a, b| a.name.cmp(&b.name));
        merged = rivet_resolver::Lockfile {
            version: rivet_resolver::LOCK_VERSION,
            packages: next,
        };
    }
    // `--precise`: pin the selected git dep to an exact SHA (the manifest
    // ref stays, so builds keep agreeing with the lock).
    if let Some(sha) = &opts.precise {
        let name = opts.package.as_ref().unwrap();
        let entry = merged
            .packages
            .iter_mut()
            .find(|p| &p.name == name)
            .ok_or_else(|| anyhow::anyhow!("package `{name}` is not a locked dependency"))?;
        match &entry.source {
            rivet_resolver::LockSource::Git { url, .. } => {
                let tree = git
                    .tree_hash(name, url, sha)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                entry.source = rivet_resolver::LockSource::Git {
                    url: url.clone(),
                    git_ref: entry_git_ref(&entry.source),
                    sha: sha.clone(),
                    tree,
                };
            }
            _ => bail!("`--precise` only supports git dependencies (`{name}` is not one)"),
        }
    }
    // Restore base entries for patched deps (their live patch builds instead).
    for (key, old_pkg) in &old_map {
        if patched.contains(key) && !merged.packages.iter().any(|p| &p.name == key) {
            merged.packages.push(old_pkg.clone());
        }
    }
    merged.packages.sort_by(|a, b| a.name.cmp(&b.name));
    // Moved tags are always an error, even on update.
    for pkg in &merged.packages {
        if let rivet_resolver::LockSource::Git {
            url, git_ref, sha, ..
        } = &pkg.source
        {
            if git_ref.kind == "tag" {
                if let Some(old_pkg) = old_map.get(&pkg.name) {
                    if let rivet_resolver::LockSource::Git { sha: old_sha, .. } = &old_pkg.source {
                        if old_sha != sha {
                            rivet_git::check_tag_not_moved(url, &git_ref.value, old_sha, sha)
                                .map_err(|e| anyhow::anyhow!("{e}"))?;
                        }
                    }
                }
            }
        }
    }
    report_lock_diff(ui, old.as_ref(), &merged, &git);
    if write_lock_if_changed(&path, &merged)? {
        ui.status("Updated", &format!("{}", path.display()));
    } else {
        ui.verbose("lockfile already up to date");
    }
    Ok(())
}

fn entry_git_ref(source: &rivet_resolver::LockSource) -> rivet_resolver::GitLockRef {
    match source {
        rivet_resolver::LockSource::Git { git_ref, .. } => git_ref.clone(),
        _ => rivet_resolver::GitLockRef {
            kind: "rev".to_string(),
            value: String::new(),
        },
    }
}

/// Report lock movements Cargo-style (`name: old -> new (N commits)`).
fn report_lock_diff(
    ui: &Ui,
    old: Option<&rivet_resolver::Lockfile>,
    fresh: &rivet_resolver::Lockfile,
    git: &rivet_git::GitCache,
) {
    let old_map: BTreeMap<&str, &rivet_resolver::LockedPackage> = old
        .map(|l| l.packages.iter().map(|p| (p.name.as_str(), p)).collect())
        .unwrap_or_default();
    let mut changed = false;
    for pkg in &fresh.packages {
        match old_map.get(pkg.name.as_str()) {
            None => {
                ui.status("Added", &format!("{} v{}", pkg.name, pkg.version));
                changed = true;
            }
            Some(prev) => {
                if prev.version != pkg.version || prev.source != pkg.source {
                    match (&prev.source, &pkg.source) {
                        (
                            rivet_resolver::LockSource::Git { url, sha: o, .. },
                            rivet_resolver::LockSource::Git { sha: n, .. },
                        ) => {
                            let count = git
                                .head_count_between(&pkg.name, url, o, n)
                                .map(|c| format!(" ({c} commits)"))
                                .unwrap_or_default();
                            ui.status(
                                "Updated",
                                &format!("{}: {} -> {}{count}", pkg.name, short7(o), short7(n)),
                            );
                        }
                        _ => {
                            ui.status(
                                "Updated",
                                &format!("{}: {} -> {}", pkg.name, prev.version, pkg.version),
                            );
                        }
                    }
                    changed = true;
                }
            }
        }
    }
    if let Some(old) = old {
        for pkg in &old.packages {
            if !fresh.packages.iter().any(|p| p.name == pkg.name) {
                ui.status("Removed", &format!("{} v{}", pkg.name, pkg.version));
                changed = true;
            }
        }
    }
    if !changed {
        ui.status("Updated", "already up to date");
    }
}

// ---------------------------------------------------------------------------
// Patch overrides (path-only)
// ---------------------------------------------------------------------------

/// Dep keys overridden by `patch` tables (own table, else the scope root's).
/// Non-path patch values are an error.
pub fn collect_patched_keys(
    root: &rivet_manifest::Manifest,
    seeds: &[rivet_manifest::Manifest],
) -> Result<BTreeSet<String>> {
    let mut keys = BTreeSet::new();
    let mut manifests = vec![root];
    manifests.extend(seeds.iter());
    for man in manifests {
        for (key, dep) in &man.patch {
            match dep {
                rivet_manifest::Dependency::Detailed(d) if d.path.is_some() => {
                    keys.insert(key.clone());
                }
                _ => {
                    bail!(
                        "patch for `{key}` uses a non-path source (local paths only). Fix: point it at a `path`"
                    );
                }
            }
        }
    }
    // Workspace-level root patch applies to every dependent.
    for (key, dep) in &root.patch {
        if matches!(
            dep,
            rivet_manifest::Dependency::Detailed(d) if d.path.is_some()
        ) {
            keys.insert(key.clone());
        }
    }
    Ok(keys)
}

/// Clone seeds with patched dep keys removed so resolution covers the base
/// graph (the build substitutes the patch via the overlay).
fn strip_patched_deps(seeds: &mut [rivet_manifest::Manifest], patched: &BTreeSet<String>) {
    for seed in seeds.iter_mut() {
        seed.dependencies.retain(|k, _| !patched.contains(k));
    }
}

// ---------------------------------------------------------------------------
// Vendor
// ---------------------------------------------------------------------------

/// Copy all locked deps into `vendor/` + write `vendor/rivet-vendor.json`
/// so builds work fully offline (git deps verified by tree hash on use).
pub fn vendor_flow(
    ui: &Ui,
    root: &rivet_manifest::Manifest,
    dry_run: bool,
    verbose: bool,
) -> Result<()> {
    let path = lock_path(&root.dir);
    if !path.is_file() {
        bail!(
            "no {} yet. Fix: run `rivet update` or `rivet build` first",
            path.display()
        );
    }
    let lock = rivet_resolver::read_lockfile(&path).map_err(|e| anyhow::anyhow!("{e}"))?;
    let git = git_cache(verbose, false)?;
    let mat = CliMaterializer::new(git, &root.dir, verbose, false)?;
    let vendor = root.dir.join("vendor");
    let mut sources = serde_json::Map::new();
    for pkg in &lock.packages {
        match &pkg.source {
            rivet_resolver::LockSource::Path { path: rel } => {
                let src = root.dir.join(rel);
                if dry_run {
                    ui.status(
                        "Vendor",
                        &format!("{} (path, would copy {})", pkg.name, src.display()),
                    );
                } else {
                    copy_dir_filtered(&src, &vendor.join(&pkg.name), true)
                        .with_context(|| format!("could not vendor path dep `{}`", pkg.name))?;
                    ui.status("Vendored", &format!("{} (path)", pkg.name));
                }
                sources.insert(
                    pkg.name.clone(),
                    serde_json::json!({"kind": "path", "path": rel}),
                );
            }
            rivet_resolver::LockSource::Git { url, sha, tree, .. } => {
                if dry_run {
                    ui.status(
                        "Vendor",
                        &format!("{} (git, would copy {} @ {})", pkg.name, url, short7(sha)),
                    );
                } else {
                    use rivet_graph::workspace::Materializer;
                    let src = mat
                        .materialize(&rivet_graph::workspace::MaterialRequest {
                            key: pkg.name.clone(),
                            url: url.clone(),
                            sha: sha.clone(),
                            tree: tree.clone(),
                            subdir: None,
                            submodules: false,
                        })
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    // Vendor the whole immutable checkout (`.git` included:
                    // tree verification needs the objects).
                    copy_dir_filtered(&src, &vendor.join(&pkg.name), false)
                        .with_context(|| format!("could not vendor git dep `{}`", pkg.name))?;
                    ui.status(
                        "Vendored",
                        &format!("{} ({} @ {})", pkg.name, url, short7(sha)),
                    );
                }
                sources.insert(
                    pkg.name.clone(),
                    serde_json::json!({"kind": "git", "url": url, "sha": sha, "tree": tree}),
                );
            }
            rivet_resolver::LockSource::Registry {
                registry,
                version,
                checksum,
            } => {
                let url = concrete_registry_url(registry)?;
                if dry_run {
                    ui.status(
                        "Vendor",
                        &format!("{} (registry, would copy {url} {version})", pkg.name),
                    );
                } else {
                    use rivet_graph::workspace::Materializer;
                    let src = mat
                        .materialize_registry(&rivet_graph::workspace::RegistryRequest {
                            key: pkg.name.clone(),
                            registry: registry.clone(),
                            version: version.clone(),
                            checksum: checksum.clone(),
                        })
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    copy_dir_filtered(&src, &vendor.join(&pkg.name), true)
                        .with_context(|| format!("could not vendor registry dep `{}`", pkg.name))?;
                    ui.status("Vendored", &format!("{} (registry {version})", pkg.name));
                }
                sources.insert(
                    pkg.name.clone(),
                    serde_json::json!({"kind": "registry", "url": url, "version": version, "checksum": checksum}),
                );
            }
            rivet_resolver::LockSource::System { version } => {
                // Installed libraries have no sources to copy.
                ui.status(
                    if dry_run { "Vendor" } else { "Vendored" },
                    &format!("{} (system {version}, nothing to copy)", pkg.name),
                );
                sources.insert(
                    pkg.name.clone(),
                    serde_json::json!({"kind": "system", "version": version}),
                );
            }
        }
    }
    if dry_run {
        return Ok(());
    }
    std::fs::create_dir_all(&vendor)
        .with_context(|| format!("could not create {}", vendor.display()))?;
    let doc = serde_json::json!({"version": 1, "sources": sources});
    let text = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string());
    std::fs::write(vendor.join("rivet-vendor.json"), format!("{text}\n")).with_context(|| {
        format!(
            "could not write {}",
            vendor.join("rivet-vendor.json").display()
        )
    })?;
    ui.status(
        "Vendored",
        &format!("{} packages into {}", lock.packages.len(), vendor.display()),
    );
    Ok(())
}

/// Adapter fingerprint + outputs snapshot (`adapter-fingerprint.json`).
#[derive(Debug, Clone)]
pub(crate) struct AdapterState {
    pub fingerprint: String,
    pub output: rivet_adapters::AdapterOutput,
}

/// Read adapter state, if present and parseable (`None` = rebuild).
pub(crate) fn read_adapter_state(path: &Path) -> Result<Option<AdapterState>> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let fingerprint = v
        .get("fingerprint")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let strings = |key: &str| {
        v.get(key)
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let paths = |key: &str| {
        strings(key)
            .into_iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>()
    };
    let libs = v
        .get("libs")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    if let Some(f) = e.get("file").and_then(|s| s.as_str()) {
                        Some(rivet_adapters::LibLink::File(PathBuf::from(f)))
                    } else {
                        e.get("name")
                            .and_then(|s| s.as_str())
                            .map(|n| rivet_adapters::LibLink::Name(n.to_string()))
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(Some(AdapterState {
        fingerprint,
        output: rivet_adapters::AdapterOutput {
            include_dirs: paths("include_dirs"),
            lib_dirs: paths("lib_dirs"),
            libs,
            defines: strings("defines"),
        },
    }))
}

/// Persist adapter fingerprint + outputs.
pub(crate) fn write_adapter_state(
    path: &Path,
    fingerprint: &str,
    output: &rivet_adapters::AdapterOutput,
) -> Result<()> {
    let libs: Vec<serde_json::Value> = output
        .libs
        .iter()
        .map(|l| match l {
            rivet_adapters::LibLink::File(p) => {
                serde_json::json!({"file": p.display().to_string()})
            }
            rivet_adapters::LibLink::Name(n) => serde_json::json!({"name": n}),
        })
        .collect();
    let doc = serde_json::json!({
        "fingerprint": fingerprint,
        "include_dirs": output.include_dirs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "lib_dirs": output.lib_dirs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "libs": libs,
        "defines": output.defines,
    });
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string());
    std::fs::write(path, format!("{text}\n"))
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

/// Recursive directory copy; skips `target/` trees when requested.
/// Symlinks are followed (content copied) — checkouts reject escaping links.
pub(crate) fn copy_dir_filtered(src: &Path, dst: &Path, skip_target: bool) -> Result<()> {
    if !src.is_dir() {
        bail!(
            "cannot copy {}: not a directory. Fix: check the dependency source",
            src.display()
        );
    }
    std::fs::create_dir_all(dst).with_context(|| format!("could not create {}", dst.display()))?;
    let mut entries: Vec<_> = std::fs::read_dir(src)
        .with_context(|| format!("could not read {}", src.display()))?
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("could not read {}", src.display()))?;
    entries.sort_by_key(|a| a.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        if skip_target
            && name_str == "target"
            && entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
        {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if from.is_dir() {
            copy_dir_filtered(&from, &to, skip_target)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("could not copy {}", from.display()))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Dev workflow
// ---------------------------------------------------------------------------
/// `(package display, dep key)` pairs with active `patch` overrides.
pub fn active_patches(root: &rivet_manifest::Manifest) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut manifests = vec![root.clone()];
    manifests.extend(scope_seeds(root)?);
    let mut seen = std::collections::HashSet::new();
    for man in &manifests {
        if !seen.insert(man.dir.clone()) {
            continue;
        }
        let pkg = rivet_manifest::package_name(man).unwrap_or("<root>");
        for key in man.patch.keys() {
            out.push((pkg.to_string(), key.clone()));
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Check out a writable clone of a git dep into `./deps-dev/<name>` and
/// auto-add the `patch` entry (warns about `publish`, per spec).
pub fn dev_flow(ui: &Ui, root: &rivet_manifest::Manifest, name: &str, verbose: bool) -> Result<()> {
    // Find the dep (BFS over path deps; git deps are collected, not resolved).
    let mut found: Option<(Box<rivet_manifest::DependencyDetail>, Vec<String>)> = None;
    let mut available: Vec<String> = Vec::new();
    let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
    let mut queue: Vec<PathBuf> = scope_seeds(root)?.into_iter().map(|m| m.dir).collect();
    queue.push(root.dir.clone());
    while let Some(dir) = queue.pop() {
        if !visited.insert(dir.clone()) {
            continue;
        }
        let manifest_file = dir.join("rivet.json");
        if !manifest_file.is_file() {
            continue;
        }
        let man = rivet_manifest::load(&dir).map_err(|e| anyhow::anyhow!("{e}"))?;
        for (key, dep) in &man.dependencies {
            match dep {
                rivet_manifest::Dependency::Detailed(d) if d.git.is_some() => {
                    available.push(key.clone());
                    if key == name {
                        found = Some((d.clone(), vec![key.clone()]));
                    }
                }
                _ => {}
            }
            if let Some(path) = dep.path() {
                queue.push(man.dir.join(path));
            }
        }
    }
    available.sort();
    available.dedup();
    let (detail, _) = found.ok_or_else(|| {
        anyhow::anyhow!(
            "git dependency `{name}` not found in this scope. Available git deps: {}",
            if available.is_empty() {
                "<none>".to_string()
            } else {
                available.join(", ")
            }
        )
    })?;
    let url = detail.git.clone().unwrap_or_default();
    let git = git_cache(verbose, false)?;
    // Locked SHA preferred (reproducible); otherwise resolve the ref now.
    let path = lock_path(&root.dir);
    let locked_sha: Option<(String, Option<String>)> = if path.is_file() {
        let lock = rivet_resolver::read_lockfile(&path).map_err(|e| anyhow::anyhow!("{e}"))?;
        lock.packages
            .iter()
            .find(|p| p.name == name)
            .and_then(|p| match &p.source {
                rivet_resolver::LockSource::Git { sha, tree, .. } => {
                    Some((sha.clone(), Some(tree.clone())))
                }
                _ => None,
            })
    } else {
        None
    };
    let (sha, expected_tree) = match locked_sha {
        Some((sha, tree)) => (sha, tree),
        None => {
            let git_ref = rivet_git::GitRef::from_parts(
                detail.tag.as_deref(),
                detail.branch.as_deref(),
                detail.rev.as_deref(),
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            let sha = git
                .resolve_ref(name, &url, &git_ref)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            (sha, None)
        }
    };
    let checkout = git
        .ensure_checkout(
            name,
            &url,
            &sha,
            &rivet_git::CheckoutOpts {
                subdir: detail.subdir.clone(),
                submodules: detail.submodules.unwrap_or(false),
                expected_tree,
            },
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let branch = match &detail.branch {
        Some(b) if b != "HEAD" => Some(b.as_str()),
        _ => None,
    };
    let dst = root.dir.join("deps-dev").join(name);
    git.clone_writable(&checkout, &dst, branch)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let rel = format!("deps-dev/{name}");
    let changed = add_patch_entry(root, name, &rel)?;
    ui.status(
        "Created",
        &format!("writable clone of `{name}` in {}", dst.display()),
    );
    if changed {
        ui.warn(
            "added a `patch` entry to rivet.json (formatting normalized); remove it before `rivet publish`",
        );
    } else {
        ui.verbose("patch entry already present");
    }
    Ok(())
}

/// Insert `"patch": {"<key>": {"path": "<rel>"}}` into the scope root
/// manifest (serde rewrite; returns whether the file changed).
fn add_patch_entry(root: &rivet_manifest::Manifest, key: &str, rel: &str) -> Result<bool> {
    let file = root.dir.join("rivet.json");
    let text = std::fs::read_to_string(&file)
        .with_context(|| format!("could not read {}", file.display()))?;
    let stripped = rivet_manifest::strip_jsonc(&text);
    let mut value: serde_json::Value = serde_json::from_str(&stripped)
        .with_context(|| format!("corrupt {} (fix the JSON first)", file.display()))?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("manifest must be a JSON object"))?;
    let patch = obj
        .entry("patch")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let patch_obj = patch
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("`patch` must be an object"))?;
    if let Some(existing) = patch_obj.get(key) {
        let existing_path = existing.get("path").and_then(|p| p.as_str());
        if existing_path == Some(rel) {
            return Ok(false);
        }
        bail!(
            "patch for `{key}` already points elsewhere. Fix: remove it or pick another dev target"
        );
    }
    patch_obj.insert(key.to_string(), serde_json::json!({"path": rel}));
    let out = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string());
    std::fs::write(&file, format!("{out}\n"))
        .with_context(|| format!("could not write {}", file.display()))?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Add / remove
// ---------------------------------------------------------------------------

/// Parsed `rivet add` spec.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AddSpec {
    /// Registry dep: (dep key, semver req; `None` = latest at update).
    Registry { name: String, req: Option<String> },
    /// Git dep: (dep key, clone URL, raw `@ref` or `None` = default branch).
    Git {
        key: String,
        url: String,
        raw_ref: Option<String>,
    },
}

impl AddSpec {
    fn key(&self) -> &str {
        match self {
            Self::Registry { name, .. } => name,
            Self::Git { key, .. } => key,
        }
    }
}

/// Split `head@ref` at the last `@`, but only when it follows the last
/// `/` — so scp-like `git@host:path`, `user@host` URLs, and `file://`
/// paths keep their `@`, while `name@req`, `owner/repo@ref`, and
/// `https://…/repo@tag` split. `@` and `/` are ASCII, so byte slicing
/// is safe. Returns (head, optional ref, which may be empty).
fn split_at_ref(spec: &str) -> (&str, Option<&str>) {
    let at = match spec.rfind('@') {
        Some(i) => i,
        None => return (spec, None),
    };
    if spec.rfind('/').is_some_and(|s| s > at) {
        return (spec, None);
    }
    (&spec[..at], Some(&spec[at + 1..]))
}

/// Repo name from a git URL path (last segment, `.git` stripped). A bare
/// host (`https://github.com/`) is rejected: there is no repo to name.
fn repo_name_from_url(url: &str) -> Result<String> {
    let without_scheme = url.split("://").nth(1).unwrap_or(url);
    let path = without_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let trimmed = path.trim_end_matches('/');
    if !trimmed.contains('/') {
        bail!("could not infer a dependency name from `{url}`. Fix: `rivet add owner/repo[@ref]` or a URL ending in `/name[.git]`");
    }
    let last = trimmed.rsplit('/').next().unwrap_or("");
    let name = last.strip_suffix(".git").unwrap_or(last);
    if name.is_empty() {
        bail!("could not infer a dependency name from `{url}`. Fix: `rivet add owner/repo[@ref]` or a URL ending in `/name[.git]`");
    }
    Ok(name.to_string())
}

/// `owner`/`repo` path characters (`[A-Za-z0-9_.-]`, non-empty).
fn is_path_segment(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Parse a `rivet add` spec (pure; no network):
/// - `name` / `name@req` → registry dep (`None` req = latest at update).
/// - `owner/repo[@ref]` → GitHub git dep (`repo` key).
/// - `<url>[@ref]` (any `://` URL, or scp-like `user@host:path`) → git dep.
fn parse_add_spec(spec: &str) -> Result<AddSpec> {
    if spec.is_empty() {
        bail!("empty dependency spec. Fix: `rivet add <name>[@version]`, `owner/repo[@ref]`, or a git URL");
    }
    if spec.contains(char::is_whitespace) {
        bail!("dependency spec `{spec}` contains whitespace. Fix: quote the whole spec, e.g. `rivet add \"owner/repo@tag\"`");
    }
    let (head, raw_ref) = split_at_ref(spec);
    if raw_ref == Some("") {
        bail!("empty `@ref` in `{spec}`. Fix: drop the trailing `@` or give a version, tag, branch, or SHA");
    }
    if head.is_empty() {
        bail!("missing dependency name in `{spec}`. Fix: `rivet add <name>[@version]`, `owner/repo[@ref]`, or a git URL");
    }
    if head.contains("://") {
        let key = repo_name_from_url(head)?;
        return Ok(AddSpec::Git {
            key,
            url: head.to_string(),
            raw_ref: raw_ref.map(str::to_string),
        });
    }
    if head.contains('/') {
        // scp-like `user@host:path` (no `://`): verbatim URL, keyed by path.
        if head.contains('@') && head.contains(':') {
            let path = head.rsplit(':').next().unwrap_or(head);
            let key = repo_name_from_url(&format!("x://{path}"))?;
            return Ok(AddSpec::Git {
                key,
                url: head.to_string(),
                raw_ref: raw_ref.map(str::to_string),
            });
        }
        let mut parts = head.split('/');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(owner), Some(repo), None) if is_path_segment(owner) && is_path_segment(repo) => {
                return Ok(AddSpec::Git {
                    key: repo.to_string(),
                    url: format!("https://github.com/{owner}/{repo}.git"),
                    raw_ref: raw_ref.map(str::to_string),
                });
            }
            _ => bail!("cannot parse `{spec}` as `owner/repo`. Fix: exactly `owner/repo[@ref]` (e.g. `ocornut/imgui@1.91.4`), a full git URL, or a registry `name[@version]`"),
        }
    }
    Ok(AddSpec::Registry {
        name: head.to_string(),
        req: raw_ref.map(str::to_string),
    })
}

/// A probed git ref and what to write for it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbedRef {
    Tag(String),
    Branch(String),
    Rev(String),
}

/// 40 lowercase/uppercase hex chars.
fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Resolve a raw `@ref` against the mirror (fetching): exact tag, then
/// `v`-prefixed tag, then branch. A 40-hex ref resolves as a SHA pin.
/// `None` tracks the remote default branch (`Branch("HEAD")`, a
/// first-class manifest value). Returns (ref, resolved SHA).
fn probe_git_ref(
    git: &rivet_git::GitCache,
    key: &str,
    url: &str,
    raw: Option<&str>,
) -> Result<(ProbedRef, String)> {
    match raw {
        None => {
            let sha = git
                .resolve_ref(key, url, &rivet_git::GitRef::Branch("HEAD".to_string()))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((ProbedRef::Branch("HEAD".to_string()), sha))
        }
        Some(r) if is_full_sha(r) => {
            let sha = git
                .resolve_ref(key, url, &rivet_git::GitRef::Rev(r.to_string()))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((ProbedRef::Rev(sha.clone()), sha))
        }
        Some(r) => {
            let mut tried = Vec::new();
            for candidate in [
                rivet_git::GitRef::Tag(r.to_string()),
                rivet_git::GitRef::Tag(format!("v{r}")),
                rivet_git::GitRef::Branch(r.to_string()),
            ] {
                match git.resolve_ref(key, url, &candidate) {
                    Ok(sha) => {
                        let probed = match candidate {
                            rivet_git::GitRef::Tag(t) => ProbedRef::Tag(t),
                            rivet_git::GitRef::Branch(b) => ProbedRef::Branch(b),
                            rivet_git::GitRef::Rev(s) => ProbedRef::Rev(s),
                        };
                        return Ok((probed, sha));
                    }
                    Err(e) => tried.push(format!("{} ({e})", candidate.describe())),
                }
            }
            bail!("could not resolve `{r}` in `{url}`. Tried: {}. Fix: give an exact tag (a `v`-prefix is tried automatically), branch, or full SHA", tried.join("; "))
        }
    }
}

/// Foreign layout of a materialized checkout (top-level files only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForeignLayout {
    /// Own `rivet.json`: plain git entry, nothing to generate.
    Manifest,
    Cmake,
    Meson,
    Make,
    /// No manifest and no recognized build: inline package without an
    /// adapter (resolves and builds vacuously; warned about).
    Bare,
}

/// Adapter tool for a layout (`None` for manifest-carrying or bare trees).
fn adapter_for_layout(layout: ForeignLayout) -> Option<&'static str> {
    match layout {
        ForeignLayout::Manifest | ForeignLayout::Bare => None,
        ForeignLayout::Cmake => Some("cmake"),
        ForeignLayout::Meson => Some("meson"),
        ForeignLayout::Make => Some("make"),
    }
}

fn sniff_foreign_layout(dir: &Path) -> ForeignLayout {
    if dir.join("rivet.json").is_file() {
        return ForeignLayout::Manifest;
    }
    if dir.join("CMakeLists.txt").is_file() {
        return ForeignLayout::Cmake;
    }
    if dir.join("meson.build").is_file() {
        return ForeignLayout::Meson;
    }
    if ["Makefile", "makefile", "GNUmakefile"]
        .iter()
        .any(|f| dir.join(f).is_file())
    {
        return ForeignLayout::Make;
    }
    ForeignLayout::Bare
}

/// Up to 8 top-level C++ source/header names (for the bare-tree warning).
fn sample_sources(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(dir)
        .map(|r| r.collect::<Vec<_>>())
        .unwrap_or_default();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if matches!(ext, "cpp" | "cc" | "cxx" | "c" | "h" | "hpp" | "hxx" | "hh") {
            out.push(name.to_string());
        }
    }
    out.sort();
    out.truncate(8);
    out
}

/// Inline `package.version` for a git dep: tags shed one leading `v`
/// when a digit follows (`v1.91.4` → `1.91.4`, like the zlib example);
/// branches and SHAs carry no version, so `0.1.0` (the same placeholder
/// the missing-manifest error suggests).
fn inline_version_for(probed: &ProbedRef) -> String {
    match probed {
        ProbedRef::Tag(t) => {
            let stripped = t.strip_prefix('v').or_else(|| t.strip_prefix('V'));
            match stripped {
                Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => rest.to_string(),
                _ => t.clone(),
            }
        }
        ProbedRef::Branch(_) | ProbedRef::Rev(_) => "0.1.0".to_string(),
    }
}

/// Manifest entry JSON for a probed git dep over an inspected layout.
fn git_dep_entry(
    key: &str,
    url: &str,
    probed: &ProbedRef,
    layout: ForeignLayout,
) -> serde_json::Value {
    let mut entry = serde_json::Map::new();
    entry.insert(
        "git".to_string(),
        serde_json::Value::String(url.to_string()),
    );
    match probed {
        ProbedRef::Tag(t) => entry.insert("tag".to_string(), serde_json::Value::String(t.clone())),
        ProbedRef::Branch(b) => {
            entry.insert("branch".to_string(), serde_json::Value::String(b.clone()))
        }
        ProbedRef::Rev(s) => entry.insert("rev".to_string(), serde_json::Value::String(s.clone())),
    };
    if layout != ForeignLayout::Manifest {
        if let Some(tool) = adapter_for_layout(layout) {
            entry.insert("adapter".to_string(), serde_json::json!({"tool": tool}));
        }
        entry.insert(
            "package".to_string(),
            serde_json::json!({"name": key, "version": inline_version_for(probed)}),
        );
    }
    serde_json::Value::Object(entry)
}

/// Insert or remove one `dependencies`-table entry in the scope manifest
/// (serde rewrite like `add_patch_entry`; returns whether it changed).
fn edit_manifest_table(
    dir: &Path,
    table: &str,
    key: &str,
    value: Option<serde_json::Value>,
) -> Result<bool> {
    let file = dir.join("rivet.json");
    let text = std::fs::read_to_string(&file)
        .with_context(|| format!("could not read {}", file.display()))?;
    let stripped = rivet_manifest::strip_jsonc(&text);
    let mut doc: serde_json::Value = serde_json::from_str(&stripped)
        .with_context(|| format!("corrupt {} (fix the JSON first)", file.display()))?;
    let obj = doc
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("manifest must be a JSON object"))?;
    let deps = obj
        .entry(table.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let deps_obj = deps
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("`{table}` must be an object"))?;
    match value {
        Some(v) => {
            if deps_obj.get(key) == Some(&v) {
                return Ok(false);
            }
            deps_obj.insert(key.to_string(), v);
        }
        None => {
            if deps_obj.remove(key).is_none() {
                return Ok(false);
            }
        }
    }
    let out = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string());
    std::fs::write(&file, format!("{out}\n"))
        .with_context(|| format!("could not write {}", file.display()))?;
    Ok(true)
}

/// Dependency keys across all three dep tables (for duplicate checks and
/// `remove` misses).
fn all_dep_keys(root: &rivet_manifest::Manifest) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    out.extend(root.dependencies.keys().cloned());
    out.extend(root.dev_dependencies.keys().cloned());
    out.extend(root.build_dependencies.keys().cloned());
    out
}

/// Add a dependency: parse the spec, verify it (probe + resolve), edit
/// the manifest, and re-resolve. The manifest and lockfile are restored
/// when verification fails, so a failed `add` never leaves a broken tree.
pub fn add_flow(ui: &Ui, root: &rivet_manifest::Manifest, spec: &str, verbose: bool) -> Result<()> {
    let parsed = parse_add_spec(spec)?;
    let key = parsed.key().to_string();
    if all_dep_keys(root).contains(&key) {
        bail!("already depends on `{key}`. Fix: `rivet remove {key}` first, or `rivet update -p {key}` to move it");
    }
    let manifest_file = root.dir.join("rivet.json");
    let orig_manifest = std::fs::read_to_string(&manifest_file)
        .with_context(|| format!("could not read {}", manifest_file.display()))?;
    let lock_file = lock_path(&root.dir);
    let orig_lock: Option<Vec<u8>> = if lock_file.is_file() {
        Some(
            std::fs::read(&lock_file)
                .with_context(|| format!("could not read {}", lock_file.display()))?,
        )
    } else {
        None
    };
    let restore = |manifest: &str, lock: &Option<Vec<u8>>| {
        let _ = std::fs::write(&manifest_file, manifest);
        match lock {
            Some(bytes) => {
                let _ = std::fs::write(&lock_file, bytes);
            }
            None => {
                let _ = std::fs::remove_file(&lock_file);
            }
        }
    };
    let entry = match &parsed {
        AddSpec::Registry { req, .. } => {
            // A bare name resolves `*` (latest) at update below.
            serde_json::Value::String(req.clone().unwrap_or_else(|| "*".to_string()))
        }
        AddSpec::Git { key, url, raw_ref } => {
            let git = git_cache(verbose, false)?;
            let (probed, sha) = probe_git_ref(&git, key, url, raw_ref.as_deref())?;
            let describe = match &probed {
                ProbedRef::Tag(t) => format!("tag \"{t}\""),
                ProbedRef::Branch(b) => format!("branch \"{b}\""),
                ProbedRef::Rev(s) => format!("rev \"{}\"", short7(s)),
            };
            ui.status(
                "Resolved",
                &format!("`{key}` {describe} ({})", short7(&sha)),
            );
            let checkout = git
                .ensure_checkout(key, url, &sha, &rivet_git::CheckoutOpts::default())
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let layout = sniff_foreign_layout(&checkout);
            if layout == ForeignLayout::Bare {
                let sources = sample_sources(&checkout);
                let found = if sources.is_empty() {
                    "no C++ sources at the top level".to_string()
                } else {
                    format!("top-level sources: {}", sources.join(", "))
                };
                ui.warn(&format!("`{key}` has no rivet.json and no recognized build (no CMakeLists.txt/meson.build/Makefile); {found}. Added as a resolving-but-empty dep: vendor the sources with your own rivet.json, ask upstream for one, or set the entry's `adapter` yourself"));
            } else if verbose {
                ui.verbose(&format!("foreign layout: {layout:?}"));
            }
            git_dep_entry(key, url, &probed, layout)
        }
    };
    edit_manifest_table(&root.dir, "dependencies", &key, Some(entry))?;
    ui.warn("added the entry to rivet.json (formatting normalized)");
    // Verify by resolving just this dep; revert both files on failure.
    let fresh_root = rivet_manifest::load(&root.dir).map_err(|e| anyhow::anyhow!("{e}"))?;
    let flags = rivet_graph::FeatureFlags {
        features: Vec::new(),
        all: true,
        no_default: false,
    };
    let opts = UpdateOpts {
        package: Some(key.clone()),
        precise: None,
        offline: false,
    };
    if let Err(e) = update_flow(ui, &fresh_root, &flags, &opts, verbose) {
        restore(&orig_manifest, &orig_lock);
        return Err(e.context(format!(
            "`rivet add {spec}` failed; manifest and lockfile restored"
        )));
    }
    ui.status("Added", &format!("`{key}` (manifest + lockfile updated)"));
    Ok(())
}

/// Remove a dependency from every dep table and drop its lock entry (no
/// network, no re-resolution: run `rivet update` to prune orphans).
pub fn remove_flow(
    ui: &Ui,
    root: &rivet_manifest::Manifest,
    name: &str,
    _verbose: bool,
) -> Result<()> {
    let mut table_of: Option<&str> = None;
    if root.dependencies.contains_key(name) {
        table_of = Some("dependencies");
    } else if root.dev_dependencies.contains_key(name) {
        table_of = Some("dev_dependencies");
    } else if root.build_dependencies.contains_key(name) {
        table_of = Some("build_dependencies");
    }
    let Some(table) = table_of else {
        let mut available: Vec<String> = all_dep_keys(root).into_iter().collect();
        available.sort();
        bail!(
            "package `{name}` not found in dependencies. Available: {}",
            if available.is_empty() {
                "<none>".to_string()
            } else {
                available.join(", ")
            }
        )
    };
    edit_manifest_table(&root.dir, table, name, None)?;
    let lock_file = lock_path(&root.dir);
    if lock_file.is_file() {
        let lock = rivet_resolver::read_lockfile(&lock_file).map_err(|e| anyhow::anyhow!("{e}"))?;
        let before = lock.packages.len();
        let kept: Vec<_> = lock
            .packages
            .into_iter()
            .filter(|p| p.name != name)
            .collect();
        if kept.len() != before {
            let next = rivet_resolver::Lockfile {
                version: rivet_resolver::LOCK_VERSION,
                packages: kept,
            };
            rivet_resolver::write_lockfile(&lock_file, &next)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    }
    ui.warn("removed the entry from rivet.json (formatting normalized); run `rivet update` to prune orphaned transitive entries");
    ui.status("Removed", &format!("`{name}` from {table}"));
    Ok(())
}

// ---------------------------------------------------------------------------
// Git cache commands
// ---------------------------------------------------------------------------

/// Human byte counts (`1.2 MiB`).
fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `rivet git list`: remotes, checkouts, sizes, and lockfile usage.
pub fn git_list_flow(ui: &Ui, root: &rivet_manifest::Manifest, verbose: bool) -> Result<()> {
    let git = git_cache(verbose, false)?;
    let inv = git.list_info().map_err(|e| anyhow::anyhow!("{e}"))?;
    // Which locked packages use each checkout SHA.
    let mut usage: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let path = lock_path(&root.dir);
    if path.is_file() {
        let lock = rivet_resolver::read_lockfile(&path).map_err(|e| anyhow::anyhow!("{e}"))?;
        for pkg in &lock.packages {
            if let rivet_resolver::LockSource::Git { sha, .. } = &pkg.source {
                usage.entry(sha.clone()).or_default().push(pkg.name.clone());
            }
        }
    }
    if inv.remotes.is_empty() && inv.checkouts.is_empty() {
        ui.status("Cache", "empty (no git mirrors or checkouts yet)");
        return Ok(());
    }
    for remote in &inv.remotes {
        ui.status(
            "Remote",
            &format!(
                "{} {} ({})",
                remote.name,
                if remote.url.is_empty() {
                    "<unknown url>".to_string()
                } else {
                    remote.url.clone()
                },
                human_bytes(remote.size_bytes)
            ),
        );
    }
    for checkout in &inv.checkouts {
        let users = usage
            .get(&checkout.sha)
            .map(|names| format!(" used by {}", names.join(", ")))
            .unwrap_or_default();
        ui.status(
            "Checkout",
            &format!(
                "{}@{} ({}{users})",
                checkout.group,
                &checkout.sha[..checkout.sha.len().min(12)],
                human_bytes(checkout.size_bytes)
            ),
        );
    }
    ui.status("Total", &human_bytes(inv.total_bytes));
    Ok(())
}

/// `rivet git prune [--dry-run]`: delete checkouts/mirrors unreferenced by
/// the scope lockfile. Without a lockfile only `--dry-run` is allowed
/// (deleting everything blindly is never the default).
pub fn git_prune_flow(
    ui: &Ui,
    root: &rivet_manifest::Manifest,
    dry_run: bool,
    verbose: bool,
) -> Result<()> {
    let git = git_cache(verbose, false)?;
    let path = lock_path(&root.dir);
    let live: BTreeSet<String> = if path.is_file() {
        let lock = rivet_resolver::read_lockfile(&path).map_err(|e| anyhow::anyhow!("{e}"))?;
        lock.packages
            .iter()
            .filter_map(|p| match &p.source {
                rivet_resolver::LockSource::Git { sha, .. } => Some(sha.clone()),
                _ => None,
            })
            .collect()
    } else if dry_run {
        BTreeSet::new()
    } else {
        bail!(
            "no {}: refusing to prune blindly. Fix: run `rivet update` first, or preview with `--dry-run`",
            path.display()
        );
    };
    let plan = git
        .prune_info(&live.into_iter().collect::<HashSet<String>>())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if plan.unused_checkouts.is_empty() && plan.unused_remotes.is_empty() {
        ui.status("Prune", "nothing to delete");
        return Ok(());
    }
    for dir in plan
        .unused_checkouts
        .iter()
        .chain(plan.unused_remotes.iter())
    {
        if dry_run {
            ui.status("Would delete", &format!("{}", dir.display()));
        } else {
            std::fs::remove_dir_all(dir)
                .with_context(|| format!("could not delete {}", dir.display()))?;
            ui.status("Deleted", &format!("{}", dir.display()));
        }
    }
    ui.status(
        if dry_run { "Reclaimable" } else { "Reclaimed" },
        &human_bytes(plan.reclaimable_bytes),
    );
    Ok(())
}

/// `rivet git fetch`: prefetch every locked remote (CI warm-up).
pub fn git_fetch_flow(ui: &Ui, root: &rivet_manifest::Manifest, verbose: bool) -> Result<()> {
    let path = lock_path(&root.dir);
    if !path.is_file() {
        bail!(
            "no {} yet. Fix: run `rivet update` or `rivet build` first",
            path.display()
        );
    }
    let lock = rivet_resolver::read_lockfile(&path).map_err(|e| anyhow::anyhow!("{e}"))?;
    let git = git_cache(verbose, false)?;
    let mut pairs: Vec<(String, String)> = Vec::new();
    for pkg in &lock.packages {
        if let rivet_resolver::LockSource::Git { url, .. } = &pkg.source {
            pairs.push((pkg.name.clone(), url.clone()));
        }
    }
    if pairs.is_empty() {
        ui.status("Fetch", "nothing to fetch (no git deps locked)");
        return Ok(());
    }
    let borrowed: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(n, u)| (n.as_str(), u.as_str()))
        .collect();
    git.fetch_all(&borrowed)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    ui.status("Fetched", &format!("{} remote(s)", pairs.len()));
    Ok(())
}

// ---------------------------------------------------------------------------
// Registry commands
// ---------------------------------------------------------------------------

/// Home dir for the credential store (`RIVET_HOME` override else the OS
/// home — mirrors `rivet-cache::cache_root` one level up).
fn credentials_home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("RIVET_HOME") {
        if !home.is_empty() {
            return Ok(PathBuf::from(home));
        }
    }
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map(PathBuf::from).ok_or_else(|| {
        anyhow::anyhow!(
            "could not determine home directory; set HOME (Linux) or USERPROFILE (Windows)"
        )
    })
}

/// Resolve the publish target package: the scope root's own package, or
/// `-p` within a workspace/virtual root. Returns an owned manifest.
fn publish_target(
    root: &rivet_manifest::Manifest,
    package: Option<&str>,
) -> Result<rivet_manifest::Manifest> {
    if let Some(name) = package {
        for member in scope_seeds(root)? {
            if rivet_manifest::package_name(&member) == Some(name) {
                if member.package.is_none() {
                    bail!("package `{name}` is a virtual manifest. Fix: publish a member with a `package` table");
                }
                return Ok(member);
            }
        }
        bail!("package `{name}` not found in this workspace. Fix: check `-p`");
    }
    if root.package.is_some() {
        Ok(root.clone())
    } else {
        bail!("workspace root has no package of its own. Fix: `rivet publish -p <package>`");
    }
}

/// Registry metadata for a publish: name/version from the manifest,
/// registry-only deps as index deps (path/git deps warn — they cannot be
/// expressed in the index).
fn publish_meta(manifest: &rivet_manifest::Manifest, ui: &Ui) -> rivet_registry::PublishMeta {
    let pkg = manifest.package.as_ref().unwrap();
    let mut deps = Vec::new();
    for (name, dep) in &manifest.dependencies {
        match dep {
            rivet_manifest::Dependency::Simple(req) => {
                deps.push(rivet_registry::IndexDep {
                    name: name.clone(),
                    req: req.clone(),
                    optional: false,
                });
            }
            rivet_manifest::Dependency::Detailed(d) => {
                if let Some(req) = d
                    .version
                    .clone()
                    .or_else(|| d.registry.clone().map(|_| "*".to_string()))
                {
                    deps.push(rivet_registry::IndexDep {
                        name: name.clone(),
                        req,
                        optional: d.optional.unwrap_or(false),
                    });
                } else if d.path.is_some() || d.git.is_some() {
                    ui.warn(&format!(
                        "dep `{name}` is not a registry dep; index consumers will not see it"
                    ));
                }
            }
        }
    }
    deps.sort_by(|a, b| a.name.cmp(&b.name));
    rivet_registry::PublishMeta {
        name: pkg.name.clone(),
        version: pkg.version.clone(),
        description: None,
        deps,
    }
}

/// `rivet publish [--registry URL] [--token T] [--dry-run] [-p PKG]`.
pub fn publish_flow(
    ui: &Ui,
    root: &rivet_manifest::Manifest,
    package: Option<String>,
    registry: Option<String>,
    token: Option<String>,
    dry_run: bool,
    _verbose: bool,
) -> Result<()> {
    let manifest = publish_target(root, package.as_deref())?;
    let meta = publish_meta(&manifest, ui);
    let packaged = rivet_registry::package_source_dir(&manifest.dir, &meta)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let url = match registry {
        Some(u) => u,
        None => default_registry_url()?,
    };
    if dry_run {
        ui.status(
            "Publish",
            &format!(
                "would upload {} v{} ({} bytes, {}) to {url}",
                meta.name,
                meta.version,
                packaged.bytes.len(),
                packaged.checksum
            ),
        );
        return Ok(());
    }
    let token = match token {
        Some(t) if !t.trim().is_empty() => Some(t),
        _ => std::env::var("RIVET_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| {
                use std::io::BufRead;
                std::io::stdin()
                    .lock()
                    .lines()
                    .next()
                    .and_then(|l| l.ok())
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
            }),
    };
    rivet_registry::publish(&url, &packaged, &meta, token.as_deref(), false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    ui.status(
        "Published",
        &format!("{} v{} to {url}", meta.name, meta.version),
    );
    Ok(())
}

/// `rivet login [--registry URL] [--token T]` (token via flag, `RIVET_TOKEN`,
/// or one stdin line).
pub fn login_flow(ui: &Ui, registry: Option<String>, token: Option<String>) -> Result<()> {
    let url = match registry {
        Some(u) => u,
        None => default_registry_url()?,
    };
    let token = match token {
        Some(t) if !t.trim().is_empty() => t,
        _ => std::env::var("RIVET_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| {
                use std::io::BufRead;
                std::io::stdin()
                    .lock()
                    .lines()
                    .next()
                    .and_then(|l| l.ok())
                    .map(|l| l.trim().to_string())
                    .unwrap_or_default()
            }),
    };
    if token.trim().is_empty() {
        bail!("no token given. Fix: `--token <t>`, `RIVET_TOKEN=<t>`, or pipe it on stdin");
    }
    let home = credentials_home()?;
    let path =
        rivet_registry::login(&url, token.trim(), &home).map_err(|e| anyhow::anyhow!("{e}"))?;
    ui.status("Logged in", &format!("{url} ({})", path.display()));
    Ok(())
}

/// `rivet search [QUERY] [--registry URL]`: substring search over the local
/// index copy (`file://` registries read directly; `https://` needs a
/// previously synced copy — sparse registries expose no listing endpoint).
pub fn search_flow(
    ui: &Ui,
    query: Option<String>,
    registry: Option<String>,
    _verbose: bool,
) -> Result<()> {
    let index_dir = match registry {
        Some(url) if url.starts_with("file://") => {
            let path =
                rivet_registry::file_url_to_path(&url).map_err(|e| anyhow::anyhow!("{e}"))?;
            path.join("index")
        }
        Some(url) => index_dir_for(&url)?,
        None => registry_index_dir()?,
    };
    if !index_dir.is_dir() {
        bail!(
            "no local index copy at {}. Fix: build a project using this registry first (it syncs on resolve)",
            index_dir.display()
        );
    }
    let rows = rivet_registry::search(&index_dir, query.as_deref().unwrap_or(""))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if rows.is_empty() {
        ui.status("Search", "no matches");
        return Ok(());
    }
    for row in rows {
        ui.status(
            "Found",
            &format!(
                "{} v{}{}",
                row.name,
                row.version,
                row.description
                    .map(|d| format!(" — {d}"))
                    .unwrap_or_default()
            ),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(name: &str, req: Option<&str>) -> AddSpec {
        AddSpec::Registry {
            name: name.to_string(),
            req: req.map(str::to_string),
        }
    }

    fn git(key: &str, url: &str, raw_ref: Option<&str>) -> AddSpec {
        AddSpec::Git {
            key: key.to_string(),
            url: url.to_string(),
            raw_ref: raw_ref.map(str::to_string),
        }
    }

    #[test]
    fn add_spec_registry_forms() {
        assert_eq!(parse_add_spec("fmt").unwrap(), registry("fmt", None));
        assert_eq!(
            parse_add_spec("fmt@11").unwrap(),
            registry("fmt", Some("11"))
        );
        assert_eq!(
            parse_add_spec("nlohmann.json@3.11").unwrap(),
            registry("nlohmann.json", Some("3.11"))
        );
        assert!(parse_add_spec("").is_err());
        assert!(parse_add_spec("foo bar").is_err());
        assert!(parse_add_spec("foo@").is_err());
        assert!(parse_add_spec("@1.0").is_err());
    }

    #[test]
    fn add_spec_shorthand_forms() {
        assert_eq!(
            parse_add_spec("ocornut/imgui").unwrap(),
            git("imgui", "https://github.com/ocornut/imgui.git", None)
        );
        assert_eq!(
            parse_add_spec("ocornut/imgui@1.91.4").unwrap(),
            git(
                "imgui",
                "https://github.com/ocornut/imgui.git",
                Some("1.91.4")
            )
        );
        assert_eq!(
            parse_add_spec("ocornut/imgui@v1.91.4").unwrap(),
            git(
                "imgui",
                "https://github.com/ocornut/imgui.git",
                Some("v1.91.4")
            )
        );
        assert!(parse_add_spec("owner/").is_err());
        assert!(parse_add_spec("/repo").is_err());
        assert!(parse_add_spec("a/b/c").is_err());
        assert!(parse_add_spec("a/b/c@v").is_err());
    }

    #[test]
    fn add_spec_url_forms() {
        assert_eq!(
            parse_add_spec("https://github.com/madler/zlib.git").unwrap(),
            git("zlib", "https://github.com/madler/zlib.git", None)
        );
        assert_eq!(
            parse_add_spec("https://github.com/madler/zlib.git@v1.3.1").unwrap(),
            git("zlib", "https://github.com/madler/zlib.git", Some("v1.3.1"))
        );
        // scp-like keeps `user@host` intact; `@ref` still splits.
        assert_eq!(
            parse_add_spec("git@github.com:ocornut/imgui.git").unwrap(),
            git("imgui", "git@github.com:ocornut/imgui.git", None)
        );
        assert_eq!(
            parse_add_spec("git@github.com:ocornut/imgui.git@v1.91.4").unwrap(),
            git("imgui", "git@github.com:ocornut/imgui.git", Some("v1.91.4"))
        );
        assert!(parse_add_spec("https://github.com/").is_err());
    }

    #[test]
    fn split_at_ref_edge_cases() {
        assert_eq!(split_at_ref("a@b"), ("a", Some("b")));
        assert_eq!(split_at_ref("a"), ("a", None));
        // `@` before the last `/` is user/host info, not a ref.
        assert_eq!(split_at_ref("git@h:o/r"), ("git@h:o/r", None));
        assert_eq!(split_at_ref("git@h:o/r@v"), ("git@h:o/r", Some("v")));
    }

    #[test]
    fn inline_version_rules() {
        assert_eq!(
            inline_version_for(&ProbedRef::Tag("v1.91.4".to_string())),
            "1.91.4"
        );
        assert_eq!(
            inline_version_for(&ProbedRef::Tag("release-2".to_string())),
            "release-2"
        );
        assert_eq!(
            inline_version_for(&ProbedRef::Branch("HEAD".to_string())),
            "0.1.0"
        );
        assert_eq!(inline_version_for(&ProbedRef::Rev("a".repeat(40))), "0.1.0");
    }

    #[test]
    fn sha_detection() {
        assert!(is_full_sha(&"a".repeat(40)));
        assert!(is_full_sha("ABCDEF1234567890ABCDEF1234567890ABCDEF12"));
        assert!(!is_full_sha("v1.91.4"));
        assert!(!is_full_sha(&"a".repeat(39)));
        assert!(!is_full_sha(""));
    }
}
