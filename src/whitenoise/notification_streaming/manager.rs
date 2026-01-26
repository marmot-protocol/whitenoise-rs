use tokio::sync::broadcast;

use super::types::NotificationUpdate;

const BUFFER_SIZE: usize = 100;

pub(crate) struct NotificationStreamManager {
    sender: broadcast::Sender<NotificationUpdate>,
}

impl NotificationStreamManager {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(BUFFER_SIZE);
        Self { sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<NotificationUpdate> {
        self.sender.subscribe()
    }

    pub fn emit(&self, update: NotificationUpdate) {
        // receiver_count() is O(1) - just reads an AtomicUsize
        if self.has_subscribers() {
            let _ = self.sender.send(update);
        }
    }

    pub fn has_subscribers(&self) -> bool {
        self.sender.receiver_count() > 0
    }
}

impl Default for NotificationStreamManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use mdk_core::prelude::GroupId;
    use nostr_sdk::Keys;

    use super::*;
    use crate::whitenoise::notification_streaming::{
        NotificationTrigger, NotificationUpdate, NotificationUser,
    };

    fn make_test_update(trigger: NotificationTrigger, seed: u8) -> NotificationUpdate {
        NotificationUpdate {
            trigger,
            mls_group_id: GroupId::from_slice(&[seed; 32]),
            group_name: Some(format!("Group {}", seed)),
            is_dm: false,
            receiver: NotificationUser {
                pubkey: Keys::generate().public_key(),
                display_name: Some("Receiver".to_string()),
                picture_url: None,
            },
            sender: NotificationUser {
                pubkey: Keys::generate().public_key(),
                display_name: Some("Sender".to_string()),
                picture_url: None,
            },
            content: "Test message".to_string(),
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn subscribe_creates_receiver() {
        let manager = NotificationStreamManager::new();

        assert!(!manager.has_subscribers());

        let _rx = manager.subscribe();

        assert!(manager.has_subscribers());
    }

    #[tokio::test]
    async fn emit_delivers_to_receivers() {
        let manager = NotificationStreamManager::new();
        let mut rx = manager.subscribe();

        let update = make_test_update(NotificationTrigger::NewMessage, 1);
        manager.emit(update);

        let received = rx.try_recv().expect("should receive update");
        assert_eq!(received.trigger, NotificationTrigger::NewMessage);
    }

    #[test]
    fn emit_without_subscribers_is_noop() {
        let manager = NotificationStreamManager::new();

        // Should not panic
        let update = make_test_update(NotificationTrigger::NewMessage, 2);
        manager.emit(update);
    }

    #[tokio::test]
    async fn multiple_subscribers_receive_same_update() {
        let manager = NotificationStreamManager::new();

        let mut rx1 = manager.subscribe();
        let mut rx2 = manager.subscribe();

        let update = make_test_update(NotificationTrigger::GroupInvite, 3);
        manager.emit(update);

        let received1 = rx1.try_recv().expect("rx1 should receive update");
        let received2 = rx2.try_recv().expect("rx2 should receive update");

        assert_eq!(received1.trigger, NotificationTrigger::GroupInvite);
        assert_eq!(received2.trigger, NotificationTrigger::GroupInvite);
    }

    #[test]
    fn has_subscribers_false_after_all_dropped() {
        let manager = NotificationStreamManager::new();

        let rx = manager.subscribe();
        assert!(manager.has_subscribers());

        drop(rx);

        assert!(!manager.has_subscribers());
    }

    #[test]
    fn default_creates_manager() {
        let manager = NotificationStreamManager::default();
        assert!(!manager.has_subscribers());
    }
}
