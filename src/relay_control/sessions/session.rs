use super::RelaySessionConfig;

/// Phase-0 skeleton for the future single-client session primitive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelaySession {
    config: RelaySessionConfig,
}

impl RelaySession {
    pub(crate) fn new(config: RelaySessionConfig) -> Self {
        Self { config }
    }

    pub(crate) fn config(&self) -> &RelaySessionConfig {
        &self.config
    }
}
