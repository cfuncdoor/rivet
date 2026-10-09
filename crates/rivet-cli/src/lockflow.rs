//! M4 lockfile flow: resolve/write `rivet.lock.json`, materialize locked
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

/// Git cache handle (requires the `git` binary; honors `RIVET_HOME`).
pub fn git_cache(verbose: bool, offline: bool) -> Result<rivet_git::GitCache> {
    rivet_git::require_git().map_err(|e| anyhow::anyhow!("{e}"))?;
    let root = rivet_cache::cache_root().map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(rivet_git::GitCache::new(&root)
        .with_verbose(verbose)
        .with_offline(offline))
}

/// Local registry index copy (`~/.rivet/registry/index/default`; M5 serves it).
fn registry_index_dir() -> Result<PathBuf> {
    let root = rivet_cache::cache_root().map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(root.join("registry").join("index").join("default"))
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
/// and source; features union).
pub fn resolve_lock(
    seeds: &[rivet_manifest::Manifest],
    flags: &rivet_graph::FeatureFlags,
    offline: bool,
    git: &rivet_git::GitCache,
    verbose: bool,
) -> Result<rivet_resolver::Lockfile> {
    let index = rivet_resolver::RegistryIndex::new(
        registry_index_dir().unwrap_or_else(|_| PathBuf::from(".")),
    );
    let _ = verbose;
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
}

impl CliMaterializer {
    /// Build from the scope root (picks up `vendor/` when present).
    pub fn new(git: rivet_git::GitCache, scope_root: &Path, verbose: bool) -> Result<Self> {
        let vendor = vendor_dir(scope_root);
        let vendor_map = read_vendor_map(scope_root)?;
        Ok(Self {
            git,
            vendor,
            vendor_map,
            verbose,
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
}

// ---------------------------------------------------------------------------
// Shared plain-or-locked resolution
// ---------------------------------------------------------------------------

/// Scope-root patch map (dep key → joined patch dir) for resolution.
/// Non-path patch values are an M4-limits error (local paths only).
fn root_patch_map(root: &rivet_manifest::Manifest) -> Result<BTreeMap<String, PathBuf>> {
    let mut map = BTreeMap::new();
    for (key, dep) in &root.patch {
        match dep {
            rivet_manifest::Dependency::Detailed(d) => match &d.path {
                Some(p) => {
                    map.insert(key.clone(), root.dir.join(p));
                }
                None => bail!(
                    "patch for `{key}` uses a non-path source, which is not supported in M4 (local paths only)"
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
        let mat = CliMaterializer::new(git.clone(), &root.dir, verbose)?;
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
                    if let Ok(fresh) = resolve_lock(&seeds, flags, offline, &git, verbose) {
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
    let fresh = resolve_lock(&seeds, flags, offline, &git, verbose).map_err(|e| {
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
    // patch instead; true patch-aware resolution is M5 work).
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
    let fresh = resolve_lock(&seeds, flags, opts.offline, &git, verbose)?;
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
// Patch overrides (path-only in M4)
// ---------------------------------------------------------------------------

/// Dep keys overridden by `patch` tables (own table, else the scope root's).
/// Non-path patch values are an M4-limits error.
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
                        "patch for `{key}` uses a non-path source, which is not supported in M4 (local paths only). Fix: point it at a `path`, or wait for M5 registries"
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
    let mat = CliMaterializer::new(git, &root.dir, verbose)?;
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
            rivet_resolver::LockSource::Registry { .. } => {
                bail!(
                    "cannot vendor registry package `{}` (fetching arrives with M5)",
                    pkg.name
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

/// Recursive directory copy; skips `target/` trees when requested.
/// Symlinks are followed (content copied) — checkouts reject escaping links.
fn copy_dir_filtered(src: &Path, dst: &Path, skip_target: bool) -> Result<()> {
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
    let mut found: Option<(rivet_manifest::DependencyDetail, Vec<String>)> = None;
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
