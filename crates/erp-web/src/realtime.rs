//! Where an open stream waits, and how a projection advance reaches it.
//!
//! # Two registries, on purpose
//!
//! A counter screen watches a tenant; a customer's phone watches one
//! reservation. They are different populations — dozens against thousands —
//! and the spec's one hard requirement is that neither can starve the other.
//! So they share nothing but the subscriber task: separate senders, separate
//! caps, and a public signal is never routed through a staff sender.
//!
//! # No database connection per stream
//!
//! Fan-out is one Redis message per advance and one `broadcast` send per
//! sender. A stream holds a receiver and a deadline, nothing else. The budget
//! in `pools.rs` is sized for tenants, not for browser tabs.
//!
//! # A lagged watcher is told to reconnect
//!
//! `broadcast` keeps [`CAPACITY`] messages per sender; a receiver further
//! behind than that gets `Lagged`. The right answer is a fresh snapshot, and the
//! cheapest way to one that touches no database from inside a stream is to end
//! the stream: the browser reconnects, and the first event of every stream is
//! the snapshot.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use erp_control::ControlPlane;
use erp_control::shared::Advanced;
use erp_types::{StreamId, TenantId};
use tokio::sync::broadcast;

/// Signals buffered per sender before a slow receiver is told it lagged.
pub const CAPACITY: usize = 64;

/// How many streams one tenant may hold open on one node, per surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub staff_per_tenant: usize,
    pub public_per_tenant: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            staff_per_tenant: 256,
            public_per_tenant: 4096,
        }
    }
}

/// The cap is reached. A 429, with a moment to wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("too many live streams are open for this tenant on this node")]
pub struct Full;

#[derive(Debug)]
pub struct Hub {
    staff: Mutex<HashMap<TenantId, broadcast::Sender<Advanced>>>,
    subjects: Mutex<HashMap<(TenantId, StreamId), broadcast::Sender<Advanced>>>,
    caps: Caps,
    lifetime: Duration,
    keep_alive: Duration,
}

impl Hub {
    #[must_use]
    pub fn new(caps: Caps) -> Self {
        Self {
            staff: Mutex::new(HashMap::new()),
            subjects: Mutex::new(HashMap::new()),
            caps,
            lifetime: Duration::from_mins(10),
            keep_alive: Duration::from_secs(15),
        }
    }

    /// The same hub with streams that end sooner. A test's.
    #[must_use]
    pub const fn living(mut self, lifetime: Duration) -> Self {
        self.lifetime = lifetime;
        self
    }

    /// How long a stream lives before it asks the client to reconnect. The
    /// reconnect re-runs authorization, which a stream held for hours would
    /// otherwise outlive.
    #[must_use]
    pub const fn lifetime(&self) -> Duration {
        self.lifetime
    }

    #[must_use]
    pub const fn keep_alive(&self) -> Duration {
        self.keep_alive
    }

    /// A receiver for everything this tenant's staff may see.
    pub fn watch_tenant(&self, tenant: TenantId) -> Result<broadcast::Receiver<Advanced>, Full> {
        let mut staff = self.staff.lock().unwrap_or_else(PoisonError::into_inner);
        watch(&mut staff, tenant, self.caps.staff_per_tenant)
    }

    /// A receiver for one subject — a reservation — and nothing else.
    pub fn watch_subject(
        &self,
        tenant: TenantId,
        stream: StreamId,
    ) -> Result<broadcast::Receiver<Advanced>, Full> {
        let mut subjects = self.subjects.lock().unwrap_or_else(PoisonError::into_inner);
        watch(&mut subjects, (tenant, stream), self.caps.public_per_tenant)
    }

    /// Hands an advance to every stream it concerns, and forgets senders
    /// nobody is holding any more.
    pub fn publish(&self, signal: &Advanced) {
        {
            let mut staff = self.staff.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(sender) = staff.get(&signal.tenant) {
                if sender.receiver_count() == 0 {
                    staff.remove(&signal.tenant);
                } else {
                    // A send fails only when every receiver is gone, which the
                    // count above just said is not so.
                    let _ = sender.send(signal.clone());
                }
            }
        }

        let mut subjects = self.subjects.lock().unwrap_or_else(PoisonError::into_inner);
        match &signal.streams {
            Some(streams) => {
                for stream in streams {
                    let key = (signal.tenant, stream.clone());
                    if let Some(sender) = subjects.get(&key) {
                        if sender.receiver_count() == 0 {
                            subjects.remove(&key);
                        } else {
                            let _ = sender.send(signal.clone());
                        }
                    }
                }
            }
            // "Many": every subject of this tenant re-checks once.
            None => {
                subjects.retain(|(tenant, _), sender| {
                    if *tenant != signal.tenant {
                        return true;
                    }
                    if sender.receiver_count() == 0 {
                        return false;
                    }
                    let _ = sender.send(signal.clone());
                    true
                });
            }
        }
    }

    /// How many streams this tenant holds open here: `(staff, public)`.
    #[must_use]
    pub fn open(&self, tenant: TenantId) -> (usize, usize) {
        let staff = self
            .staff
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&tenant)
            .map_or(0, broadcast::Sender::receiver_count);
        let public = self
            .subjects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|((t, _), _)| *t == tenant)
            .map(|(_, sender)| sender.receiver_count())
            .sum();
        (staff, public)
    }
}

fn watch<K: std::hash::Hash + Eq>(
    senders: &mut HashMap<K, broadcast::Sender<Advanced>>,
    key: K,
    cap: usize,
) -> Result<broadcast::Receiver<Advanced>, Full> {
    match senders.get(&key) {
        Some(sender) if sender.receiver_count() >= cap => Err(Full),
        Some(sender) => Ok(sender.subscribe()),
        None => {
            if cap == 0 {
                return Err(Full);
            }
            let (sender, receiver) = broadcast::channel(CAPACITY);
            senders.insert(key, sender);
            Ok(receiver)
        }
    }
}

/// Forwards every advance announced in the fleet to this node's hub, for as
/// long as the control plane lives. Resubscribes when Redis goes away and
/// comes back, as the invalidation listener does.
pub fn listen_in_background(
    control: &Arc<ControlPlane>,
    hub: Arc<Hub>,
) -> Option<tokio::task::JoinHandle<()>> {
    let shared = control.shared()?.clone();
    let weak = Arc::downgrade(control);

    Some(tokio::spawn(async move {
        use futures_util::StreamExt as _;

        loop {
            if weak.upgrade().is_none() {
                return;
            }
            match shared.subscribe_advanced().await {
                Ok(mut pubsub) => {
                    tracing::info!("listening for projection advances");
                    let mut stream = pubsub.on_message();
                    while let Some(message) = stream.next().await {
                        if weak.upgrade().is_none() {
                            return;
                        }
                        match message.get_payload::<String>() {
                            Ok(raw) => match serde_json::from_str::<Advanced>(&raw) {
                                Ok(signal) => hub.publish(&signal),
                                Err(e) => tracing::error!(
                                    error = %e, %raw,
                                    "unreadable advance; a newer build may be announcing what this one cannot read"
                                ),
                            },
                            Err(e) => tracing::warn!(error = %e, "unreadable advance payload"),
                        }
                    }
                    tracing::warn!("advance subscription ended; resubscribing");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not subscribe to advances; retrying");
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_types::{AggregateId, DomainName, LogPosition, ModuleId};

    fn reservation(id: &str) -> StreamId {
        StreamId::new(
            DomainName::new("booking_reservation").expect("a domain"),
            AggregateId::new(id).expect("an id"),
        )
    }

    fn advance(tenant: TenantId, streams: Option<Vec<StreamId>>) -> Advanced {
        Advanced {
            tenant,
            group: "booking".to_owned(),
            module: ModuleId::new("booking").expect("a module"),
            position: LogPosition::new(7).expect("a position"),
            streams,
        }
    }

    /// **A tenant's advance reaches every staff watcher and no subject**, and
    /// another tenant's watchers hear nothing.
    #[tokio::test]
    async fn a_tenant_signal_reaches_every_staff_watcher_and_no_subject() {
        let hub = Hub::new(Caps::default());
        let acme = TenantId::new();
        let other = TenantId::new();
        let mut first = hub.watch_tenant(acme).expect("opens");
        let mut second = hub.watch_tenant(acme).expect("opens");
        let mut elsewhere = hub.watch_tenant(other).expect("opens");
        let mut phone = hub.watch_subject(acme, reservation("r-1")).expect("opens");

        hub.publish(&advance(acme, Some(vec![reservation("r-9")])));

        assert_eq!(first.try_recv().expect("heard").position.get(), 7);
        assert_eq!(second.try_recv().expect("heard").position.get(), 7);
        assert!(elsewhere.try_recv().is_err(), "another tenant heard it");
        assert!(
            phone.try_recv().is_err(),
            "a subject heard a stream not its own"
        );
    }

    /// **A subject hears its own stream, and "many" wakes every subject.**
    #[tokio::test]
    async fn a_subject_signal_reaches_only_its_stream_and_many_reaches_all() {
        let hub = Hub::new(Caps::default());
        let acme = TenantId::new();
        let mut one = hub.watch_subject(acme, reservation("r-1")).expect("opens");
        let mut two = hub.watch_subject(acme, reservation("r-2")).expect("opens");

        hub.publish(&advance(acme, Some(vec![reservation("r-1")])));
        assert!(one.try_recv().is_ok());
        assert!(two.try_recv().is_err(), "the wrong phone woke");

        hub.publish(&advance(acme, None));
        assert!(one.try_recv().is_ok());
        assert!(two.try_recv().is_ok(), "many did not wake every subject");
    }

    /// **The caps are counted apart**, which is the whole reason there are two
    /// registries: a tenant at its public cap still opens a staff stream.
    #[tokio::test]
    async fn the_staff_and_public_caps_are_counted_apart() {
        let hub = Hub::new(Caps {
            staff_per_tenant: 1,
            public_per_tenant: 1,
        });
        let acme = TenantId::new();
        let _phone = hub.watch_subject(acme, reservation("r-1")).expect("opens");
        assert_eq!(
            hub.watch_subject(acme, reservation("r-1")).err(),
            Some(Full)
        );
        let _screen = hub
            .watch_tenant(acme)
            .expect("the public cap is not the staff cap");
        assert_eq!(hub.watch_tenant(acme).err(), Some(Full));
        assert_eq!(hub.open(acme), (1, 1));
    }

    /// **A watcher further behind than the buffer is told so**, which the
    /// stream turns into a reconnect.
    #[tokio::test]
    async fn a_lagged_watcher_is_told_so() {
        let hub = Hub::new(Caps::default());
        let acme = TenantId::new();
        let mut slow = hub.watch_tenant(acme).expect("opens");
        for _ in 0..=CAPACITY {
            hub.publish(&advance(acme, None));
        }
        assert!(matches!(
            slow.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
    }
}
