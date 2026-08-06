//! Deciding which worker looks at which tenant, and when.
//!
//! # The lease is per *visit*, not per tenant
//!
//! The obvious model — a worker owns a shard of tenants and renews forever — is
//! more machinery than the problem needs. Two workers processing the same
//! projection group is already refused by the checkpoint lock (L4), so a lease
//! is not what makes concurrency safe. What it is for is stopping two workers
//! from *opening connections to the same tenant at the same moment* to discover
//! there is nothing to do.
//!
//! So [`claim_tenants`](crate::ControlPlane::claim_tenants) does the claiming
//! and the scheduling in one statement: it returns tenants that are due, marks
//! them as this worker's for the length of one visit, and lets the mark lapse
//! afterwards. Nothing renews, nothing rebalances, and a worker that dies is
//! recovered from by doing nothing.
//!
//! # Why idle tenants are cheap
//!
//! Most tenants are idle most of the time, and the measured sizing rule is
//! `connections ≈ active_tenants × per_tenant_pool` — so a design that visits
//! every tenant constantly makes every tenant active and blows the budget.
//!
//! `next_visit_at` is the throttle. A visit that finds nothing pushes it out by
//! [`WorkSchedule::idle_interval`], and per-tenant pools hold no connection in
//! between (`min = 0`, ten-second idle timeout). At five thousand tenants and a
//! thirty-second interval that is under two hundred short-lived queries a second
//! across the whole fleet.
//!
//! The jitter is not decoration. Without it, a batch claimed together becomes
//! due together, forever — a thundering herd re-forming itself once per
//! interval.

use std::time::Duration;

use spa_types::TenantId;

/// How eagerly a worker revisits tenants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkSchedule {
    /// How long a claim holds a tenant. Only needs to exceed one visit.
    pub lease: Duration,
    /// How long to wait before revisiting a tenant that had nothing to do.
    pub idle_interval: Duration,
    /// Up to this much is added at random to `idle_interval`, so tenants
    /// claimed together do not stay synchronized.
    pub jitter: Duration,
}

impl Default for WorkSchedule {
    fn default() -> Self {
        Self {
            // Comfortably longer than a visit, short enough that a crashed
            // worker's tenants are picked up before anyone notices.
            lease: Duration::from_secs(30),
            idle_interval: Duration::from_secs(30),
            jitter: Duration::from_secs(10),
        }
    }
}

impl WorkSchedule {
    /// The delay before the next visit to a tenant that had nothing to do.
    ///
    /// Jitter comes from the tenant's own id rather than a random source, so the
    /// spread is stable per tenant and this stays a pure function — a worker
    /// restart does not reshuffle everything, and the value is testable.
    #[must_use]
    pub fn next_idle_delay(&self, tenant: TenantId) -> Duration {
        if self.jitter.is_zero() {
            return self.idle_interval;
        }
        // The low bits of a v7 UUID are random — the timestamp lives in the high
        // ones — so a modulus over the whole value spreads evenly.
        let window = self.jitter.as_millis().max(1);
        let spread = tenant.as_uuid().as_u128() % window;
        self.idle_interval + Duration::from_millis(u64::try_from(spread).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_delays_are_spread_but_bounded() {
        let schedule = WorkSchedule {
            idle_interval: Duration::from_secs(30),
            jitter: Duration::from_secs(10),
            ..WorkSchedule::default()
        };

        let delays: Vec<Duration> = (0..64)
            .map(|_| schedule.next_idle_delay(TenantId::new()))
            .collect();

        assert!(
            delays
                .iter()
                .all(|d| *d >= schedule.idle_interval
                    && *d < schedule.idle_interval + schedule.jitter),
            "every delay stays inside the window"
        );
        // The property that matters: not all the same. A constant here would
        // mean the herd re-forms every interval.
        let distinct: std::collections::HashSet<_> = delays.iter().collect();
        assert!(
            distinct.len() > 32,
            "got {} distinct delays",
            distinct.len()
        );
    }

    #[test]
    fn a_tenants_jitter_does_not_move() {
        let schedule = WorkSchedule::default();
        let tenant = TenantId::new();
        assert_eq!(
            schedule.next_idle_delay(tenant),
            schedule.next_idle_delay(tenant),
            "jitter is derived from the id, so a restart does not reshuffle"
        );
    }

    #[test]
    fn zero_jitter_is_allowed_for_tests_that_need_determinism() {
        let schedule = WorkSchedule {
            jitter: Duration::ZERO,
            idle_interval: Duration::from_millis(5),
            ..WorkSchedule::default()
        };
        assert_eq!(
            schedule.next_idle_delay(TenantId::new()),
            Duration::from_millis(5)
        );
    }
}
