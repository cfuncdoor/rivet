//! Global cache in `~/.rivet` (registry, sources, git, built artifacts).
//!
//! M7 milestone (artifact cache). In M1 this crate resolves the cache root
//! and exposes the final error type; no network or artifact storage yet.

use std::path::PathBuf;

use thiserror::Error;

/// Cache errors.
#[derive(Debug, Error)]
pub enum Error {
    /// Could not determine a home directory.
    #[error("could not determine home directory; set HOME (Linux) or USERPROFILE (Windows)")]
    NoHome,
    /// Feature reserved for a later milestone.
    #[error("artifact cache is not implemented yet (planned for M7)")]
    NotImplemented,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respects_rivet_home_override() {
        // SAFETY: tests run single-threaded per binary by default for env
        // mutation only if we serialize; use a unique var value and restore.
        let prev = std::env::var_os("RIVET_HOME");
        std::env::set_var("RIVET_HOME", "/tmp/rivet-test-home");
        let root = cache_root().unwrap();
        assert_eq!(root, PathBuf::from("/tmp/rivet-test-home"));
        match prev {
            Some(v) => std::env::set_var("RIVET_HOME", v),
            None => std::env::remove_var("RIVET_HOME"),
        }
    }
}
