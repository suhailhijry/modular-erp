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
//! Signup was the first unauthenticated write path and it is bounded by a row
//! per address in the control database. That works because a signup is rare and
//! costs a database. A booking site is read constantly, and a control-plane
//! write per read would be a worse denial of service than the one it prevents.
//!
//! # What this is honestly not
//!
//! **It is per node.** Ten API processes mean ten buckets and ten times the
//! stated rate. That is stated rather than hidden, and the number below is
//! chosen with it in mind.
//!
//! Making it fleet-wide means Redis on the request path, and this codebase
//! treats Redis as a cache with a documented fallback (L6 forbids *degrading a
//! guarantee*; a limit that is per-node is a weaker limit, not a broken one).
//! A shared counter that fails open when Redis is down would be exactly the
//! degradation the law refuses; one that fails closed makes a cache outage an
//! outage. Per-node is the honest third answer.
//!
//! **It is not per person.** Behind a reverse proxy the client address is
//! whatever the proxy says it is, and trusting `X-Forwarded-For` from an
//! untrusted hop lets a caller mint a new identity per request. So the sharper
//! key is the one Phase 12c brings: an API key, which is a thing the caller
//! holds rather than a thing they assert.
//!
//! What is left is still worth having: a bound per (business, origin) that stops
//! one page hammering one tenant, and a bound per business that stops any amount
//! of hammering costing more than one tenant's share.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// Requests per window from one origin to one business.
///
/// Sixty a minute is a person clicking through a booking form quickly, with
/// room to spare; it is not a script. A real site's traffic is many origins'
/// worth of *browsers*, and each browser is its own IP but not its own origin —
/// which is exactly why the per-tenant bound below exists as well.
const PER_CALLER: u32 = 60;

/// And per business, across every caller.
///
/// Deliberately not a multiple of the above: it bounds what one tenant's public
/// surface can cost this node regardless of how many origins are pointed at it.
const PER_TENANT: u32 = 600;

/// The window both are counted over.
const WINDOW: Duration = Duration::from_mins(1);

/// How many buckets are kept before the oldest are dropped.
///
/// Same argument as the control plane's entry cache, and the same resolution:
/// when it is full, what goes is what is closest to expiring, never what is
/// arriving. A limiter that stops tracking new callers when full is a limiter
/// that stops limiting exactly when it is under attack.
const CAPACITY: usize = 20_000;

#[derive(Debug, Clone, Copy)]
struct Window {
    started: Instant,
    seen: u32,
}

/// A fixed-window counter per key.
///
/// Fixed rather than sliding or leaky: a sliding window needs the timestamps
/// kept, and at this key count that is the memory. The known cost of a fixed
/// window is that a caller can send `2 × limit` across a window boundary, which
/// is the difference between a bound and a smooth one — and a bound is what is
/// needed here.
#[derive(Debug)]
pub struct Limiter {
    windows: RwLock<HashMap<String, Window>>,
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new()
    }
}

impl Limiter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            windows: RwLock::new(HashMap::new()),
        }
    }

    /// Whether a public request to this tenant, from this caller, may proceed.
    ///
    /// Both bounds are charged, and **both are charged even when the first one
    /// refuses**, so a caller who is already over their own limit still counts
    /// against the tenant they are hammering. Charging only until the first
    /// refusal would let one blocked caller mask the load they are still
    /// causing.
    ///
    /// `Err` carries the seconds until the window turns over, for `Retry-After`.
    pub fn check(&self, tenant: &str, caller: &str) -> Result<(), u64> {
        let per_caller = self.charge(&format!("{tenant}\u{1f}{caller}"), PER_CALLER);
        let per_tenant = self.charge(tenant, PER_TENANT);
        per_caller.and(per_tenant)
    }

    fn charge(&self, key: &str, limit: u32) -> Result<(), u64> {
        // A poisoned lock means a thread panicked holding it. There is no
        // invariant here a panic could corrupt — the worst case is one counter
        // half-written, which `RwLock` does not permit — so recovering is
        // correct, and refusing every request because of it would not be.
        let mut guard = self
            .windows
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let now = Instant::now();
        let window = guard
            .get(key)
            .copied()
            .filter(|w| now.saturating_duration_since(w.started) < WINDOW);

        let mut window = window.unwrap_or(Window {
            started: now,
            seen: 0,
        });
        window.seen = window.seen.saturating_add(1);

        if guard.len() >= CAPACITY && !guard.contains_key(key) {
            Self::evict(&mut guard, now);
        }
        guard.insert(key.to_owned(), window);

        if window.seen > limit {
            let elapsed = now.saturating_duration_since(window.started);
            return Err(WINDOW.saturating_sub(elapsed).as_secs().max(1));
        }
        Ok(())
    }

    /// Drops expired windows, and failing that the oldest tenth.
    fn evict(windows: &mut HashMap<String, Window>, now: Instant) {
        windows.retain(|_, w| now.saturating_duration_since(w.started) < WINDOW);
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

    #[test]
    fn a_caller_is_bounded_and_told_when_to_come_back() {
        let limiter = Limiter::new();
        for _ in 0..PER_CALLER {
            assert_eq!(limiter.check("acme", "https://salon.example"), Ok(()));
        }

        let retry = limiter
            .check("acme", "https://salon.example")
            .expect_err("the limit did not bind");
        assert!(retry > 0, "Retry-After must be a time a caller can wait");
    }

    /// **A blocked caller still counts against the tenant they are hammering.**
    ///
    /// Charging only until the first refusal would let one caller sit over
    /// their own limit while the load they cause goes unmeasured — so a second
    /// origin would find the tenant's budget untouched.
    #[test]
    fn a_refused_caller_still_costs_the_tenant() {
        let limiter = Limiter::new();
        for _ in 0..PER_TENANT + 10 {
            let _ = limiter.check("acme", "https://noisy.example");
        }

        assert!(
            limiter.check("acme", "https://quiet.example").is_err(),
            "the tenant's bound did not see traffic its caller was refused for"
        );
    }

    /// Two businesses do not share a budget. One tenant's popular booking page
    /// must not close another's.
    #[test]
    fn one_business_cannot_exhaust_anothers_budget() {
        let limiter = Limiter::new();
        for _ in 0..PER_TENANT + 10 {
            let _ = limiter.check("acme", "https://salon.example");
        }

        assert_eq!(
            limiter.check("other", "https://salon.example"),
            Ok(()),
            "one tenant's flood closed another tenant's door"
        );
    }
}
