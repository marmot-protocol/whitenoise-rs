use cgka_traits::app_components::{
    AGENT_TEXT_STREAM_QUIC_COMPONENT_ID, GROUP_MESSAGE_RETENTION_COMPONENT_ID,
    NOSTR_ROUTING_COMPONENT_ID, default_group_components,
};

pub(crate) fn whitenoise_supported_app_components() -> impl IntoIterator<Item = u16> {
    let mut components = default_group_components();
    components.insert(NOSTR_ROUTING_COMPONENT_ID);
    components.insert(GROUP_MESSAGE_RETENTION_COMPONENT_ID);
    components.insert(AGENT_TEXT_STREAM_QUIC_COMPONENT_ID);
    components
}

pub(crate) fn whitenoise_supported_app_component_tags() -> Vec<String> {
    whitenoise_supported_app_components()
        .into_iter()
        .map(|component_id| format!("0x{component_id:04x}"))
        .collect()
}
