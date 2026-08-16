//! What every handler shares.

use std::sync::Arc;

use spa_control::ControlPlane;

/// Where a request lands, and which tenant it is for.
#[derive(Debug, Clone)]
pub struct AppState {
    pub control: Arc<ControlPlane>,
    /// The domain tenants are subdomains of — `spa.com`, so Bassat Media
    /// Productions is at `bassat.spa.com`.
    ///
    /// Configuration rather than a constant because it differs per deployment
    /// and, more to the point, per developer: `acme.localhost` resolves without
    /// touching `/etc/hosts` in every browser and in curl, which is what makes
    /// running this locally bearable.
    pub domain: Arc<str>,
}

impl AppState {
    /// For local work and tests, where tenants live under `.localhost`.
    #[must_use]
    pub fn new(control: Arc<ControlPlane>) -> Self {
        Self::on(control, "localhost")
    }

    #[must_use]
    pub fn on(control: Arc<ControlPlane>, domain: &str) -> Self {
        Self {
            control,
            domain: domain.trim().trim_start_matches('.').to_lowercase().into(),
        }
    }
}
