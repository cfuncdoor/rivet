//! rivet binary entry point (thin wrapper over rivet-cli).

use anyhow::Result;

fn main() -> Result<()> {
    rivet_cli::run()
}
