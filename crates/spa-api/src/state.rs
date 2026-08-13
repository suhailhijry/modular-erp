//! What every handler shares.

use std::sync::Arc;

use spa_control::ControlPlane;

#[derive(Debug, Clone)]
pub struct AppState {
    pub control: Arc<ControlPlane>,
}

impl AppState {
    #[must_use]
    pub const fn new(control: Arc<ControlPlane>) -> Self {
        Self { control }
    }
}
