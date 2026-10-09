//! Registry materialize API for the CLI integrator: locked
//! `(name, version, checksum)` + registry URL → verified sources in the
//! cache, with include dirs for the build graph.
//!
//! Flow: resolve the cache dir → reuse when the extract marker matches →
//! else download (offline = error unless cached) → SHA-256 verify (always,
//! mismatch = hard error) → safe extract → marker → [`MaterializedSource`].
//!
//! Going from a lock entry to built sources (integrator notes):
//! * `LockSource::Registry { registry, version, checksum }` maps to
//!   `materialize(name, version, checksum, &registry_url, cache_root, opts)`
//!   where `registry_url` is the lock's `registry` string, except the
//!   sentinel `"default"` which the CLI must first resolve to a concrete
//!   `https://…` URL (config); passing `"default"` here is an error.
//! * `cache_root` is `<home>/.rivet` (same root the git subsystem uses).
//! * `MaterializedSource.source_dir` is the package root (contains
//!   `rivet.json` for rivet packages); `include_dirs` (empty or
//!   `[source_dir/include]`) joins the dependent's header search path;
//!   `lib_hint` is always `None` in M5 — lib roots stay graph-side.
//! * Locked builds must redirect output like git deps (never write
//!   `target/` into the immutable cache tree).

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Marker file written into every successfully extracted package dir,
/// holding the verified `sha256:…` checksum (reuse = marker matches).
pub const EXTRACT_MARKER: &str = ".rivet-extract-ok";

/// Source-URL record written next to each package dir (debugging: which
/// registry URL a cached copy came from).
const SOURCE_URL_FILE: &str = ".rivet-source-url";

/// Options for [`materialize`].
#[derive(Debug, Clone, Default)]
pub struct MaterializeOpts {
    /// Cache only: reuse without network; a cache miss is an actionable
    /// error (local `file://` registries still read — they are local disk,
    /// not network).
    pub offline: bool,
    /// Print progress (`Fetching …`, `Verified …`, `Reusing …`) to stderr
    /// (the CLI wires this to `-v`).
    pub verbose: bool,
}

/// Verified, extracted registry sources ready for the build graph.
#[derive(Debug, Clone)]
pub struct MaterializedSource {
    /// Package root in the cache (see the source-root rule in the crate
    /// docs: single top-level dir unwrapped, else the package dir itself).
    pub source_dir: PathBuf,
    /// `[source_dir/include]` when that dir exists, else empty.
    pub include_dirs: Vec<PathBuf>,
    /// Reserved for future lib-root hints; always `None` in M5 (lib
    /// discovery stays graph-side).
    pub lib_hint: Option<PathBuf>,
}

/// Cache dir for one registry under `cache_root`:
/// `<cache_root>/registry/<registry-hash>`.
pub fn registry_cache_dir(cache_root: &Path, registry_url: &str) -> PathBuf {
    cache_root
        .join("registry")
        .join(crate::registry_hash(registry_url))
}

/// Tarball URL for `(name, version)` under a registry base:
/// `<base>/tarballs/<name>-<version>.tar.gz`.
pub fn tarball_url_for(registry_url: &str, name: &str, version: &str) -> Result<String, Error> {
    if !crate::is_valid_package_name(name) {
        return Err(Error::BadUrl {
            url: registry_url.to_string(),
            reason: format!("invalid package name `{name}`"),
            hint: "package names use only [a-zA-Z0-9_-]".to_string(),
        });
    }
    semver::Version::parse(version).map_err(|e| Error::InvalidVersion {
        name: name.to_string(),
        version: version.to_string(),
        detail: e.to_string(),
    })?;
    let base = crate::normalize_registry_url(registry_url);
    if base.is_empty() || base == "default" {
        return Err(Error::BadUrl {
            url: registry_url.to_string(),
            reason: "no concrete registry URL (found empty or the `\"default\"` sentinel)"
                .to_string(),
            hint: "resolve `default` to a concrete registry URL (CLI config) before materializing"
                .to_string(),
        });
    }
    Ok(format!("{base}/tarballs/{name}-{version}.tar.gz"))
}

/// Fetch + verify + extract `(name, version, checksum)` from `registry_url`
/// into `cache_root`, returning build-ready source info.
///
/// `checksum` is the locked `sha256:…` (verbatim resolver form) and is
/// verified against the downloaded bytes before extraction, every time a
/// download happens. Reuse (marker match) skips download + extract.
pub fn materialize(
    name: &str,
    version: &str,
    checksum: &str,
    registry_url: &str,
    cache_root: &Path,
    opts: &MaterializeOpts,
) -> Result<MaterializedSource, Error> {
    if !crate::is_valid_package_name(name) {
        return Err(Error::BadUrl {
            url: registry_url.to_string(),
            reason: format!("invalid package name `{name}`"),
            hint: "package names use only [a-zA-Z0-9_-]".to_string(),
        });
    }
    // Validate checksum shape eagerly (bad lock content surfaces here,
    // not after a download).
    crate::parse_checksum(checksum).map_err(|e| match e {
        Error::BadChecksum { value, reason } => Error::BadChecksum { value, reason },
        other => other,
    })?;
    let package_dir =
        registry_cache_dir(cache_root, registry_url).join(format!("{name}-{version}"));
    let marker = package_dir.join(EXTRACT_MARKER);
    if marker_matches(&marker, checksum) {
        if opts.verbose {
            eprintln!(
                "Reusing cached {name} v{version} in `{}`",
                package_dir.display()
            );
        }
        return finish(&package_dir);
    }
    // Stale dir without a matching marker: never merge into it.
    if package_dir.exists() {
        std::fs::remove_dir_all(&package_dir)
            .map_err(|e| Error::io("clear stale package dir", &package_dir, e))?;
    }
    let url = tarball_url_for(registry_url, name, version)?;
    let bytes = crate::download_bytes(&url, opts.offline).map_err(|e| match e {
        Error::Offline { .. } => Error::Offline {
            action: format!("fetch {name} v{version}"),
            where_: url.clone(),
        },
        other => other,
    })?;
    verify(name, version, checksum, &bytes)?;
    if opts.verbose {
        eprintln!("Verified checksum for {name} v{version} from `{registry_url}`");
    }
    crate::extract_tarball_gz(&bytes, &package_dir)?;
    std::fs::write(&marker, format!("{checksum}\n"))
        .map_err(|e| Error::io("write extract marker", &marker, e))?;
    let source_url_file = package_dir.join(SOURCE_URL_FILE);
    let _ = std::fs::write(&source_url_file, format!("{registry_url}\n"));
    if opts.verbose {
        eprintln!("Extracted {name} v{version} to `{}`", package_dir.display());
    }
    finish(&package_dir)
}

/// Compare downloaded bytes against the locked checksum (always run before
/// extraction; mismatch is a hard error, never a warning).
fn verify(name: &str, version: &str, expected: &str, bytes: &[u8]) -> Result<(), Error> {
    let actual = crate::sha256_checksum(bytes);
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(Error::ChecksumMismatch {
            name: name.to_string(),
            version: version.to_string(),
            expected: expected.to_string(),
            actual,
        })
    }
}

fn marker_matches(marker: &Path, checksum: &str) -> bool {
    std::fs::read_to_string(marker)
        .map(|text| text.trim().eq_ignore_ascii_case(checksum.trim()))
        .unwrap_or(false)
}

/// Resolve the source root (unwrap a single top-level dir) + include dirs.
fn finish(package_dir: &Path) -> Result<MaterializedSource, Error> {
    let source_dir = resolve_source_root(package_dir)?;
    let mut include_dirs = Vec::new();
    if source_dir.join("include").is_dir() {
        include_dirs.push(source_dir.join("include"));
    }
    Ok(MaterializedSource {
        source_dir,
        include_dirs,
        lib_hint: None,
    })
}

/// Source-root rule: when the package dir holds exactly one entry and it is
/// a directory, that directory is the source root (this is what
/// `package_source_dir` produces: `<name>-<version>/…`); otherwise the
/// package dir itself is the root.
fn resolve_source_root(package_dir: &Path) -> Result<PathBuf, Error> {
    let mut entries = Vec::new();
    let read = std::fs::read_dir(package_dir)
        .map_err(|e| Error::io("list package dir", package_dir, e))?;
    for entry in read {
        let entry = entry.map_err(|e| Error::io("list package entry", package_dir, e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == EXTRACT_MARKER || name == SOURCE_URL_FILE {
            continue;
        }
        entries.push(entry.path());
    }
    if entries.len() == 1 && entries[0].is_dir() {
        Ok(entries[0].clone())
    } else {
        Ok(package_dir.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{package_source_dir, PublishMeta};

    /// Build a hermetic file:// registry root: `<root>/index/<name>` +
    /// `<root>/tarballs/<name>-<version>.tar.gz`. Returns `(base_url, checksum)`.
    fn fixture_registry() -> (tempfile::TempDir, String, String) {
        let root = tempfile::TempDir::new().expect("tempdir");
        let src = root.path().join("src-pkg");
        std::fs::create_dir_all(src.join("src")).expect("mkdir");
        std::fs::create_dir_all(src.join("include")).expect("mkdir");
        std::fs::write(src.join("rivet.json"), "{\"package\":{}}").expect("write");
        std::fs::write(src.join("src/lib.cpp"), "int f(){return 1;}\n").expect("write");
        std::fs::write(src.join("include/a.h"), "#pragma once\n").expect("write");
        let meta = PublishMeta {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            description: Some("Fast vectors".to_string()),
            deps: Vec::new(),
        };
        let packed = package_source_dir(&src, &meta).expect("package");
        let index_dir = root.path().join("index");
        let tar_dir = root.path().join("tarballs");
        std::fs::create_dir_all(&index_dir).expect("mkdir");
        std::fs::create_dir_all(&tar_dir).expect("mkdir");
        let line = format!(
            "{{\"version\":\"1.0.0\",\"checksum\":\"{}\",\"yanked\":false,\"deps\":[],\"description\":\"Fast vectors\"}}",
            packed.checksum
        );
        std::fs::write(index_dir.join("foo"), &line).expect("write index");
        std::fs::write(tar_dir.join("foo-1.0.0.tar.gz"), &packed.bytes).expect("write tar");
        let raw = root.path().display().to_string().replace('\\', "/");
        let base = if raw.starts_with('/') {
            format!("file://{raw}")
        } else {
            format!("file:///{raw}")
        };
        (root, base, packed.checksum)
    }

    fn opts() -> MaterializeOpts {
        MaterializeOpts {
            offline: false,
            verbose: false,
        }
    }

    #[test]
    fn tarball_url_layout() {
        assert_eq!(
            tarball_url_for("https://example.com/r/", "foo", "1.0.0").expect("url"),
            "https://example.com/r/tarballs/foo-1.0.0.tar.gz"
        );
        assert!(tarball_url_for("default", "foo", "1.0.0").is_err());
        assert!(tarball_url_for("https://example.com/r", "../x", "1.0.0").is_err());
        assert!(tarball_url_for("https://example.com/r", "foo", "banana").is_err());
    }

    #[test]
    fn materialize_end_to_end_from_file_registry() {
        let (_root, base, checksum) = fixture_registry();
        let cache = tempfile::TempDir::new().expect("tempdir");
        let got = materialize("foo", "1.0.0", &checksum, &base, cache.path(), &opts())
            .expect("materialize");
        assert!(
            got.source_dir.join("rivet.json").is_file(),
            "source root unwraps the single top-level dir"
        );
        assert_eq!(got.include_dirs.len(), 1);
        assert!(got.include_dirs[0].ends_with("include"));
        assert_eq!(got.lib_hint, None);
        // Cache layout: <cache>/registry/<hash>/foo-1.0.0/.
        let expect_dir = registry_cache_dir(cache.path(), &base).join("foo-1.0.0");
        assert!(expect_dir.is_dir());
    }

    #[test]
    fn materialize_reuse_skips_download_and_extract() {
        let (_root, base, checksum) = fixture_registry();
        let cache = tempfile::TempDir::new().expect("tempdir");
        let first =
            materialize("foo", "1.0.0", &checksum, &base, cache.path(), &opts()).expect("first");
        let marker = registry_cache_dir(cache.path(), &base)
            .join("foo-1.0.0")
            .join(EXTRACT_MARKER);
        let before = std::fs::metadata(&marker)
            .expect("meta")
            .modified()
            .expect("mtime");
        // Poison the upstream tarball: reuse must not touch it.
        std::fs::write(
            crate::file_url_to_path(&format!("{base}/tarballs/foo-1.0.0.tar.gz")).expect("path"),
            b"corrupt",
        )
        .expect("poison");
        let second =
            materialize("foo", "1.0.0", &checksum, &base, cache.path(), &opts()).expect("second");
        assert_eq!(first.source_dir, second.source_dir);
        let after = std::fs::metadata(&marker)
            .expect("meta")
            .modified()
            .expect("mtime");
        assert_eq!(before, after, "reuse must not re-extract");
    }

    #[test]
    fn materialize_checksum_mismatch_is_hard_error() {
        let (_root, base, _checksum) = fixture_registry();
        let cache = tempfile::TempDir::new().expect("tempdir");
        let wrong = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let err =
            materialize("foo", "1.0.0", wrong, &base, cache.path(), &opts()).expect_err("mismatch");
        assert!(matches!(err, Error::ChecksumMismatch { .. }), "got: {err}");
        assert!(err.to_string().contains("rivet update"));
    }

    #[test]
    fn materialize_offline_missing_errors() {
        let cache = tempfile::TempDir::new().expect("tempdir");
        let checksum = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let off = MaterializeOpts {
            offline: true,
            verbose: false,
        };
        let err = materialize(
            "foo",
            "1.0.0",
            checksum,
            "https://example.com/r",
            cache.path(),
            &off,
        )
        .expect_err("offline");
        assert!(matches!(err, Error::Offline { .. }), "got: {err}");
    }

    #[test]
    fn materialize_bad_checksum_shape_errors_early() {
        let cache = tempfile::TempDir::new().expect("tempdir");
        let err = materialize(
            "foo",
            "1.0.0",
            "not-a-checksum",
            "https://example.com/r",
            cache.path(),
            &opts(),
        )
        .expect_err("shape");
        assert!(matches!(err, Error::BadChecksum { .. }), "got: {err}");
    }
}
