//! Choosing which cluster a new tenant lands on.
//!
//! # What capacity means here
//!
//! The soak test settled this, and it is not what one would guess. Open
//! connections are bounded by
//!
//! ```text
//! concurrently_active_tenants × max_connections_per_tenant
//! ```
//!
//! — not by the lane budget, and not by request rate. So the question "can this
//! cluster take another tenant" is really "how many of its tenants are busy at
//! once", and a cluster holding ten thousand dormant tenants may have more room
//! than one holding two hundred busy ones.
//!
//! Two limits follow, and they answer different questions:
//!
//! | limit | bounds | binds when |
//! |---|---|---|
//! | `max_active_tenants` | connections | tenants are busy |
//! | `max_databases` | storage, migration time, catalog size | tenants are numerous |
//!
//! Placement respects both. At the scale in view — 5,000 tenants, 25% concurrent
//! — they happen to give similar answers. They will not always, and when they
//! diverge the first one is the one that takes a cluster down.

use serde::{Deserialize, Serialize};

use crate::AccessError;

/// Whether a cluster will accept new tenants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterStatus {
    /// Accepting placements.
    Available,
    /// Serving its tenants but taking no new ones. The state for a cluster
    /// being retired, or one an operator wants to stop growing.
    Draining,
    /// At capacity. Distinct from `Draining` so an operator can tell "I stopped
    /// this" from "this filled up".
    Full,
    /// Not serving. Its tenants are unreachable — a state to alert on, not to
    /// place into.
    Offline,
}

impl ClusterStatus {
    #[must_use]
    pub const fn accepts_placements(self) -> bool {
        matches!(self, Self::Available)
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Draining => "draining",
            Self::Full => "full",
            Self::Offline => "offline",
        }
    }

    pub(crate) fn parse(raw: &str) -> Result<Self, AccessError> {
        match raw {
            "available" => Ok(Self::Available),
            "draining" => Ok(Self::Draining),
            "full" => Ok(Self::Full),
            "offline" => Ok(Self::Offline),
            other => Err(AccessError::Corrupt(format!(
                "cluster.status: unknown value {other:?}"
            ))),
        }
    }
}

/// A cluster and what it is currently carrying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterLoad {
    pub name: String,
    pub status: ClusterStatus,
    /// Tenants placed here and not deleted. Bounded by `max_databases`.
    pub live_tenants: i64,
    /// Tenants in `active` status. The proxy for concurrent activity, and the
    /// input to the limit that actually matters.
    pub active_tenants: i64,
    pub max_active_tenants: i64,
    pub max_databases: i64,
    /// Tie-break among otherwise-equal clusters. Higher wins.
    pub weight: i32,
}

impl ClusterLoad {
    /// Fraction of the binding limit consumed, in basis points — 10,000 is 100%.
    ///
    /// Integer rather than a float for the same reason money is: `float_arithmetic`
    /// is denied workspace-wide, comparison is then exact, and there is no
    /// rounding to reason about when two clusters are close.
    ///
    /// The *maximum* of the two ratios, not an average: a cluster 20% full on
    /// storage and 99% full on active tenants is 99% full, and averaging would
    /// place a tenant onto it.
    #[must_use]
    pub fn utilization_bp(&self) -> i64 {
        let by_activity = ratio_bp(self.active_tenants, self.max_active_tenants);
        let by_storage = ratio_bp(self.live_tenants, self.max_databases);
        by_activity.max(by_storage)
    }

    /// Whether this cluster can take one more tenant right now.
    #[must_use]
    pub fn has_room(&self) -> bool {
        self.status.accepts_placements()
            && self.active_tenants < self.max_active_tenants
            && self.live_tenants < self.max_databases
    }
}

/// Basis points of `limit` consumed by `used`.
fn ratio_bp(used: i64, limit: i64) -> i64 {
    if limit <= 0 {
        // A non-positive limit means "no room", not "infinite room". The schema
        // forbids it; this is the belt to that suspenders.
        return i64::MAX;
    }
    used.saturating_mul(10_000) / limit
}

/// How to pick among clusters with room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlacementPolicy {
    /// Least-utilized first, weight breaking ties.
    ///
    /// The default because it spreads activity, and activity is what binds. The
    /// alternative — fill one cluster before starting the next — concentrates
    /// busy tenants and reaches the connection ceiling on one box while others
    /// idle.
    #[default]
    Balanced,
    /// Fill the highest-weighted cluster with room before moving on.
    ///
    /// For deliberately consolidating onto fewer machines: cheaper, and
    /// reasonable while tenants are mostly dormant. Riskier once they are not.
    Packed,
}

impl PlacementPolicy {
    /// Picks a cluster, or `None` when every one is full or unavailable.
    #[must_use]
    pub fn choose(self, clusters: &[ClusterLoad]) -> Option<&ClusterLoad> {
        let candidates = clusters.iter().filter(|c| c.has_room());
        match self {
            Self::Balanced => candidates.min_by(|a, b| {
                a.utilization_bp()
                    .cmp(&b.utilization_bp())
                    // Higher weight wins a tie, so `b` before `a`.
                    .then_with(|| b.weight.cmp(&a.weight))
                    // Name last, so the choice is deterministic and a test can
                    // assert it rather than accepting whatever order the
                    // database returned.
                    .then_with(|| a.name.cmp(&b.name))
            }),
            Self::Packed => candidates.max_by(|a, b| {
                a.weight
                    .cmp(&b.weight)
                    .then_with(|| a.utilization_bp().cmp(&b.utilization_bp()))
                    .then_with(|| b.name.cmp(&a.name))
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster(name: &str, active: i64, live: i64) -> ClusterLoad {
        ClusterLoad {
            name: name.to_owned(),
            status: ClusterStatus::Available,
            active_tenants: active,
            live_tenants: live,
            max_active_tenants: 100,
            max_databases: 700,
            weight: 100,
        }
    }

    #[test]
    fn utilization_takes_the_binding_limit_not_the_average() {
        // 20% of storage, 99% of activity. Averaging would call this 60% full
        // and place a tenant onto a cluster about to run out of connections.
        let c = cluster("a", 99, 140);
        assert_eq!(c.utilization_bp(), 9_900, "99% of the activity limit");
    }

    #[test]
    fn a_cluster_is_full_when_either_limit_is_reached() {
        assert!(cluster("a", 99, 699).has_room());
        assert!(
            !cluster("a", 100, 10).has_room(),
            "activity limit must bind"
        );
        assert!(!cluster("a", 10, 700).has_room(), "storage limit must bind");
    }

    #[test]
    fn only_available_clusters_accept_placements() {
        for status in [
            ClusterStatus::Draining,
            ClusterStatus::Full,
            ClusterStatus::Offline,
        ] {
            let c = ClusterLoad {
                status,
                ..cluster("a", 0, 0)
            };
            assert!(!c.has_room(), "{status:?} must not accept placements");
        }
    }

    #[test]
    fn balanced_placement_picks_the_least_loaded() {
        let clusters = vec![
            cluster("a", 80, 100),
            cluster("b", 10, 100),
            cluster("c", 50, 100),
        ];
        assert_eq!(
            PlacementPolicy::Balanced.choose(&clusters).unwrap().name,
            "b"
        );
    }

    #[test]
    fn balanced_placement_breaks_ties_by_weight_then_name() {
        let clusters = vec![
            ClusterLoad {
                weight: 50,
                ..cluster("a", 10, 100)
            },
            ClusterLoad {
                weight: 200,
                ..cluster("b", 10, 100)
            },
        ];
        assert_eq!(
            PlacementPolicy::Balanced.choose(&clusters).unwrap().name,
            "b",
            "equal load should favour the heavier weight"
        );

        // Fully equal: deterministic by name, so placement is reproducible.
        let clusters = vec![cluster("z", 10, 100), cluster("a", 10, 100)];
        assert_eq!(
            PlacementPolicy::Balanced.choose(&clusters).unwrap().name,
            "a"
        );
    }

    #[test]
    fn packed_placement_fills_the_heaviest_cluster_first() {
        let clusters = vec![
            ClusterLoad {
                weight: 200,
                ..cluster("a", 80, 100)
            },
            ClusterLoad {
                weight: 100,
                ..cluster("b", 10, 100)
            },
        ];
        assert_eq!(
            PlacementPolicy::Packed.choose(&clusters).unwrap().name,
            "a",
            "packed should keep filling the heavier cluster even though it is busier"
        );
    }

    #[test]
    fn no_cluster_is_chosen_when_all_are_full() {
        let clusters = vec![cluster("a", 100, 100), cluster("b", 10, 700)];
        assert!(PlacementPolicy::Balanced.choose(&clusters).is_none());
        assert!(PlacementPolicy::Packed.choose(&clusters).is_none());
        assert!(PlacementPolicy::Balanced.choose(&[]).is_none());
    }

    #[test]
    fn a_nonpositive_limit_means_no_room_not_infinite_room() {
        let broken = ClusterLoad {
            max_active_tenants: 0,
            ..cluster("a", 0, 0)
        };
        assert!(!broken.has_room());
        assert_eq!(broken.utilization_bp(), i64::MAX);
    }
}
