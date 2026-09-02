//! What every handler shares.

use std::sync::Arc;

use erp_control::ControlPlane;

/// Where a request lands, and which tenant it is for.
#[derive(Debug, Clone)]
pub struct AppState {
    pub control: Arc<ControlPlane>,
    /// The domain tenants are subdomains of — `erp.com`, so Bassat Media
    /// Productions is at `bassat.erp.com`.
    ///
    /// Configuration rather than a constant because it differs per deployment
    /// and, more to the point, per developer: `acme.localhost` resolves without
    /// touching `/etc/hosts` in every browser and in curl, which is what makes
    /// running this locally bearable.
    pub domain: Arc<str>,
    /// The key module secrets are sealed under.
    ///
    /// `None` when the deployment has not configured one, and then anything
    /// that would store a secret **refuses** rather than storing it in the
    /// clear (law L6). A tenant's ZATCA signing key is the first thing this
    /// protects; there is no version of it that is safe to keep unsealed
    /// because an environment variable was missing.
    pub sealing: Option<erp_eventlog::SealingKey>,
    /// What bounds the public surface, which has no session to attribute abuse
    /// to. Shared across every request this process serves — see
    /// [`crate::rate`] for what it is and honestly is not.
    pub limiter: Arc<crate::rate::Limiter>,
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
            sealing: None,
            limiter: Arc::new(crate::rate::Limiter::new()),
        }
    }

    /// The same state, able to seal secrets.
    #[must_use]
    pub fn sealing_with(mut self, sealing: erp_eventlog::SealingKey) -> Self {
        self.sealing = Some(sealing);
        self
    }
}
