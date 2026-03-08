# Plan: Relay Control Plane Rearchitecture

## Recommendation

Keep `nostr-sdk` / `nostr-relay-pool` for transport, reconnect, and relay
machinery. Change White Noise's control plane above that layer.

The core issue is not upstream ownership of the relay stack. The issue is that
White Noise currently treats one shared `nostr-sdk::Client` as if it were all
of these at once:

- a stable discovery and indexing client
- a stable group-message listener
- an authenticated inbox listener
- an anonymous publisher
- a transient query engine

Those workloads need different relay sets, connection lifetimes,
authentication rules, retry policies, and observability. The fix is to stop
orchestrating everything through one shared client and replace that model with
explicit relay planes.

## Why the Current Boundary is Wrong

Today `NostrManager` stores one shared `Client` in
`src/nostr_manager/mod.rs`, and the rest of the module assumes that client is
the universal execution surface:

- `with_signer` temporarily attaches a signer to the shared client
- `ensure_relays_connected` grows the shared relay pool and then calls
  `client.connect()`
- `publisher.rs`, `query.rs`, and `subscriptions.rs` all operate through that
  same client

That creates a few hard constraints that White Noise should not keep:

1. Relay auth becomes global even though only some relay classes should auth.
2. Relay pool growth becomes global even though some relays are transient.
3. Subscription bookkeeping becomes global even though some subscriptions are
   session-critical and others are disposable.
4. Notification handling, retry, and health policy become global even though
   each workload needs different behavior.

`NostrManager` is still useful, but not as the system boundary. It is a good
home for parsing helpers, stateless event helpers, and thin session-level
wrappers. The new control-plane boundary should sit above it.

## Target Architecture

Introduce a new top-level module:

```text
src/
  relay_control/
    mod.rs
    router.rs
    discovery.rs
    groups.rs
    account_inbox.rs
    ephemeral.rs
    sessions/
      mod.rs
      session.rs
      notifications.rs
      config.rs
    observability.rs
```

Recommended ownership model:

- `Whitenoise` owns a `RelayControlPlane`
- `RelayControlPlane` owns:
  - one discovery session
  - one group session
  - zero or more per-account inbox sessions
  - an ephemeral session factory for one-off work
- each session is built on a reusable `RelaySession`
- `NostrManager` becomes a thin compatibility facade during migration

Suggested reusable primitive:

```rust
struct RelaySession { /* wraps one nostr-sdk::Client */ }
```

Responsibilities of `RelaySession`:

- own exactly one `nostr-sdk::Client`
- register exactly one notification handler
- expose targeted connect, disconnect, subscribe, query, and publish methods
- emit structured relay telemetry
- never own cross-plane routing decisions

## Relay Planes

### Discovery Plane

Purpose:

- fetch metadata
- fetch follow lists
- fetch relay lists
- fetch inbox relay lists
- fetch key package relay lists and related routing metadata
- keep local discovery and routing data fresh

Properties:

- long-lived
- stable
- unauthenticated
- curated relay set
- low relay churn

Relay sources:

- primary indexers
  - `wss://index.hzrd149.com`
  - `wss://indexer.coracle.social`
  - `wss://purplepag.es`
- curated general relays
  - `wss://relay.primal.net`
  - `wss://relay.damus.io`
  - `wss://relay.ditto.pub`
  - `wss://nos.lol`

Important rule:

Discovery relays are an explicit curated set. They are not "whatever relays
the app happens to be connected to."

### Group Plane

Purpose:

- maintain long-lived subscriptions for MLS group messages
- update filters as the watched set of group IDs changes

Properties:

- long-lived
- stable
- unauthenticated
- small relay set
- subscription churn is expected
- connection churn is not

Important rule:

This plane must never authenticate. Group messages are published by ephemeral
keys and consumed anonymously, so a relay that requires auth here is a policy
mismatch.

### Account Inbox Plane

Purpose:

- maintain long-lived giftwrap subscriptions on inbox relays
- support relays that require auth to read messages for a specific account

Properties:

- one plane per logged-in account
- long-lived
- auth-capable
- signer remains attached for the full account session

Important rules:

- this is the only long-lived auth-capable plane
- do not reuse temporary `with_signer` attachment on a shared client
- support both local signers and external signers through a long-lived signer
  handle
- distinguish "supported signer type" from "guaranteed silent background auth
  experience"

### Ephemeral Plane

Purpose:

- targeted one-off queries
- welcome delivery to recipient inbox relays
- bounded fallback fetches when discovery data is incomplete

Properties:

- short-lived or non-reconnecting
- targeted
- no long-lived subscriptions
- default unauthenticated

Important rule:

Welcome publishing belongs here, not on a long-lived shared client.

## Cross-Cutting Design Rules

### Subscription Privacy and Routing

Relay-facing subscription IDs should stay opaque and privacy-preserving.
Internal routing should not depend on parsing those IDs.

Use two layers:

1. relay-facing opaque subscription IDs
2. local typed subscription context used only inside White Noise

Suggested shape:

```rust
struct SubscriptionContext {
    plane: RelayPlane,
    account_pubkey: Option<PublicKey>,
    relay_url: RelayUrl,
    stream: SubscriptionStream,
}
```

Incoming events should be routed by looking up the opaque ID in a local map,
not by decoding string prefixes.

### Event Intake

Keep a central event-processing layer, but move to typed source context:

```rust
enum ProcessableEvent {
    NostrEvent {
        event: Event,
        source: EventSource,
        retry_info: RetryInfo,
    },
    RelayTelemetry(RelayTelemetryEvent),
}
```

The event processor should continue to own:

- deduplication and processed-event tracking
- domain routing
- retry of application-level processing failures

It should not own:

- relay reconnect policy
- auth retry policy
- transport backoff

### Observability

Relay health must be tracked per `(plane, relay_url, account_pubkey?)`, not
just per relay URL. The same relay may behave differently in discovery, group,
and account inbox contexts.

Recommended stored data:

- relay URL
- plane
- account pubkey when applicable
- last connect attempt timestamp
- last connect success timestamp
- last failure timestamp
- failure category
- last `NOTICE` / `CLOSED` / `AUTH` reason
- auth-required flag
- rolling success and failure counters
- latency sample or aggregate
- backoff-until timestamp

Recommended tables:

- `relay_status`
- `relay_events`

Stored state must influence retry policy but must never become a permanent hard
blacklist.

### Auth Policy

Default auth policy by plane:

- discovery: auth disabled
- group: auth disabled
- ephemeral: auth disabled
- account inbox: auth allowed

### Retry Policy

Retry policy must be explicit per plane:

- discovery: reconnect enabled, slow conservative backoff
- group: reconnect enabled, freshness-biased backoff
- account inbox: reconnect enabled, auth-aware retry behavior
- ephemeral: bounded one-shot retries, no background reconnect loop

### Gossip

Do not make `nostr-sdk` gossip the foundation of the new design.

If adopted later, keep it private to the discovery plane and use it only for
public discovery workloads. It should not be the core mechanism for:

- authenticated inbox reading
- group relay listening
- welcome delivery

## Proposed Interface Direction

The exact API can change, but the boundary should look like this:

```rust
impl RelayControlPlane {
    async fn start_discovery_plane(&self) -> Result<()>;
    async fn update_discovery_relays(&self, relays: &[RelayUrl]) -> Result<()>;

    async fn update_group_plane(&self, relays: &[RelayUrl], group_ids: &[String]) -> Result<()>;

    async fn activate_account_inbox_plane(
        &self,
        account: &Account,
        inbox_relays: &[RelayUrl],
        signer: impl NostrSigner + 'static,
    ) -> Result<()>;

    async fn deactivate_account_inbox_plane(&self, pubkey: PublicKey) -> Result<()>;

    async fn publish_welcome(
        &self,
        receiver: PublicKey,
        rumor: UnsignedEvent,
        relays: &[RelayUrl],
        signer: impl NostrSigner + 'static,
    ) -> Result<Output<EventId>>;

    async fn fetch_user_profile_bundle(&self, pubkey: PublicKey) -> Result<UserProfileBundle>;
}
```

The important point is the routing boundary: plane selection happens in the
control plane, not inside a universal client object.

## Implementation Snapshot

Status as of March 8, 2026:

- completed:
  - `RelayControlPlane`, `RelayPlane`, `SubscriptionContext`, and
    `SubscriptionStream` are in place
  - `relay_status` and `relay_events` schema plus DB access helpers are in
    place
  - migrated discovery, group, and account inbox sessions now persist
    telemetry into `relay_status` and `relay_events`
  - `RelaySession` exists and is used by the dedicated discovery, group, and
    account inbox planes
  - long-lived discovery subscriptions run on the discovery plane
  - long-lived MLS group subscriptions run on the group plane
  - long-lived giftwrap subscriptions run on per-account inbox planes
  - relay-plane events already enter the event processor with typed source
    context instead of legacy subscription-ID parsing
- in progress:
  - `NostrManager` still exists as a compatibility path for legacy fetch and
    publish work
- still legacy / not started:
  - one-off discovery fetches still use the shared client
  - welcome publishing and other targeted publish/query work still use the
    shared client
  - the ephemeral plane is still a placeholder
  - the shared `nostr-sdk::Client` inside `NostrManager` is still the
    production path for remaining publish/query call sites

This means the migration is no longer a strict phase-by-phase sequence. Some
later subscription phases landed before the observability runtime wiring and
before the final removal of shared-client compatibility code. The rest of this
document keeps the original phase structure, but each phase below now includes
an explicit implementation status.

## Migration Rules

Every implementation phase below must obey these rules:

- the phase must land independently and leave the app in a working state
- the phase must include all code, tests, and docs needed to prove the phase
  works on its own
- compatibility shims are allowed, but only for unfinished later phases
- no phase may depend on a later phase to restore existing functionality
- do not widen relay fanout as part of transitional compatibility code
- validate with `just precommit-quick` at minimum
- when a phase changes network behavior, also run targeted Docker-backed
  integration scenarios with `just docker-up` and `just int-test <scenario>`

## Implementation Phases

### Phase 0: Define the Boundary and Scaffolding

Status: Completed on March 7, 2026.

Objective:

Create the new internal boundary without changing runtime behavior.

In scope:

- add `src/relay_control/` with module skeletons
- define shared types used by later phases:
  - `RelayControlPlane`
  - `RelayPlane`
  - `SubscriptionContext`
  - `SubscriptionStream`
  - relay telemetry enums and config structs
- decide the ownership model in `Whitenoise`
- add comments and docs that describe the new boundary

Out of scope:

- no new relay behavior
- no new DB migrations
- no new client instances used in production
- no call-site routing changes

Deliverables:

- compiling `relay_control` module tree
- initial type definitions with clear responsibilities
- `Whitenoise` wiring that can host the new control plane, even if the field
  is not yet active in production paths
- updated plan/docs if naming changed during implementation

Validation steps:

1. `just check-fmt`
2. `just check-clippy`
3. `just test`
4. `just precommit-quick`

Success state:

- the repo compiles with the new `relay_control` boundary present
- there is no user-visible behavior change
- later phases can add behavior inside the new boundary without renaming the
  core types again

### Phase 1: Build Observability First

Status: Completed on March 8, 2026.

Objective:

Introduce structured relay telemetry types, classification, and persistence
before changing relay ownership or retry behavior.

In scope:

- add DB tables and data-access code for `relay_status` and `relay_events`
- define structured telemetry types that can represent `NOTICE`, `CLOSED`,
  `AUTH`, connect, disconnect, publish, query, and subscription outcomes
- add notification/error classification helpers for explicit failure
  categories
- include plane and account context in logs and persisted events
- classify failures into explicit categories such as `transport`, `timeout`,
  `auth_required`, `auth_failed`, `relay_policy`, `invalid_filter`,
  `rate_limited`, `closed_by_relay`, and `unknown`

Out of scope:

- no plane split yet
- no retry-policy changes yet
- no auth-policy changes yet
- no instrumentation of the legacy shared `NostrManager` client
- no new routing decisions based on stored status beyond lightweight read/write
  plumbing

Deliverables:

- new SQL migration files for `relay_status` and `relay_events`
- relay observability data-access layer under `src/whitenoise/database/`
- structured telemetry types and notification/error classification code
- observability APIs that later phases can call when new relay sessions are
  introduced
- unit tests for telemetry classification and persistence

Validation steps:

1. Add unit tests that classify representative `NOTICE`, `CLOSED`, and `AUTH`
   payloads into the expected failure categories.
2. Add tests for DB write and read behavior for `relay_status` and
   `relay_events`.
3. Run `just precommit-quick`.

Success state:

- structured relay telemetry can be classified and persisted with plane and
  account context
- relay status and recent events are persisted in the database
- migrated control-plane subscription traffic persists telemetry to
  `relay_status` and `relay_events`
- the legacy shared client remains intentionally uninstrumented for now

### Phase 2: Extract `RelaySession`

Status: Partially completed as of March 8, 2026.

Objective:

Create the reusable single-session primitive and make the existing shared
client flow use it internally.

In scope:

- implement `relay_control::sessions::RelaySession`
- move shared client setup and notification-handler registration into
  `RelaySession`
- move plane-neutral connect, disconnect, subscribe, publish, and query
  helpers into the session layer
- emit structured telemetry from `RelaySession`
- wrap the current `NostrManager` shared-client behavior around one
  compatibility `RelaySession`

Out of scope:

- no separate discovery, group, inbox, or ephemeral production sessions yet
- no call-site routing changes yet
- no auth model change yet

Deliverables:

- `RelaySession` implementation with a single notification handler
- session config types for auth, reconnect, and relay policy
- compatibility adapter that preserves the current shared-client behavior
- tests for session setup and notification wiring

Validation steps:

1. Add unit tests that verify session construction preserves:
   - one client per session
   - one notification handler per session
   - telemetry emission for connection and subscription events
2. Run `just precommit-quick`.
3. Run Docker-backed regression scenarios:
   - `just docker-up`
   - `just int-test basic-messaging`
   - `just int-test user-discovery`
   - `just int-test login-flow`
   - `just docker-down`

Success state:

- dedicated relay planes already execute through `RelaySession`
- the codebase can instantiate more than one client session without
  duplicating setup logic
- remaining work:
  - decide whether the legacy shared-client compatibility path should itself be
    wrapped by `RelaySession` before deletion, or whether we should delete it
    directly as later call sites migrate

### Phase 3: Stand Up the Group Plane

Status: Completed on March 8, 2026, for long-lived group subscriptions.

Objective:

Move MLS group-message subscriptions off the shared session and onto a
dedicated unauthenticated group plane.

In scope:

- create the long-lived group plane and its session
- route group relay selection and group subscriptions through the group plane
- update group filters as the watched set of group IDs changes
- emit `RelayPlane::Group` context for group traffic
- ensure auth is disabled for this plane

Out of scope:

- discovery fetches stay on the compatibility/shared path
- giftwrap subscriptions stay on the compatibility/shared path
- welcome publishing stays on the compatibility/shared path
- no gossip work

Deliverables:

- `relay_control/groups.rs`
- control-plane API for updating the watched group relay set and group ID set
- call-site migration for group subscription setup and refresh
- tests that cover group-plane routing and auth-disabled config

Validation steps:

1. Add unit tests for:
   - group-plane config always disabling auth
   - dynamic group filter updates preserving the full watched group set
   - typed subscription context for group subscriptions
2. Run `just precommit-quick`.
3. Run Docker-backed scenarios:
   - `just docker-up`
   - `just int-test basic-messaging`
   - `just int-test group-membership`
   - `just int-test message-streaming`
   - `just docker-down`

Success state:

- MLS group subscriptions are owned by the dedicated group plane
- group-message delivery works without relying on the old shared subscription
  path
- no long-lived group subscription path can trigger relay auth

### Phase 4: Stand Up the Discovery Plane

Status: Partially completed as of March 8, 2026.

Objective:

Move discovery and indexing work onto a dedicated curated discovery plane.

In scope:

- create the long-lived discovery plane and its session
- define the curated discovery seed set
- move these workloads to the discovery plane:
  - metadata fetch
  - follow list fetch
  - relay list fetch
  - inbox relay list fetch
  - key package relay-list discovery
- stop treating "default relays plus currently connected relays" as the
  discovery fallback model

Out of scope:

- no gossip-based routing yet
- no move of welcome publishing yet
- no move of long-lived account inbox subscriptions yet
- no change to group-plane ownership from Phase 3

Deliverables:

- `relay_control/discovery.rs`
- explicit discovery relay configuration
- migrated discovery call sites
- tests that cover curated relay selection and discovery-only routing

Validation steps:

1. Add unit tests for:
   - curated discovery seed selection
   - routing decisions that send discovery work only to the discovery plane
   - fallback behavior when indexer results are incomplete
2. Run `just precommit-quick`.
3. Run Docker-backed scenarios:
   - `just docker-up`
   - `just int-test user-discovery`
   - `just int-test follow-management`
   - `just int-test login-flow`
   - `just int-test metadata-management`
   - `just docker-down`

Success state:

- completed:
  - long-lived discovery subscriptions now belong to the curated discovery
    plane
- remaining work:
  - move one-off discovery fetch/query call sites to the discovery or
    ephemeral plane
  - remove the old "shared client plus whatever is already connected" fallback
    model from discovery fetches

### Phase 5: Introduce Ephemeral Publish and Query Operations

Status: Not started beyond module scaffolding as of March 8, 2026.

Objective:

Move targeted one-off work onto short-lived session flows so long-lived planes
stop accumulating transient relays.

In scope:

- implement the ephemeral session factory
- move welcome publishing to ephemeral targeted operations
- move one-off fallback fetches to ephemeral operations where appropriate
- ensure ephemeral operations do not leave long-lived subscriptions behind
- ensure ephemeral operations do not mutate the long-lived relay sets owned by
  discovery, group, or account inbox planes

Out of scope:

- no long-lived inbox plane split yet
- no change to discovery-plane ownership from Phase 4
- no change to group-plane ownership from Phase 3

Deliverables:

- `relay_control/ephemeral.rs`
- bounded connect / publish / query helpers for one-off work
- migrated welcome publish path
- tests that prove long-lived relay sets are unchanged after ephemeral
  operations

Validation steps:

1. Add unit tests that verify:
   - ephemeral operations do not add relays to long-lived planes
   - ephemeral operations do not create long-lived subscriptions
   - bounded retry behavior is enforced
2. Run `just precommit-quick`.
3. Run Docker-backed scenarios:
   - `just docker-up`
   - `just int-test basic-messaging`
   - `just int-test notification-streaming`
   - `just int-test login-flow`
   - `just docker-down`

Success state:

- welcome delivery and targeted fallback fetches run through ephemeral
  operations
- long-lived plane relay membership remains stable before and after ephemeral
  work
- no new transient relays are accumulated on long-lived sessions

### Phase 6: Introduce Per-Account Inbox Planes

Status: Completed on March 8, 2026, for long-lived giftwrap subscriptions and
session lifecycle.

Objective:

Move authenticated inbox reading and giftwrap subscriptions to dedicated
per-account sessions with long-lived signer handles.

In scope:

- create one inbox session per logged-in account
- keep the signer attached for the full account session lifetime
- support both local signers and external signers through a persistent signer
  abstraction
- move giftwrap subscriptions to the per-account inbox plane
- allow auth only on this plane
- implement account login, logout, and session teardown behavior for inbox
  planes
- emit `RelayPlane::AccountInbox` with account context on all inbox events

Out of scope:

- do not add auth to discovery, group, or ephemeral planes
- do not assume every external signer can guarantee silent background auth
- do not remove compatibility code that still supports unfinished cleanup work

Deliverables:

- `relay_control/account_inbox.rs`
- account inbox session registry inside `RelayControlPlane`
- persistent signer-handle abstraction that works for local and external
  signers
- migrated giftwrap subscription path
- teardown logic on logout and account deactivation
- tests for multi-account isolation and logout cleanup

Validation steps:

1. Add unit tests for:
   - account session registry lifecycle
   - auth-enabled inbox config
   - isolation between two logged-in accounts
   - logout removing only the targeted account session
2. Run `just precommit-quick`.
3. Run Docker-backed scenarios:
   - `just docker-up`
   - `just int-test login-flow`
   - `just int-test account-management`
   - `just int-test notification-streaming`
   - `just int-test basic-messaging`
   - `just docker-down`

Success state:

- each logged-in account has its own inbox session
- giftwrap subscriptions no longer rely on a shared long-lived client
- logout tears down only the correct inbox session
- long-lived relay auth exists only on account inbox planes

### Phase 7: Remove Shared-Client Assumptions

Status: Partially completed as of March 8, 2026.

Objective:

Finish the migration by making `RelayControlPlane` the real system boundary and
eliminating universal shared-client assumptions from production code.

In scope:

- replace remaining production uses of `self.nostr.client`
- delete or deprecate shared-client orchestration helpers such as
  `ensure_relays_connected`
- move the `Whitenoise` boundary to `RelayControlPlane`
- update `ProcessableEvent` and related routing to use typed source context
- reduce `NostrManager` to parser helpers, stateless helpers, and thin
  compatibility code if anything still remains

Out of scope:

- no discovery-plane gossip adoption yet
- no transport-layer rewrite below `nostr-sdk`

Deliverables:

- `Whitenoise` wired through `RelayControlPlane`
- last relay call sites migrated
- typed event-source routing in the event processor
- removed or clearly deprecated shared universal-client orchestration APIs
- updated docs that describe the new steady-state architecture

Validation steps:

1. Add unit tests for typed event-source routing and subscription-context
   lookup.
2. Run `just precommit-quick`.
3. Run full Docker-backed regression coverage for the most relay-sensitive
   scenarios:
   - `just docker-up`
   - `just int-test basic-messaging`
   - `just int-test user-discovery`
   - `just int-test login-flow`
   - `just int-test account-management`
   - `just int-test group-membership`
   - `just int-test notification-streaming`
   - `just docker-down`

Success state:

- completed:
  - relay-plane event intake already uses typed source context in the event
    processor
- remaining work:
  - migrate remaining query and publish call sites off `NostrManager`
  - delete or sharply reduce shared-client orchestration helpers
  - make `RelayControlPlane` the only production relay boundary

### Phase 8: Re-evaluate Gossip for Discovery

Objective:

Optionally test whether discovery-plane-only gossip improves public discovery
quality without harming predictability.

In scope:

- add an internal discovery relay-provider interface if needed
- evaluate gossip only inside the discovery plane
- measure relay fanout, bootstrap predictability, and lookup quality
- keep the feature behind an internal flag or isolated adapter until the
  results are clear

Out of scope:

- no gossip for group listening
- no gossip for authenticated inbox reading
- no gossip for welcome delivery
- no mandatory default switch without evidence

Deliverables:

- either:
  - a discovery-plane gossip adapter behind a non-default gate, or
  - a decision record that says not to adopt gossip now
- benchmark notes or measurements
- regression tests for discovery behavior

Validation steps:

1. Run `just precommit-quick`.
2. Run Docker-backed discovery regressions:
   - `just docker-up`
   - `just int-test user-discovery`
   - `just int-test login-flow`
   - `just docker-down`
3. If a benchmarkable implementation is added, run:
   - `just docker-up`
   - `just benchmark user-discovery`
   - `just docker-down`

Success state:

- gossip is either rejected with documented reasons, or adopted only inside
  the discovery plane with measured benefit
- there is no regression in key package relay-list discovery, login bootstrap,
  or curated discovery behavior

## Changes Needed in `Whitenoise`

### Initialization

Current startup:

- creates one `NostrManager`
- adds default relays directly to its client
- starts `client.connect()`

Target startup:

- create `RelayControlPlane`
- initialize the discovery plane with curated discovery relays
- initialize the group plane with stored group relays
- delay account inbox plane startup until account activation or login

### Account Activation

Current activation:

- merges NIP-65, inbox, and key package relays
- calls `ensure_relays_connected` on the shared client
- refreshes subscriptions on that same client

Target activation:

- discovery plane remains separate
- account inbox plane starts for giftwrap subscriptions
- group plane remains separate
- key package publishing and fetching use explicit routing rules instead of
  shared-pool accumulation

### User Discovery

Current discovery:

- can fall back to defaults plus all connected relays

Target discovery:

- discovery plane only
- curated fallback only
- no dependence on unrelated connected relays

## Code Areas Likely to Change

Primary existing areas:

- `src/whitenoise/mod.rs`
- `src/whitenoise/accounts/setup.rs`
- `src/whitenoise/accounts/login.rs`
- `src/whitenoise/users/relay_sync.rs`
- `src/whitenoise/event_processor/event_handlers/handle_relay_list.rs`
- `src/whitenoise/event_processor/event_handlers/handle_contact_list.rs`
- `src/nostr_manager/mod.rs`
- `src/nostr_manager/publisher.rs`
- `src/nostr_manager/query.rs`
- `src/nostr_manager/subscriptions.rs`

New areas:

- `src/relay_control/`
- relay observability DB migrations
- relay observability DB access code

## Risks and Mitigations

Main risks:

- duplicate subscriptions during migration
- missing events while moving workloads between planes
- auth regressions on inbox relays
- incorrect account session teardown
- accidental widening of relay fanout during compatibility periods

Mitigations:

- land observability before behavior changes
- migrate one plane at a time
- keep compatibility wrappers short-lived
- add targeted integration coverage for discovery, group messaging, welcome
  delivery, authenticated inbox flow, and logout/relogin

## Decision Summary

Recommended path:

- do not rewrite the websocket or relay-pool layer from scratch
- do not keep growing the current single-client `NostrManager`
- introduce a `RelayControlPlane` with explicit relay planes
- reuse upstream `nostr-sdk` sessions inside those planes
- migrate incrementally in the order above

This gives White Noise a new control plane without an all-at-once transport
rewrite.

## Revisit Before Completion

### Group Subscription Privacy

Current group-plane behavior batches all `nostr_group_id` values for one
account into a single long-lived MLS subscription by placing every group ID
into the same `h`-tag filter.

That is operationally simple, but it has a privacy cost: a network observer who
can see subscription filters can infer the full set of group IDs currently being
tracked for that account.

Before this project is considered complete, revisit whether we should keep this
shape or move to a more privacy-preserving strategy, for example:

- smaller group batches per account
- one subscription per group
- rotating or partitioned group filters
- another approach that reduces group-set disclosure without making relay load
  or recovery behavior unacceptable
