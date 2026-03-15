use core::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use nostr_sdk::{PublicKey, RelayUrl};

use super::RelayPlane;
use crate::perf_instrument;
use crate::whitenoise::database::{
    Database, DatabaseError, relay_events::RelayEventRecord, relay_status::RelayStatusRecord,
};

/// High-level relay failure classification to be persisted in later phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RelayFailureCategory {
    Transport,
    Timeout,
    AuthRequired,
    AuthFailed,
    RelayPolicy,
    InvalidFilter,
    RateLimited,
    ClosedByRelay,
    Unknown,
}

impl RelayFailureCategory {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Timeout => "timeout",
            Self::AuthRequired => "auth_required",
            Self::AuthFailed => "auth_failed",
            Self::RelayPolicy => "relay_policy",
            Self::InvalidFilter => "invalid_filter",
            Self::RateLimited => "rate_limited",
            Self::ClosedByRelay => "closed_by_relay",
            Self::Unknown => "unknown",
        }
    }

    pub(crate) fn classify_notice(message: &str) -> Self {
        Self::classify_text(message, Self::Unknown)
    }

    pub(crate) fn classify_closed(message: &str) -> Self {
        Self::classify_text(message, Self::ClosedByRelay)
    }

    pub(crate) fn classify_auth(message: &str) -> Self {
        Self::classify_text(message, Self::AuthRequired)
    }

    fn classify_text(message: &str, fallback: Self) -> Self {
        let normalized = message.to_lowercase();

        if normalized.contains("timed out") || normalized.contains("timeout") {
            return Self::Timeout;
        }

        if normalized.contains("auth-required")
            || normalized.contains("auth required")
            || normalized.contains("requires auth")
            || normalized.contains("authentication required")
        {
            return Self::AuthRequired;
        }

        if normalized.contains("auth failed")
            || normalized.contains("authentication failed")
            || normalized.contains("invalid auth")
            || normalized.contains("invalid signature")
            || normalized.contains("challenge failed")
        {
            return Self::AuthFailed;
        }

        if normalized.contains("invalid filter")
            || normalized.contains("bad filter")
            || normalized.contains("malformed filter")
            || normalized.contains("unsupported filter")
        {
            return Self::InvalidFilter;
        }

        if normalized.contains("rate limit")
            || normalized.contains("rate-limited")
            || normalized.contains("too many requests")
            || normalized.contains("slow down")
        {
            return Self::RateLimited;
        }

        if normalized.contains("policy")
            || normalized.contains("blocked")
            || normalized.contains("banned")
            || normalized.contains("not allowed")
        {
            return Self::RelayPolicy;
        }

        if normalized.contains("transport")
            || normalized.contains("connection")
            || normalized.contains("network")
            || normalized.contains("dns")
            || normalized.contains("unreachable")
            || normalized.contains("i/o")
            || normalized.contains("io error")
        {
            return Self::Transport;
        }

        fallback
    }
}

impl FromStr for RelayFailureCategory {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "transport" => Ok(Self::Transport),
            "timeout" => Ok(Self::Timeout),
            "auth_required" => Ok(Self::AuthRequired),
            "auth_failed" => Ok(Self::AuthFailed),
            "relay_policy" => Ok(Self::RelayPolicy),
            "invalid_filter" => Ok(Self::InvalidFilter),
            "rate_limited" => Ok(Self::RateLimited),
            "closed_by_relay" => Ok(Self::ClosedByRelay),
            "unknown" => Ok(Self::Unknown),
            _ => Err(format!("invalid relay failure category: {value}")),
        }
    }
}

/// Structured relay telemetry kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RelayTelemetryKind {
    Connected,
    Disconnected,
    Notice,
    Closed,
    AuthChallenge,
    PublishAttempt,
    PublishSuccess,
    PublishFailure,
    QueryAttempt,
    QuerySuccess,
    QueryFailure,
    SubscriptionAttempt,
    SubscriptionSuccess,
    SubscriptionFailure,
}

impl RelayTelemetryKind {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Disconnected => "disconnected",
            Self::Notice => "notice",
            Self::Closed => "closed",
            Self::AuthChallenge => "auth_challenge",
            Self::PublishAttempt => "publish_attempt",
            Self::PublishSuccess => "publish_success",
            Self::PublishFailure => "publish_failure",
            Self::QueryAttempt => "query_attempt",
            Self::QuerySuccess => "query_success",
            Self::QueryFailure => "query_failure",
            Self::SubscriptionAttempt => "subscription_attempt",
            Self::SubscriptionSuccess => "subscription_success",
            Self::SubscriptionFailure => "subscription_failure",
        }
    }
}

impl FromStr for RelayTelemetryKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "connected" => Ok(Self::Connected),
            "disconnected" => Ok(Self::Disconnected),
            "notice" => Ok(Self::Notice),
            "closed" => Ok(Self::Closed),
            "auth_challenge" => Ok(Self::AuthChallenge),
            "publish_attempt" => Ok(Self::PublishAttempt),
            "publish_success" => Ok(Self::PublishSuccess),
            "publish_failure" => Ok(Self::PublishFailure),
            "query_attempt" => Ok(Self::QueryAttempt),
            "query_success" => Ok(Self::QuerySuccess),
            "query_failure" => Ok(Self::QueryFailure),
            "subscription_attempt" => Ok(Self::SubscriptionAttempt),
            "subscription_success" => Ok(Self::SubscriptionSuccess),
            "subscription_failure" => Ok(Self::SubscriptionFailure),
            _ => Err(format!("invalid relay telemetry kind: {value}")),
        }
    }
}

/// Normalized relay telemetry payload shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayTelemetry {
    pub(crate) kind: RelayTelemetryKind,
    pub(crate) plane: RelayPlane,
    pub(crate) account_pubkey: Option<PublicKey>,
    pub(crate) relay_url: RelayUrl,
    pub(crate) occurred_at: DateTime<Utc>,
    pub(crate) subscription_id: Option<String>,
    pub(crate) failure_category: Option<RelayFailureCategory>,
    pub(crate) message: Option<String>,
}

impl RelayTelemetry {
    pub(crate) fn new(kind: RelayTelemetryKind, plane: RelayPlane, relay_url: RelayUrl) -> Self {
        Self {
            kind,
            plane,
            account_pubkey: None,
            relay_url,
            occurred_at: Utc::now(),
            subscription_id: None,
            failure_category: None,
            message: None,
        }
    }

    pub(crate) fn with_account_pubkey(mut self, account_pubkey: PublicKey) -> Self {
        self.account_pubkey = Some(account_pubkey);
        self
    }

    #[allow(dead_code)]
    pub(crate) fn with_occurred_at(mut self, occurred_at: DateTime<Utc>) -> Self {
        self.occurred_at = occurred_at;
        self
    }

    pub(crate) fn with_subscription_id<T>(mut self, subscription_id: T) -> Self
    where
        T: Into<String>,
    {
        self.subscription_id = Some(subscription_id.into());
        self
    }

    pub(crate) fn with_message<T>(mut self, message: T) -> Self
    where
        T: Into<String>,
    {
        self.message = Some(message.into());
        self
    }

    pub(crate) fn with_failure_category(mut self, failure_category: RelayFailureCategory) -> Self {
        self.failure_category = Some(failure_category);
        self
    }

    pub(crate) fn notice(plane: RelayPlane, relay_url: RelayUrl, message: &str) -> Self {
        Self::new(RelayTelemetryKind::Notice, plane, relay_url)
            .with_message(message)
            .with_failure_category(RelayFailureCategory::classify_notice(message))
    }

    pub(crate) fn closed(plane: RelayPlane, relay_url: RelayUrl, message: &str) -> Self {
        Self::new(RelayTelemetryKind::Closed, plane, relay_url)
            .with_message(message)
            .with_failure_category(RelayFailureCategory::classify_closed(message))
    }

    pub(crate) fn auth_challenge(plane: RelayPlane, relay_url: RelayUrl, message: &str) -> Self {
        Self::new(RelayTelemetryKind::AuthChallenge, plane, relay_url)
            .with_message(message)
            .with_failure_category(RelayFailureCategory::classify_auth(message))
    }
}

/// Static observability configuration owned by the control plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayObservabilityConfig {
    pub(crate) recent_event_limit: usize,
    pub(crate) status_staleness_window: Duration,
}

impl Default for RelayObservabilityConfig {
    fn default() -> Self {
        Self {
            recent_event_limit: 200,
            status_staleness_window: Duration::from_secs(60 * 5),
        }
    }
}

/// Relay-observability host for persistence and later aggregation logic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayObservability {
    config: RelayObservabilityConfig,
}

impl RelayObservability {
    pub(crate) fn new(config: RelayObservabilityConfig) -> Self {
        Self { config }
    }

    #[allow(dead_code)]
    pub(crate) fn config(&self) -> &RelayObservabilityConfig {
        &self.config
    }

    #[perf_instrument("relay")]
    pub(crate) async fn record(
        &self,
        database: &Database,
        telemetry: &RelayTelemetry,
    ) -> Result<(), DatabaseError> {
        // Attempt events carry no actionable state; skip persistence entirely.
        if matches!(
            telemetry.kind,
            RelayTelemetryKind::PublishAttempt
                | RelayTelemetryKind::QueryAttempt
                | RelayTelemetryKind::SubscriptionAttempt
        ) {
            return Ok(());
        }

        tracing::debug!(
            target: "whitenoise::relay_control::observability",
            plane = telemetry.plane.as_str(),
            relay_url = %telemetry.relay_url,
            account_pubkey = telemetry
                .account_pubkey
                .map(|pubkey| pubkey.to_hex())
                .unwrap_or_default(),
            kind = telemetry.kind.as_str(),
            failure_category = telemetry
                .failure_category
                .map(|category| category.as_str())
                .unwrap_or(""),
            "Recording relay telemetry"
        );

        // Only append to relay_events for failure and state-change events.
        // Success and connection events update relay_status counters only.
        if matches!(
            telemetry.kind,
            RelayTelemetryKind::Disconnected
                | RelayTelemetryKind::Notice
                | RelayTelemetryKind::Closed
                | RelayTelemetryKind::AuthChallenge
                | RelayTelemetryKind::PublishFailure
                | RelayTelemetryKind::QueryFailure
                | RelayTelemetryKind::SubscriptionFailure
        ) {
            RelayEventRecord::create(telemetry, database).await?;
        }

        RelayStatusRecord::upsert_from_telemetry(telemetry, database).await?;

        Ok(())
    }

    #[allow(dead_code)]
    #[perf_instrument("relay")]
    pub(crate) async fn status(
        &self,
        database: &Database,
        relay_url: &RelayUrl,
        plane: RelayPlane,
        account_pubkey: Option<PublicKey>,
    ) -> Result<Option<RelayStatusRecord>, DatabaseError> {
        RelayStatusRecord::find(relay_url, plane, account_pubkey, database).await
    }

    #[allow(dead_code)]
    #[perf_instrument("relay")]
    pub(crate) async fn recent_events(
        &self,
        database: &Database,
        relay_url: &RelayUrl,
        plane: RelayPlane,
        account_pubkey: Option<PublicKey>,
    ) -> Result<Vec<RelayEventRecord>, DatabaseError> {
        RelayEventRecord::list_recent_for_scope(
            relay_url,
            plane,
            account_pubkey,
            self.config.recent_event_limit,
            database,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn test_classify_notice_rate_limited() {
        assert_eq!(
            RelayFailureCategory::classify_notice("rate-limited: slow down"),
            RelayFailureCategory::RateLimited
        );
    }

    #[test]
    fn test_classify_notice_invalid_filter() {
        assert_eq!(
            RelayFailureCategory::classify_notice("invalid filter: unsupported kind"),
            RelayFailureCategory::InvalidFilter
        );
    }

    #[test]
    fn test_classify_closed_auth_required() {
        assert_eq!(
            RelayFailureCategory::classify_closed("auth-required: send AUTH first"),
            RelayFailureCategory::AuthRequired
        );
    }

    #[test]
    fn test_classify_auth_failed() {
        assert_eq!(
            RelayFailureCategory::classify_auth("authentication failed: invalid signature"),
            RelayFailureCategory::AuthFailed
        );
    }

    #[test]
    fn test_relay_telemetry_builders_classify_messages() {
        let relay_url = RelayUrl::parse("wss://relay.example.com").unwrap();

        let notice = RelayTelemetry::notice(RelayPlane::Discovery, relay_url.clone(), "timeout");
        let closed = RelayTelemetry::closed(
            RelayPlane::Discovery,
            relay_url.clone(),
            "blocked by relay policy",
        );
        let auth = RelayTelemetry::auth_challenge(
            RelayPlane::AccountInbox,
            relay_url,
            "authentication required",
        );

        assert_eq!(notice.failure_category, Some(RelayFailureCategory::Timeout));
        assert_eq!(
            closed.failure_category,
            Some(RelayFailureCategory::RelayPolicy)
        );
        assert_eq!(
            auth.failure_category,
            Some(RelayFailureCategory::AuthRequired)
        );
    }

    #[test]
    fn test_telemetry_builder_preserves_explicit_fields() {
        let account_pubkey =
            PublicKey::from_hex("1111111111111111111111111111111111111111111111111111111111111111")
                .unwrap();
        let occurred_at = Utc.with_ymd_and_hms(2026, 3, 7, 12, 0, 0).unwrap();
        let telemetry = RelayTelemetry::new(
            RelayTelemetryKind::SubscriptionSuccess,
            RelayPlane::Group,
            RelayUrl::parse("wss://relay.example.com").unwrap(),
        )
        .with_account_pubkey(account_pubkey)
        .with_occurred_at(occurred_at)
        .with_subscription_id("opaque-sub")
        .with_message("ok");

        assert_eq!(telemetry.account_pubkey, Some(account_pubkey));
        assert_eq!(telemetry.occurred_at, occurred_at);
        assert_eq!(telemetry.subscription_id.as_deref(), Some("opaque-sub"));
        assert_eq!(telemetry.message.as_deref(), Some("ok"));
    }
}
