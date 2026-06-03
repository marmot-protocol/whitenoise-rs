pub(crate) mod app_components;
pub(crate) mod capabilities;
pub mod forensics;
pub(crate) mod key_packages;
pub(crate) mod media;
pub mod message;
pub(crate) mod publish;
pub mod push;
pub(crate) mod session;
pub(crate) mod storage;
pub(crate) mod transport;
pub mod types;

pub use forensics::GroupForensics;
pub use message::{Message, MessageState};
pub(crate) use types::MarmotCreatedGroupProjection;
pub use types::group_types;
pub use types::{Group, GroupConfig, GroupDataUpdate, GroupId, GroupState, Secret};

#[cfg(test)]
mod tests {
    use cgka_traits::GroupId;
    use cgka_traits::NOSTR_ROUTING_COMPONENT_ID;
    use transport_nostr_adapter::{KIND_MARMOT_KEY_PACKAGE, NostrTransportAdapter};
    use transport_nostr_peeler::NostrMlsPeeler;
    use transport_nostr_peeler::{KIND_MARMOT_GROUP_MESSAGE, KIND_MARMOT_WELCOME_RUMOR};

    #[test]
    fn darkmatter_transport_crates_are_available() {
        let group_id = GroupId::new(vec![1, 2, 3]);
        let peeler = NostrMlsPeeler::new();

        assert_eq!(group_id.as_slice(), &[1, 2, 3]);
        assert_eq!(NOSTR_ROUTING_COMPONENT_ID, 0x8004);
        assert_eq!(KIND_MARMOT_GROUP_MESSAGE, 445);
        assert_eq!(KIND_MARMOT_WELCOME_RUMOR, 444);
        assert_eq!(nostr_sdk::Kind::MlsWelcome.as_u16(), 444);
        assert_eq!(KIND_MARMOT_KEY_PACKAGE, 30_443);

        let _peeler = peeler;
        let _adapter = core::any::TypeId::of::<NostrTransportAdapter>();
    }

    #[test]
    fn darkmatter_engine_crate_is_available() {
        let ciphersuite: cgka_engine::Ciphersuite = cgka_engine::DEFAULT_CIPHERSUITE;

        assert_eq!(u16::from(ciphersuite), 0x0001);
    }
}
