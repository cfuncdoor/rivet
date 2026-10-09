//! Dependency resolver producing `rivet.lock.json`.
//!
//! M4 milestone. In M1 this crate exposes the final error type and a stub
//! [`resolve`] that returns [`Error::NotImplemented`].

use thiserror::Error;

/// Resolver errors.
#[derive(Debug, Error)]
pub enum Error {
    /// Feature reserved for a later milestone.
    #[error("dependency resolution is not implemented yet (planned for M4)")]
    NotImplemented,
}

/// Resolve dependencies (M4 stub).
pub fn resolve() -> Result<(), Error> {
    Err(Error::NotImplemented)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_returns_not_implemented() {
        assert!(matches!(resolve(), Err(Error::NotImplemented)));
    }
}
