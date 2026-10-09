//! Foreign package adapters: build non-rivet sources (CMake, Meson, Make)
//! and expose their include dirs + libs, or probe installed system
//! libraries (`system: true`, pkg-config).
//!
//! Implemented by the M6 agent. The coordinator wires adapter outputs into
//! the build graph after this lands.

use thiserror::Error;

/// Adapter errors.
#[derive(Debug, Error)]
pub enum Error {
    /// Placeholder until the M6 agent implements this crate.
    #[error("foreign package adapters are not implemented yet")]
    NotImplemented,
}

/// Adapter entry point (M6 stub).
pub fn probe() -> Result<(), Error> {
    Err(Error::NotImplemented)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_returns_not_implemented() {
        assert!(matches!(probe(), Err(Error::NotImplemented)));
    }
}
