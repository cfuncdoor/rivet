//! Dependency resolver producing `rivet.lock.json` (contract C2).
//!
//! Resolution honors contract C1 (optional deps + `dep:` features):
//!
//! * Enabled features per package = union of `default` (unless
//!   `--no-default-features`), `--features` requests (root package only),
//!   or all features (with `--all-features`). Unknown feature names and
//!   `dep:` entries pointing at nonexistent/non-optional deps are errors.
//! * Feature tables may chain (`"a": ["b"]` enables `b`); closure is computed
//!   per package. Transitive (non-root) packages get their defaults (or all,
//!   with `--all-features`); `--features` does not propagate to transitive
//!   deps in M4 (no `pkg/feat` syntax yet — deferred to M5 with the real
//!   registry; documented here rather than guessed).
//! * Active deps = non-optional deps plus optional deps named by a `dep:`
//!   entry of an enabled feature.
//!
//! Requirement model:
//!
//! * Registry reqs (`Simple("11")` / `{"version": …}`) use the `semver` crate
//!   with caret semantics; the maximum version satisfying **all** reqs on a
//!   name (unification across the graph) wins.
//! * Path deps read the version from the target manifest; when the dep entry
//!   also carries a `version` field, the manifest version must satisfy it
//!   (as a semver req), else resolution errors.
//! * Git deps resolve via a [`SourceProvider`] (tests inject fakes; real
//!   builds use [`RealProvider`] over `rivet-git`). `tag`/`branch` float
//!   until `update`; `rev` pins. A bare `git` URL without ref means the
//!   remote default branch (`branch = "HEAD"`).
//!
//! Same-URL-different-refs rule (M4 decision): one URL is one logical source.
//! Two packages requesting the same URL with different refs must resolve to
//! the **same SHA** (unified); divergent SHAs are an error spelling both
//! refs and SHAs. The `rivet-git` checkout layout keys trees by SHA so
//! divergence could technically coexist, but building one repo twice at two
//! commits is a typo magnet, so rivet refuses it.
//!
//! Lockfile (contract C2): `{"version":1,"packages":[{name,version,source,
//! features,checksum?}]}` with packages sorted by name, features sorted,
//! stable pretty JSON + trailing newline. Sources:
//!
//! * path: `{"kind":"path","path":"<as-written>"}`.
//! * git: `{"kind":"git","url","ref":{"kind":"tag"|"branch"|"rev",
//!   "value":"…"},"sha","tree"}`. (PLAN.md's `ref` sketch is ambiguous; this
//!   `kind`+`value` shape is the canonical write form. `read` additionally
//!   accepts the single-key form `{"tag":"v1"}` for forward compatibility.)
//! * registry: `{"kind":"registry","registry":"default","version",
//!   "checksum"}` (`"default"` always in M4 — manifests cannot name a
//!   registry yet).
//!
//! Offline/frozen: `offline=true` forbids network — git refs resolve from
//! local mirrors only (missing refs error actionably), the registry uses the
//! local index copy only (never fetched in M4). Frozen = offline resolve +
//! [`check_locked`] against the existing lockfile (no separate flag; the CLI
//! composes them).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use rivet_manifest::{Dependency, Manifest};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Resolver errors. Every variant is actionable (what/where/fix).
#[derive(Debug, Error)]
pub enum Error {
    /// Filesystem failure.
    #[error("{action} `{path}`: {message}")]
    Io {
        /// What was attempted.
        action: String,
        /// Path involved.
        path: String,
        /// OS message + fix hint.
        message: String,
    },
    /// Lockfile JSON malformed.
    #[error("invalid lockfile `{path}`: {message}")]
    BadLock {
        /// Lockfile path.
        path: String,
        /// What was wrong + fix hint.
        message: String,
    },
    /// Unsupported lockfile version.
    #[error("unsupported lockfile version {found} in `{path}` (expected 1). Hint: delete the lockfile and re-resolve")]
    BadLockVersion {
        /// Lockfile path.
        path: String,
        /// Version found.
        found: u32,
    },
    /// Registry index problem.
    #[error("registry index: {message}. Hint: {hint}")]
    BadIndex {
        /// What was wrong.
        message: String,
        /// What to do.
        hint: String,
    },
    /// Requested feature does not exist.
    #[error(
        "package `{package}` has no feature `{feature}`. Hint: available features: {available}"
    )]
    UnknownFeature {
        /// Package name.
        package: String,
        /// Requested feature.
        feature: String,
        /// Comma-joined known features (or "(none)").
        available: String,
    },
    /// Bad `dep:` entry inside a feature.
    #[error("package `{package}` feature `{feature}`: `dep:{entry}` is invalid ({reason}). Hint: `dep:` must name an existing `optional = true` dependency")]
    BadFeatureDep {
        /// Package name.
        package: String,
        /// Feature containing the entry.
        feature: String,
        /// The dep name after `dep:`.
        entry: String,
        /// Why it was rejected.
        reason: String,
    },
    /// Path manifest version does not satisfy the `version` field.
    #[error("path dep `{dep}` version mismatch: manifest is {actual}, requirement is `{req}`. Hint: bump the dep's version or fix the requirement")]
    VersionMismatch {
        /// Dep key.
        dep: String,
        /// Requirement as written.
        req: String,
        /// Manifest version found.
        actual: String,
    },
    /// Unparseable version requirement.
    #[error("dep `{dep}` has invalid version requirement `{req}`: {detail}. Hint: use caret SemVer like `\"11\"`, `\"^1.2\"`, or `\"1.2.3\"`")]
    InvalidReq {
        /// Dep key.
        dep: String,
        /// Requirement as written.
        req: String,
        /// Parser detail.
        detail: String,
    },
    /// Unparseable package version.
    #[error("package `{name}` has invalid version `{version}`. Hint: use `major.minor.patch`, e.g. `0.1.0`")]
    InvalidVersion {
        /// Package name.
        name: String,
        /// Bad version.
        version: String,
    },
    /// No index version satisfies all reqs.
    #[error("no version of `{name}` satisfies [{reqs}]. Hint: relax the requirement or `rivet update` the index copy")]
    NoMatchingVersion {
        /// Package name.
        name: String,
        /// Comma-joined reqs.
        reqs: String,
    },
    /// The locked version was yanked upstream.
    #[error("locked version {name} v{version} was yanked upstream. Hint: run `rivet update -p {name}` to move to a supported version")]
    YankedPinned {
        /// Package name.
        name: String,
        /// Yanked version.
        version: String,
    },
    /// Same URL requested at divergent refs (boxed: 7 strings would push
    /// `Error` over clippy's `result_large_err` threshold).
    #[error("{0}")]
    SameUrlDifferentRefs(Box<SameUrlRefs>),
    /// Same package name from incompatible sources.
    #[error("duplicate package `{name}`: {detail}. Hint: {hint}")]
    DuplicatePackage {
        /// Package name.
        name: String,
        /// What clashed.
        detail: String,
        /// What to do.
        hint: String,
    },
    /// Dependency cycle through path/git deps.
    #[error("dependency cycle: {chain}. Hint: break the cycle with a registry dep or restructure the packages")]
    Cycle {
        /// `a -> b -> a`.
        chain: String,
    },
    /// Expected manifest missing.
    #[error("{where_}: {message}. Hint: {hint}")]
    MissingManifest {
        /// Where it was expected.
        where_: String,
        /// What was wrong.
        message: String,
        /// What to do.
        hint: String,
    },
    /// Dep key and child manifest package name disagree.
    #[error("dep `{dep}` resolves to package `{actual}` (manifest package name differs). Hint: rename the dep key to `{actual}` or fix the child `package.name`")]
    NameMismatch {
        /// Dep key.
        dep: String,
        /// Manifest name.
        actual: String,
    },
    /// Network needed but offline.
    #[error("offline: {action} needs network but offline mode is on ({where_}). Hint: run once online to populate the cache, then retry offline")]
    Offline {
        /// What was attempted.
        action: String,
        /// Where.
        where_: String,
    },
    /// Underlying git failure.
    #[error(transparent)]
    Git(#[from] rivet_git::Error),
    /// Underlying manifest failure.
    #[error(transparent)]
    Manifest(#[from] rivet_manifest::Error),
}

impl Error {
    fn io(action: &str, path: &Path, err: std::io::Error) -> Self {
        Self::Io {
            action: action.to_string(),
            path: path.display().to_string(),
            message: format!("{err}. Hint: check permissions and disk space"),
        }
    }
}

/// Detail for [`Error::SameUrlDifferentRefs`]: the two divergent requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SameUrlRefs {
    /// URL requested twice.
    pub url: String,
    /// First ref kind (`tag`/`branch`/`rev`).
    pub first_kind: String,
    /// First ref value.
    pub first_value: String,
    /// SHA the first ref resolved to.
    pub first_sha: String,
    /// Second ref kind.
    pub second_kind: String,
    /// Second ref value.
    pub second_value: String,
    /// SHA the second ref resolved to.
    pub second_sha: String,
}

impl std::fmt::Display for SameUrlRefs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "git URL `{}` requested at two refs resolving to different commits: {} `{}` ({}) vs {} `{}` ({}). Hint: unify on one ref so the repo builds once",
            self.url,
            self.first_kind,
            self.first_value,
            self.first_sha,
            self.second_kind,
            self.second_value,
            self.second_sha
        )
    }
}

/// The lockfile is stale relative to a fresh resolution.
///
/// Produced by [`check_locked`]; the CLI surfaces this for `--locked`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("lockfile is out of date:\n{diff}\nHint: run `rivet update` to refresh it, or build without --locked")]
pub struct LockedError {
    /// Human-readable unified diff (added/removed/changed lines).
    pub diff: String,
}

// ---------------------------------------------------------------------------
// Resolve options
// ---------------------------------------------------------------------------

/// Feature + network options for resolution.
#[derive(Debug, Clone, Default)]
pub struct ResolveOpts {
    /// Features to enable on the root package.
    pub features: Vec<String>,
    /// Enable all features of every package.
    pub all_features: bool,
    /// Disable `default` features everywhere.
    pub no_default_features: bool,
    /// Forbid network (mirrors + local index copy only).
    pub offline: bool,
}

// ---------------------------------------------------------------------------
// Lockfile model (contract C2)
// ---------------------------------------------------------------------------

/// Lockfile version constant.
pub const LOCK_VERSION: u32 = 1;

/// A resolved lockfile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    /// Schema version (always 1).
    pub version: u32,
    /// Packages sorted by name.
    pub packages: Vec<LockedPackage>,
}

/// One locked package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPackage {
    /// Package (dep-key) name.
    pub name: String,
    /// Resolved version.
    pub version: String,
    /// Where the source came from.
    pub source: LockSource,
    /// Enabled features (sorted).
    #[serde(default)]
    pub features: Vec<String>,
    /// Registry checksum (`sha256:…`); registry packages only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
}

/// Locked source discriminant (internally tagged `kind`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LockSource {
    /// Local path dep (path as written in the manifest).
    Path {
        /// Path as written.
        path: String,
    },
    /// Registry package (M4: always the default registry).
    Registry {
        /// Registry URL or `"default"`.
        registry: String,
        /// Selected version (duplicates package `version` for readability).
        version: String,
        /// Checksum from the index (`sha256:…`).
        checksum: String,
    },
    /// Git checkout.
    Git {
        /// Repo URL.
        url: String,
        /// Requested ref.
        #[serde(rename = "ref")]
        git_ref: GitLockRef,
        /// Resolved full SHA.
        sha: String,
        /// `sha^{tree}` at resolve time.
        tree: String,
    },
}

/// A locked git ref: `{"kind":"tag"|"branch"|"rev","value":"…"}`.
///
/// `read` additionally accepts the single-key form (`{"tag":"v1"}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitLockRef {
    /// `tag` | `branch` | `rev`.
    pub kind: String,
    /// Ref value as written.
    pub value: String,
}

impl GitLockRef {
    /// Build from a [`rivet_git::GitRef`].
    pub fn from_git_ref(r: &rivet_git::GitRef) -> Self {
        let (kind, value) = r.kind_value();
        Self {
            kind: kind.to_string(),
            value: value.to_string(),
        }
    }
}

impl Serialize for GitLockRef {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("GitLockRef", 2)?;
        st.serialize_field("kind", &self.kind)?;
        st.serialize_field("value", &self.value)?;
        st.end()
    }
}

impl<'de> Deserialize<'de> for GitLockRef {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        let obj = v.as_object().ok_or_else(|| {
            serde::de::Error::custom(
                "git `ref` must be an object like {\"kind\":\"tag\",\"value\":\"v1\"}",
            )
        })?;
        if let (Some(k), Some(val)) = (obj.get("kind"), obj.get("value")) {
            let kind = k
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("git `ref.kind` must be a string"))?;
            let value = val
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("git `ref.value` must be a string"))?;
            if !matches!(kind, "tag" | "branch" | "rev") {
                return Err(serde::de::Error::custom(
                    "git `ref.kind` must be one of tag, branch, rev",
                ));
            }
            return Ok(Self {
                kind: kind.to_string(),
                value: value.to_string(),
            });
        }
        // Single-key form: {"tag": "v1"}.
        if obj.len() == 1 {
            for key in ["tag", "branch", "rev"] {
                if let Some(val) = obj.get(key) {
                    let value = val.as_str().ok_or_else(|| {
                        serde::de::Error::custom("git ref value must be a string")
                    })?;
                    return Ok(Self {
                        kind: key.to_string(),
                        value: value.to_string(),
                    });
                }
            }
        }
        Err(serde::de::Error::custom(
            "git `ref` must look like {\"kind\":\"tag\",\"value\":\"v1\"}",
        ))
    }
}

// ---------------------------------------------------------------------------
// Registry index stub (M5-compatible)
// ---------------------------------------------------------------------------

/// One index line for a package version.
///
/// File layout: `<index-dir>/<name>`, one JSON object per line:
/// `{"version":"1.2.3","checksum":"sha256:…","yanked":false,
/// "deps":[{"name":"…","req":"^1","optional":false}]}`.
/// Checksums are stored verbatim into the lockfile. Unknown fields are
/// ignored so M5 can extend the format without breaking this reader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// Version string.
    pub version: String,
    /// Checksum (`sha256:…`).
    #[serde(default)]
    pub checksum: String,
    /// Yanked upstream (skipped unless the lock pins it — then error).
    #[serde(default)]
    pub yanked: bool,
    /// Transitive deps (non-optional only honored in M4; optional index
    /// deps are recorded but skipped — full `dep:`/feature tables for
    /// registry packages arrive with M5).
    #[serde(default)]
    pub deps: Vec<IndexDep>,
}

/// One transitive index dep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexDep {
    /// Package name.
    pub name: String,
    /// Semver requirement (caret semantics).
    #[serde(default = "default_index_req")]
    pub req: String,
    /// Optional (skipped in M4 stub resolution).
    #[serde(default)]
    pub optional: bool,
}

fn default_index_req() -> String {
    "*".to_string()
}

/// Minimal JSON-lines registry index reader over a directory.
///
/// M5-compatible stub: reads `<dir>/<name>` line by line. Missing file =
/// unknown package (error). Malformed lines are errors (fail fast — a corrupt
/// index copy must not resolve silently).
#[derive(Debug, Clone)]
pub struct RegistryIndex {
    dir: PathBuf,
}

impl RegistryIndex {
    /// Point at an index directory (local copy; never fetched in M4).
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// All entries for `name` (file `<dir>/<name>`).
    pub fn versions(&self, name: &str) -> Result<Vec<IndexEntry>, Error> {
        let file = self.dir.join(name);
        let text = std::fs::read_to_string(&file).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::BadIndex {
                    message: format!("package `{name}` not in the local index copy"),
                    hint: "check the package name; registry publishing arrives with M5".to_string(),
                }
            } else {
                Error::Io {
                    action: "read index file".to_string(),
                    path: file.display().to_string(),
                    message: format!("{e}. Hint: check permissions"),
                }
            }
        })?;
        let mut out = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let entry: IndexEntry = serde_json::from_str(line).map_err(|e| Error::BadIndex {
                message: format!("package `{name}` line {} is corrupt: {e}", i + 1),
                hint: "re-create the index copy; index lines must be JSON objects".to_string(),
            })?;
            out.push(entry);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Source provider (test fakes + real git)
// ---------------------------------------------------------------------------

/// How the resolver loads transitive manifests and git SHAs.
///
/// Tests inject fakes; real builds use [`RealProvider`] (rivet-git +
/// filesystem). Registry manifests are not loaded through this trait —
/// registry transitive deps come from the index stub (M5 reads real
/// manifests).
pub trait SourceProvider {
    /// Load the manifest in path dep directory `dir`.
    fn load_path_manifest(&self, dir: &Path) -> Result<Manifest, Error>;
    /// Resolve `(name, url, ref)` to a full SHA.
    fn resolve_git(
        &self,
        name: &str,
        url: &str,
        git_ref: &rivet_git::GitRef,
    ) -> Result<String, Error>;
    /// `sha^{tree}` for a resolved SHA.
    fn tree_for_git(&self, name: &str, url: &str, sha: &str) -> Result<String, Error>;
    /// Load the manifest at `sha`/`subdir` (materializing the checkout).
    fn load_git_manifest(
        &self,
        name: &str,
        url: &str,
        sha: &str,
        subdir: Option<&str>,
        submodules: bool,
    ) -> Result<Manifest, Error>;
}

/// In-memory provider for tests: pre-seeded maps, no git binary, no network.
#[derive(Debug, Clone, Default)]
pub struct TestProvider {
    /// `dir (as string)` -> manifest.
    pub path_manifests: BTreeMap<String, Manifest>,
    /// `(url, ref-kind, ref-value)` -> full SHA.
    pub git_shas: BTreeMap<(String, String, String), String>,
    /// `(url, sha)` -> tree hash.
    pub git_trees: BTreeMap<(String, String), String>,
    /// `(url, sha, subdir-or-"")` -> manifest.
    pub git_manifests: BTreeMap<(String, String, String), Manifest>,
    /// When true, every git resolve fails with [`Error::Offline`]
    /// (simulates "no network, nothing cached").
    pub offline: bool,
}

impl TestProvider {
    /// Empty provider.
    pub fn new() -> Self {
        Self::default()
    }

    /// Lexically normalize a path (resolve `.`/`..` without touching the
    /// filesystem) so `dir.join("../sib")` keys match plain inserts.
    fn norm(dir: &Path) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut rooted = false;
        let mut prefix: Option<String> = None;
        for comp in dir.components() {
            match comp {
                std::path::Component::Prefix(p) => {
                    prefix = Some(p.as_os_str().to_string_lossy().to_string());
                }
                std::path::Component::RootDir => rooted = true,
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    parts.pop();
                }
                std::path::Component::Normal(s) => {
                    parts.push(s.to_string_lossy().to_string());
                }
            }
        }
        let mut out = String::new();
        if let Some(p) = prefix {
            out.push_str(&p);
        }
        if rooted {
            out.push('/');
        }
        out.push_str(&parts.join("/"));
        out
    }

    /// Key helper: manifests are looked up by normalized `dir`.
    pub fn insert_path(&mut self, dir: &Path, manifest: Manifest) {
        self.path_manifests.insert(Self::norm(dir), manifest);
    }

    /// Seed one git ref (`kind` is `tag`/`branch`/`rev`).
    pub fn insert_git_ref(&mut self, url: &str, kind: &str, value: &str, sha: &str) {
        self.git_shas.insert(
            (url.to_string(), kind.to_string(), value.to_string()),
            sha.to_string(),
        );
    }
}

impl SourceProvider for TestProvider {
    fn load_path_manifest(&self, dir: &Path) -> Result<Manifest, Error> {
        self.path_manifests
            .get(&Self::norm(dir))
            .cloned()
            .ok_or_else(|| Error::MissingManifest {
                where_: dir.display().to_string(),
                message: "no manifest seeded for this path".to_string(),
                hint: "seed TestProvider::insert_path in the test".to_string(),
            })
    }

    fn resolve_git(
        &self,
        name: &str,
        url: &str,
        git_ref: &rivet_git::GitRef,
    ) -> Result<String, Error> {
        if self.offline {
            return Err(Error::Offline {
                action: format!("resolve {} {}", name, git_ref.describe()),
                where_: url.to_string(),
            });
        }
        let (kind, value) = git_ref.kind_value();
        self.git_shas
            .get(&(url.to_string(), kind.to_string(), value.to_string()))
            .cloned()
            .ok_or_else(|| Error::MissingManifest {
                where_: format!("{url} {}", git_ref.describe()),
                message: "no SHA seeded for this ref".to_string(),
                hint: "seed TestProvider::insert_git_ref in the test".to_string(),
            })
    }

    fn tree_for_git(&self, _name: &str, url: &str, sha: &str) -> Result<String, Error> {
        self.git_trees
            .get(&(url.to_string(), sha.to_string()))
            .cloned()
            .ok_or_else(|| Error::MissingManifest {
                where_: format!("{url}@{sha}"),
                message: "no tree hash seeded".to_string(),
                hint: "seed TestProvider::git_trees in the test".to_string(),
            })
    }

    fn load_git_manifest(
        &self,
        name: &str,
        url: &str,
        sha: &str,
        subdir: Option<&str>,
        _submodules: bool,
    ) -> Result<Manifest, Error> {
        let key = (
            url.to_string(),
            sha.to_string(),
            subdir.unwrap_or("").to_string(),
        );
        self.git_manifests
            .clone()
            .get(&key)
            .cloned()
            .ok_or_else(|| Error::MissingManifest {
                where_: format!("{url}@{sha}#{}", subdir.unwrap_or("")),
                message: format!("no manifest seeded for git dep `{name}`"),
                hint: "seed TestProvider::git_manifests in the test".to_string(),
            })
    }
}

/// Real provider: filesystem manifests + `rivet-git` checkouts.
#[derive(Debug, Clone)]
pub struct RealProvider {
    git: rivet_git::GitCache,
}

impl RealProvider {
    /// Wrap a configured [`rivet_git::GitCache`] (offline/verbose preset by
    /// the caller to match [`ResolveOpts`]).
    pub fn new(git: rivet_git::GitCache) -> Self {
        Self { git }
    }
}

impl SourceProvider for RealProvider {
    fn load_path_manifest(&self, dir: &Path) -> Result<Manifest, Error> {
        Ok(rivet_manifest::load(dir)?)
    }

    fn resolve_git(
        &self,
        name: &str,
        url: &str,
        git_ref: &rivet_git::GitRef,
    ) -> Result<String, Error> {
        Ok(self.git.resolve_ref(name, url, git_ref)?)
    }

    fn tree_for_git(&self, name: &str, url: &str, sha: &str) -> Result<String, Error> {
        Ok(self.git.tree_hash(name, url, sha)?)
    }

    fn load_git_manifest(
        &self,
        name: &str,
        url: &str,
        sha: &str,
        subdir: Option<&str>,
        submodules: bool,
    ) -> Result<Manifest, Error> {
        let opts = rivet_git::CheckoutOpts {
            subdir: subdir.map(str::to_string),
            submodules,
            expected_tree: None,
        };
        let dir = self.git.ensure_checkout(name, url, sha, &opts)?;
        rivet_manifest::load(&dir).map_err(|e| Error::MissingManifest {
            where_: format!("{url}@{sha}#{}", subdir.unwrap_or("")),
            message: format!("no rivet.json here ({e}); provide an inline `package` block"),
            hint: "add a rivet.json to the repo/subdir or use `package = { name, version }` inline"
                .to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// Feature closure (contract C1)
// ---------------------------------------------------------------------------

/// Eagerly validate a manifest's whole feature table (every feature, not
/// just the enabled ones): each entry must be `dep:<existing-optional-dep>`
/// or the name of another declared feature. Typos in disabled features are
/// still errors — Cargo-like strictness, and the CLI should surface them
/// before anyone passes `--features`.
fn validate_feature_table(manifest: &Manifest) -> Result<(), Error> {
    let pkg_name = manifest
        .package
        .as_ref()
        .map(|p| p.name.as_str())
        .unwrap_or("<root>");
    for (feat, entries) in &manifest.features {
        for entry in entries {
            if let Some(dep_name) = entry.strip_prefix("dep:") {
                match manifest.dependencies.get(dep_name) {
                    Some(d) if d.is_optional() => {}
                    Some(_) => {
                        return Err(Error::BadFeatureDep {
                            package: pkg_name.to_string(),
                            feature: feat.clone(),
                            entry: dep_name.to_string(),
                            reason: format!("`{dep_name}` exists but is not `optional = true`"),
                        });
                    }
                    None => {
                        return Err(Error::BadFeatureDep {
                            package: pkg_name.to_string(),
                            feature: feat.clone(),
                            entry: dep_name.to_string(),
                            reason: "no such dependency".to_string(),
                        });
                    }
                }
            } else if !manifest.features.contains_key(entry) {
                let mut avail: Vec<String> = manifest.features.keys().cloned().collect();
                avail.sort();
                return Err(Error::UnknownFeature {
                    package: pkg_name.to_string(),
                    feature: entry.clone(),
                    available: if avail.is_empty() {
                        "(none)".to_string()
                    } else {
                        avail.join(", ")
                    },
                });
            }
        }
    }
    Ok(())
}

/// Compute `(enabled features, activated optional deps)` for one manifest.
///
/// `is_root` selects whether `--features` applies (root only). With
/// `all_features`, every declared feature is enabled. Errors on unknown
/// requested features, unknown chained features, and `dep:` entries that do
/// not name an existing optional dep.
fn feature_closure(
    manifest: &Manifest,
    opts: &ResolveOpts,
    is_root: bool,
) -> Result<(BTreeSet<String>, BTreeSet<String>), Error> {
    let pkg_name = manifest
        .package
        .as_ref()
        .map(|p| p.name.as_str())
        .unwrap_or("<root>");
    let declared: BTreeSet<String> = manifest.features.keys().cloned().collect();
    let mut enabled = BTreeSet::new();
    if opts.all_features {
        enabled.extend(declared.iter().cloned());
    } else {
        if !opts.no_default_features && declared.contains("default") {
            enabled.insert("default".to_string());
        }
        if is_root {
            for f in &opts.features {
                if !declared.contains(f) {
                    let mut avail: Vec<String> = declared.iter().cloned().collect();
                    avail.sort();
                    return Err(Error::UnknownFeature {
                        package: pkg_name.to_string(),
                        feature: f.clone(),
                        available: if avail.is_empty() {
                            "(none)".to_string()
                        } else {
                            avail.join(", ")
                        },
                    });
                }
                enabled.insert(f.clone());
            }
        }
    }
    // Closure over feature-to-feature refs; collect dep: activations.
    let mut activated = BTreeSet::new();
    let mut stack: Vec<String> = enabled.iter().cloned().collect();
    let mut seen: BTreeSet<String> = enabled.clone();
    while let Some(feat) = stack.pop() {
        let entries = manifest
            .features
            .get(&feat)
            .ok_or_else(|| Error::UnknownFeature {
                package: pkg_name.to_string(),
                feature: feat.clone(),
                available: {
                    let mut avail: Vec<String> = declared.iter().cloned().collect();
                    avail.sort();
                    if avail.is_empty() {
                        "(none)".to_string()
                    } else {
                        avail.join(", ")
                    }
                },
            })?;
        for entry in entries {
            if let Some(dep_name) = entry.strip_prefix("dep:") {
                match manifest.dependencies.get(dep_name) {
                    Some(d) if d.is_optional() => {
                        activated.insert(dep_name.to_string());
                    }
                    Some(_) => {
                        return Err(Error::BadFeatureDep {
                            package: pkg_name.to_string(),
                            feature: feat.clone(),
                            entry: dep_name.to_string(),
                            reason: format!("`{dep_name}` exists but is not `optional = true`"),
                        });
                    }
                    None => {
                        return Err(Error::BadFeatureDep {
                            package: pkg_name.to_string(),
                            feature: feat.clone(),
                            entry: dep_name.to_string(),
                            reason: "no such dependency".to_string(),
                        });
                    }
                }
            } else if declared.contains(entry) {
                if seen.insert(entry.clone()) {
                    enabled.insert(entry.clone());
                    stack.push(entry.clone());
                }
            } else {
                return Err(Error::UnknownFeature {
                    package: pkg_name.to_string(),
                    feature: entry.clone(),
                    available: {
                        let mut avail: Vec<String> = declared.iter().cloned().collect();
                        avail.sort();
                        if avail.is_empty() {
                            "(none)".to_string()
                        } else {
                            avail.join(", ")
                        }
                    },
                });
            }
        }
    }
    Ok((enabled, activated))
}

/// Active `(dep-key, dep)` pairs: non-optional always + activated optionals.
fn active_deps(manifest: &Manifest, activated: &BTreeSet<String>) -> Vec<(String, Dependency)> {
    let mut out = Vec::new();
    for (name, dep) in &manifest.dependencies {
        if dep.is_optional() && !activated.contains(name) {
            continue;
        }
        out.push((name.clone(), dep.clone()));
    }
    out
}

/// Walk `parent` links from `key`; `Some("a -> b -> a")` when `key` is its
/// own ancestor (a path/git dependency cycle), else `None`.
fn cycle_chain(parent: &BTreeMap<String, String>, key: &str) -> Option<String> {
    let mut chain = vec![key.to_string()];
    let mut cur = key.to_string();
    while let Some(p) = parent.get(&cur).cloned() {
        chain.push(p.clone());
        if p == key {
            chain.reverse();
            return Some(chain.join(" -> "));
        }
        cur = p;
        // Defensive bound: chains are short; bail rather than loop forever
        // on corrupt input.
        if chain.len() > 1024 {
            break;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Resolver
// ---------------------------------------------------------------------------

/// Dependency resolver over an index + source provider.
pub struct Resolver<'a, P: SourceProvider> {
    index: RegistryIndex,
    provider: &'a P,
    existing: Option<&'a Lockfile>,
}

impl<'a, P: SourceProvider> Resolver<'a, P> {
    /// Create a resolver (fresh resolve; yanked versions always skipped).
    pub fn new(index: RegistryIndex, provider: &'a P) -> Self {
        Self {
            index,
            provider,
            existing: None,
        }
    }

    /// Create a resolver that honors an existing lockfile for yank
    /// detection (a pinned-then-yanked version is an error, not a silent
    /// skip — the user must `update` deliberately).
    pub fn with_existing(index: RegistryIndex, provider: &'a P, existing: &'a Lockfile) -> Self {
        Self {
            index,
            provider,
            existing: Some(existing),
        }
    }

    /// Resolve `root` to a deterministic [`Lockfile`].
    pub fn resolve(&self, root: &Manifest, opts: &ResolveOpts) -> Result<Lockfile, Error> {
        let root_name = root
            .package
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "root".to_string());

        // Per-package accumulators keyed by dep key (= lockfile name).
        let mut versions: BTreeMap<String, String> = BTreeMap::new();
        let mut sources: BTreeMap<String, LockSource> = BTreeMap::new();
        let mut checksums: BTreeMap<String, Option<String>> = BTreeMap::new();
        let mut enabled_by: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut registry_reqs: BTreeMap<String, Vec<String>> = BTreeMap::new();
        // Same-URL rule state: url -> (kind, value, sha).
        let mut git_urls: BTreeMap<String, (String, String, String)> = BTreeMap::new();

        // Work queue of (dep-key, manifest, is_root). Feature union may
        // re-queue a package when new features appear.
        let mut queue: Vec<(String, Manifest, bool)> =
            vec![(root_name.clone(), root.clone(), true)];
        // Parent links for cycle reports (`child -> parent`, first wins).
        let mut parent: BTreeMap<String, String> = BTreeMap::new();
        let mut expanded: BTreeSet<String> = BTreeSet::new();

        // Seed the root itself as a package? No: the lockfile holds only
        // dependencies (Cargo convention — the root is not locked). Root
        // features still gate which direct deps are active.
        while let Some((key, manifest, is_root)) = queue.pop() {
            // True dependency cycles (a -> b -> a through path/git deps)
            // surface here; the build graph also rejects them at build time.
            if let Some(chain) = cycle_chain(&parent, &key) {
                return Err(Error::Cycle { chain });
            }
            validate_feature_table(&manifest)?;
            let (enabled, activated) = feature_closure(&manifest, opts, is_root)?;
            let grown = match enabled_by.get(&key) {
                Some(prev) => !enabled.is_subset(prev),
                None => true,
            };
            if !grown && expanded.contains(&key) {
                continue;
            }
            enabled_by
                .entry(key.clone())
                .or_default()
                .extend(enabled.iter().cloned());
            if expanded.contains(&key) && !grown {
                continue;
            }

            for (dep_name, dep) in active_deps(&manifest, &activated) {
                if let Some(path) = dep.path().map(str::to_string) {
                    let dir = manifest.dir.join(&path);
                    let child = self.provider.load_path_manifest(&dir)?;
                    let child_pkg =
                        child
                            .package
                            .clone()
                            .ok_or_else(|| Error::MissingManifest {
                                where_: dir.display().to_string(),
                                message: "virtual workspace manifest cannot be a dependency"
                                    .to_string(),
                                hint: "depend on a member package directory instead".to_string(),
                            })?;
                    // `version` field + path manifest version must agree.
                    if let Dependency::Detailed(d) = &dep {
                        if let Some(req) = &d.version {
                            let req_parsed =
                                semver::VersionReq::parse(req).map_err(|e| Error::InvalidReq {
                                    dep: dep_name.clone(),
                                    req: req.clone(),
                                    detail: e.to_string(),
                                })?;
                            let actual =
                                semver::Version::parse(&child_pkg.version).map_err(|_| {
                                    Error::InvalidVersion {
                                        name: child_pkg.name.clone(),
                                        version: child_pkg.version.clone(),
                                    }
                                })?;
                            if !req_parsed.matches(&actual) {
                                return Err(Error::VersionMismatch {
                                    dep: dep_name.clone(),
                                    req: req.clone(),
                                    actual: child_pkg.version.clone(),
                                });
                            }
                        }
                    }
                    if child_pkg.name != dep_name {
                        return Err(Error::NameMismatch {
                            dep: dep_name.clone(),
                            actual: child_pkg.name.clone(),
                        });
                    }
                    if let Some(prev) = versions.get(&dep_name) {
                        if prev != &child_pkg.version {
                            return Err(Error::DuplicatePackage {
                                name: dep_name.clone(),
                                detail: format!("path dep version {prev} vs {}", child_pkg.version),
                                hint: "unify on one version of the path dependency".to_string(),
                            });
                        }
                        if sources.get(&dep_name) != Some(&LockSource::Path { path: path.clone() })
                        {
                            return Err(Error::DuplicatePackage {
                                name: dep_name.clone(),
                                detail: "same name from two different sources".to_string(),
                                hint: "rename one dependency or unify the source".to_string(),
                            });
                        }
                    } else {
                        versions.insert(dep_name.clone(), child_pkg.version.clone());
                        sources.insert(dep_name.clone(), LockSource::Path { path: path.clone() });
                        checksums.insert(dep_name.clone(), None);
                    }
                    parent
                        .entry(dep_name.clone())
                        .or_insert_with(|| key.clone());
                    queue.push((dep_name, child, false));
                } else if dep.is_git() {
                    let d = match &dep {
                        Dependency::Detailed(d) => d,
                        Dependency::Simple(_) => {
                            return Err(Error::MissingManifest {
                                where_: dep_name.clone(),
                                message: "unreachable: simple deps are registry".to_string(),
                                hint: "report a bug".to_string(),
                            });
                        }
                    };
                    let url = d.git.clone().unwrap_or_default();
                    let git_ref =
                        git_ref_of(d.tag.as_deref(), d.branch.as_deref(), d.rev.as_deref())?;
                    let (kind, value) = git_ref.kind_value();
                    let sha = self.provider.resolve_git(&dep_name, &url, &git_ref)?;
                    // Same-URL rule: divergent SHAs error; identical SHAs unify.
                    if let Some((pk, pv, ps)) = git_urls.get(&url) {
                        if ps != &sha {
                            return Err(Error::SameUrlDifferentRefs(Box::new(SameUrlRefs {
                                url: url.clone(),
                                first_kind: pk.clone(),
                                first_value: pv.clone(),
                                first_sha: ps.clone(),
                                second_kind: kind.to_string(),
                                second_value: value.to_string(),
                                second_sha: sha.clone(),
                            })));
                        }
                    } else {
                        git_urls.insert(
                            url.clone(),
                            (kind.to_string(), value.to_string(), sha.clone()),
                        );
                    }
                    let tree = self.provider.tree_for_git(&dep_name, &url, &sha)?;
                    // Manifest or inline package block.
                    let child: Manifest = match self.provider.load_git_manifest(
                        &dep_name,
                        &url,
                        &sha,
                        d.subdir.as_deref(),
                        d.submodules.unwrap_or(false),
                    ) {
                        Ok(m) => {
                            if let Some(pkg) = &m.package {
                                if pkg.name != dep_name {
                                    return Err(Error::NameMismatch {
                                        dep: dep_name.clone(),
                                        actual: pkg.name.clone(),
                                    });
                                }
                            }
                            m
                        }
                        Err(Error::MissingManifest { .. }) => match &d.package {
                            Some(inline) => inline_manifest(
                                &dep_name,
                                inline.name.clone(),
                                inline.version.clone(),
                            )?,
                            None => {
                                return Err(Error::MissingManifest {
                                        where_: format!(
                                            "{url}@{sha}#{}",
                                            d.subdir.as_deref().unwrap_or("")
                                        ),
                                        message: format!(
                                            "git dep `{dep_name}` has no rivet.json"
                                        ),
                                        hint: "add a rivet.json upstream or use an inline `package = { name, version }` block".to_string(),
                                    });
                            }
                        },
                        Err(other) => return Err(other),
                    };
                    let child_version = child
                        .package
                        .as_ref()
                        .map(|p| p.version.clone())
                        .unwrap_or_else(|| "0.0.0".to_string());
                    if let Some(prev) = versions.get(&dep_name) {
                        if prev != &child_version {
                            return Err(Error::DuplicatePackage {
                                name: dep_name.clone(),
                                detail: format!("git dep version {prev} vs {child_version}"),
                                hint: "unify on one ref of the git dependency".to_string(),
                            });
                        }
                    } else {
                        versions.insert(dep_name.clone(), child_version);
                        sources.insert(
                            dep_name.clone(),
                            LockSource::Git {
                                url: url.clone(),
                                git_ref: GitLockRef::from_git_ref(&git_ref),
                                sha: sha.clone(),
                                tree,
                            },
                        );
                        checksums.insert(dep_name.clone(), None);
                    }
                    parent
                        .entry(dep_name.clone())
                        .or_insert_with(|| key.clone());
                    queue.push((dep_name, child, false));
                } else {
                    // Registry.
                    let req = match &dep {
                        Dependency::Simple(s) => s.clone(),
                        Dependency::Detailed(d) => {
                            d.version.clone().unwrap_or_else(|| "*".to_string())
                        }
                    };
                    // Validate req shape now (actionable, even if unified later).
                    semver::VersionReq::parse(&req).map_err(|e| Error::InvalidReq {
                        dep: dep_name.clone(),
                        req: req.clone(),
                        detail: e.to_string(),
                    })?;
                    registry_reqs.entry(dep_name).or_default().push(req);
                }
            }
            expanded.insert(key.clone());
        }

        // Registry selection with unification (all reqs per name intersect).
        let mut pending: Vec<String> = registry_reqs.keys().cloned().collect();
        let mut done_registry: HashSet<String> = HashSet::new();
        while let Some(name) = pending.pop() {
            if !done_registry.insert(name.clone()) {
                continue;
            }
            let reqs = registry_reqs.get(&name).cloned().unwrap_or_default();
            let parsed: Vec<semver::VersionReq> = reqs
                .iter()
                .map(|r| {
                    semver::VersionReq::parse(r).map_err(|e| Error::InvalidReq {
                        dep: name.clone(),
                        req: r.clone(),
                        detail: e.to_string(),
                    })
                })
                .collect::<Result<_, _>>()?;
            let entries = self.index.versions(&name)?;
            // Partition yanked.
            let mut matching: Vec<(semver::Version, &IndexEntry)> = Vec::new();
            let mut yanked_matching: Vec<(semver::Version, &IndexEntry)> = Vec::new();
            for e in &entries {
                let v = semver::Version::parse(&e.version).map_err(|_| Error::InvalidVersion {
                    name: name.clone(),
                    version: e.version.clone(),
                })?;
                if !parsed.iter().all(|r| r.matches(&v)) {
                    continue;
                }
                if e.yanked {
                    yanked_matching.push((v, e));
                } else {
                    matching.push((v, e));
                }
            }
            if matching.is_empty() {
                // Pinned-then-yanked is an explicit error, not a silent skip.
                if let Some(existing) = self.existing {
                    if let Some(pinned) = existing.packages.iter().find(|p| p.name == name) {
                        if yanked_matching
                            .iter()
                            .any(|(v, _)| v.to_string() == pinned.version)
                        {
                            return Err(Error::YankedPinned {
                                name: name.clone(),
                                version: pinned.version.clone(),
                            });
                        }
                    }
                }
                if !yanked_matching.is_empty() {
                    // Only yanked versions match: refuse with update guidance.
                    let vs: Vec<String> =
                        yanked_matching.iter().map(|(v, _)| v.to_string()).collect();
                    return Err(Error::NoMatchingVersion {
                        name: name.clone(),
                        reqs: format!("{} (only yanked match: {})", reqs.join(", "), vs.join(", ")),
                    });
                }
                return Err(Error::NoMatchingVersion {
                    name: name.clone(),
                    reqs: reqs.join(", "),
                });
            }
            matching.sort_by(|a, b| a.0.cmp(&b.0));
            // Non-empty (checked above): the maximum satisfying version wins.
            let best_e: &IndexEntry = matching.last().map(|(_, e)| *e).unwrap_or(&entries[0]);
            versions.insert(name.clone(), best_e.version.clone());
            sources.insert(
                name.clone(),
                LockSource::Registry {
                    registry: "default".to_string(),
                    version: best_e.version.clone(),
                    checksum: best_e.checksum.clone(),
                },
            );
            checksums.insert(name.clone(), Some(best_e.checksum.clone()));
            // Transitive index deps (non-optional only in the M4 stub).
            // Re-queue only when a genuinely new requirement appears, so
            // index cycles (a -> b -> a) terminate instead of ping-ponging.
            let transitive: Vec<IndexDep> = best_e.deps.clone();
            for dep in &transitive {
                if dep.optional {
                    continue;
                }
                let reqs = registry_reqs.entry(dep.name.clone()).or_default();
                if reqs.contains(&dep.req) {
                    continue;
                }
                reqs.push(dep.req.clone());
                if done_registry.contains(&dep.name) {
                    done_registry.remove(&dep.name);
                }
                if !pending.contains(&dep.name) {
                    pending.push(dep.name.clone());
                }
            }
        }

        // Assemble the lockfile. The root itself is never locked (Cargo
        // convention): it was the queue seed but was never inserted into
        // `versions`, so every entry here is a dependency by construction.
        let mut packages: Vec<LockedPackage> = Vec::new();
        for (name, version) in &versions {
            let source = sources
                .get(name)
                .cloned()
                .ok_or_else(|| Error::DuplicatePackage {
                    name: name.clone(),
                    detail: "resolved version without a source".to_string(),
                    hint: "report a bug".to_string(),
                })?;
            let mut feats: Vec<String> = enabled_by
                .get(name)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();
            feats.sort();
            feats.dedup();
            packages.push(LockedPackage {
                name: name.clone(),
                version: version.clone(),
                source,
                features: feats,
                checksum: checksums.get(name).cloned().flatten(),
            });
        }
        packages.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.version.cmp(&b.version)));
        Ok(Lockfile {
            version: LOCK_VERSION,
            packages,
        })
    }
}

fn git_ref_of(
    tag: Option<&str>,
    branch: Option<&str>,
    rev: Option<&str>,
) -> Result<rivet_git::GitRef, Error> {
    rivet_git::GitRef::from_parts(tag, branch, rev).map_err(|_| Error::MissingManifest {
        where_: "git ref".to_string(),
        message: "`tag`, `branch` and `rev` are mutually exclusive".to_string(),
        hint: "specify exactly one; prefer `tag` for releases, `rev` for pins".to_string(),
    })
}

fn inline_manifest(
    name: &str,
    inline_name: String,
    version: Option<String>,
) -> Result<Manifest, Error> {
    if inline_name != name {
        return Err(Error::NameMismatch {
            dep: name.to_string(),
            actual: inline_name,
        });
    }
    let version = version.ok_or_else(|| Error::MissingManifest {
        where_: name.to_string(),
        message: "inline `package` block has no version".to_string(),
        hint: "add `version = \"0.1.0\"` to the inline package block".to_string(),
    })?;
    let text = format!("{{\"package\": {{\"name\": \"{name}\", \"version\": \"{version}\"}}}}");
    Ok(rivet_manifest::parse_str(&text)?)
}

// ---------------------------------------------------------------------------
// Lockfile I/O
// ---------------------------------------------------------------------------

/// Serialize a lockfile to deterministic bytes (pretty + trailing newline).
///
/// Packages are sorted by name, features sorted per package; field order is
/// the C2 example order (stable across runs — resolving twice yields
/// identical bytes).
pub fn lockfile_to_bytes(lock: &Lockfile) -> Result<Vec<u8>, Error> {
    let mut sorted = lock.clone();
    sorted
        .packages
        .sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.version.cmp(&b.version)));
    for p in &mut sorted.packages {
        p.features.sort();
        p.features.dedup();
    }
    let mut text = serde_json::to_string_pretty(&sorted).map_err(|e| Error::BadLock {
        path: "<lockfile>".to_string(),
        message: format!("could not serialize: {e}"),
    })?;
    text.push('\n');
    Ok(text.into_bytes())
}

/// Write `rivet.lock.json` (deterministic bytes).
pub fn write_lockfile(path: &Path, lock: &Lockfile) -> Result<(), Error> {
    let bytes = lockfile_to_bytes(lock)?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io("create lockfile dir", parent, e))?;
        }
    }
    std::fs::write(path, bytes).map_err(|e| Error::io("write lockfile", path, e))?;
    Ok(())
}

/// Read + validate `rivet.lock.json`.
pub fn read_lockfile(path: &Path) -> Result<Lockfile, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io("read lockfile", path, e))?;
    let lock: Lockfile = serde_json::from_str(&text).map_err(|e| Error::BadLock {
        path: path.display().to_string(),
        message: format!("{e}. Hint: delete the lockfile and re-resolve"),
    })?;
    if lock.version != LOCK_VERSION {
        return Err(Error::BadLockVersion {
            path: path.display().to_string(),
            found: lock.version,
        });
    }
    Ok(lock)
}

/// Compare an existing lockfile against a fresh resolution.
///
/// `Ok(())` when they match; `Err(LockedError)` spelling added/removed/
/// changed packages (the `--locked` error; frozen = offline resolve +
/// this check).
pub fn check_locked(existing: &Lockfile, fresh: &Lockfile) -> Result<(), LockedError> {
    let old: BTreeMap<&str, &LockedPackage> = existing
        .packages
        .iter()
        .map(|p| (p.name.as_str(), p))
        .collect();
    let new: BTreeMap<&str, &LockedPackage> = fresh
        .packages
        .iter()
        .map(|p| (p.name.as_str(), p))
        .collect();
    let mut lines: Vec<String> = Vec::new();
    for name in old.keys() {
        if !new.contains_key(*name) {
            lines.push(format!("- removed package `{name}`"));
        }
    }
    for name in new.keys() {
        if !old.contains_key(*name) {
            let p = new[*name];
            lines.push(format!("+ added package `{}` v{}", p.name, p.version));
        }
    }
    for name in old.keys().filter(|k| new.contains_key(**k)) {
        let a = old[*name];
        let b = new[*name];
        if a != b {
            let mut bits = Vec::new();
            if a.version != b.version {
                bits.push(format!("version {} -> {}", a.version, b.version));
            }
            if a.source != b.source {
                bits.push(format!("source {:?} -> {:?}", a.source, b.source));
            }
            if a.features != b.features {
                bits.push(format!("features {:?} -> {:?}", a.features, b.features));
            }
            if a.checksum != b.checksum {
                bits.push("checksum changed".to_string());
            }
            if bits.is_empty() {
                bits.push("changed".to_string());
            }
            lines.push(format!("~ package `{name}`: {}", bits.join("; ")));
        }
    }
    if lines.is_empty() {
        Ok(())
    } else {
        lines.sort();
        Err(LockedError {
            diff: lines.join("\n"),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_with(name: &str, version: &str, json_extra: &str) -> Manifest {
        let text = format!(
            "{{\"package\": {{\"name\": \"{name}\", \"version\": \"{version}\"}}{json_extra}}}"
        );
        let mut m: Manifest = rivet_manifest::parse_str(&text).expect("test manifest");
        m.dir = PathBuf::from(format!("/pkg/{name}"));
        m
    }

    fn root_manifest(deps_json: &str, features_json: &str) -> Manifest {
        let text = format!(
            "{{\"package\": {{\"name\": \"root\", \"version\": \"0.1.0\"}}, \
              \"dependencies\": {{{deps_json}}}, \"features\": {{{features_json}}}}}"
        );
        let mut m: Manifest = rivet_manifest::parse_str(&text).expect("root manifest");
        m.dir = PathBuf::from("/pkg/root");
        m
    }

    fn temp_index(entries: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        for (name, lines) in entries {
            std::fs::write(dir.path().join(name), lines).expect("write index");
        }
        dir
    }

    fn index_line(version: &str, checksum: &str, yanked: bool, deps: &str) -> String {
        format!(
            "{{\"version\":\"{version}\",\"checksum\":\"{checksum}\",\"yanked\":{yanked},\"deps\":[{deps}]}}"
        )
    }

    fn resolve_with(
        root: &Manifest,
        opts: &ResolveOpts,
        index_dir: &Path,
        provider: &TestProvider,
    ) -> Result<Lockfile, Error> {
        let index = RegistryIndex::new(index_dir.to_path_buf());
        Resolver::new(index, provider).resolve(root, opts)
    }

    #[test]
    fn max_compatible_selection() {
        let root = root_manifest("\"a\": \"1\"", "");
        let idx = temp_index(&[(
            "a",
            &[
                index_line("1.0.0", "sha256:00", false, ""),
                index_line("1.2.0", "sha256:12", false, ""),
                index_line("2.0.0", "sha256:20", false, ""),
            ]
            .join("\n"),
        )]);
        let p = TestProvider::new();
        let lock = resolve_with(&root, &ResolveOpts::default(), idx.path(), &p).expect("resolve");
        assert_eq!(lock.packages.len(), 1);
        assert_eq!(lock.packages[0].version, "1.2.0");
        assert_eq!(lock.packages[0].checksum.as_deref(), Some("sha256:12"));
    }

    #[test]
    fn caret_ranges_unify_across_parents() {
        // root needs a^1, path child needs a^1.2 -> unified pick satisfies both.
        let mut child = manifest_with("child", "0.2.0", ", \"dependencies\": {\"a\": \"^1.2\"}");
        child.dir = PathBuf::from("/pkg/child");
        let root = {
            let mut m = root_manifest("\"a\": \"^1.0\", \"child\": {\"path\": \"../child\"}", "");
            m.dir = PathBuf::from("/pkg/root");
            m
        };
        let idx = temp_index(&[(
            "a",
            &[
                index_line("1.1.0", "sha256:11", false, ""),
                index_line("1.5.0", "sha256:15", false, ""),
            ]
            .join("\n"),
        )]);
        let mut p = TestProvider::new();
        p.insert_path(Path::new("/pkg/child"), child);
        let lock = resolve_with(&root, &ResolveOpts::default(), idx.path(), &p).expect("resolve");
        let a = lock
            .packages
            .iter()
            .find(|x| x.name == "a")
            .expect("a locked");
        assert_eq!(a.version, "1.5.0");
    }

    #[test]
    fn conflicting_registry_reqs_error() {
        let mut child = manifest_with("child", "0.2.0", ", \"dependencies\": {\"a\": \"^2.0\"}");
        child.dir = PathBuf::from("/pkg/child");
        let root = {
            let mut m = root_manifest("\"a\": \"^1.0\", \"child\": {\"path\": \"../child\"}", "");
            m.dir = PathBuf::from("/pkg/root");
            m
        };
        let idx = temp_index(&[(
            "a",
            &[
                index_line("1.9.0", "sha256:19", false, ""),
                index_line("2.1.0", "sha256:21", false, ""),
            ]
            .join("\n"),
        )]);
        let mut p = TestProvider::new();
        p.insert_path(Path::new("/pkg/child"), child);
        let err =
            resolve_with(&root, &ResolveOpts::default(), idx.path(), &p).expect_err("conflict");
        assert!(matches!(err, Error::NoMatchingVersion { .. }), "got: {err}");
    }

    #[test]
    fn optional_off_by_default_on_with_feature() {
        let root_off = root_manifest(
            "\"opt\": {\"version\": \"1\", \"optional\": true}",
            "\"use-opt\": [\"dep:opt\"]",
        );
        let idx = temp_index(&[("opt", &index_line("1.0.0", "sha256:o", false, ""))]);
        let p = TestProvider::new();
        let lock =
            resolve_with(&root_off, &ResolveOpts::default(), idx.path(), &p).expect("resolve");
        assert!(lock.packages.is_empty(), "optional must stay off");
        let opts = ResolveOpts {
            features: vec!["use-opt".to_string()],
            ..Default::default()
        };
        let lock2 = resolve_with(&root_off, &opts, idx.path(), &p).expect("resolve");
        assert_eq!(lock2.packages.len(), 1);
        assert_eq!(lock2.packages[0].name, "opt");
    }

    #[test]
    fn unknown_feature_errors() {
        let root = root_manifest("\"a\": \"1\"", "\"x\": []");
        let idx = temp_index(&[]);
        let p = TestProvider::new();
        let opts = ResolveOpts {
            features: vec!["nope".to_string()],
            ..Default::default()
        };
        let err = resolve_with(&root, &opts, idx.path(), &p).expect_err("unknown");
        assert!(matches!(err, Error::UnknownFeature { .. }), "got: {err}");
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn dep_entry_on_nonoptional_errors() {
        let root = root_manifest("\"a\": \"1\"", "\"x\": [\"dep:a\"]");
        let idx = temp_index(&[("a", &index_line("1.0.0", "sha256:a", false, ""))]);
        let p = TestProvider::new();
        let err =
            resolve_with(&root, &ResolveOpts::default(), idx.path(), &p).expect_err("bad dep:");
        assert!(matches!(err, Error::BadFeatureDep { .. }), "got: {err}");
    }

    #[test]
    fn dep_entry_on_missing_dep_errors() {
        let root = root_manifest("", "\"x\": [\"dep:ghost\"]");
        let idx = temp_index(&[]);
        let p = TestProvider::new();
        let err = resolve_with(&root, &ResolveOpts::default(), idx.path(), &p).expect_err("ghost");
        assert!(matches!(err, Error::BadFeatureDep { .. }), "got: {err}");
    }

    #[test]
    fn transitive_closure_and_determinism() {
        // a 1.5 depends on b ^1 -> b resolves transitively; bytes identical twice.
        let root = root_manifest("\"a\": \"^1\"", "");
        let idx = temp_index(&[
            (
                "a",
                &[index_line(
                    "1.5.0",
                    "sha256:a15",
                    false,
                    "{\"name\":\"b\",\"req\":\"^1\",\"optional\":false}",
                )]
                .join("\n"),
            ),
            (
                "b",
                &[index_line("1.1.0", "sha256:b11", false, "")].join("\n"),
            ),
        ]);
        let p = TestProvider::new();
        let lock1 = resolve_with(&root, &ResolveOpts::default(), idx.path(), &p).expect("r1");
        assert_eq!(lock1.packages.len(), 2);
        let b1 = lockfile_to_bytes(&lock1).expect("bytes");
        let lock2 = resolve_with(&root, &ResolveOpts::default(), idx.path(), &p).expect("r2");
        let b2 = lockfile_to_bytes(&lock2).expect("bytes");
        assert_eq!(b1, b2, "resolve twice must be byte-identical");
        assert!(b1.ends_with(b"\n"), "trailing newline required");
        // Sorted by name.
        let names: Vec<&str> = lock1.packages.iter().map(|x| x.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn locked_diff_spells_changes() {
        let old = Lockfile {
            version: 1,
            packages: vec![
                LockedPackage {
                    name: "a".to_string(),
                    version: "1.0.0".to_string(),
                    source: LockSource::Registry {
                        registry: "default".to_string(),
                        version: "1.0.0".to_string(),
                        checksum: "sha256:old".to_string(),
                    },
                    features: vec![],
                    checksum: Some("sha256:old".to_string()),
                },
                LockedPackage {
                    name: "gone".to_string(),
                    version: "0.1.0".to_string(),
                    source: LockSource::Path {
                        path: "../gone".to_string(),
                    },
                    features: vec![],
                    checksum: None,
                },
            ],
        };
        let new = Lockfile {
            version: 1,
            packages: vec![
                LockedPackage {
                    name: "a".to_string(),
                    version: "1.2.0".to_string(),
                    source: LockSource::Registry {
                        registry: "default".to_string(),
                        version: "1.2.0".to_string(),
                        checksum: "sha256:new".to_string(),
                    },
                    features: vec!["f".to_string()],
                    checksum: Some("sha256:new".to_string()),
                },
                LockedPackage {
                    name: "fresh".to_string(),
                    version: "0.1.0".to_string(),
                    source: LockSource::Path {
                        path: "../fresh".to_string(),
                    },
                    features: vec![],
                    checksum: None,
                },
            ],
        };
        let err = check_locked(&old, &new).expect_err("must differ");
        assert!(
            err.diff.contains("removed package `gone`"),
            "got: {}",
            err.diff
        );
        assert!(
            err.diff.contains("added package `fresh`"),
            "got: {}",
            err.diff
        );
        assert!(err.diff.contains("`a`"), "got: {}", err.diff);
        assert!(check_locked(&new, &new.clone()).is_ok());
    }

    #[test]
    fn yank_skipped_then_pinned_errors() {
        let root = root_manifest("\"a\": \"^1\"", "");
        let idx = temp_index(&[(
            "a",
            &[
                index_line("1.5.0", "sha256:good", false, ""),
                index_line("1.9.0", "sha256:bad", true, ""),
            ]
            .join("\n"),
        )]);
        let p = TestProvider::new();
        // Fresh resolve skips the yanked 1.9.
        let lock = resolve_with(&root, &ResolveOpts::default(), idx.path(), &p).expect("resolve");
        assert_eq!(lock.packages[0].version, "1.5.0");
        // An existing lock pinning the yanked version errors with guidance.
        let pinned = Lockfile {
            version: 1,
            packages: vec![LockedPackage {
                name: "a".to_string(),
                version: "1.9.0".to_string(),
                source: LockSource::Registry {
                    registry: "default".to_string(),
                    version: "1.9.0".to_string(),
                    checksum: "sha256:bad".to_string(),
                },
                features: vec![],
                checksum: Some("sha256:bad".to_string()),
            }],
        };
        let index = RegistryIndex::new(idx.path().to_path_buf());
        let r = Resolver::with_existing(index, &p, &pinned);
        // Force the yanked pick by requiring exactly it.
        let root_exact = root_manifest("\"a\": \"=1.9.0\"", "");
        let err = r
            .resolve(&root_exact, &ResolveOpts::default())
            .expect_err("yanked pin must error");
        assert!(matches!(err, Error::YankedPinned { .. }), "got: {err}");
        assert!(err.to_string().contains("rivet update"));
    }

    #[test]
    fn offline_refuses_git() {
        let root = root_manifest(
            "\"g\": {\"git\": \"https://example.com/g.git\", \"tag\": \"v1\"}",
            "",
        );
        let idx = temp_index(&[]);
        let mut p = TestProvider::new();
        p.offline = true;
        let opts = ResolveOpts {
            offline: true,
            ..Default::default()
        };
        let err = resolve_with(&root, &opts, idx.path(), &p).expect_err("offline");
        assert!(matches!(err, Error::Offline { .. }), "got: {err}");
    }

    #[test]
    fn path_version_mismatch_errors() {
        let mut child = manifest_with("child", "2.0.0", "");
        child.dir = PathBuf::from("/pkg/child");
        let root = {
            let mut m = root_manifest(
                "\"child\": {\"path\": \"../child\", \"version\": \"^1.0\"}",
                "",
            );
            m.dir = PathBuf::from("/pkg/root");
            m
        };
        let idx = temp_index(&[]);
        let mut p = TestProvider::new();
        p.insert_path(Path::new("/pkg/child"), child);
        let err =
            resolve_with(&root, &ResolveOpts::default(), idx.path(), &p).expect_err("mismatch");
        assert!(matches!(err, Error::VersionMismatch { .. }), "got: {err}");
    }

    #[test]
    fn same_url_divergent_refs_error() {
        let root = root_manifest(
            "\"a\": {\"git\": \"https://example.com/r.git\", \"tag\": \"v1\"}, \
             \"b\": {\"git\": \"https://example.com/r.git\", \"branch\": \"main\"}",
            "",
        );
        let idx = temp_index(&[]);
        let mut p = TestProvider::new();
        let sha1 = "a".repeat(40);
        let sha2 = "b".repeat(40);
        p.insert_git_ref("https://example.com/r.git", "tag", "v1", &sha1);
        p.insert_git_ref("https://example.com/r.git", "branch", "main", &sha2);
        p.git_trees.insert(
            ("https://example.com/r.git".to_string(), sha1.clone()),
            "t1".repeat(10),
        );
        p.git_trees.insert(
            ("https://example.com/r.git".to_string(), sha2.clone()),
            "t2".repeat(10),
        );
        let ma = manifest_with("a", "0.1.0", "");
        let mb = manifest_with("b", "0.1.0", "");
        p.git_manifests.insert(
            (
                "https://example.com/r.git".to_string(),
                sha1.clone(),
                String::new(),
            ),
            ma,
        );
        p.git_manifests.insert(
            (
                "https://example.com/r.git".to_string(),
                sha2.clone(),
                String::new(),
            ),
            mb,
        );
        let err =
            resolve_with(&root, &ResolveOpts::default(), idx.path(), &p).expect_err("diverge");
        assert!(matches!(err, Error::SameUrlDifferentRefs(..)), "got: {err}");
        assert!(err.to_string().contains("unify"));
    }

    #[test]
    fn same_url_same_sha_unifies() {
        // Same URL at two refs resolving to the SAME sha unifies (one source).
        // `b` uses subdir "b" so the TestProvider (keyed by location) can
        // serve a distinct manifest for each dep key.
        let root = root_manifest(
            "\"a\": {\"git\": \"https://example.com/r.git\", \"tag\": \"v1\"}, \
             \"b\": {\"git\": \"https://example.com/r.git\", \"rev\": \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\", \"subdir\": \"b\"}",
            "",
        );
        let idx = temp_index(&[]);
        let mut p = TestProvider::new();
        let sha = "A".repeat(40);
        p.insert_git_ref("https://example.com/r.git", "tag", "v1", &sha);
        p.insert_git_ref("https://example.com/r.git", "rev", &"A".repeat(40), &sha);
        p.git_trees.insert(
            ("https://example.com/r.git".to_string(), sha.clone()),
            "t".repeat(40),
        );
        p.git_manifests.insert(
            (
                "https://example.com/r.git".to_string(),
                sha.clone(),
                String::new(),
            ),
            manifest_with("a", "0.1.0", ""),
        );
        p.git_manifests.insert(
            (
                "https://example.com/r.git".to_string(),
                sha.clone(),
                "b".to_string(),
            ),
            manifest_with("b", "0.1.0", ""),
        );
        let lock = resolve_with(&root, &ResolveOpts::default(), idx.path(), &p).expect("unify");
        assert_eq!(lock.packages.len(), 2);
    }

    #[test]
    fn git_ref_roundtrip_kind_value() {
        let r = rivet_git::GitRef::Tag("v1".to_string());
        let locked = GitLockRef::from_git_ref(&r);
        assert_eq!(locked.kind, "tag");
        assert_eq!(locked.value, "v1");
        // Single-key read form.
        let back: GitLockRef = serde_json::from_str("{\"tag\":\"v1\"}").expect("single-key");
        assert_eq!(back, locked);
        let canon: GitLockRef =
            serde_json::from_str("{\"kind\":\"tag\",\"value\":\"v1\"}").expect("canon");
        assert_eq!(canon, locked);
    }

    #[test]
    fn lockfile_roundtrip_byte_identical() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("rivet.lock.json");
        let lock = Lockfile {
            version: 1,
            packages: vec![
                LockedPackage {
                    name: "b".to_string(),
                    version: "1.0.0".to_string(),
                    source: LockSource::Path {
                        path: "../b".to_string(),
                    },
                    features: vec!["z".to_string(), "a".to_string()],
                    checksum: None,
                },
                LockedPackage {
                    name: "a".to_string(),
                    version: "1.2.0".to_string(),
                    source: LockSource::Registry {
                        registry: "default".to_string(),
                        version: "1.2.0".to_string(),
                        checksum: "sha256:x".to_string(),
                    },
                    features: vec![],
                    checksum: Some("sha256:x".to_string()),
                },
            ],
        };
        write_lockfile(&path, &lock).expect("write");
        let bytes1 = std::fs::read(&path).expect("read bytes");
        let back = read_lockfile(&path).expect("read");
        write_lockfile(&path, &back).expect("rewrite");
        let bytes2 = std::fs::read(&path).expect("read bytes");
        assert_eq!(bytes1, bytes2, "re-write must be byte-identical");
        // Sorted by name + features sorted.
        assert_eq!(back.packages[0].name, "a");
        assert_eq!(
            back.packages[1].features,
            vec!["a".to_string(), "z".to_string()]
        );
        assert!(bytes1.ends_with(b"\n"));
    }

    #[test]
    fn real_provider_git_end_to_end() {
        if rivet_git::require_git().is_err() {
            eprintln!("SKIP: no git");
            return;
        }
        // Build a tiny file:// repo with a rivet.json at HEAD.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bare = dir.path().join("o.git");
        let work = dir.path().join("w");
        let cache = rivet_git::GitCache::from_git_root(dir.path().join("g"));
        cache
            .run_git(&["init", "--bare", "--", &bare.to_string_lossy()], None)
            .expect("bare");
        std::fs::create_dir_all(&work).expect("dir");
        cache
            .run_git(&["init", "--", &work.to_string_lossy()], None)
            .expect("init");
        std::fs::write(
            work.join("rivet.json"),
            "{\"package\": {\"name\": \"inner\", \"version\": \"0.3.0\"}}",
        )
        .expect("write");
        cache.run_git(&["add", "."], Some(&work)).expect("add");
        cache
            .run_git(
                &[
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "commit",
                    "-m",
                    "i",
                ],
                Some(&work),
            )
            .expect("commit");
        cache.run_git(&["tag", "v1"], Some(&work)).expect("tag");
        let br = cache
            .run_git(&["branch", "--show-current"], Some(&work))
            .unwrap_or_default();
        let br = if br.is_empty() { "master" } else { br.as_str() };
        cache
            .run_git(
                &["remote", "add", "origin", &bare.to_string_lossy()],
                Some(&work),
            )
            .expect("remote");
        cache
            .run_git(&["push", "origin", br, "--tags"], Some(&work))
            .expect("push");
        // Same `file://` spelling as the rivet-git tests (Windows-safe).
        let raw = bare.display().to_string().replace('\\', "/");
        let file_url = if raw.starts_with('/') {
            format!("file://{raw}")
        } else {
            format!("file:///{raw}")
        };
        let root = root_manifest(
            &format!("\"inner\": {{\"git\": \"{file_url}\", \"tag\": \"v1\"}}"),
            "",
        );
        let idx = temp_index(&[]);
        let provider = RealProvider::new(rivet_git::GitCache::from_git_root(
            dir.path().join("cache-git"),
        ));
        let index = RegistryIndex::new(idx.path().to_path_buf());
        let lock = Resolver::new(index, &provider)
            .resolve(&root, &ResolveOpts::default())
            .expect("real resolve");
        assert_eq!(lock.packages.len(), 1);
        assert_eq!(lock.packages[0].version, "0.3.0");
        match &lock.packages[0].source {
            LockSource::Git { sha, tree, .. } => {
                assert_eq!(sha.len(), 40);
                assert_eq!(tree.len(), 40);
            }
            other => panic!("expected git source, got {other:?}"),
        }
    }
}
