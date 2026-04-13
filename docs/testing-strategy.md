# Testing Strategy: Post-Refactor Migration

Drafted: 2026-04-13

## Overview

The session/projection rearchitecture changes how the app is constructed and owned at runtime. This has a direct
impact on testing: the singleton removal (Phase 12) makes it possible — and necessary — to rethink how integration
tests work. This document describes the target testing model and when the transition happens.

## Three-Tier Testing Model

### Tier 1: Unit tests

Tests against in-memory databases and individually constructed services. No relays, no network, no Docker. These cover
services, repos, views, projection logic, chat list ordering, message aggregation, group membership CRUD, drafts,
settings, follows, key package lifecycle, and any business logic that doesn't require relay event propagation.

```rust
#[tokio::test]
async fn test_chat_list_sorts_pinned_first() {
    let db = Arc::new(AccountDatabase::open_in_memory().await.unwrap());
    let repos = AccountRepositories::new(db);
    // seed data, call the service, assert ordering
}
```

Run with `cargo test`. Parallel, fast, no infrastructure. This is where the bulk of coverage lives.

### Tier 2: In-process integration tests (MockRelay)

Tests that need relay behavior use `nostr-relay-builder::MockRelay` — a full in-process Nostr relay with WebSocket
server, in-memory event storage, NIP-01 filter support, and NIP-42 auth. No Docker required.

```rust
#[tokio::test]
async fn test_message_delivery_across_instances() {
    let relay = MockRelay::run().await.unwrap();
    let url = relay.url().await;

    let alice_wn = Whitenoise::new(test_config(&[url.clone()])).await.unwrap();
    let bob_wn = Whitenoise::new(test_config(&[url.clone()])).await.unwrap();

    let alice = alice_wn.create_identity(...).await.unwrap();
    let bob = bob_wn.create_identity(...).await.unwrap();

    let session_a = alice_wn.session(alice.pubkey).unwrap();
    session_a.groups().create(&[bob.pubkey]).await.unwrap();
    // ... verify Bob receives the welcome and can read messages
}
```

Each test spins up its own mock relays and its own `Whitenoise` instances. Zero shared state. Tests are parallel and
deterministic — relay-side latency is effectively zero since events propagate through in-memory storage.

MockRelay capabilities relevant to our tests:
- Full NIP-01 event storage and subscription filtering
- NIP-42 auth (for testing inbox plane authentication)
- Custom write/query policies (for simulating relay rejection or rate limiting)
- Unresponsive connection simulation (for testing reconnection and timeouts)
- Multiple independent instances per test (for testing relay topology)

`nostr-relay-builder` is already a transitive dependency via `nostr-sdk` — no new crate to add.

### Tier 3: Docker integration tests (external services only)

The only thing MockRelay can't replace is the Blossom media server. Media upload, download, blurhash generation, and
format validation need a real HTTP server that speaks the Blossom protocol.

Keep Docker for:
- `chat_media_upload` scenario (~1,289 LOC)
- Any future tests that require Transponder or other non-relay external services

Everything else moves to Tier 1 or Tier 2.

## When the transition happens

The current integration test suite (~15k LOC, 18 scenarios) depends on the `Whitenoise` singleton and a single shared
instance. It runs against Docker relay containers.

**During Phases 1–11:** Keep the current integration tests passing as-is. They are the validation step for each
refactor phase. Don't rewrite them while the refactor is in progress.

**At Phase 12 (singleton removal):** The current tests break because `Whitenoise::get_instance()` no longer exists.
This is the natural inflection point to rewrite them. At this point:

1. Write Tier 1 unit tests for services that were previously only testable via integration tests (account management,
   chat list ordering, drafts, settings, follows, scheduler lifecycle).
2. Write Tier 2 MockRelay tests for scenarios that need relay behavior (messaging, group membership, subscriptions,
   notifications, user discovery, login flow, streaming).
3. Delete the Docker-based versions of those scenarios — they're now covered by Tier 1 + Tier 2.
4. Keep the `chat_media_upload` scenario on Docker (Tier 3).

**After the transition:**
- `cargo test` runs Tier 1 + Tier 2. No Docker needed. Developers never need `just docker-up` for routine work.
- `just int-test` runs only the Blossom media tests (Tier 3). CI runs this; developers run it when touching media code.
- The integration test framework (Scenario/TestCase traits, ScenarioContext) is either simplified for Tier 3 or
  replaced entirely — most of its complexity (cleanup via logout-sleep-reset-wipe-sleep) exists because of the
  singleton and goes away with per-test instances.

## Scenario migration map

| Current scenario            | Target tier          | Reason                                                        |
| --------------------------- | -------------------- | ------------------------------------------------------------- |
| account-management          | Tier 1 (unit)        | Pure session lifecycle, no relay needed                       |
| app-settings                | Tier 1 (unit)        | Pure DB CRUD                                                  |
| drafts                      | Tier 1 (unit)        | Pure DB CRUD                                                  |
| metadata-management         | Tier 2 (MockRelay)   | Publishes metadata to relays                                  |
| basic-messaging             | Tier 2 (MockRelay)   | MLS + relay delivery                                          |
| advanced-messaging          | Tier 2 (MockRelay)   | MLS message modifications via relay                           |
| follow-management           | Tier 2 (MockRelay)   | Publishes contact list to relays                              |
| subscription-processing     | Tier 2 (MockRelay)   | External user publishes, Whitenoise picks up via subscription |
| group-membership            | Tier 2 (MockRelay)   | MLS key exchange + welcome delivery                           |
| chat-media-upload           | Tier 3 (Docker)      | Needs real Blossom media server                               |
| user-discovery              | Tier 2 (MockRelay)   | Relay queries for user metadata                               |
| scheduler                   | Tier 1 / Tier 2 split| Lifecycle is unit-testable; key package publish needs relay    |
| message-streaming           | Tier 2 (MockRelay)   | Multi-instance message delivery now testable                  |
| chat-list                   | Tier 1 (unit)        | Pure query/sort logic                                         |
| chat-list-streaming         | Tier 2 (MockRelay)   | Subscription-driven updates                                   |
| notification-streaming      | Tier 2 (MockRelay)   | Multi-instance — can finally test NewMessage notifications    |
| login-flow                  | Tier 2 (MockRelay)   | NIP-65 relay discovery                                        |
| user-search                 | Tier 2 (MockRelay)   | Relay-based user search                                       |

## What the refactor unlocks for testing

The singleton is the root cause of three testing limitations today:

1. **No parallel tests** — all tests share one `Whitenoise` instance via `OnceCell`. After the refactor, each test
   constructs its own instance with its own in-memory databases.

2. **No multi-instance tests** — you can't test "Alice sends, Bob receives" because both accounts live in the same
   instance. Notification filtering suppresses NewMessage notifications for same-instance senders. After the refactor,
   each test can create separate `Whitenoise` instances connected to the same MockRelay.

3. **Heavy cleanup between scenarios** — the current suite does logout-all → sleep → reset-nostr-client → wipe-database
   → sleep between every scenario to avoid state leakage. After the refactor, cleanup is just "let the instance drop."
   Temp directories handle the rest.
