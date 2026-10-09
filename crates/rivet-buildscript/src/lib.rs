//! Build scripts: compile and run `build.rivet.cpp` before the main build
//! and apply the `rivet:` directives it prints (include dirs, link libs,
//! defines, rerun-if-changed).
//!
//! Implemented by the M7 agent. The coordinator wires script outputs into
//! the build graph after this lands.

use thiserror::Error;

/// Build script errors.
#[derive(Debug, Error)]
pub enum Error {
    /// Placeholder until the M7 agent implements this crate.
    #[error("build scripts are not implemented yet")]
    NotImplemented,
}

/// Build script entry point (M7 stub).
pub fn run() -> Result<(), Error> {
    Err(Error::NotImplemented)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_returns_not_implemented() {
        assert!(matches!(run(), Err(Error::NotImplemented)));
    }
}
