//! tamias-deploy — the deploy port: provision a remote registry.

use std::path::Path;

use tamias_core::Error;

/// A registry deployer — materializes the deployable artifacts (Worker,
/// R2 binding, secret setup) so an operator can ship a hosted registry.
pub trait Deploy {
    /// Write the deployable artifacts under `out`; return the next steps.
    ///
    /// # Errors
    /// Propagates write failures.
    fn deploy(&self, out: &Path) -> Result<Vec<String>, Error>;
}
