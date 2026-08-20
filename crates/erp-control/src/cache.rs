//! A short-lived cache for the lookups on the entry path.
//!
//! # Why this is not optional
//!
//! [`ControlPlane::enter`](crate::ControlPlane::enter) asks four questions:
//! is the identity active, does the tenant exist and is it enterable, is there a
//! live membership, and which modules are on. Uncached, that is four queries per
//! request against a single control database:
//!
//! | requests/second | control-plane queries/second |
//! |---|---|
//! | 2,000 | 8,000 |
//! | 10,000 | 40,000 |
//! | 25,000 | 100,000 |
//!
//! No amount of connection tuning survives the second row. The control plane is
//! one database and cannot be sharded the way tenant data is, so the entry path
//! has to be nearly free.
//!
//! # What is traded away
//!
//! Freshness, bounded by the TTL. A suspension or a revoked membership takes
//! effect within `ttl` on nodes that did not perform it, and immediately on the
//! node that did (writes invalidate locally).
//!
//! **That window is a deliberate, documented security property, not an
//! oversight.** A five-second TTL means a revoked user may complete requests for
//! up to five more seconds. If a shorter bound is ever required, the answer is
//! not a smaller TTL — it is out-of-band invalidation, which is a Phase 3
//! decision recorded in the implementation notes.
//!
//! Nothing security-critical is cached *longer* than the TTL, and nothing is
//! cached negatively for longer either: a failed lookup is not stored, so
//! granting access takes effect at once.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// A read-mostly TTL cache.
///
/// `RwLock<HashMap>` rather than a sharded or lock-free structure: the write
/// path is a cache miss, which is rare by construction, and reads are parallel.
/// The soak test measures whether that holds under load — if the lock ever
/// shows up in a profile, this is the type to replace, and it is small enough
/// that doing so is a contained change.
#[derive(Debug)]
pub(crate) struct TtlCache<K, V> {
    entries: RwLock<HashMap<K, Entry<V>>>,
    ttl: Duration,
    capacity: usize,
}

#[derive(Debug, Clone)]
struct Entry<V> {
    value: V,
    stored_at: Instant,
}

impl<K, V> TtlCache<K, V>
where
    K: std::hash::Hash + Eq + Clone,
    V: Clone,
{
    pub(crate) fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            ttl,
            capacity,
        }
    }

    pub(crate) fn get(&self, key: &K) -> Option<V> {
        // A poisoned lock means another thread panicked while holding it. The
        // cache holds no invariant that a panic could corrupt — worst case an
        // entry is half-written, which cannot happen under `RwLock` — so
        // recovering is correct here rather than propagating the panic.
        let guard = self
            .entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = guard.get(key)?;
        if entry.stored_at.elapsed() >= self.ttl {
            return None;
        }
        Some(entry.value.clone())
    }

    pub(crate) fn put(&self, key: K, value: V) {
        let mut guard = self
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.len() >= self.capacity && !guard.contains_key(&key) {
            Self::evict_expired(&mut guard, self.ttl);
            // Still full after dropping expired entries: the working set exceeds
            // capacity. Skip the insert rather than evict something live —
            // a cache that thrashes is worse than one that occasionally misses.
            if guard.len() >= self.capacity {
                return;
            }
        }
        guard.insert(
            key,
            Entry {
                value,
                stored_at: Instant::now(),
            },
        );
    }

    /// Drops an entry so a write takes effect immediately on this node.
    pub(crate) fn invalidate(&self, key: &K) {
        let mut guard = self
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove(key);
    }

    pub(crate) fn clear(&self) {
        let mut guard = self
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.clear();
    }

    fn evict_expired(entries: &mut HashMap<K, Entry<V>>, ttl: Duration) {
        entries.retain(|_, entry| entry.stored_at.elapsed() < ttl);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_value_is_returned() {
        let cache: TtlCache<u32, &str> = TtlCache::new(Duration::from_mins(1), 10);
        cache.put(1, "one");
        assert_eq!(cache.get(&1), Some("one"));
        assert_eq!(cache.get(&2), None);
    }

    #[test]
    fn an_expired_value_is_not_returned() {
        let cache: TtlCache<u32, &str> = TtlCache::new(Duration::ZERO, 10);
        cache.put(1, "one");
        assert_eq!(
            cache.get(&1),
            None,
            "a zero TTL must expire immediately, not serve stale data"
        );
    }

    /// The mechanism that makes a revocation take effect at once on the node
    /// that performed it.
    #[test]
    fn invalidation_is_immediate() {
        let cache: TtlCache<u32, &str> = TtlCache::new(Duration::from_mins(1), 10);
        cache.put(1, "one");
        cache.invalidate(&1);
        assert_eq!(cache.get(&1), None);
    }

    #[test]
    fn capacity_is_respected_and_expired_entries_make_room() {
        let cache: TtlCache<u32, u32> = TtlCache::new(Duration::from_mins(1), 2);
        cache.put(1, 1);
        cache.put(2, 2);
        cache.put(3, 3);
        assert_eq!(cache.len(), 2, "capacity must bound the map");

        // Expired entries are reclaimed rather than blocking inserts forever.
        let short: TtlCache<u32, u32> = TtlCache::new(Duration::from_millis(1), 2);
        short.put(1, 1);
        short.put(2, 2);
        std::thread::sleep(Duration::from_millis(5));
        short.put(3, 3);
        assert!(short.get(&3).is_some(), "expired entries must make room");
    }

    #[test]
    fn updating_an_existing_key_works_at_capacity() {
        // Refreshing a cached value must not be blocked by a full map, or a hot
        // key would go stale permanently once capacity is reached.
        let cache: TtlCache<u32, u32> = TtlCache::new(Duration::from_mins(1), 2);
        cache.put(1, 1);
        cache.put(2, 2);
        cache.put(1, 99);
        assert_eq!(cache.get(&1), Some(99));
    }
}
