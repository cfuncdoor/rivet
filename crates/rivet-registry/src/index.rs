//! Registry index reads: local directory (primary, M4-compatible) and sparse
//! HTTPS (`<registry-url>/index/<name>`), with yank filtering and semver
//! selection using the same `semver`-crate semantics as `rivet-resolver`
//! (caret requirements, max satisfying version wins, pre-releases only match
//! an explicitly pre-release requirement — never forked here).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// One index line for a package version.
///
/// JSON-lines shape shared with `rivet-resolver::IndexEntry` (same field
/// names; unknown fields ignored on read, so either side can extend).
/// Checksums are `sha256:…` verbatim as stored in the lockfile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// Version string (`major.minor.patch[-pre][+build]`).
    pub version: String,
    /// Checksum (`sha256:…`); empty when the line omits it (fails later at
    /// verify time with an actionable error, never silently).
    #[serde(default)]
    pub checksum: String,
    /// Yanked upstream: skipped by fresh selection. A lock pinning a yanked
    /// version is the resolver's `YankedPinned` error, not a silent skip.
    #[serde(default)]
    pub yanked: bool,
    /// Transitive deps.
    #[serde(default)]
    pub deps: Vec<IndexDep>,
    /// One-line package description (used by [`crate::search`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One transitive index dep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexDep {
    /// Package name.
    pub name: String,
    /// Semver requirement (caret semantics).
    #[serde(default = "default_index_req")]
    pub req: String,
    /// Optional (skipped by stub resolution; wired with real feature
    /// tables by the coordinator).
    #[serde(default)]
    pub optional: bool,
}

fn default_index_req() -> String {
    "*".to_string()
}

/// `true` when `name` is a safe package/index file name (`[A-Za-z0-9_-]+`,
/// same rule as manifest package names). Rejects `/`, `..`, empty, etc. so
/// index lookups can never escape the index directory.
pub fn is_valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn check_name(name: &str) -> Result<()> {
    if is_valid_package_name(name) {
        Ok(())
    } else {
        Err(Error::BadIndex {
            message: format!("invalid package name `{name}`"),
            hint: "package names use only [a-zA-Z0-9_-]".to_string(),
        })
    }
}

/// Index file URL under a sparse registry base: `<base>/index/<name>`.
pub fn index_url_for(registry_url: &str, name: &str) -> Result<String, Error> {
    check_name(name)?;
    let base = super::normalize_registry_url(registry_url);
    if base.is_empty() {
        return Err(Error::BadUrl {
            url: registry_url.to_string(),
            reason: "registry URL is empty".to_string(),
            hint: "pass a registry base URL like `https://example.com/rivet`".to_string(),
        });
    }
    Ok(format!("{base}/index/{name}"))
}

/// Where index bytes come from.
#[derive(Debug, Clone)]
enum IndexSource {
    /// Read `<dir>/<name>` from disk (primary; offline-friendly).
    Local(PathBuf),
    /// Fetch `<base>/index/<name>` over HTTPS, or read `<root>/index/<name>`
    /// from disk when the base is a `file://` URL (hermetic tests).
    Sparse(String),
}

/// M4-compatible JSON-lines registry index reader.
#[derive(Debug, Clone)]
pub struct RegistryIndex {
    source: IndexSource,
}

impl RegistryIndex {
    /// Point at a local index directory: reads `<dir>/<name>`.
    pub fn local(dir: PathBuf) -> Self {
        Self {
            source: IndexSource::Local(dir),
        }
    }

    /// Point at a sparse registry base URL (`https://…` or `file://…`).
    /// Rejects empty URLs and non-`https`/`file` schemes eagerly so a typo
    /// surfaces here instead of mid-build.
    pub fn sparse(base_url: &str) -> Result<Self, Error> {
        let base = super::normalize_registry_url(base_url);
        if base.is_empty() {
            return Err(Error::BadUrl {
                url: base_url.to_string(),
                reason: "registry URL is empty".to_string(),
                hint: "pass a registry base URL like `https://example.com/rivet`".to_string(),
            });
        }
        if base == "default" {
            return Err(Error::BadUrl {
                url: base_url.to_string(),
                reason: "lock entries record the sentinel `\"default\"`, not a concrete registry"
                    .to_string(),
                hint: "resolve `default` to a concrete registry URL (CLI config) before reading the index"
                    .to_string(),
            });
        }
        if !(base.starts_with("https://") || base.starts_with("file://")) {
            return Err(Error::BadUrl {
                url: base_url.to_string(),
                reason: "only `https://` and `file://` registry URLs are supported".to_string(),
                hint: "use an https:// registry URL, or file:// for local/test registries"
                    .to_string(),
            });
        }
        Ok(Self {
            source: IndexSource::Sparse(base),
        })
    }

    /// Raw index text for `name` (one JSON object per line).
    fn read_text(&self, name: &str) -> Result<String, Error> {
        check_name(name)?;
        match &self.source {
            IndexSource::Local(dir) => {
                let file = dir.join(name);
                std::fs::read_to_string(&file).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        Error::BadIndex {
                            message: format!("package `{name}` not in the local index copy"),
                            hint: "check the package name, or refresh the index copy".to_string(),
                        }
                    } else {
                        Error::io("read index file", &file, e)
                    }
                })
            }
            IndexSource::Sparse(base) => {
                let url = index_url_for(base, name)?;
                crate::download_bytes(&url, false)
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            }
        }
    }

    /// Parse index text (shared by both sources; fail fast on corrupt lines
    /// — a corrupt index copy must never resolve silently).
    fn parse_text(name: &str, text: &str) -> Result<Vec<IndexEntry>, Error> {
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
            // Fail fast on versions nothing downstream could order.
            if semver::Version::parse(&entry.version).is_err() {
                return Err(Error::BadIndex {
                    message: format!(
                        "package `{name}` line {} has invalid version `{}`",
                        i + 1,
                        entry.version
                    ),
                    hint: "versions must be `major.minor.patch`, e.g. `0.1.0`".to_string(),
                });
            }
            out.push(entry);
        }
        Ok(out)
    }

    /// All entries for `name`, in file order (yanked included; each entry
    /// carries its own `yanked` flag).
    pub fn versions(&self, name: &str) -> Result<Vec<IndexEntry>, Error> {
        let text = self.read_text(name)?;
        Self::parse_text(name, &text)
    }

    /// Non-yanked entries for `name`, in file order.
    pub fn available(&self, name: &str) -> Result<Vec<IndexEntry>, Error> {
        Ok(self
            .versions(name)?
            .into_iter()
            .filter(|e| !e.yanked)
            .collect())
    }

    /// The exact entry for `(name, version)` — yanked or not (the lockfile
    /// is the source of truth at materialize time; yank policy is enforced
    /// at selection/resolve time instead).
    pub fn lookup(&self, name: &str, version: &str) -> Result<IndexEntry, Error> {
        let entries = self.versions(name)?;
        entries
            .into_iter()
            .find(|e| e.version == version)
            .ok_or_else(|| Error::BadIndex {
                message: format!("package `{name}` has no version `{version}` in the index"),
                hint: "run `rivet update -p {name}` to refresh, or check the version spelling"
                    .to_string(),
            })
    }

    /// Max non-yanked version of `name` satisfying `req` (caret SemVer via
    /// the `semver` crate — identical semantics to `rivet-resolver`).
    pub fn select_best(&self, name: &str, req: &str) -> Result<IndexEntry, Error> {
        let entries = self.versions(name)?;
        select_best_version(&entries, name, req)
    }
}

/// Max non-yanked entry satisfying `req` (pure over already-read entries).
///
/// Yanked versions are skipped; when only yanked versions match, the error
/// names them (mirrors `rivet-resolver`'s yanked-only message). Pre-release
/// matching follows the `semver` crate (a pre-release only matches a
/// requirement that opts into the same `major.minor.patch` pre-release).
pub fn select_best_version(
    entries: &[IndexEntry],
    name: &str,
    req: &str,
) -> Result<IndexEntry, Error> {
    let parsed = semver::VersionReq::parse(req).map_err(|e| Error::InvalidReq {
        name: name.to_string(),
        req: req.to_string(),
        detail: e.to_string(),
    })?;
    let mut matching: Vec<(semver::Version, &IndexEntry)> = Vec::new();
    let mut yanked_matching: Vec<String> = Vec::new();
    for entry in entries {
        let v = semver::Version::parse(&entry.version).map_err(|e| Error::InvalidVersion {
            name: name.to_string(),
            version: entry.version.clone(),
            detail: e.to_string(),
        })?;
        if !parsed.matches(&v) {
            continue;
        }
        if entry.yanked {
            yanked_matching.push(entry.version.clone());
        } else {
            matching.push((v, entry));
        }
    }
    matching.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some((_, best)) = matching.last() {
        return Ok((*best).clone());
    }
    if yanked_matching.is_empty() {
        Err(Error::NoMatchingVersion {
            name: name.to_string(),
            reqs: req.to_string(),
        })
    } else {
        Err(Error::NoMatchingVersion {
            name: name.to_string(),
            reqs: format!("{req} (only yanked match: {})", yanked_matching.join(", ")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn fixture_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        for (name, content) in files {
            let mut f = std::fs::File::create(dir.path().join(name)).expect("create");
            f.write_all(content.as_bytes()).expect("write");
        }
        dir
    }

    fn line(version: &str, checksum: &str, yanked: bool, extra: &str) -> String {
        format!(
            "{{\"version\":\"{version}\",\"checksum\":\"{checksum}\",\"yanked\":{yanked}{extra}}}"
        )
    }

    #[test]
    fn local_parse_and_yank_filtering() {
        let dir = fixture_dir(&[(
            "foo",
            &[
                line("1.0.0", "sha256:aa", false, ""),
                line("1.2.0", "sha256:bb", false, ",\"deps\":[]"),
                line("2.0.0", "sha256:cc", true, ""),
            ]
            .join("\n"),
        )]);
        let idx = RegistryIndex::local(dir.path().to_path_buf());
        let all = idx.versions("foo").expect("versions");
        assert_eq!(all.len(), 3);
        assert!(all[2].yanked);
        let avail = idx.available("foo").expect("available");
        assert_eq!(avail.len(), 2);
        assert!(avail.iter().all(|e| !e.yanked));
    }

    #[test]
    fn unknown_fields_ignored_and_description_read() {
        let dir = fixture_dir(&[(
            "foo",
            &line(
                "1.0.0",
                "sha256:aa",
                false,
                ",\"description\":\"Fast vectors\",\"future\":123",
            ),
        )]);
        let idx = RegistryIndex::local(dir.path().to_path_buf());
        let all = idx.versions("foo").expect("versions");
        assert_eq!(all[0].description.as_deref(), Some("Fast vectors"));
    }

    #[test]
    fn missing_package_errors_actionably() {
        let dir = fixture_dir(&[]);
        let idx = RegistryIndex::local(dir.path().to_path_buf());
        let err = idx.versions("ghost").expect_err("missing");
        assert!(matches!(err, Error::BadIndex { .. }), "got: {err}");
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn corrupt_line_errors() {
        let dir = fixture_dir(&[("foo", "{\"version\":")]);
        let idx = RegistryIndex::local(dir.path().to_path_buf());
        let err = idx.versions("foo").expect_err("corrupt");
        assert!(matches!(err, Error::BadIndex { .. }), "got: {err}");
        assert!(err.to_string().contains("line 1"));
    }

    #[test]
    fn bad_version_in_index_errors() {
        let dir = fixture_dir(&[("foo", &line("banana", "sha256:aa", false, ""))]);
        let idx = RegistryIndex::local(dir.path().to_path_buf());
        let err = idx.versions("foo").expect_err("bad version");
        assert!(matches!(err, Error::BadIndex { .. }), "got: {err}");
    }

    #[test]
    fn traversal_name_rejected() {
        let dir = fixture_dir(&[]);
        let idx = RegistryIndex::local(dir.path().to_path_buf());
        for bad in ["../evil", "a/b", "", "a b", "a.json"] {
            // "a.json" contains '.' which is not in the allowed set.
            assert!(idx.versions(bad).is_err(), "name `{bad}` must be rejected");
        }
        assert!(!is_valid_package_name("../evil"));
        assert!(is_valid_package_name("my-pkg_2"));
    }

    #[test]
    fn select_best_picks_max_compatible() {
        let entries = vec![
            entry("1.0.0", false),
            entry("1.2.0", false),
            entry("2.0.0", false),
        ];
        let best = select_best_version(&entries, "a", "^1").expect("best");
        assert_eq!(best.version, "1.2.0");
    }

    #[test]
    fn select_best_skips_yanked() {
        let entries = vec![entry("1.5.0", false), entry("1.9.0", true)];
        let best = select_best_version(&entries, "a", "^1").expect("best");
        assert_eq!(best.version, "1.5.0");
    }

    #[test]
    fn select_best_yanked_only_names_them() {
        let entries = vec![entry("1.9.0", true)];
        let err = select_best_version(&entries, "a", "^1").expect_err("yanked-only");
        assert!(matches!(err, Error::NoMatchingVersion { .. }), "got: {err}");
        assert!(err.to_string().contains("yanked"), "got: {err}");
    }

    #[test]
    fn select_best_no_match() {
        let entries = vec![entry("1.0.0", false)];
        let err = select_best_version(&entries, "a", "^2").expect_err("no match");
        assert!(matches!(err, Error::NoMatchingVersion { .. }), "got: {err}");
    }

    #[test]
    fn select_best_prerelease_rules() {
        // Stable requirement never matches a pre-release (semver-crate /
        // Cargo semantics shared with rivet-resolver).
        let entries = vec![entry("1.0.0-alpha", false), entry("0.9.0", false)];
        assert!(select_best_version(&entries, "a", "^1").is_err());
        // Explicit pre-release requirement matches the pre-release.
        let best = select_best_version(&entries, "a", "=1.0.0-alpha").expect("pre");
        assert_eq!(best.version, "1.0.0-alpha");
        // A stable release still wins over an older pre-release.
        let entries2 = vec![entry("1.0.0-alpha", false), entry("1.0.0", false)];
        let best2 = select_best_version(&entries2, "a", "^1").expect("stable");
        assert_eq!(best2.version, "1.0.0");
    }

    #[test]
    fn select_best_invalid_req() {
        let entries = vec![entry("1.0.0", false)];
        let err = select_best_version(&entries, "a", "not a req!!").expect_err("bad req");
        assert!(matches!(err, Error::InvalidReq { .. }), "got: {err}");
    }

    #[test]
    fn lookup_exact_finds_yanked_too() {
        let dir = fixture_dir(&[("foo", &line("1.9.0", "sha256:cc", true, ""))]);
        let idx = RegistryIndex::local(dir.path().to_path_buf());
        let e = idx.lookup("foo", "1.9.0").expect("lookup");
        assert!(e.yanked);
        assert!(matches!(
            idx.lookup("foo", "9.9.9"),
            Err(Error::BadIndex { .. })
        ));
    }

    #[test]
    fn sparse_rejects_schemes_and_default_sentinel() {
        assert!(RegistryIndex::sparse("https://example.com/r").is_ok());
        assert!(RegistryIndex::sparse("file:///tmp/r").is_ok());
        assert!(matches!(
            RegistryIndex::sparse("http://example.com/r"),
            Err(Error::BadUrl { .. })
        ));
        assert!(matches!(
            RegistryIndex::sparse("ftp://example.com/r"),
            Err(Error::BadUrl { .. })
        ));
        assert!(matches!(
            RegistryIndex::sparse("default"),
            Err(Error::BadUrl { .. })
        ));
        assert!(matches!(
            RegistryIndex::sparse(""),
            Err(Error::BadUrl { .. })
        ));
    }

    #[test]
    fn sparse_file_base_reads_index_layout() {
        // file:// registry root: <root>/index/<name>.
        let root = tempfile::TempDir::new().expect("tempdir");
        let index_dir = root.path().join("index");
        std::fs::create_dir_all(&index_dir).expect("mkdir");
        std::fs::write(index_dir.join("foo"), line("1.0.0", "sha256:aa", false, ""))
            .expect("write");
        let raw = root.path().display().to_string().replace('\\', "/");
        let base = if raw.starts_with('/') {
            format!("file://{raw}")
        } else {
            format!("file:///{raw}")
        };
        let idx = RegistryIndex::sparse(&base).expect("sparse");
        let all = idx.versions("foo").expect("versions");
        assert_eq!(all.len(), 1);
        assert_eq!(
            index_url_for(&base, "foo").expect("url"),
            format!("{base}/index/foo")
        );
    }

    fn entry(version: &str, yanked: bool) -> IndexEntry {
        IndexEntry {
            version: version.to_string(),
            checksum: "sha256:00".to_string(),
            yanked,
            deps: Vec::new(),
            description: None,
        }
    }
}
