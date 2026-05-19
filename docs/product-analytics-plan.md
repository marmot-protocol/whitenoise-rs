# Privacy-Preserving Product Analytics Plan

## Summary

White Noise should support product analytics only when the person using the app has opted in.
The Rust library should own the privacy boundary: Flutter may ask to record approved product
events, but `whitenoise-rs` decides whether analytics is enabled, validates every event, strips
unsafe data, and sends only approved values to the analytics backend.

The backend is self-hosted Aptabase. Aptabase fits the shape of the problem because it accepts
explicit client-side events, has an HTTP ingestion endpoint, supports self-hosting, and does not
require stable user identifiers.

This plan is for the Rust library and its Flutter-facing public API. It does not implement the
Flutter consent UI, App Store privacy labels, or Aptabase operations.

## Product Decisions

These decisions are set for the first implementation:

- Consent is device-local only. Every device is treated as a separate analytics participant.
- The launch backend is the self-hosted Aptabase instance, not Aptabase Cloud.
- The first pass should answer as many useful product questions as the event registry can cover
  safely.
- Common system props include bundle identifier, OS name, app version, locale, and coarse device
  class.
- Consent is offered during onboarding and remains editable in settings.
- A public transparency page is planned in another repo, so this crate only needs to maintain the
  event inventory that page can later publish.
- Analytics retention should be 90 days.
- The consent version string is `product-analytics-v1`.
- Rust should emit operation-owned analytics events internally whenever Rust owns the operation.
- Flutter should pass app/build analytics config into `WhitenoiseConfig`; `whitenoise-rs` should
  not depend on runtime environment variables for mobile builds.

## Source Notes

The plan is based on the current Aptabase public documentation and repository docs:

- Aptabase positions itself as open source, privacy-first app analytics and says apps control
  which events are sent.
- Aptabase can be self-hosted through its `aptabase/self-hosting` Docker Compose setup.
- Custom SDKs send a `POST` request to `{host}/api/v0/events`, using an `App-Key` header.
- Aptabase documents a maximum event batch size of 25.
- Aptabase custom event properties are limited to strings and numbers.
- Aptabase SDKs do not auto-track events; app code manually calls `trackEvent`.

References:

- <https://aptabase.com/>
- <https://github.com/aptabase/aptabase>
- <https://github.com/aptabase/self-hosting>
- <https://github.com/aptabase/aptabase/wiki/How-to-build-your-own-SDK>
- <https://github.com/aptabase/aptabase_flutter>
- <https://github.com/aptabase/tauri-plugin-aptabase>

## Goals

1. Analytics is off by default.
2. Analytics sends nothing until the app records explicit opt-in consent.
3. Flutter can record approved product events through `whitenoise-rs`.
4. Rust validates all event names, property names, and property values.
5. No event can contain identifiers for people, groups, messages, relays, devices, or push tokens.
6. Network failures never break product flows.
7. Opt-out stops future delivery and drops unsent events.
8. The design works with the current `Arc<Whitenoise>` and `AccountSession` architecture.

## Current Code Shape

The session/projection refactor is now on `master`, so analytics should fit the new ownership
model:

- `Whitenoise::new(config)` returns `Arc<Whitenoise>`.
- The process-lifetime `ensure_initialized` cache exists for FFI/background wake-ups, not as the
  general app access pattern.
- App-wide services live under `Whitenoise.shared: Arc<SharedServices>`.
- `WhitenoiseConfig` is stored under `SharedServices` and is accessed through
  `Whitenoise::config()`.
- Account-scoped operations live under `AccountSession` views such as `session.settings()`,
  `session.messages()`, and `session.groups()`.
- Account-scoped data lives in per-account SQLite files under `<data_dir>/accounts/<pubkey>.db`.
- App-wide data, including `app_settings`, lives in the shared database.
- Schema changes use Rust migrations in `src/whitenoise/database/rust_migrations/`; the old
  `db_migrations/` directory is gone.

Product analytics is app-wide and device-local, so it should live with `SharedServices` and the
shared database. It should not be attached to `AccountSession` or any per-account database.

## Non-Goals

1. User-level analytics.
2. Retention, cohorts, funnels keyed by user identity, or account identity.
3. Exact message volume, exact group size, exact file size, or exact latency reporting.
4. Raw search queries, message text, usernames, group names, relay URLs, event IDs, pubkeys, npubs,
   nsecs, device IDs, APNs tokens, FCM tokens, media hashes, file names, or NIP-05 values.
5. Automatic tracking of every public method call.
6. Persistent event queues.
7. Adding an analytics SDK directly to Flutter as the main event path.

## First Principles

Product analytics for White Noise should answer product questions without creating a shadow
identity system.

The product team needs aggregate signals: which features are used, where flows fail, which
platforms have problems, and which releases regress. None of those questions require knowing who
sent a message, which group they sent it to, what relay they used, or what content they handled.

That gives three hard rules:

1. There is no stable analytics identity.
2. Event payloads are closed-world, reviewed Rust types.
3. Event delivery is disposable.

The third rule matters. A durable queue would improve delivery, but it would also create a local
analytics database and raises hard opt-out semantics. Product analytics should lose events before
it stores privacy-sensitive exhaust.

## Threat Model

Analytics can become privacy-hostile through small mistakes:

- A developer puts dynamic data in an event name.
- A prop key starts as harmless and later carries user input.
- A failure event includes an error string with a relay URL, event ID, or pubkey.
- Offline queueing sends events after the user opts out.
- A self-hosted reverse proxy stores IP addresses forever.
- Flutter bypasses Rust validation by using an SDK directly.
- Debug builds send test data into production dashboards.

Mitigations:

- Export typed event enums instead of accepting arbitrary event names.
- Export typed property values and controlled bucket enums.
- Keep the analytics queue in memory only.
- Purge the queue immediately on opt-out.
- Use a separate debug Aptabase app key or mark `isDebug=true`.
- Turn off or minimize production access logs for the Aptabase host and reverse proxy.
- Document that Flutter must not include the Aptabase SDK for White Noise product events.

## Consent Model

Consent is app-level and device-local. It should not be tied to any Nostr account, because account
identity is exactly what the analytics layer must avoid. A person who uses White Noise on two
devices has two separate analytics consent states and two unrelated ephemeral analytics sessions.

Persisted state:

```rust
pub struct ProductAnalyticsSettings {
    pub enabled: bool,
    pub updated_at: DateTime<Utc>,
    pub consent_version: String,
}
```

Recommended storage:

- Add a new single-row `product_analytics_settings` table.
- Store it in the shared database, not a per-account database.
- Add a global Rust migration in `src/whitenoise/database/rust_migrations/`; the migration for this
  branch is `m0045_product_analytics_settings`.
- Register the migration in `all_global_migrations()`.
- Do not add a SQLx migration under `db_migrations/`; that directory no longer exists.
- Fresh installs run the Rust migration timeline after the baseline schema, so
  `m0045_product_analytics_settings` should create the table for both existing and fresh databases.
- Do not store app keys, hosts, session IDs, or event queues in SQLite.

Why a separate table:

- Consent is a privacy boundary, not a UI preference.
- It keeps the existing `app_settings` table focused on display settings.
- It keeps analytics consent out of account-scoped databases.
- It lets future consent metadata evolve without turning `app_settings` into a catch-all row.

Initial defaults:

- `enabled = false`
- `consent_version = "product-analytics-v1"`
- `updated_at = now`

Opt-in behavior:

- `set_product_analytics_enabled(true)` persists consent.
- The analytics worker may start accepting events after the setting is saved.
- Rust may record an `analytics_enabled` event after opt-in if that event is in the allowlist.

Opt-out behavior:

- `set_product_analytics_enabled(false)` persists the change first.
- The in-memory queue is dropped.
- No `analytics_disabled` event is sent.
- Future calls to track events return `IgnoredDisabled`.

## Backend Model

Use a small internal Aptabase client instead of adding a general analytics dependency.

Request shape:

- Method: `POST`
- URL: `{host}/api/v0/events`
- Headers:
  - `Content-Type: application/json`
  - `App-Key: <aptabase app key>`
- Body: JSON array of events.
- Batch size: at most 25.

Configuration:

```rust
#[derive(Debug, Clone)]
pub struct ProductAnalyticsConfig {
    pub backend: ProductAnalyticsBackend,
    pub app_version: String,
    pub bundle_identifier: String,
    pub device_class: ProductAnalyticsDeviceClass,
    pub os_name: String,
    pub locale: String,
    pub is_debug: bool,
}

#[derive(Debug, Clone)]
pub enum ProductAnalyticsBackend {
    Disabled,
    Aptabase(AptabaseAnalyticsConfig),
}

#[derive(Debug, Clone)]
pub struct AptabaseAnalyticsConfig {
    pub app_key: String,
    pub host: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum ProductAnalyticsDeviceClass {
    Phone,
    Tablet,
    Desktop,
    Unknown,
}
```

`host` should be explicit and should point at the self-hosted Aptabase origin. The client should
not infer Aptabase Cloud endpoints from the app key.

`WhitenoiseConfig` should gain:

```rust
pub product_analytics_config: Option<ProductAnalyticsConfig>,
```

and a builder:

```rust
pub fn with_product_analytics_config(
    mut self,
    config: ProductAnalyticsConfig,
) -> Self
```

`WhitenoiseConfig` is moved into `SharedServices` during `Whitenoise::from_components`, so the
analytics service should read its immutable backend/system-prop config from `self.shared.config`.
If this config is absent, tracking calls return `IgnoredUnconfigured`.

Configuration source:

- Flutter should populate `ProductAnalyticsConfig` from its build/flavor configuration and pass it
  into `WhitenoiseConfig`.
- Do not hardcode the Aptabase app key or host in `whitenoise-rs` source.
- Do not rely on runtime environment variables for iOS or Android builds; mobile app processes do
  not have a dependable environment-variable contract.
- A compile-time environment fallback such as `option_env!("WHITENOISE_APTABASE_APP_KEY")` may be
  useful for CLI/dev/test binaries, but it still embeds the value in the compiled binary.
- Treat the Aptabase app key as a client-side ingest key, not a server secret. Anyone with the app
  binary can extract client-side analytics configuration.

## Session Model

Aptabase expects a `sessionId`. Generate an ephemeral session ID in memory when the analytics
worker starts.

Rules:

- Never persist the session ID.
- Never derive it from a pubkey, device ID, install ID, push token, or database key.
- Rotate it on app process start.
- Rotate it when analytics is enabled.
- Consider rotating it on app foreground if Flutter exposes a clean lifecycle hook.

The Aptabase custom SDK guide describes the session ID format as epoch seconds plus eight random
digits. Implement that format unless Aptabase changes the contract.

## System Properties

Send the smallest useful `systemProps` set:

```json
{
  "locale": "en",
  "osName": "iOS",
  "isDebug": false,
  "bundleIdentifier": "com.example.whitenoise",
  "deviceClass": "phone",
  "appVersion": "1.0.0",
  "sdkVersion": "whitenoise-rs@0.2.1"
}
```

Avoid by default:

- device model
- OS patch version
- device identifier
- install identifier
- timezone
- carrier
- network type

`osName`, `locale`, `appVersion`, and `deviceClass` can come from Flutter because Flutter has the
best platform view. Flutter should also pass the platform bundle identifier or Android application
ID so staging and production builds are clearly separated in Aptabase. Rust should accept these
values only as bounded build metadata and should reject empty or suspicious values.

Bundle identifier rules:

- Required when analytics backend is configured.
- Sent on every analytics event as `systemProps.bundleIdentifier`.
- Used only to distinguish builds, such as staging and production.
- Never derived from account, device, install, push token, or Nostr identity.
- Bounded to a short ASCII reverse-DNS style value.

Device class rules:

- Required when analytics backend is configured.
- Sent on every analytics event as `systemProps.deviceClass`.
- Allowed values: `phone`, `tablet`, `desktop`, `unknown`.
- Never derived from a device identifier or install identifier.

## Public Rust API

These APIs should be exported from `src/lib.rs` so flutter_rust_bridge can generate bindings.
The Flutter bridge should hold the same `Arc<Whitenoise>` handle created by `Whitenoise::new` or
`Whitenoise::ensure_initialized`.

```rust
pub use whitenoise::product_analytics::{
    ProductAnalyticsBackend,
    ProductAnalyticsConfig,
    ProductAnalyticsDeviceClass,
    ProductAnalyticsEvent,
    ProductAnalyticsEventName,
    ProductAnalyticsFlushStatus,
    ProductAnalyticsNumberProp,
    ProductAnalyticsSettings,
    ProductAnalyticsStringProp,
    ProductAnalyticsTrackStatus,
};
```

`Whitenoise` methods:

```rust
impl Whitenoise {
    pub async fn product_analytics_settings(&self) -> Result<ProductAnalyticsSettings>;

    pub async fn set_product_analytics_enabled(
        &self,
        enabled: bool,
        consent_version: String,
    ) -> Result<ProductAnalyticsSettings>;

    pub async fn track_product_analytics_event(
        &self,
        event: ProductAnalyticsEvent,
    ) -> Result<ProductAnalyticsTrackStatus>;

    pub async fn flush_product_analytics(
        &self,
    ) -> Result<ProductAnalyticsFlushStatus>;
}
```

Tracking returns a status instead of treating disabled analytics as an error:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum ProductAnalyticsTrackStatus {
    Queued,
    IgnoredDisabled,
    IgnoredUnconfigured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum ProductAnalyticsFlushStatus {
    Flushed,
    NothingToFlush,
    Disabled,
    Unconfigured,
    TimedOut,
}
```

Invalid events should return `Err(WhitenoiseError::ProductAnalytics(...))`, because that is a
developer bug in the Flutter caller or Rust event registry.

## Flutter-Facing Event Type

Avoid a raw `HashMap<String, serde_json::Value>` at the public boundary. A dynamic JSON map makes
it easy to smuggle sensitive data.

Use string and number prop lists because they are easy for Flutter to construct and map to
Aptabase's allowed property types:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProductAnalyticsEvent {
    pub name: ProductAnalyticsEventName,
    pub string_props: Vec<ProductAnalyticsStringProp>,
    pub number_props: Vec<ProductAnalyticsNumberProp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct ProductAnalyticsStringProp {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ProductAnalyticsNumberProp {
    pub key: String,
    pub value: f64,
}
```

The validator should reject:

- unknown prop keys for the event name
- empty event names after serialization
- duplicate prop keys
- strings over a small fixed length
- non-finite numbers
- exact raw counts where a bucket is required
- any value that matches known sensitive patterns, such as hex pubkeys, npubs, nsecs, relay URLs,
  event IDs, group IDs, APNs tokens, FCM tokens, file paths, or Blossom URLs

The pattern check is a backstop. The main control is the typed event registry.

## Event Registry

Start with an event set broad enough to answer the first-pass product questions across onboarding,
login, messaging, media, groups, notifications, settings, and failures. Every event still needs a
clear product question, an allowlisted name, and reviewed props.

Event names:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum ProductAnalyticsEventName {
    AnalyticsEnabled,
    AppStarted,
    AppForegrounded,
    AppBackgrounded,
    OnboardingStarted,
    OnboardingCompleted,
    IdentityCreated,
    LoginStarted,
    LoginCompleted,
    LoginFailed,
    MessageSendStarted,
    MessageSendCompleted,
    MessageSendFailed,
    GroupCreateStarted,
    GroupCreateCompleted,
    GroupCreateFailed,
    MembersAdded,
    MembersRemoved,
    GroupDataUpdated,
    MediaUploadStarted,
    MediaUploadCompleted,
    MediaUploadFailed,
    PushRegistrationCompleted,
    PushRegistrationFailed,
    SettingChanged,
}
```

Serialize to snake case:

- `AnalyticsEnabled` to `analytics_enabled`
- `AppStarted` to `app_started`
- `MessageSendFailed` to `message_send_failed`
- `GroupCreateStarted` to `group_create_started`
- `GroupCreateCompleted` to `group_create_completed`
- `GroupCreateFailed` to `group_create_failed`
- `MembersAdded` to `members_added`
- `MembersRemoved` to `members_removed`
- `GroupDataUpdated` to `group_data_updated`

Approved string props:

| Prop | Allowed Values |
| --- | --- |
| `platform` | `ios`, `android`, `macos`, `linux`, `windows`, `web`, `unknown` |
| `account_type` | `local_key`, `external_signer`, `unknown` |
| `chat_type` | `dm`, `group`, `unknown` |
| `media_kind` | `image`, `video`, `audio`, `pdf`, `other` |
| `error_kind` | `network`, `timeout`, `permission`, `validation`, `storage`, `crypto`, `unknown` |
| `setting` | `theme`, `language`, `notifications`, `analytics` |
| `value` | closed enum values for the selected setting |
| `member_count_bucket` | `1`, `2`, `3_5`, `6_10`, `11_25`, `26_plus` |
| `media_size_bucket` | `lt_1mb`, `1_5mb`, `5_25mb`, `25mb_plus` |
| `duration_bucket` | `lt_250ms`, `250ms_1s`, `1_5s`, `5_30s`, `30s_plus` |

Approved number props:

Use number props sparingly. Prefer bucket strings. Initial number props can be limited to:

| Prop | Events | Notes |
| --- | --- | --- |
| `schema_version` | all events | fixed product analytics schema version |

Do not send exact message counts, exact member counts, exact attachment sizes, exact retry counts,
or exact latencies in the first release.

## Per-Event Property Matrix

Implement a conservative first-pass matrix in Rust. The global prop allowlist is not enough; each
event should have a smaller event-specific allowlist.

Initial examples:

| Event | Allowed Props |
| --- | --- |
| `AppStarted` | `schema_version`, `platform` |
| `AppForegrounded` | `schema_version`, `platform` |
| `OnboardingStarted` | `schema_version` |
| `OnboardingCompleted` | `schema_version` |
| `LoginFailed` | `schema_version`, `account_type`, `error_kind`, `duration_bucket` |
| `MessageSendCompleted` | `schema_version`, `chat_type`, `duration_bucket` |
| `MessageSendFailed` | `schema_version`, `chat_type`, `error_kind`, `duration_bucket` |
| `GroupCreateStarted` | `schema_version` |
| `GroupCreateCompleted` | `schema_version`, `duration_bucket` |
| `GroupCreateFailed` | `schema_version`, `error_kind`, `duration_bucket` |
| `MembersAdded` | `schema_version`, `member_count_bucket` |
| `MembersRemoved` | `schema_version`, `member_count_bucket` |
| `GroupDataUpdated` | `schema_version` |
| `MediaUploadCompleted` | `schema_version`, `media_kind`, `media_size_bucket`, `duration_bucket` |
| `MediaUploadFailed` | `schema_version`, `media_kind`, `media_size_bucket`, `error_kind`, `duration_bucket` |
| `PushRegistrationCompleted` | `schema_version`, `platform` |
| `PushRegistrationFailed` | `schema_version`, `platform`, `error_kind` |
| `SettingChanged` | `schema_version`, `setting`, `value` |

The implementation should reject a globally valid prop when the specific event does not allow it.
It should also reject raw error strings, group names, group IDs, member pubkeys, relay URLs,
message IDs, and exact counts for every event.

## Internal Module Design

Add a module:

```text
src/whitenoise/product_analytics/
  mod.rs
  aptabase.rs
  client.rs
  events.rs
  settings.rs
  worker.rs
```

Responsibilities:

- `mod.rs`: public types and `Whitenoise` methods.
- `events.rs`: event enum, prop types, serialization, validation, privacy pattern checks.
- `settings.rs`: database-facing settings type and helpers.
- `client.rs`: provider-agnostic analytics client trait used by tests.
- `aptabase.rs`: Aptabase request body, HTTP client, host validation.
- `worker.rs`: in-memory queue, batching, flush, shutdown, opt-out purge.

`src/whitenoise/database/product_analytics.rs` should own SQL for consent settings only.

The service is app-level, so it belongs with shared services rather than an `AccountSession`.
`Whitenoise` should expose public methods that delegate through `self.shared.product_analytics`.

`src/whitenoise/shared.rs` should hold:

```rust
product_analytics: ProductAnalytics,
```

where `ProductAnalytics` contains:

- config
- in-memory queue sender
- ephemeral session ID
- HTTP client
- worker handle

`Whitenoise::from_components` should construct the analytics service while building
`SharedServices`. The worker should integrate with `Whitenoise::shutdown()`; either expose a
`ProductAnalytics::shutdown()` method called from `Whitenoise::shutdown()` or register the worker
with the existing background-task lifecycle.

Do not put analytics on `AccountSession`. Account sessions own per-account MLS, signer, relay, and
account database state; analytics consent and delivery are device-local and app-wide.

## Delivery Behavior

Tracking path:

1. Rust code emits an internal event for operations Rust owns, or Flutter calls
   `track_product_analytics_event` for app lifecycle/onboarding/UI-only events.
2. Rust loads or reads cached analytics settings.
3. If disabled, return `IgnoredDisabled`.
4. If unconfigured, return `IgnoredUnconfigured`.
5. Validate event name and props.
6. Add system props.
7. Enqueue into an in-memory channel.
8. Return `Queued` without waiting for HTTP.

Worker path:

1. Read queued events.
2. Send continuously as events arrive; do not wait until shutdown to deliver analytics.
3. Batch opportunistically up to 25 events when a burst is already queued.
4. Send to Aptabase.
5. Log failures with `tracing::warn!(target: "whitenoise::product_analytics", ...)`.
6. Drop failed batches after bounded retry attempts.

Failures should never bubble into app product flows after an event is accepted into the queue.
`flush_product_analytics` should drain pending in-memory events on demand, app backgrounding, and
`Whitenoise::shutdown()`, but normal delivery should happen throughout the process lifetime. Never
persist events.

## Host and Key Validation

Validation should happen during `WhitenoiseConfig` normalization or analytics initialization.

Host rules:

- Must parse as `https://...` for release builds.
- `http://localhost` and `http://127.0.0.1` may be accepted in debug/test builds.
- Must not include path, query, or fragment.
- Must have no embedded credentials.

App key rules:

- Must be non-empty when backend is Aptabase.
- Must be logged only as redacted text.
- Should never be stored in SQLite.

## Self-Hosted Operations

The self-hosted Aptabase deployment is outside this crate, but the plan depends on operational
privacy:

- Serve Aptabase over HTTPS.
- Disable long-lived reverse proxy access logs, or strip IP/User-Agent where possible.
- Separate debug and production app keys.
- Limit dashboard access to the White Noise team.
- Set analytics retention to 90 days.
- Document retention and export practices.
- Monitor ingestion errors without logging raw event bodies.

Aptabase Cloud is not part of the first release plan. The client shape still keeps backend config
explicit so the host and app key remain environment-specific.

## Flutter Integration Contract

Flutter owns:

- Consent UI copy for onboarding and settings.
- Calling `set_product_analytics_enabled`.
- Passing platform system props at initialization, including bundle identifier and device class.
- Calling the Rust analytics API for app lifecycle, onboarding, and UI-only events.
- Updating App Store / Play Store disclosures.

Rust owns:

- Persisting consent.
- Enforcing disabled-by-default behavior.
- Validating events.
- Emitting analytics for product operations Rust owns.
- Sending to Aptabase.
- Dropping events on opt-out.
- Redacting analytics logs.

Flutter should not import `aptabase_flutter` for White Noise product events. All calls to Aptabase
for product analytics should originate in `whitenoise-rs`.

Example Flutter call shape after FRB generation:

```dart
await api.setProductAnalyticsEnabled(
  enabled: true,
  consentVersion: 'product-analytics-v1',
);

await api.trackProductAnalyticsEvent(
  event: ProductAnalyticsEvent(
    name: ProductAnalyticsEventName.onboardingCompleted,
    stringProps: [],
    numberProps: [
      ProductAnalyticsNumberProp(key: 'schema_version', value: 1),
    ],
  ),
);
```

## Test Plan

Unit tests:

- Default analytics settings are disabled.
- Enabling and disabling consent persists correctly.
- The global Rust migration creates `product_analytics_settings`.
- The migration is registered in `all_global_migrations()`.
- Disabled tracking sends no HTTP request.
- Unconfigured tracking returns `IgnoredUnconfigured`.
- Opt-out purges queued events.
- Event validator rejects unknown event names and prop keys.
- Event validator rejects a globally valid prop when that event does not allow it.
- Event validator rejects duplicate prop keys.
- Event validator rejects long strings and non-finite numbers.
- Event validator rejects sensitive patterns.
- Aptabase client sends `App-Key`, JSON list body, and max 25 events per batch.
- Sent event bodies include `systemProps.bundleIdentifier`.
- Sent event bodies include `systemProps.deviceClass`.
- Analytics config rejects empty or malformed bundle identifiers.
- Analytics config rejects unknown device classes.
- Failed HTTP sends do not fail the caller after queueing.
- Debug/test builds allow localhost host config.
- Release host validation rejects non-HTTPS hosts.

Integration-style tests with `mockito`:

- Enabled analytics queues and flushes one approved event.
- Multiple events are batched with a max batch size of 25.
- Events are sent during normal operation, before shutdown.
- `flush_product_analytics` drains pending events before app shutdown.

Manual app tests:

- Fresh install sends no events before opt-in.
- Onboarding offers an analytics consent choice.
- Opt-in sends only events after consent.
- Opt-out stops future delivery.
- A second device starts with analytics disabled even if another device opted in.
- App background flush does not block UI past the agreed timeout.

Verification commands:

```sh
just check-fmt
just check-clippy
just test
just precommit-quick
```

## Implementation Phases

### Phase 1: Consent and API Types

- Add `ProductAnalyticsSettings`.
- Add `src/whitenoise/database/product_analytics.rs`.
- Add global Rust migration `m0045_product_analytics_settings`.
- Register `m0045_product_analytics_settings` in
  `src/whitenoise/database/rust_migrations/mod.rs`.
- Add public settings methods on `Whitenoise`.
- Re-export public types from `src/lib.rs`.
- Add unit tests for default-off and setting updates.

### Phase 2: Event Registry and Validator

- Add `ProductAnalyticsEventName`.
- Add prop structs and allowed prop registry.
- Add a conservative per-event prop matrix.
- Add serialization to Aptabase event names.
- Add privacy pattern backstop.
- Add tests for allowed and rejected events.

### Phase 3: Aptabase Client

- Add internal HTTP client for `/api/v0/events`.
- Add host and app key validation.
- Add request/response tests with `mockito`.
- Keep provider-specific code behind the `ProductAnalyticsBackend::Aptabase` variant.

### Phase 4: Worker and Flush

- Add in-memory queue and batching.
- Add flush API.
- Add the analytics service to `SharedServices`.
- Send events continuously as they arrive.
- Integrate shutdown behavior with `Whitenoise::shutdown()`.
- Drop queue on opt-out.
- Add tests for queue, batching, failure, and opt-out purge.

### Phase 5: Flutter Wiring

- Regenerate FRB bindings.
- Add consent UI and settings copy in Flutter.
- Add onboarding consent step.
- Add calls for app lifecycle, onboarding, and UI-only events.
- Add App Store / Play Store disclosure updates.

### Phase 6: Self-Hosted Deployment

- Use the existing self-hosted Aptabase deployment.
- Configure production and debug apps.
- Document app keys and host config outside git.
- Set analytics retention to 90 days.
- Set access-log retention policy.
- Verify dashboard events from a test build before production rollout.

## Deferred Decisions

- The public transparency page belongs in another repo. This crate should keep the event registry
  and prop inventory clear enough for that page to publish later.

## Recommendation

Build this as a Rust-owned, opt-in, allowlisted analytics subsystem with an internal Aptabase
client pointed at the self-hosted Aptabase instance. Start with a broad event registry that answers
the main product questions, but keep each event and prop reviewed, typed, bucketed where needed,
and free of identifiers. This gives Flutter a simple API while keeping the privacy boundary in the
same layer that owns the product logic.

The first implementation should answer product questions across the core app flows:

- Are users completing onboarding?
- Are messages failing to send?
- Are media uploads failing?
- Are push registrations failing?
- Which settings are changed often enough to improve?
- Where do login, group creation, member management, discovery, and notification setup fail?
- Which coarse build, platform, and device class combinations see errors?

New events should still require a concrete product question and a reviewed event proposal.
