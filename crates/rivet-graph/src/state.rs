//! Content hashing and incremental-build state.
//!
//! Staleness is decided by content hashes only (D15: no mtime fast path in
//! M2 — always hash). Per-package state lives at
//! `target/<prof>/incremental/rivet-state.json`:
//!
//! ```json
//! {
//!   "version": 1,
//!   "fingerprint": "<toolchain+flags>",
//!   "tus": { "<unit>": { "<src-rel>": {
//!     "hash": "<sha256 of source>",
//!     "object": "<rel obj path>",
//!     "depfile": "<rel depfile path|null>",
//!     "deps": ["<abs header>..."],
//!     "dep_hashes": {"<abs header>": "<sha256>"}
//!   } } },
//!   "links": { "<output-id>": "<digest over fingerprint+input hashes>" }
//! }
//! ```
//!
//! A version or fingerprint mismatch invalidates everything. Writes go
//! through a temp file + rename so an interrupted build never leaves a
//! half-written state file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// State errors.
#[derive(Debug, Error)]
pub enum Error {
    /// IO failure.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// State file is corrupt JSON.
    #[error("corrupt incremental state at {path}: {message}. Fix: delete it (`rivet clean`) and rebuild")]
    Corrupt {
        /// State file path.
        path: String,
        /// Parse error.
        message: String,
    },
}

/// On-disk state format version. Bump when the schema changes; a mismatch
/// forces a full rebuild.
pub const STATE_VERSION: u32 = 1;

/// SHA-256 hex of bytes.
pub fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// SHA-256 hex of a file's contents, or `None` when it does not exist.
/// A missing file must compare as "changed", never vanish silently.
pub fn hash_file(path: &Path) -> Result<Option<String>, std::io::Error> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(hash_bytes(&bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Digest for a link step over the fingerprint plus ordered input hashes
/// (object + archive content hashes).
pub fn link_digest(fingerprint: &str, input_hashes: &[String]) -> String {
    let mut h = Sha256::new();
    h.update(fingerprint.as_bytes());
    for input in input_hashes {
        h.update(input.as_bytes());
    }
    format!("{:x}", h.finalize())
}

/// Per-TU record.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TuState {
    /// SHA-256 of the source file at last compile.
    pub hash: String,
    /// Object path, relative to the target dir.
    pub object: String,
    /// Depfile path, relative to the target dir (`None` for MSVC-like).
    pub depfile: Option<String>,
    /// Absolute header paths seen at last compile.
    pub deps: Vec<String>,
    /// Their content hashes at last compile.
    pub dep_hashes: BTreeMap<String, String>,
}

/// A fresh TU record for [`BuildState::record_tu`].
#[derive(Debug, Clone)]
pub struct TuRecord {
    /// SHA-256 of the source file.
    pub hash: String,
    /// Object path, relative to the target dir.
    pub object_rel: String,
    /// Depfile path, relative to the target dir.
    pub depfile_rel: Option<String>,
    /// Absolute header paths seen at compile.
    pub deps: Vec<String>,
    /// Their content hashes.
    pub dep_hashes: BTreeMap<String, String>,
}

/// Per-package incremental state.
#[derive(Debug, Clone, Default)]
pub struct BuildState {
    fingerprint: String,
    tus: BTreeMap<String, BTreeMap<String, TuState>>,
    links: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
struct StoredState {
    version: u32,
    fingerprint: String,
    #[serde(default)]
    tus: BTreeMap<String, BTreeMap<String, TuState>>,
    #[serde(default)]
    links: BTreeMap<String, String>,
}

/// State file location for a target dir.
pub fn state_path(target_dir: &Path) -> PathBuf {
    target_dir.join("incremental").join("rivet-state.json")
}

impl BuildState {
    /// Empty state for `fingerprint`.
    pub fn new(fingerprint: String) -> Self {
        Self {
            fingerprint,
            tus: BTreeMap::new(),
            links: BTreeMap::new(),
        }
    }

    /// Load state for `target_dir`. Returns `(state, usable)`: `usable` is
    /// `false` when the file is missing, has another version, or another
    /// fingerprint — all of which mean "rebuild everything". A corrupt file
    /// is an error (it exists but cannot be trusted; the message says how
    /// to recover).
    pub fn load(target_dir: &Path, fingerprint: &str) -> Result<(Self, bool), Error> {
        let path = state_path(target_dir);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Self::new(fingerprint.to_string()), false));
            }
            Err(e) => return Err(Error::Io(e)),
        };
        let stored: StoredState = serde_json::from_str(&text).map_err(|e| Error::Corrupt {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        if stored.version != STATE_VERSION || stored.fingerprint != fingerprint {
            return Ok((Self::new(fingerprint.to_string()), false));
        }
        Ok((
            Self {
                fingerprint: stored.fingerprint,
                tus: stored.tus,
                links: stored.links,
            },
            true,
        ))
    }

    /// Persist state atomically (temp file + rename).
    pub fn save(&self, target_dir: &Path) -> Result<(), Error> {
        let path = state_path(target_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let stored = StoredState {
            version: STATE_VERSION,
            fingerprint: self.fingerprint.clone(),
            tus: self.tus.clone(),
            links: self.links.clone(),
        };
        let text = serde_json::to_string_pretty(&stored).map_err(|e| Error::Corrupt {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Record a freshly compiled TU.
    pub fn record_tu(&mut self, unit: &str, src_rel: &str, record: TuRecord) {
        self.tus.entry(unit.to_string()).or_default().insert(
            src_rel.to_string(),
            TuState {
                hash: record.hash,
                object: record.object_rel,
                depfile: record.depfile_rel,
                deps: record.deps,
                dep_hashes: record.dep_hashes,
            },
        );
    }

    /// Look up a TU record.
    pub fn tu(&self, unit: &str, src_rel: &str) -> Option<&TuState> {
        self.tus.get(unit)?.get(src_rel)
    }

    /// Unit ids currently in state.
    pub fn units(&self) -> Vec<String> {
        self.tus.keys().cloned().collect()
    }

    /// Source keys of a unit currently in state.
    pub fn sources_of(&self, unit: &str) -> Vec<String> {
        self.tus
            .get(unit)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Drop one source record, returning its artifact rel paths for cleanup.
    pub fn remove_source(&mut self, unit: &str, src_rel: &str) -> Option<(String, Option<String>)> {
        let rec = self.tus.get_mut(unit)?.remove(src_rel)?;
        if self.tus.get(unit).is_some_and(|m| m.is_empty()) {
            self.tus.remove(unit);
        }
        Some((rec.object, rec.depfile))
    }

    /// Drop a whole unit, returning artifact rel paths for cleanup.
    pub fn remove_unit(&mut self, unit: &str) -> Vec<(String, Option<String>)> {
        self.tus
            .remove(unit)
            .map(|m| m.into_values().map(|r| (r.object, r.depfile)).collect())
            .unwrap_or_default()
    }

    /// Look up a link digest.
    pub fn link(&self, id: &str) -> Option<&String> {
        self.links.get(id)
    }

    /// Record a link digest.
    pub fn record_link(&mut self, id: &str, digest: String) {
        self.links.insert(id.to_string(), digest);
    }

    /// Link ids currently in state.
    pub fn link_ids(&self) -> Vec<String> {
        self.links.keys().cloned().collect()
    }

    /// Drop a link record.
    pub fn remove_link(&mut self, id: &str) {
        self.links.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_deterministic() {
        assert_eq!(hash_bytes(b"abc"), hash_bytes(b"abc"));
        assert_ne!(hash_bytes(b"abc"), hash_bytes(b"abd"));
        assert_eq!(hash_bytes(b"").len(), 64);
    }

    #[test]
    fn missing_file_hashes_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.h");
        assert_eq!(hash_file(&missing).unwrap(), None);
        std::fs::write(dir.path().join("a.h"), "x").unwrap();
        assert!(hash_file(&dir.path().join("a.h")).unwrap().is_some());
    }

    #[test]
    fn roundtrip_and_fingerprint_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target").join("debug");
        let mut st = BuildState::new("fp1".to_string());
        let mut hashes = BTreeMap::new();
        hashes.insert("/pkg/a.h".to_string(), "deadbeef".to_string());
        st.record_tu(
            "bin-a",
            "src/a.cpp",
            TuRecord {
                hash: "abc123".to_string(),
                object_rel: "build/a.o".to_string(),
                depfile_rel: None,
                deps: vec!["/pkg/a.h".to_string()],
                dep_hashes: hashes,
            },
        );
        st.record_link("bin:a", "digest1".to_string());
        st.save(&target).unwrap();

        let (loaded, usable) = BuildState::load(&target, "fp1").unwrap();
        assert!(usable);
        assert_eq!(loaded.tu("bin-a", "src/a.cpp").unwrap().hash, "abc123");
        assert_eq!(loaded.link("bin:a").unwrap(), "digest1");

        let (_, usable) = BuildState::load(&target, "fp2").unwrap();
        assert!(!usable, "fingerprint change must invalidate");
    }

    #[test]
    fn missing_state_is_not_usable() {
        let dir = tempfile::tempdir().unwrap();
        let (_, usable) = BuildState::load(&dir.path().join("target"), "fp").unwrap();
        assert!(!usable);
    }

    #[test]
    fn corrupt_state_errors_actionably() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("t");
        std::fs::create_dir_all(target.join("incremental")).unwrap();
        std::fs::write(state_path(&target), "{oops").unwrap();
        let err = BuildState::load(&target, "fp").unwrap_err().to_string();
        assert!(err.contains("corrupt"), "got: {err}");
        assert!(err.contains("rivet clean"), "got: {err}");
    }

    #[test]
    fn prune_helpers() {
        let mut st = BuildState::new("fp".to_string());
        st.record_tu(
            "u",
            "a.cpp",
            TuRecord {
                hash: "h".to_string(),
                object_rel: "a.o".to_string(),
                depfile_rel: Some("a.d".to_string()),
                deps: Vec::new(),
                dep_hashes: BTreeMap::new(),
            },
        );
        st.record_link("x", "d".to_string());
        let removed = st.remove_source("u", "a.cpp").unwrap();
        assert_eq!(removed.0, "a.o");
        assert!(st.sources_of("u").is_empty());
        assert!(st.units().is_empty());
        st.remove_link("x");
        assert!(st.link_ids().is_empty());
    }
}
