//! Generic keyed broadcast hub for per-key streaming channels.
//!
//! Provides lazy channel creation. Cleanup is also lazy: a stream entry is
//! retained after its last receiver is dropped and is only removed on the
//! next [`BroadcastHub::emit`] for that key, when [`broadcast::Sender::send`]
//! detects no live receivers. Used as the backing implementation for message,
//! chat list, and user stream managers.

use std::fmt::Debug;
use std::hash::Hash;

use dashmap::DashMap;
use tokio::sync::broadcast;

const BUFFER_SIZE: usize = 100;

pub(crate) struct BroadcastHub<K, V>
where
    K: Eq + Hash,
{
    streams: DashMap<K, broadcast::Sender<V>>,
    label: &'static str,
}

impl<K, V> BroadcastHub<K, V>
where
    K: Clone + Eq + Hash + Debug,
    V: Clone,
{
    pub fn new(label: &'static str) -> Self {
        Self {
            streams: DashMap::new(),
            label,
        }
    }

    pub fn subscribe(&self, key: &K) -> broadcast::Receiver<V> {
        self.streams
            .entry(key.clone())
            .or_insert_with(|| broadcast::channel(BUFFER_SIZE).0)
            .subscribe()
    }

    pub fn emit(&self, key: &K, update: V) {
        if let Some(sender) = self.streams.get(key)
            && sender.send(update).is_err()
        {
            drop(sender);
            // Atomically check and remove to avoid race with concurrent subscribe()
            if self
                .streams
                .remove_if(key, |_, s| s.receiver_count() == 0)
                .is_some()
            {
                tracing::debug!(
                    target: "whitenoise::broadcast_hub",
                    "Cleaned up {} stream for key {:?} (no active receivers)",
                    self.label,
                    key,
                );
            }
        }
    }

    pub fn has_subscribers(&self, key: &K) -> bool {
        self.streams
            .get(key)
            .map(|sender| sender.receiver_count() > 0)
            .unwrap_or(false)
    }

    pub fn subscriber_keys(&self) -> Vec<K> {
        self.streams
            .iter()
            .filter(|entry| entry.receiver_count() > 0)
            .map(|entry| entry.key().clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub() -> BroadcastHub<i32, String> {
        BroadcastHub::new("test")
    }

    #[test]
    fn subscribe_creates_new_stream() {
        let h = hub();

        assert!(!h.streams.contains_key(&1));

        let _rx = h.subscribe(&1);

        assert!(h.streams.contains_key(&1));
    }

    #[test]
    fn multiple_subscribes_share_sender() {
        let h = hub();

        let _rx1 = h.subscribe(&1);
        let _rx2 = h.subscribe(&1);

        assert_eq!(h.streams.len(), 1);

        let sender = h.streams.get(&1).unwrap();
        assert_eq!(sender.receiver_count(), 2);
    }

    #[tokio::test]
    async fn emit_delivers_to_receivers() {
        let h = hub();
        let mut rx = h.subscribe(&1);

        h.emit(&1, "hello".to_string());

        let received = rx.try_recv().expect("should receive update");
        assert_eq!(received, "hello");
    }

    #[test]
    fn emit_without_subscribers_is_noop() {
        let h = hub();

        h.emit(&1, "ignored".to_string());

        assert!(!h.streams.contains_key(&1));
    }

    #[test]
    fn emit_cleans_up_when_all_receivers_dropped() {
        let h = hub();

        let rx = h.subscribe(&1);
        drop(rx);

        assert!(h.streams.contains_key(&1));

        h.emit(&1, "trigger cleanup".to_string());

        assert!(!h.streams.contains_key(&1));
    }

    #[test]
    fn different_keys_have_separate_streams() {
        let h = hub();

        let _rx1 = h.subscribe(&1);
        let _rx2 = h.subscribe(&2);

        assert_eq!(h.streams.len(), 2);
        assert!(h.streams.contains_key(&1));
        assert!(h.streams.contains_key(&2));
    }

    #[tokio::test]
    async fn emit_delivers_to_all_subscribers() {
        let h = hub();

        let mut rx1 = h.subscribe(&1);
        let mut rx2 = h.subscribe(&1);

        h.emit(&1, "broadcast".to_string());

        let r1 = rx1.try_recv().expect("rx1 should receive");
        let r2 = rx2.try_recv().expect("rx2 should receive");

        assert_eq!(r1, "broadcast");
        assert_eq!(r2, "broadcast");
    }

    #[test]
    fn has_subscribers_returns_true_when_active() {
        let h = hub();

        assert!(!h.has_subscribers(&1));

        let _rx = h.subscribe(&1);

        assert!(h.has_subscribers(&1));
    }

    #[test]
    fn has_subscribers_returns_false_when_empty() {
        let h = hub();

        assert!(!h.has_subscribers(&1));
    }

    #[test]
    fn has_subscribers_returns_false_after_all_dropped() {
        let h = hub();

        let rx = h.subscribe(&1);
        assert!(h.has_subscribers(&1));

        drop(rx);

        assert!(!h.has_subscribers(&1));
    }

    #[test]
    fn subscriber_keys_returns_only_keys_with_live_receivers() {
        let h = hub();

        let _rx1 = h.subscribe(&1);
        let rx2 = h.subscribe(&2);
        let _rx3 = h.subscribe(&3);
        drop(rx2);

        let mut keys = h.subscriber_keys();
        keys.sort_unstable();

        assert_eq!(keys, vec![1, 3]);
    }
}
