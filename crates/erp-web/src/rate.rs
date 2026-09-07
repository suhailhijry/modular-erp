//! What bounds a surface that has nobody to blame.
//!
//! # Why this exists now, and did not before
//!
//! Every other write path in this system belongs to a session, so abuse has a
//! name and the answer to it is to suspend that identity. The public surface
//! has no session by design — that is what makes it public — so the only thing
//! left to scope a limit by is where the request appears to come from and which
//! business it is reaching.
//!
//! # What was wrong with the first version, and what this fixes
//!
//! The first limiter keyed a caller on the `Origin` header, which is whatever
//! the client chooses to send, so a flood rotated it and had a fresh budget per
//! request. It lived in one process, so ten API nodes were ten limiters. And it
//! guarded only the public booking surface: login, signup, invitation acceptance
//! and one-time codes had no limiter at all, which made every one of them a
//! password oracle with Argon2 attached and made the OTP route an SMS bill
//! somebody else pays.
//!
//! So, three changes, each closing one of those:
//!
//! - **The caller is an address.** [`crate::extract::caller_address`] reads the
//!   last hop of `X-Forwarded-For` when the deployment says its proxy can be
//!   trusted, and the socket's peer address otherwise. Neither is a header the
//!   caller writes.
//! - **The count is shared.** When the control plane has its Redis layer, every
//!   node charges the same counter. When Redis is unreachable this falls back to
//!   the per-node count — a weaker limit rather than no limit (L6), and a cache
//!   outage rather than an API outage.
//! - **Every unauthenticated route is bounded**, because
//!   [`crate::extract::Anonymous`] is the extractor they all take and
//!   `every_public_route_is_rate_limited` hammers each of them until it sees a
//!   429. A route added without one fails the build.
//!
//! # The numbers
//!
//! [`PUBLIC_PER_CALLER`] and [`PUBLIC_PER_TENANT`] bound the booking site:
//! generous, because a page that loads availability for a week makes many
//! requests. [`AUTH_PER_CALLER`] and [`AUTH_PER_HANDLE`] bound password and code
//! verification: tight, because a person mistypes a password twice and a
//! program tries ten thousand. [`CODES_PER_CALLER`] and [`CODES_PER_PLATFORM`]
//! bound how many texts one address, and the whole platform, can cause in an
//! hour — the second one is the circuit breaker for the fraud where an
//! attacker's own premium numbers receive the codes.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// How many, in how long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub count: u32,
    pub window: Duration,
}

impl Limit {
    #[must_use]
    pub const fn per_minute(count: u32) -> Self {
        Self {
            count,
            window: Duration::from_mins(1),
        }
    }

    #[must_use]
    pub const fn per_hour(count: u32) -> Self {
        Self {
            count,
            window: Duration::from_hours(1),
        }
    }
}

/// Requests one caller may make to one business's public surface in a minute.
pub const PUBLIC_PER_CALLER: Limit = Limit::per_minute(60);

/// Requests one business's public surface answers in a minute, from everybody.
/// Ten callers' worth: a flood from many addresses is still a flood.
pub const PUBLIC_PER_TENANT: Limit = Limit::per_minute(600);

/// Attempts one address may make against the authentication surface in a
/// minute — logins, signups, invitation acceptances, code requests and code
/// verifications together. A person needs a handful; a program wants millions.
pub const AUTH_PER_CALLER: Limit = Limit::per_minute(10);

/// Attempts anybody may make against **one account** in a minute, whatever
/// address they come from. This is what stops a distributed guess at one
/// person's password; the per-caller bound above stops one address guessing at
/// everybody's.
pub const AUTH_PER_HANDLE: Limit = Limit::per_minute(5);

/// One-time codes one address may cause to be sent in an hour. Each is a text
/// somebody pays for, and a legitimate caller needs one or two.
pub const CODES_PER_CALLER: Limit = Limit::per_hour(5);

/// One-time codes the whole platform sends in an hour. **A circuit breaker,
/// not a budget**: the attack this stops is a caller with a block of premium
/// numbers, each receiving codes at the per-number cooldown, and the only
/// number that bounds *that* is the total. Sized so a real fleet never reaches
/// it and a fraud does within minutes.
pub const CODES_PER_PLATFORM: Limit = Limit::per_hour(2_000);

/// One address's requests inside the current window.
#[derive(Debug, Clone, Copy)]
struct Window {
    started: Instant,
    length: Duration,
    seen: u32,
}

impl Window {
    fn is_live(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) < self.length
    }
}

/// Keys this will hold before it starts forgetting the oldest. A key is a few
/// dozen bytes, so this is a few megabytes, and a flood of distinct addresses
/// evicts the quiet ones first — which is the right thing to forget.
const CAPACITY: usize = 20_000;

/// Fixed windows per key, shared across nodes when the control plane has a
/// Redis layer and counted per node when it does not.
#[derive(Debug)]
pub struct Limiter {
    windows: RwLock<HashMap<String, Window>>,
    shared: Option<erp_control::shared::Shared>,
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new()
    }
}

impl Limiter {
    /// A per-node limiter. What a single process, or a test, gets.
    #[must_use]
    pub fn new() -> Self {
        Self {
            windows: RwLock::new(HashMap::new()),
            shared: None,
        }
    }

    /// A limiter every node agrees with, falling back to per-node counting
    /// when Redis cannot be reached.
    #[must_use]
    pub fn sharing(shared: Option<erp_control::shared::Shared>) -> Self {
        Self {
            windows: RwLock::new(HashMap::new()),
            shared,
        }
    }

    /// Whether counts are fleet-wide.
    #[must_use]
    pub const fn is_shared(&self) -> bool {
        self.shared.is_some()
    }

    /// One request to a business's public surface, from `caller`.
    ///
    /// Two bounds, both charged: the caller's, so one page cannot hammer one
    /// shop, and the tenant's, so no amount of hammering costs more than one
    /// shop's share. A caller refused on the first still costs the second,
    /// because the requests did arrive.
    pub async fn check(&self, tenant: &str, caller: &str) -> Result<(), u64> {
        let per_caller = self
            .charge(&format!("public:{tenant}\u{1f}{caller}"), PUBLIC_PER_CALLER)
            .await;
        let per_tenant = self
            .charge(&format!("public:{tenant}"), PUBLIC_PER_TENANT)
            .await;
        per_caller.and(per_tenant)
    }

    /// One request against `key`, under `limit`.
    ///
    /// `Err(seconds)` is how long until the window ends — what the caller is
    /// told in the 429. Shared when it can be, per-node when Redis is not there
    /// to ask; the fallback is logged once per failure so a limiter that has
    /// quietly become per-node is visible in the logs.
    pub async fn charge(&self, key: &str, limit: Limit) -> Result<(), u64> {
        if let Some(shared) = &self.shared {
            match shared.charge(key, limit.count, limit.window).await {
                Ok(verdict) => return verdict,
                Err(e) => {
                    tracing::warn!(error = %e, "shared rate limiter unreachable; counting per node");
                }
            }
        }
        self.charge_locally(key, limit)
    }

    /// The per-node count. Public so a test can exercise the fallback without a
    /// Redis, and so [`Self::charge`] has one place to fall back to.
    pub fn charge_locally(&self, key: &str, limit: Limit) -> Result<(), u64> {
        let mut guard = self
            .windows
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();

        let mut window = guard
            .get(key)
            .copied()
            .filter(|w| w.is_live(now))
            .unwrap_or(Window {
                started: now,
                length: limit.window,
                seen: 0,
            });
        window.seen = window.seen.saturating_add(1);

        if guard.len() >= CAPACITY && !guard.contains_key(key) {
            Self::evict(&mut guard, now);
        }
        guard.insert(key.to_owned(), window);

        if window.seen > limit.count {
            let elapsed = now.saturating_duration_since(window.started);
            return Err(limit.window.saturating_sub(elapsed).as_secs().max(1));
        }
        Ok(())
    }

    /// Drops expired windows; if that is not enough, the oldest tenth.
    fn evict(windows: &mut HashMap<String, Window>, now: Instant) {
        windows.retain(|_, w| w.is_live(now));
        if windows.len() < CAPACITY {
            return;
        }
        let mut ages: Vec<_> = windows
            .iter()
            .map(|(k, w)| (w.started, k.clone()))
            .collect();
        let count = (CAPACITY / 10 + 1).min(ages.len());
        ages.select_nth_unstable_by_key(count.saturating_sub(1), |(at, _)| *at);
        for (_, key) in ages.into_iter().take(count) {
            windows.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_caller_is_bounded_and_told_when_to_come_back() {
        let limiter = Limiter::new();
        for _ in 0..PUBLIC_PER_CALLER.count {
            assert_eq!(limiter.check("acme", "203.0.113.9").await, Ok(()));
        }
        let retry = limiter
            .check("acme", "203.0.113.9")
            .await
            .expect_err("the limit did not bind");
        assert!(retry > 0, "Retry-After must be a time a caller can wait");
    }

    #[tokio::test]
    async fn a_refused_caller_still_costs_the_tenant() {
        let limiter = Limiter::new();
        for _ in 0..PUBLIC_PER_TENANT.count + 10 {
            let _ = limiter.check("acme", "198.51.100.7").await;
        }
        assert!(
            limiter.check("acme", "198.51.100.8").await.is_err(),
            "the tenant's bound did not see traffic its caller was refused for"
        );
    }

    #[tokio::test]
    async fn one_business_cannot_exhaust_anothers_budget() {
        let limiter = Limiter::new();
        for _ in 0..PUBLIC_PER_TENANT.count + 10 {
            let _ = limiter.check("acme", "203.0.113.9").await;
        }
        assert_eq!(
            limiter.check("other", "203.0.113.9").await,
            Ok(()),
            "one tenant's flood closed another tenant's door"
        );
    }

    /// **Different limits are different keys.** A caller who has spent their
    /// authentication budget still has their public one, and the reverse:
    /// otherwise a flood at the booking page would lock the shop's own staff
    /// out of logging in from the same office address.
    #[test]
    fn limits_are_counted_apart() {
        let limiter = Limiter::new();
        for _ in 0..AUTH_PER_CALLER.count {
            assert_eq!(
                limiter.charge_locally("auth:203.0.113.9", AUTH_PER_CALLER),
                Ok(())
            );
        }
        assert!(
            limiter
                .charge_locally("auth:203.0.113.9", AUTH_PER_CALLER)
                .is_err()
        );
        assert_eq!(
            limiter.charge_locally("public:acme\u{1f}203.0.113.9", PUBLIC_PER_CALLER),
            Ok(()),
            "spending the login budget must not spend the public one"
        );
    }

    /// The window is the limit's, not a global minute: an hourly limit stays
    /// exhausted past the first minute.
    #[test]
    fn a_window_is_as_long_as_its_limit_says() {
        let limiter = Limiter::new();
        for _ in 0..CODES_PER_CALLER.count {
            assert_eq!(
                limiter.charge_locally("codes:203.0.113.9", CODES_PER_CALLER),
                Ok(())
            );
        }
        let wait = limiter
            .charge_locally("codes:203.0.113.9", CODES_PER_CALLER)
            .expect_err("bounded");
        assert!(
            wait > 60,
            "an hourly limit told the caller to come back in {wait}s"
        );
    }

    /// The map cannot grow without bound under a flood of distinct addresses.
    #[test]
    fn the_table_forgets_rather_than_growing() {
        let limiter = Limiter::new();
        for n in 0..(CAPACITY + 500) {
            let _ = limiter.charge_locally(&format!("public:t\u{1f}{n}"), PUBLIC_PER_CALLER);
        }
        let held = limiter
            .windows
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        assert!(held <= CAPACITY + 1, "held {held} windows");
    }
}
