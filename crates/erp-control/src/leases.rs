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

use erp_types::TenantId;

/// A tenant a worker has taken, and how long it has been quiet.
///
/// The streak is deliberately **not** on [`Tenant`](crate::Tenant): how many
/// times a scheduler has looked at something and found nothing is the
/// scheduler's business, and a domain model that carried it would invite code
/// that read it for some other purpose.
#[derive(Debug, Clone)]
pub struct Claimed {
    pub tenant: crate::Tenant,
    /// Consecutive visits that found nothing, **before** this one.
    pub idle_visits: i32,
}

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
    /// The ceiling the idle interval backs off to.
    ///
    /// **This is what makes a dormant tenant nearly free.** Without it the
    /// interval is a constant, so five thousand tenants cost 167 visits a second
    /// for ever whether or not any of them is doing anything. With it, a tenant
    /// that has been quiet for a day is asked a handful of times a day.
    ///
    /// Bounded rather than unbounded, because a tenant is never *certainly*
    /// finished: a scheduled effect, a lapsed lease, an outbox row a previous
    /// worker died holding. Something has to come back and look eventually, and
    /// the cap is how often eventually is.
    pub max_idle_interval: Duration,
}

impl Default for WorkSchedule {
    fn default() -> Self {
        Self {
            // Comfortably longer than a visit, short enough that a crashed
            // worker's tenants are picked up before anyone notices.
            lease: Duration::from_secs(30),
            idle_interval: Duration::from_secs(30),
            jitter: Duration::from_secs(10),
            // Six hours. Long enough that the standing cost of a dormant fleet
            // rounds to nothing, short enough that anything a visit is the only
            // cure for is fixed the same working day.
            max_idle_interval: Duration::from_hours(6),
        }
    }
}

impl WorkSchedule {
    /// The delay before the next visit to a tenant that had nothing to do.
    ///
    /// Jitter comes from the tenant's own id rather than a random source, so the
    /// spread is stable per tenant and this stays a pure function — a worker
    /// restart does not reshuffle everything, and the value is testable.
    /// `idle_visits` is how many consecutive visits have found nothing, so a
    /// tenant that just worked passes 0 and is back on the base interval at
    /// once. Doubling from there, capped at [`Self::max_idle_interval`].
    #[must_use]
    pub fn next_idle_delay(&self, tenant: TenantId, idle_visits: i32) -> Duration {
        // Saturating rather than wrapping: a tenant idle for a month has a large
        // count, and `1 << 40` is not a duration anybody meant.
        let doublings = u32::try_from(idle_visits.max(0))
            .unwrap_or(u32::MAX)
            .min(24);
        let backed_off = self
            .idle_interval
            .saturating_mul(1_u32.checked_shl(doublings).unwrap_or(u32::MAX))
            .min(self.max_idle_interval);

        if self.jitter.is_zero() {
            return backed_off;
        }

        // The low bits of a v7 UUID are random — the timestamp lives in the high
        // ones — so a modulus over the whole value spreads evenly.
        //
        // Jitter scales with the interval it spreads. A fixed ten seconds across
        // a six-hour interval leaves five thousand tenants landing in the same
        // ten-second window every six hours, which is a thundering herd with a
        // long fuse.
        let window = self
            .jitter
            .saturating_mul(
                u32::try_from(backed_off.as_secs().max(1) / self.idle_interval.as_secs().max(1))
                    .unwrap_or(1),
            )
            .as_millis()
            .max(1);
        let spread = tenant.as_uuid().as_u128() % window;
        backed_off + Duration::from_millis(u64::try_from(spread).unwrap_or(0))
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
            .map(|_| schedule.next_idle_delay(TenantId::new(), 0))
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
            schedule.next_idle_delay(tenant, 0),
            schedule.next_idle_delay(tenant, 0),
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
            schedule.next_idle_delay(TenantId::new(), 0),
            Duration::from_millis(5)
        );
    }

    /// **A tenant that has nothing to do stops being asked so often.**
    ///
    /// The interval was a constant, so five thousand tenants cost 167 visits a
    /// second for ever whether any of them was doing anything or not. Every one
    /// of those opens a connection, runs each enabled module's projection query,
    /// and writes a row back.
    #[test]
    fn a_quiet_tenant_backs_off_and_a_busy_one_does_not() {
        let schedule = WorkSchedule {
            idle_interval: Duration::from_secs(30),
            jitter: Duration::ZERO,
            max_idle_interval: Duration::from_hours(6),
            ..WorkSchedule::default()
        };
        let tenant = TenantId::new();
        let at = |idle| schedule.next_idle_delay(tenant, idle);

        // A tenant that just worked is back on the base interval at once, which
        // is what makes the backoff invisible to anybody using the system.
        assert_eq!(at(0), Duration::from_secs(30));
        assert_eq!(at(1), Duration::from_mins(1));
        assert_eq!(at(2), Duration::from_mins(2));

        // And it stops somewhere. A tenant is never *certainly* finished — a
        // lapsed lease, an outbox row a dead worker was holding — so something
        // has to come back eventually.
        assert_eq!(at(20), Duration::from_hours(6));
        assert_eq!(
            at(1_000),
            Duration::from_hours(6),
            "and never grows past it"
        );
        assert_eq!(
            at(i32::MAX),
            Duration::from_hours(6),
            "including absurd counts"
        );

        // A negative count would be corrupt data; treat it as fresh rather than
        // panicking on a subtraction nobody meant.
        assert_eq!(at(-5), Duration::from_secs(30));
    }

    /// **Jitter has to scale with the interval it spreads.**
    ///
    /// Ten seconds of spread across a six-hour interval leaves five thousand
    /// tenants landing in the same ten-second window every six hours — a
    /// thundering herd with a long fuse, and one that only appears in
    /// production.
    #[test]
    fn the_spread_grows_with_the_interval() {
        let schedule = WorkSchedule {
            idle_interval: Duration::from_secs(30),
            jitter: Duration::from_secs(10),
            max_idle_interval: Duration::from_hours(6),
            ..WorkSchedule::default()
        };

        let spread = |idle| {
            let delays: Vec<Duration> = (0..256)
                .map(|_| schedule.next_idle_delay(TenantId::new(), idle))
                .collect();
            let low = delays.iter().min().copied().unwrap_or_default();
            let high = delays.iter().max().copied().unwrap_or_default();
            high.saturating_sub(low)
        };

        let fresh = spread(0);
        let dormant = spread(20);
        assert!(
            dormant > fresh * 10,
            "a six-hour interval spread over {dormant:?} is barely wider than \
             the thirty-second one's {fresh:?}; the herd re-forms"
        );
    }
}
