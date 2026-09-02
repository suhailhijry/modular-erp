//! What a business is called, and what language it writes in.
//!
//! Two facts every message needs and no other module holds. The business name
//! is the `{{ business }}` binding — a reminder that does not say who it is
//! from is a text from an unknown number — and the language is what a template
//! is rendered in when the caller does not say.

use erp_i18n::Locale;
use serde::{Deserialize, Serialize};

/// Where they live.
pub const KEY: &str = "messaging.settings";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// What to sign messages as. **Not the tenant's slug and not their legal
    /// name**: it is what a customer would recognise, which is a third thing.
    pub business: String,
    /// The language messages are written in when nothing says otherwise.
    ///
    /// A tenant-wide default rather than a per-customer preference, because a
    /// Saudi salon writes Arabic to everybody and one setting says so in one
    /// place instead of on ten thousand records. A per-customer field is a
    /// `crm` change nobody has asked for.
    pub language: Locale,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            // Empty rather than a guess. `{{ business }}` then renders as itself
            // — braces and all — which is a template somebody fixes, rather
            // than a message signed "Company".
            business: String::new(),
            language: Locale::DEFAULT,
        }
    }
}
