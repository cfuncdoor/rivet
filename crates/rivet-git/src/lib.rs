//! Git dependency subsystem (shell-out to the system `git` binary).
//!
//! M4 milestone. In M1 this crate exposes the final error type and a stub
//! that reports "not implemented". The real implementation must follow
//! `ARCHITECTURE.md` / AGENTS.md: `std::process::Command` only (never
//! git2/gitoxide), bare mirror + immutable checkouts under `~/.rivet/git/`,
//! `GIT_TERMINAL_PROMPT=0`, hooks disabled, https/ssh/file URLs only.

use thiserror::Error;

/// Git subsystem errors.
#[derive(Debug, Error)]
pub enum Error {
    /// Feature reserved for a later milestone.
    #[error("git dependencies are not implemented yet (planned for M4)")]
    NotImplemented,
    /// System `git` binary is missing.
    #[error(
        "system `git` binary not found in PATH; install git from https://git-scm.com and retry"
    )]
    GitNotFound,
}

/// Git dependency entry point (M4 stub).
pub fn fetch() -> Result<(), Error> {
    Err(Error::NotImplemented)
}

/// Returns `Ok(())` if a `git` binary is discoverable, else [`Error::GitNotFound`].
pub fn require_git() -> Result<(), Error> {
    // Deliberately avoid the `which` crate here to keep this crate dep-free;
    // PATH probing is trivial and fully testable.
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut found = false;
    for dir in std::env::split_paths(&path) {
        #[cfg(windows)]
        {
            if dir.join("git.exe").is_file() {
                found = true;
                break;
            }
        }
        #[cfg(not(windows))]
        {
            if dir.join("git").is_file() {
                found = true;
                break;
            }
        }
    }
    if found {
        Ok(())
    } else {
        Err(Error::GitNotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_returns_not_implemented() {
        assert!(matches!(fetch(), Err(Error::NotImplemented)));
    }
}
