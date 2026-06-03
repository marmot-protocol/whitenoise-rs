use cgka_engine::FeatureRegistry;
use cgka_traits::agent_text_stream::{
    AGENT_TEXT_STREAM_QUIC_FANOUT_FEATURE, AGENT_TEXT_STREAM_QUIC_RECEIVE_FEATURE,
    AGENT_TEXT_STREAM_QUIC_SEND_FEATURE,
};
use cgka_traits::app_components::AGENT_TEXT_STREAM_QUIC_COMPONENT_ID;
use cgka_traits::capabilities::{Capability, CapabilityRequirement, Feature, RequirementLevel};

pub(crate) const SELF_REMOVE_CODEPOINT: u16 = 0x000a;
pub(crate) const APP_DATA_UPDATE_PROPOSAL_CODEPOINT: u16 = 0x0008;
pub(crate) const SELF_REMOVE_FEATURE: Feature = Feature("self-remove");

pub(crate) fn whitenoise_feature_registry() -> FeatureRegistry {
    let mut registry = FeatureRegistry::new();
    registry.register(
        SELF_REMOVE_FEATURE,
        CapabilityRequirement {
            requires: Capability::Proposal(SELF_REMOVE_CODEPOINT),
            level: RequirementLevel::Required,
            description: "MIP-03 SelfRemove group departure",
        },
    );

    for (feature, description) in [
        (
            AGENT_TEXT_STREAM_QUIC_RECEIVE_FEATURE,
            "receive QUIC-backed agent text stream previews",
        ),
        (
            AGENT_TEXT_STREAM_QUIC_SEND_FEATURE,
            "send QUIC-backed agent text stream frames",
        ),
        (
            AGENT_TEXT_STREAM_QUIC_FANOUT_FEATURE,
            "fan out QUIC-backed agent text stream frames",
        ),
    ] {
        registry.register(
            feature,
            CapabilityRequirement {
                requires: Capability::AppComponent(AGENT_TEXT_STREAM_QUIC_COMPONENT_ID),
                level: RequirementLevel::Optional,
                description,
            },
        );
    }

    registry
}
