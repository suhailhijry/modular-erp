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
    /// What bounds every surface that has no session to attribute abuse to:
    /// the public booking site, the login and signup routes, one-time codes.
    /// Fleet-wide when the control plane has its Redis layer — see
    /// [`crate::rate`].
    pub limiter: Arc<crate::rate::Limiter>,
    /// **Whether the last hop of `X-Forwarded-For` is the client.** True only
    /// when a proxy this deployment controls sits in front and appends the peer
    /// address; false means the socket's own peer is the client and the header
    /// is ignored, because a header the caller can write is not an identity.
    /// See [`crate::extract::caller_address`].
    pub trust_forwarded: bool,
    /// Where files are kept.
    ///
    /// `None` when the deployment has configured no storage, and then anything
    /// that would keep a file **refuses** rather than dropping it — the same
    /// call [`AppState::sealing`] makes. A tenant told their contract uploaded
    /// when it went nowhere is worse served than one told it did not.
    ///
    /// An `Arc<dyn Storage>` rather than a concrete engine because **the tenant
    /// chooses** (D15): a business that keeps its own documents is the reason
    /// some of them can buy this at all, and that is a deployment fact this
    /// crate must not have an opinion about.
    pub storage: Option<Arc<dyn erp_storage::Storage>>,
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
            domain: domain.trim().trim_start_matches('.').to_lowercase().into(),
            sealing: None,
            // Fleet-wide when the control plane has its Redis layer, per node
            // otherwise — the same fallback the session cache makes.
            limiter: Arc::new(crate::rate::Limiter::sharing(control.shared().cloned())),
            control,
            storage: None,
            trust_forwarded: false,
        }
    }

    /// Reads the client address from the last hop of `X-Forwarded-For`.
    ///
    /// **Only behind a proxy you run.** With this on and no proxy, a caller
    /// writes the header and is whoever they say — which is the failure the
    /// address-keyed limiter exists to prevent.
    #[must_use]
    pub const fn trusting_forwarded_for(mut self, trust: bool) -> Self {
        self.trust_forwarded = trust;
        self
    }

    /// The same state, with somewhere to keep files.
    #[must_use]
    pub fn storing_in(mut self, storage: Arc<dyn erp_storage::Storage>) -> Self {
        self.storage = Some(storage);
        self
    }

    /// The same state, able to seal secrets.
    #[must_use]
    pub fn sealing_with(mut self, sealing: erp_eventlog::SealingKey) -> Self {
        self.sealing = Some(sealing);
        self
    }
}
