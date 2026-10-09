//! Global cache in `~/.rivet` (registry, sources, git, built artifacts).
//!
//! The artifact cache stores immutable build outputs under
//! `<root>/artifacts/<key>/` so a later build with identical inputs can
//! skip compilation entirely. The key (see [`artifact_key`]) hashes the
//! package id, sorted features, toolchain fingerprint, profile, sorted
//! flags, and an mtime-independent content hash — never mtimes, so clock
//! skew or a fresh checkout cannot poison hits.
//!
//! Layout per entry:
//!
//! ```text
//! <root>/artifacts/<key>/rivet-artifact.json   manifest (written LAST)
//! <root>/artifacts/<key>/<stored files>
//! <root>/artifacts/quarantined/<key>[-n]/      corrupt entries moved aside
//! ```
//!
//! Corrupt entries are quarantined (renamed aside, never deleted silently)
//! and reported as a miss — a lookup never crashes, it just misses.
//!
//! Hook point for a future remote cache: consult it on local miss inside
//! [`lookup`] (download + [`store`]), and upload after [`store`]. This crate
//! stays local-only.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Subdirectory of the cache root holding artifact entries.
pub const ARTIFACTS_DIR: &str = "artifacts";

/// Manifest file name inside each artifact entry.
pub const MANIFEST_FILE_NAME: &str = "rivet-artifact.json";

/// Subdirectory of [`ARTIFACTS_DIR`] where corrupt entries are quarantined.
pub const QUARANTINE_DIR: &str = "quarantined";

/// Cache errors.
#[derive(Debug, Error)]
pub enum Error {
    /// Could not determine a home directory.
    #[error("could not determine home directory; set HOME (Linux) or USERPROFILE (Windows)")]
    NoHome,
    /// A caller-supplied key is not a safe directory name.
    #[error("invalid cache key `{key}`. Fix: use the lowercase hex from `artifact_key` (letters, digits, `-` and `_` only; no `/` or `..`)")]
    InvalidKey {
        /// The rejected key.
        key: String,
    },
    /// An output listed for storage is not a file.
    #[error("output file not found: {path}. Fix: build the package first so its outputs exist before storing")]
    MissingOutput {
        /// Path that was missing.
        path: String,
    },
    /// A cached entry failed validation (moved aside as quarantine).
    #[error("cached artifact `{key}` is corrupt ({reason}); the entry was quarantined, treating it as a cache miss. Fix: rebuild — a fresh `store` replaces it")]
    Corrupt {
        /// Entry key.
        key: String,
        /// What was wrong.
        reason: String,
    },
    /// IO failure.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// Manifest JSON failure.
    #[error("cache manifest error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Returns the global cache root (`~/.rivet`).
///
/// Honors `RIVET_HOME` override (useful for tests/CI), then
/// `USERPROFILE` (Windows) / `HOME` (other).
pub fn cache_root() -> Result<PathBuf, Error> {
    if let Some(dir) = std::env::var_os("RIVET_HOME") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    match home {
        Some(h) if !h.is_empty() => Ok(PathBuf::from(h).join(".rivet")),
        _ => Err(Error::NoHome),
    }
}

// ---------------------------------------------------------------------------
// Keys + content hashing
// ---------------------------------------------------------------------------

/// SHA-256 hex of bytes.
pub fn hash_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// Mtime-independent content hash over files: paths sorted, then per file
/// the path text, a zero byte, the length, and the bytes. Missing files are
/// an error ([`Error::MissingOutput`]) — hashing sources that do not exist
/// is a caller bug, not a hash input.
pub fn inputs_hash(files: &[PathBuf]) -> Result<String, Error> {
    let mut sorted = files.to_vec();
    sorted.sort();
    let mut hasher = Sha256::new();
    for file in &sorted {
        if !file.is_file() {
            return Err(Error::MissingOutput {
                path: file.display().to_string(),
            });
        }
        let bytes = std::fs::read(file)?;
        hasher.update(file.to_string_lossy().as_bytes());
        hasher.update([0u8]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Compute the artifact key for a package build.
///
/// Layout: `sha256("package\\0version\\0<sorted,csv features>\\0
/// <toolchain fingerprint>\\0profile\\0<sorted,csv flags>\\0<inputs hash>")`,
/// returned as 64 lowercase hex chars. Features and flags are sorted
/// internally, so flag order never affects the key. Every dimension that can
/// change the output must be an input here — when in doubt, add a dimension
/// (a spurious miss is always safe; a spurious hit is a wrong build).
#[allow(clippy::too_many_arguments)]
pub fn artifact_key(
    package: &str,
    version: &str,
    features: &[String],
    toolchain_fingerprint: &str,
    profile: &str,
    flags: &[String],
    inputs_hash: &str,
) -> String {
    let mut sorted_features = features.to_vec();
    sorted_features.sort();
    let mut sorted_flags = flags.to_vec();
    sorted_flags.sort();
    let canonical = format!(
        "{package}\0{version}\0{}\0{toolchain_fingerprint}\0{profile}\0{}\0{inputs_hash}",
        sorted_features.join(","),
        sorted_flags.join(","),
    );
    hash_bytes(canonical.as_bytes())
}

/// Directory of the artifact entry (`<root>/artifacts/<key>/`), validating
/// the key so `..` / `/` can never escape the cache (path traversal is
/// rejected, never sanitized silently).
pub fn artifact_dir(cache_root: &Path, key: &str) -> Result<PathBuf, Error> {
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(Error::InvalidKey {
            key: key.to_string(),
        });
    }
    Ok(cache_root.join(ARTIFACTS_DIR).join(key))
}

// ---------------------------------------------------------------------------
// Stored artifacts
// ---------------------------------------------------------------------------

/// One file inside an artifact manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestFile {
    /// Original file name (basename of the stored output).
    name: String,
    /// File name inside the entry dir (disambiguated on collision).
    stored: String,
    /// Byte size at store time (checked by [`StoredArtifact::validate`]).
    size: u64,
    /// SHA-256 hex at store time (recorded for debugging / future strict
    /// verification; `validate` compares presence + size only, keeping
    /// lookups cheap).
    sha256: String,
}

/// On-disk manifest (`rivet-artifact.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArtifactManifest {
    key: String,
    files: Vec<ManifestFile>,
}

/// A cached artifact: cache-resident copies of build outputs, in manifest
/// order. The coordinator copies these back into the package `target/` dir
/// on a hit instead of compiling.
#[derive(Debug, Clone)]
pub struct StoredArtifact {
    /// Entry key.
    pub key: String,
    /// Entry directory (`<root>/artifacts/<key>/`).
    pub dir: PathBuf,
    /// Cache-resident file paths, in manifest order.
    pub files: Vec<PathBuf>,
    /// Manifest records (sizes/hashes for [`StoredArtifact::validate`]).
    entries: Vec<ManifestFile>,
}

impl StoredArtifact {
    /// Build from a validated manifest (files must already be on disk).
    fn from_manifest(dir: &Path, key: &str, manifest: ArtifactManifest) -> Self {
        let files = manifest.files.iter().map(|e| dir.join(&e.stored)).collect();
        Self {
            key: key.to_string(),
            dir: dir.to_path_buf(),
            files,
            entries: manifest.files,
        }
    }

    /// Validate the entry: every recorded file exists, is a file, and has
    /// the recorded size. Anything else is [`Error::Corrupt`] (the caller —
    /// [`lookup`] — quarantines and misses).
    pub fn validate(&self) -> Result<(), Error> {
        for entry in &self.entries {
            let path = self.dir.join(&entry.stored);
            let meta = std::fs::metadata(&path).map_err(|_| Error::Corrupt {
                key: self.key.clone(),
                reason: format!("`{}` is missing", entry.stored),
            })?;
            if !meta.is_file() {
                return Err(Error::Corrupt {
                    key: self.key.clone(),
                    reason: format!("`{}` is not a file", entry.stored),
                });
            }
            if meta.len() != entry.size {
                return Err(Error::Corrupt {
                    key: self.key.clone(),
                    reason: format!(
                        "`{}` size mismatch: manifest says {} bytes, found {}",
                        entry.stored,
                        entry.size,
                        meta.len()
                    ),
                });
            }
        }
        Ok(())
    }
}

/// Pick a collision-free stored name (`foo.a`, then `foo-1.a`, …).
fn unique_name(wanted: &str, used: &mut Vec<String>) -> String {
    if !used.iter().any(|u| u == wanted) {
        used.push(wanted.to_string());
        return wanted.to_string();
    }
    let (stem, ext) = match wanted.rfind('.') {
        Some(i) if i > 0 => (&wanted[..i], &wanted[i..]),
        _ => (wanted, ""),
    };
    let mut n = 1u32;
    loop {
        let candidate = format!("{stem}-{n}{ext}");
        if !used.iter().any(|u| u == &candidate) {
            used.push(candidate.clone());
            return candidate;
        }
        n += 1;
    }
}

/// Store build outputs under `key`.
///
/// Files are copied into `<root>/artifacts/<key>/` with a manifest recording
/// name, size, and SHA-256. The manifest is written LAST so an interrupted
/// store never looks valid (a manifest-less dir is a plain miss, overwritten
/// by the next store). Returns the stored artifact.
pub fn store(cache_root: &Path, key: &str, outputs: &[PathBuf]) -> Result<StoredArtifact, Error> {
    let dir = artifact_dir(cache_root, key)?;
    std::fs::create_dir_all(&dir)?;
    let mut entries = Vec::with_capacity(outputs.len());
    let mut used: Vec<String> = Vec::with_capacity(outputs.len());
    for src in outputs {
        if !src.is_file() {
            return Err(Error::MissingOutput {
                path: src.display().to_string(),
            });
        }
        let bytes = std::fs::read(src)?;
        let name = src
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "artifact".to_string());
        let stored = unique_name(&name, &mut used);
        std::fs::write(dir.join(&stored), &bytes)?;
        entries.push(ManifestFile {
            name,
            stored,
            size: bytes.len() as u64,
            sha256: hash_bytes(&bytes),
        });
    }
    let manifest = ArtifactManifest {
        key: key.to_string(),
        files: entries,
    };
    let text = serde_json::to_string_pretty(&manifest)? + "\n";
    std::fs::write(dir.join(MANIFEST_FILE_NAME), text)?;
    let manifest: ArtifactManifest =
        serde_json::from_str(&std::fs::read_to_string(dir.join(MANIFEST_FILE_NAME))?)?;
    Ok(StoredArtifact::from_manifest(&dir, key, manifest))
}

/// Move a corrupt entry aside under `artifacts/quarantined/`. Best-effort:
/// never panics, never errors — a failed quarantine just leaves the entry
/// for the next lookup to trip over (which still misses safely).
fn quarantine(dir: &Path) {
    let Some(parent) = dir.parent() else {
        return;
    };
    let key = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if key.is_empty() {
        return;
    }
    let base = parent.join(QUARANTINE_DIR);
    if std::fs::create_dir_all(&base).is_err() {
        return;
    }
    for n in 0..1000u32 {
        let dest = if n == 0 {
            base.join(&key)
        } else {
            base.join(format!("{key}-{n}"))
        };
        if dest.exists() {
            continue;
        }
        if std::fs::rename(dir, &dest).is_ok() {
            return;
        }
    }
}

/// Look up a cached artifact.
///
/// Returns `Some` only when the manifest exists, names this key, and every
/// file validates (presence + size). Anything else — missing entry,
/// unreadable/invalid manifest, failed validation — quarantines the dregs
/// (when a manifest was present) and returns `None`. Infallible by design:
/// cache trouble is always a miss, never a build failure.
pub fn lookup(cache_root: &Path, key: &str) -> Option<StoredArtifact> {
    let dir = artifact_dir(cache_root, key).ok()?;
    let text = std::fs::read_to_string(dir.join(MANIFEST_FILE_NAME)).ok()?;
    let manifest: ArtifactManifest = match serde_json::from_str(&text) {
        Ok(manifest) => manifest,
        Err(_) => {
            quarantine(&dir);
            return None;
        }
    };
    if manifest.key != key {
        quarantine(&dir);
        return None;
    }
    let artifact = StoredArtifact::from_manifest(&dir, key, manifest);
    match artifact.validate() {
        Ok(()) => Some(artifact),
        Err(_) => {
            quarantine(&dir);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn key() -> String {
        artifact_key(
            "demo",
            "0.1.0",
            &["a".to_string()],
            "gcc|v1|std=c++17",
            "dev",
            &["-O0".to_string()],
            "inputs1",
        )
    }

    fn write(dir: &std::path::Path, name: &str, content: &[u8]) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn respects_rivet_home_override() {
        let prev = std::env::var_os("RIVET_HOME");
        std::env::set_var("RIVET_HOME", "/tmp/rivet-test-home");
        let root = cache_root().unwrap();
        assert_eq!(root, PathBuf::from("/tmp/rivet-test-home"));
        match prev {
            Some(v) => std::env::set_var("RIVET_HOME", v),
            None => std::env::remove_var("RIVET_HOME"),
        }
    }

    #[test]
    fn roundtrip_preserves_bytes_and_order() {
        let tmp = root();
        let pkg = tmp.path().join("pkg");
        let a = write(&pkg, "libdemo.a", b"archive-bytes");
        let b = write(&pkg, "demo.exe", b"exe-bytes");
        let stored = store(tmp.path(), &key(), &[a, b]).unwrap();
        assert_eq!(stored.files.len(), 2);
        let hit = lookup(tmp.path(), &key()).unwrap();
        hit.validate().unwrap();
        assert_eq!(hit.files.len(), 2);
        assert_eq!(std::fs::read(&hit.files[0]).unwrap(), b"archive-bytes");
        assert_eq!(std::fs::read(&hit.files[1]).unwrap(), b"exe-bytes");
    }

    #[test]
    fn same_basename_collisions_stay_distinct() {
        let tmp = root();
        let pkg = tmp.path().join("pkg");
        let a = write(&pkg.join("one"), "out.a", b"one");
        let b = write(&pkg.join("two"), "out.a", b"two");
        let stored = store(tmp.path(), &key(), &[a, b]).unwrap();
        assert_ne!(stored.files[0], stored.files[1]);
        let hit = lookup(tmp.path(), &key()).unwrap();
        let mut contents: Vec<Vec<u8>> = hit
            .files
            .iter()
            .map(|p| std::fs::read(p).unwrap())
            .collect();
        contents.sort();
        assert_eq!(contents, vec![b"one".to_vec(), b"two".to_vec()]);
    }

    #[test]
    fn key_is_stable_hex_and_sorts_inputs() {
        let k1 = key();
        assert_eq!(k1.len(), 64);
        assert!(k1.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(k1, key());
        // Feature/flag ORDER does not matter …
        let reordered = artifact_key(
            "demo",
            "0.1.0",
            &["b".to_string(), "a".to_string()],
            "gcc|v1|std=c++17",
            "dev",
            &["-O2".to_string(), "-O0".to_string()],
            "inputs1",
        );
        let ordered = artifact_key(
            "demo",
            "0.1.0",
            &["a".to_string(), "b".to_string()],
            "gcc|v1|std=c++17",
            "dev",
            &["-O0".to_string(), "-O2".to_string()],
            "inputs1",
        );
        assert_eq!(reordered, ordered);
    }

    #[test]
    fn key_sensitive_to_every_dimension() {
        let base = key();
        let feat = artifact_key(
            "demo",
            "0.1.0",
            &["b".to_string()],
            "gcc|v1|std=c++17",
            "dev",
            &["-O0".to_string()],
            "inputs1",
        );
        let flag = artifact_key(
            "demo",
            "0.1.0",
            &["a".to_string()],
            "gcc|v1|std=c++17",
            "dev",
            &["-O2".to_string()],
            "inputs1",
        );
        let prof = artifact_key(
            "demo",
            "0.1.0",
            &["a".to_string()],
            "gcc|v1|std=c++17",
            "release",
            &["-O0".to_string()],
            "inputs1",
        );
        let tc = artifact_key(
            "demo",
            "0.1.0",
            &["a".to_string()],
            "msvc|v2|std=c++17",
            "dev",
            &["-O0".to_string()],
            "inputs1",
        );
        let ver = artifact_key(
            "demo",
            "0.2.0",
            &["a".to_string()],
            "gcc|v1|std=c++17",
            "dev",
            &["-O0".to_string()],
            "inputs1",
        );
        let inp = artifact_key(
            "demo",
            "0.1.0",
            &["a".to_string()],
            "gcc|v1|std=c++17",
            "dev",
            &["-O0".to_string()],
            "inputs2",
        );
        for other in [feat, flag, prof, tc, ver, inp] {
            assert_ne!(base, other);
        }
    }

    #[test]
    fn flag_change_is_a_miss() {
        let tmp = root();
        let pkg = tmp.path().join("pkg");
        let a = write(&pkg, "libdemo.a", b"bytes");
        store(tmp.path(), &key(), &[a]).unwrap();
        let other = artifact_key(
            "demo",
            "0.1.0",
            &["a".to_string()],
            "gcc|v1|std=c++17",
            "dev",
            &["-O2".to_string()],
            "inputs1",
        );
        assert!(lookup(tmp.path(), &other).is_none());
        assert!(lookup(tmp.path(), &key()).is_some());
    }

    #[test]
    fn corrupt_size_quarantines_and_misses() {
        let tmp = root();
        let pkg = tmp.path().join("pkg");
        let a = write(&pkg, "libdemo.a", b"0123456789");
        let k = key();
        store(tmp.path(), &k, &[a]).unwrap();
        // Truncate one cached file.
        let cached = artifact_dir(tmp.path(), &k).unwrap().join("libdemo.a");
        std::fs::write(&cached, b"short").unwrap();
        assert!(lookup(tmp.path(), &k).is_none());
        // Entry moved aside, never left half-valid in place.
        assert!(!artifact_dir(tmp.path(), &k).unwrap().exists());
        let q = tmp.path().join(ARTIFACTS_DIR).join(QUARANTINE_DIR).join(&k);
        assert!(q.is_dir(), "quarantine should hold the entry");
        // A later lookup still misses cleanly (no crash on the quarantine).
        assert!(lookup(tmp.path(), &k).is_none());
    }

    #[test]
    fn corrupt_manifest_quarantines_and_misses() {
        let tmp = root();
        let k = key();
        let dir = artifact_dir(tmp.path(), &k).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MANIFEST_FILE_NAME), "{ not json").unwrap();
        assert!(lookup(tmp.path(), &k).is_none());
        assert!(!dir.exists());
        assert!(tmp
            .path()
            .join(ARTIFACTS_DIR)
            .join(QUARANTINE_DIR)
            .join(&k)
            .is_dir());
    }

    #[test]
    fn missing_manifest_is_a_plain_miss() {
        let tmp = root();
        let k = key();
        let dir = artifact_dir(tmp.path(), &k).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("orphan.a"), b"x").unwrap();
        assert!(lookup(tmp.path(), &k).is_none());
        // Incomplete stores are left for the next store to overwrite.
        assert!(dir.is_dir());
        assert!(!tmp.path().join(ARTIFACTS_DIR).join(QUARANTINE_DIR).exists());
    }

    #[test]
    fn unknown_key_is_a_miss() {
        let tmp = root();
        assert!(lookup(tmp.path(), &key()).is_none());
    }

    #[test]
    fn invalid_keys_rejected() {
        let tmp = root();
        let pkg = tmp.path().join("pkg");
        let a = write(&pkg, "x.a", b"x");
        for bad in ["", "../evil", "a/b", "key with spaces", "k*y"] {
            let err = store(tmp.path(), bad, std::slice::from_ref(&a))
                .unwrap_err()
                .to_string();
            assert!(err.contains("invalid cache key"), "key {bad:?} got: {err}");
            assert!(lookup(tmp.path(), bad).is_none());
        }
    }

    #[test]
    fn missing_output_errors() {
        let tmp = root();
        let err = store(tmp.path(), &key(), &[tmp.path().join("nope.a")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "got: {err}");
    }

    #[test]
    fn validate_detects_deleted_file() {
        let tmp = root();
        let pkg = tmp.path().join("pkg");
        let a = write(&pkg, "x.a", b"data");
        let k = key();
        let stored = store(tmp.path(), &k, &[a]).unwrap();
        stored.validate().unwrap();
        std::fs::remove_file(&stored.files[0]).unwrap();
        let err = stored.validate().unwrap_err().to_string();
        assert!(err.contains("corrupt"), "got: {err}");
        // And a lookup now quarantines + misses.
        assert!(lookup(tmp.path(), &k).is_none());
    }

    #[test]
    fn inputs_hash_ignores_mtime_and_order() {
        let tmp = root();
        let a = write(tmp.path(), "a.cpp", b"int a();");
        let b = write(tmp.path(), "b.cpp", b"int b();");
        let h1 = inputs_hash(&[a.clone(), b.clone()]).unwrap();
        // Same bytes rewritten (fresh mtime) → same hash.
        std::fs::write(&a, b"int a();").unwrap();
        assert_eq!(inputs_hash(&[a.clone(), b.clone()]).unwrap(), h1);
        // Order does not matter.
        assert_eq!(inputs_hash(&[b.clone(), a.clone()]).unwrap(), h1);
        // Content change → different hash.
        std::fs::write(&b, b"int b2();").unwrap();
        assert_ne!(inputs_hash(&[a, b]).unwrap(), h1);
        // Missing file → actionable error, not a hash.
        let err = inputs_hash(&[tmp.path().join("gone.cpp")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "got: {err}");
    }

    #[test]
    fn hash_bytes_matches_known_vector() {
        assert_eq!(
            hash_bytes(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }
}
