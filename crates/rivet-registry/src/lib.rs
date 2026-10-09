//! Package registry client: index reads, tarball fetch + verify + extract,
//! deterministic packaging, login tokens, publish requests, search, and the
//! materialize API consumed by the CLI.
//!
//! This crate is intentionally standalone: it shares the [`RegistryIndex`]
//! JSON-lines shape with `rivet-resolver` (contract C2/C4) but takes no
//! dependency on it, so index files written by either side stay mutually
//! readable.
//!
//! # Index layouts
//!
//! * Local index dir (primary; also what `rivet-resolver` reads and what
//!   `--offline` uses): files `<index-dir>/<name>`, one JSON object per line:
//!   `{"version":"1.2.3","checksum":"sha256:…","yanked":false,
//!   "deps":[{"name":"…","req":"^1","optional":false}],"description":"…"}.
//!   Unknown fields are ignored on read.
//! * Sparse HTTPS registry: index files live at
//!   `<registry-url>/index/<name>` ([`index::index_url_for`]). A `file://`
//!   base URL maps to the same structure on disk (`<root>/index/<name>`), so
//!   the sparse layout is hermetically testable without network.
//!
//! # Tarball layout
//!
//! Tarballs are gzip-compressed tar archives downloaded from
//! `<registry-url>/tarballs/<name>-<version>.tar.gz`
//! ([`materialize::tarball_url_for`]; `file://` bases map to
//! `<root>/tarballs/<name>-<version>.tar.gz`). Every downloaded tarball is
//! SHA-256 verified against the locked/index checksum **before** extraction;
//! a mismatch is a hard error. Extraction rejects absolute paths, `..`
//! traversal, special files, and symlinks/hardlinks that escape the
//! destination (same discipline as `rivet-git`'s checkout validation;
//! in-root links are preserved).
//!
//! # Cache layout
//!
//! `<cache>/registry/<registry-hash>/<name>-<version>/`
//! ([`materialize::registry_cache_dir`]), where `<registry-hash>` is the
//! first 16 hex chars of the SHA-256 of the normalized registry URL
//! ([`registry_hash`]). A successful extraction writes a marker file
//! `.rivet-extract-ok` holding the verified checksum; a later call with the
//! same checksum reuses the directory without re-downloading or
//! re-extracting. The source root is the single top-level directory when the
//! archive has exactly one (as [`package::package_source_dir`] produces),
//! otherwise the extraction directory itself.
//!
//! # Checksums
//!
//! Always `sha256:<lowercase-hex>`, verbatim as stored in `rivet.lock.json`
//! (`LockSource::Registry.checksum`). Use [`sha256_checksum`] to compute and
//! [`parse_checksum`] to validate the shape.
//!
//! # Credentials
//!
//! `~/.rivet/credentials` holds a JSON object mapping registry URL → token
//! (see [`auth`]). Unix files are chmodded `0600` best-effort; Windows has
//! no std chmod equivalent, so login warns there.
//!
//! # Publish wire format
//!
//! `PUT <registry-url>/publish/<name>/<version>` with the gzip tarball as
//! the body, `Content-Type: application/gzip`, `Authorization: Bearer
//! <token>`, `X-Rivet-Checksum: sha256:…`, plus optional
//! `X-Rivet-Description` / `X-Rivet-Deps` (JSON) metadata headers. The
//! registry server arrives later; [`publish::build_publish_request`]
//! constructs the request for unit tests and the CLI.

mod auth;
mod fetch;
mod index;
mod materialize;
mod package;
mod publish;
mod search;

pub use auth::{credentials_path, load_token, login, logout};
pub use fetch::{
    download_bytes, extract_tarball_gz, file_url_to_path, validate_tree, MAX_INDEX_BYTES,
    MAX_TARBALL_BYTES,
};
pub use index::{
    index_url_for, is_valid_package_name, select_best_version, IndexDep, IndexEntry, RegistryIndex,
};
pub use materialize::{
    materialize, registry_cache_dir, tarball_url_for, MaterializeOpts, MaterializedSource,
    EXTRACT_MARKER,
};
pub use package::{package_source_dir, tarball_entries, PackagedTarball, PublishMeta, FIXED_MTIME};
pub use publish::{build_publish_request, publish, PublishRequest};
pub use search::{search, SearchRow};

use thiserror::Error;

/// Convenience alias for registry results (defaults to [`Error`]).
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Registry errors. Every variant is actionable (what/where/fix); network
/// bodies are truncated to a snippet so errors stay readable.
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
    /// Registry index problem (missing package, corrupt line, bad version).
    #[error("registry index: {message}. Hint: {hint}")]
    BadIndex {
        /// What was wrong.
        message: String,
        /// What to do.
        hint: String,
    },
    /// Malformed registry URL or unsupported scheme.
    #[error("invalid registry URL `{url}`: {reason}. Hint: {hint}")]
    BadUrl {
        /// URL as given.
        url: String,
        /// Why it was rejected.
        reason: String,
        /// What to do.
        hint: String,
    },
    /// Checksum with the wrong shape (must be `sha256:<64 hex>`).
    #[error("bad checksum `{value}`: {reason}. Hint: checksums look like `sha256:<64 lowercase hex>` and come from the index/lockfile")]
    BadChecksum {
        /// Value as given.
        value: String,
        /// Why it was rejected.
        reason: String,
    },
    /// Downloaded bytes do not match the locked/index checksum.
    #[error("checksum mismatch for {name} v{version}: expected {expected}, got {actual}. Hint: the download is corrupt or the lockfile is stale — remove the cached copy and retry, or run `rivet update -p {name}`")]
    ChecksumMismatch {
        /// Package name.
        name: String,
        /// Package version.
        version: String,
        /// Expected `sha256:…`.
        expected: String,
        /// Actual `sha256:…`.
        actual: String,
    },
    /// Tarball entry refused (traversal, absolute path, escaping link…).
    #[error("unsafe tarball entry `{entry}`: {reason}. Hint: the package is corrupt or malicious — refusing to extract")]
    UnsafeEntry {
        /// Entry path as stored in the archive.
        entry: String,
        /// Why it was rejected.
        reason: String,
    },
    /// Network needed but offline mode is on.
    #[error("offline: {action} needs network but offline mode is on ({where_}). Hint: run once online to populate the cache, then retry offline")]
    Offline {
        /// What was attempted.
        action: String,
        /// Where.
        where_: String,
    },
    /// Publish attempted without a stored token.
    #[error("registry `{registry}` needs a login token but none is stored. Hint: run `rivet login --registry {registry}` first")]
    AuthMissing {
        /// Registry URL.
        registry: String,
    },
    /// No index version satisfies the requirement.
    #[error("no version of `{name}` satisfies [{reqs}]. Hint: relax the requirement or publish a matching version first")]
    NoMatchingVersion {
        /// Package name.
        name: String,
        /// Comma-joined requirements (plus a yanked-only note when apt).
        reqs: String,
    },
    /// Unparseable package version (index data or caller input).
    #[error("invalid version `{version}` for `{name}`: {detail}. Hint: use `major.minor.patch`, e.g. `0.1.0`")]
    InvalidVersion {
        /// Package name.
        name: String,
        /// Bad version.
        version: String,
        /// Parser detail.
        detail: String,
    },
    /// Unparseable version requirement.
    #[error("invalid version requirement `{req}` for `{name}`: {detail}. Hint: use caret SemVer like `\"11\"`, `\"^1.2\"`, or `\"1.2.3\"`")]
    InvalidReq {
        /// Package name.
        name: String,
        /// Requirement as written.
        req: String,
        /// Parser detail.
        detail: String,
    },
    /// HTTPS transport failure (DNS, TLS, refused, bad status…).
    #[error("network error while {action} ({url}): {message}. Hint: check your connection, proxy settings, and that the registry URL is reachable")]
    Transport {
        /// What was attempted.
        action: String,
        /// URL involved.
        url: String,
        /// Transport detail (+ truncated body for HTTP errors).
        message: String,
    },
    /// The registry rejected a publish upload.
    #[error("publish to `{registry}` failed (HTTP {status}): {message}. Hint: {hint}")]
    Publish {
        /// Registry URL.
        registry: String,
        /// HTTP status code.
        status: u16,
        /// Truncated server body.
        message: String,
        /// What to do (bad token? already exists?).
        hint: String,
    },
    /// Credential store failure.
    #[error("login failed: {message}. Hint: {hint}")]
    Login {
        /// What went wrong.
        message: String,
        /// What to do.
        hint: String,
    },
    /// Source packaging failure.
    #[error("cannot package `{path}`: {message}. Hint: {hint}")]
    Package {
        /// Source dir involved.
        path: String,
        /// What went wrong.
        message: String,
        /// What to do.
        hint: String,
    },
}

impl Error {
    pub(crate) fn io(action: &str, path: &std::path::Path, err: std::io::Error) -> Self {
        Self::Io {
            action: action.to_string(),
            path: path.display().to_string(),
            message: format!("{err}. Hint: check permissions and disk space"),
        }
    }
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(hex_nibble(byte >> 4));
        out.push(hex_nibble(byte & 0x0f));
    }
    out
}

fn hex_nibble(n: u8) -> char {
    (match n {
        0..=9 => b'0' + n,
        _ => b'a' + (n - 10),
    }) as char
}

/// Full lockfile/index checksum form `sha256:<hex>` for `bytes`.
pub fn sha256_checksum(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(bytes))
}

/// Validate `sha256:<64 hex>` shape and decode the digest.
///
/// Accepts upper- or lowercase hex; the canonical lockfile form is
/// lowercase.
pub fn parse_checksum(expected: &str) -> Result<[u8; 32], Error> {
    let hex = expected
        .strip_prefix("sha256:")
        .ok_or_else(|| Error::BadChecksum {
            value: expected.to_string(),
            reason: "missing `sha256:` prefix".to_string(),
        })?;
    if hex.len() != 64 {
        return Err(Error::BadChecksum {
            value: expected.to_string(),
            reason: format!("expected 64 hex chars, found {}", hex.len()),
        });
    }
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = hex_val(chunk[0]).ok_or_else(|| Error::BadChecksum {
            value: expected.to_string(),
            reason: format!("invalid hex char `{}`", chunk[0] as char),
        })?;
        let lo = hex_val(chunk[1]).ok_or_else(|| Error::BadChecksum {
            value: expected.to_string(),
            reason: format!("invalid hex char `{}`", chunk[1] as char),
        })?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Normalize a registry base URL for hashing/comparison (trailing `/`
/// stripped; empty stays empty so callers can error actionably).
pub fn normalize_registry_url(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

/// Short content hash identifying a registry in the cache layout:
/// first 16 hex chars of SHA-256 over the normalized URL (64 bits —
/// collision-impossible in practice; used only as a directory suffix, the
/// full URL is recorded alongside in `.rivet-source-url`).
pub fn registry_hash(registry_url: &str) -> String {
    sha256_hex(normalize_registry_url(registry_url).as_bytes())[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_roundtrip() {
        let sum = sha256_checksum(b"hello");
        assert!(sum.starts_with("sha256:"));
        assert_eq!(sum.len(), "sha256:".len() + 64);
        let raw = parse_checksum(&sum).expect("parse");
        assert_eq!(hex::of(&raw), &sum["sha256:".len()..]);
        // Uppercase accepted.
        parse_checksum(&sum.to_uppercase().replace("SHA256", "sha256")).expect("upper");
    }

    #[test]
    fn checksum_bad_shapes() {
        assert!(matches!(
            parse_checksum("abc123"),
            Err(Error::BadChecksum { .. })
        ));
        assert!(matches!(
            parse_checksum("sha256:zzzz"),
            Err(Error::BadChecksum { .. })
        ));
        assert!(matches!(
            parse_checksum("sha256:ab"),
            Err(Error::BadChecksum { .. })
        ));
        assert!(matches!(
            parse_checksum("md5:8b1a9953c4611296a827abf8c47804d7"),
            Err(Error::BadChecksum { .. })
        ));
    }

    #[test]
    fn registry_hash_stable_and_normalized() {
        assert_eq!(
            registry_hash("https://example.com/rivet"),
            registry_hash("https://example.com/rivet/")
        );
        assert_ne!(
            registry_hash("https://example.com/a"),
            registry_hash("https://example.com/b")
        );
        assert_eq!(registry_hash("https://example.com/a").len(), 16);
    }

    /// Tiny local hex formatter for the test above (avoids a `hex` dep).
    mod hex {
        pub fn of(bytes: &[u8]) -> String {
            let mut s = String::with_capacity(bytes.len() * 2);
            for b in bytes {
                s.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('?'));
                s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap_or('?'));
            }
            s
        }
    }
}
