//! Package registry client (index format, fetch, checksum verify, publish).
//!
//! M5 milestone. In M1 this crate exposes the final error type and a stub.

use thiserror::Error;

/// Registry errors.
#[derive(Debug, Error)]
pub enum Error {
    /// Feature reserved for a later milestone.
    #[error("registry support is not implemented yet (planned for M5)")]
    NotImplemented,
}

/// Registry entry point (M5 stub).
pub fn fetch() -> Result<(), Error> {
    Err(Error::NotImplemented)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_returns_not_implemented() {
        assert!(matches!(fetch(), Err(Error::NotImplemented)));
    }
}
