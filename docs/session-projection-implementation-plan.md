# Implementation Plan: Session and Projection Rearchitecture

Drafted: 2026-04-12

## Context

The architecture doc (`docs/session-projection-rearchitecture.md`) defines the target shape. This document breaks the
refactor into concrete, independently-landable phases that should stay under ~1k LOC each, with enough detail to
surface type issues and implementation blockers early.

Based on codebase exploration:
- 196 total `impl Whitenoise` methods: 113 account-scoped, 77 shared, 6 lifecycle
- 87 `create_mdk_for_account()` call sites (all recreate MDK from scratch every call)
- 17 `Whitenoise::get_instance()` call sites (mostly event handlers + background tasks)
- 5 `DashMap<PublicKey, _>` fields to move to session
- 4 structurally identical streaming managers
- 280 `anyhow` usage sites across 72 files in the pre-#740 baseline

## Prerequisites

These are intentionally outside the session/projection refactor. Land them first, then rebase this work on top.

### Prerequisite A: typed errors / `anyhow` removal

**Status:** in progress in https://github.com/marmot-protocol/whitenoise-rs/pull/740.

That PR removes the `anyhow` dependency and introduces typed error surfaces. Do not carry a late "remove anyhow"
phase in this refactor after #740 lands; instead, rebase and adjust any new code to the typed-error shape from that PR.

**Validation:** #740 validation.

## Phases

### Phase 1: AccountSession scaffolding + startup restore + MDK caching (~700 LOC)

- [x] Completed in PR #743

<details>
<summary>Details</summary>

**Files to create:**
- `src/whitenoise/session/mod.rs` — `AccountSession`, `AccountManager`, `Whitenoise::session()` lookup

**Files to modify:**
- `src/whitenoise/mod.rs` — add `accounts: AccountManager`, add `mod session;`
- `src/whitenoise/accounts/setup.rs` — after successful account activation, insert/update session
- `src/whitenoise/accounts/login.rs` — on `logout()`, remove session from manager
- `src/whitenoise/accounts/login_multistep.rs` — move pending login state to `AccountManager`
- `src/whitenoise/accounts/login_external_signer.rs` — create/update session at completion
- `src/whitenoise/subscriptions.rs` — restore sessions for persisted accounts at startup before subscription setup
- `src/whitenoise/signer.rs` — register external signers into existing sessions

**Initial shape:**
```rust
pub struct AccountSession {
    pub account_pubkey: PublicKey,
    pub mdk: Arc<MDK<MdkSqliteStorage>>,
    pub signer: RwLock<Option<Arc<dyn NostrSigner>>>,
    // Transitional back-reference until singleton removal.
    wn: &'static Whitenoise,
}

pub struct AccountManager {
    sessions: RwLock<HashMap<PublicKey, Arc<AccountSession>>>,
    pending_logins: RwLock<HashMap<PublicKey, PendingLogin>>,
}
```

Use `Option<Arc<dyn NostrSigner>>` so restored external-signer accounts can have a session before the platform signer
is re-registered. Operations that need signing return the existing external-signer unavailable error until
`register_external_signer()` fills the slot and recovers subscriptions.

**Important behavior:**
- `AccountManager::restore_sessions()` loads active persisted accounts and creates sessions during startup.
- Existing "active session" checks that use `background_task_cancellation.contains_key()` switch to the session map.
- `create_mdk_for_account()` remains as a compatibility fallback, but migrated code uses `session.mdk`.

**Validation:** `just precommit-quick`, `just int-test login-flow`, startup restore test.

</details>

### Phase 2: Move per-account guards and cancellation to session (~600 LOC)

- [x] Completed in PR #743

<details>
<summary>Details</summary>

**Fields to move from `Whitenoise` to `AccountSession`:**
- `contact_list_guards: DashMap<PublicKey, Arc<Semaphore>>` -> `session.contact_list_guard: Arc<Semaphore>`
- `user_resolution_guards: DashMap<PublicKey, Arc<Mutex<()>>>` -> `session.user_resolution_guard: Arc<Mutex<()>>`
- `background_task_cancellation: DashMap<PublicKey, watch::Sender<bool>>` -> `session.cancellation: watch::Sender<bool>`
- `external_signers: DashMap<PublicKey, Arc<dyn NostrSigner>>` -> session `signer` slot from phase 1

**Call sites to update:**
- `handle_contact_list.rs`: acquire `session.contact_list_guard`
- `users.rs`: acquire `session.user_resolution_guard` only for account-driven resolution; keep shared/user-directory
  concurrency separate if the work is not account-scoped
- `setup.rs` / `login.rs`: remove replace/remove cancellation map helpers; session lifecycle sends cancellation
- background tasks: clone/subscribe to `session.cancellation`
- `signer.rs`: update the session signer slot rather than a global DashMap

Until the singleton is killed, background tasks can still use `Whitenoise::get_instance()?.session(pubkey)?` as a
transitional lookup.

**Validation:** `just precommit-quick`, `just int-test login-flow`, `just int-test user-discovery`.

</details>

### Phase 3: Scoped relay handles (~700 LOC)

- [x] Completed in PRs #753, #755, #756

<details>
<summary>Details</summary>

**Files to create:**
- `src/whitenoise/session/relay_handles.rs` — `AccountEphemeralHandle`, `AccountGroupHandle`

**Files to modify:**
- `src/whitenoise/session/mod.rs` — add handle fields to `AccountSession`
- `src/whitenoise/accounts/setup.rs` — construct handles during session creation
- `src/whitenoise/accounts/login.rs` — deactivate handles during logout
- `src/relay_control/mod.rs` — add account-inbox factory method; expose only the scoped surfaces needed by the session

**Guidance:**
- Reuse the existing `EphemeralPlane::account_scope(pubkey)` / `EphemeralScope` path. Do not expose
  `EphemeralExecutor` unless the existing scope API is genuinely insufficient.
- `AccountEphemeralHandle` should carry account identity and signer access. It can wrap `EphemeralScope` plus signing
  helpers for gift wraps, metadata, relay lists, key packages, and deletions.
- `GroupPlane` is not currently `Clone`/`Arc` owned. Either make `RelayControlPlane` hold it in an `Arc`, or let
  `AccountGroupHandle` hold `Arc<RelayControlPlane>` and delegate through scoped methods.
- Moving inbox ownership means the factory should also define who owns the telemetry persistor task and shutdown path.

**Validation:** `just precommit-quick`, `just int-test basic-messaging`, `just int-test login-flow`.

</details>

### Phase 4: Account repository scaffold for the first small domains (~500 LOC)

- [x] Completed in PR #754

<details>
<summary>Details</summary>

Create the repository module and only the repos needed by phase 5:
- `src/whitenoise/database/account/mod.rs` — `AccountRepositories` with optional/partial fields
- `src/whitenoise/database/account/drafts.rs` — `DraftsRepo`
- `src/whitenoise/database/account/settings.rs` — `AccountSettingsRepo`
- `src/whitenoise/database/account/follows.rs` — `AccountFollowsRepo`

Each repo holds `account_pubkey: PublicKey` + `db: Arc<Database>` and delegates to existing DB functions without
exposing a pubkey argument. Add the remaining repos when their domains migrate rather than creating all repos up front.

**Validation:** `just precommit-quick`, focused repo tests.

</details>

### Phase 5: View pattern + migrate drafts/follows/settings (~700 LOC)

- [x] Completed on branch `phase-5-view-pattern-migrate-ops`

<details>
<summary>Details</summary>

**Files to create:**
- `src/whitenoise/session/social.rs` — `SocialOps`
- `src/whitenoise/session/drafts.rs` — `DraftOps`
- `src/whitenoise/session/settings.rs` — `SettingsOps`
- placeholder modules/accessors for messages/groups if useful

**Methods to migrate:**
- `SocialOps`: `follow_user`, `unfollow_user`, `is_following_user`, `follows`
- `DraftOps`: `save_draft`, `load_draft`, `delete_draft`
- `SettingsOps`: `account_settings`, `update_notifications_enabled`

**View shape:**
Views borrow `&AccountSession` and do not spawn background tasks internally:

```rust
pub struct SocialOps<'a> {
    session: &'a AccountSession,
}

impl AccountSession {
    pub fn social(&self) -> SocialOps<'_> {
        SocialOps { session: self }
    }
}
```

Views never spawn `tokio::spawn` internally. All work is awaited inline. If a caller needs fire-and-forget behavior,
the caller owns the `Arc<AccountSession>` and decides to spawn — the view method itself is a normal async function.
See appendix for detailed rationale.

**Compatibility:** Add temporary `#[deprecated]` `Whitenoise` facades that delegate to session ops. Remove them after
call sites are migrated.

**Validation:** `just precommit-quick`, `just int-test follow-management`.

</details>

### Phase 6: Migrate message read/search operations (~500 LOC)

- [x] Completed in PR #760

<details>
<summary>Details</summary>

**Methods to move to `MessageOps`:**
- `fetch_messages_for_group`
- `fetch_aggregated_messages_for_group`
- `fetch_messages_unread_with_minimum`
- `fetch_message_by_id`
- `search_messages_in_group`
- `search_messages`

Add `DeliveryStatusRepo` / message projection read repo as needed, scoped by account pubkey but still backed by the
current shared `Database`.

**Validation:** `just precommit-quick`, message read/search tests.

</details>

### Phase 7: Migrate message send/retry/projection write path + delivery-status scope fix (~1000 LOC)

- [x] Completed in PR #760

<details>
<summary>Details</summary>

**Methods to move to `MessageOps`:**
- `send_message_to_group`
- `retry_message_publish`
- optimistic outgoing cache helpers
- delivery-status update helpers

**Delivery-status scope fix (GitHub issue #739):**
This phase also fixes the `message_delivery_status` account scope bug. The table is currently keyed by
`(message_id, mls_group_id)` with no `account_pubkey`, so sender-local delivery state bleeds across accounts in the
same group. As part of migrating the write path:
- Add `account_pubkey` to the `message_delivery_status` primary key via a new migration.
- Backfill existing rows using sender-owned evidence (`aggregated_messages.author == account_pubkey`) where possible;
  document any rows that cannot be safely attributed.
- Update all delivery-status reads/writes to carry account identity.
- Add a two-local-account same-group regression test covering the isolation bug.

This is the natural place for the fix because the delivery-status write path is being migrated to session-scoped ops
in the same phase.

**Guidance:**
- Use `session.mdk` instead of `create_mdk_for_account()`.
- Use `session.ephemeral` for publishing.
- Keep stream emission in the writer/builder path.
- Do not put the account-scoped `MessageProjectionBuilder` under `SharedServices`.

**Validation:** `just precommit-quick`, `just int-test basic-messaging`, `just int-test reactions`, two-account
delivery-status isolation test.

</details>

### Phase 8: Migrate group read operations (~500 LOC)

- [x] Completed in PR #763

<details>
<summary>Details</summary>

**Methods to move to `GroupOps`:**
- `groups`
- `visible_groups`
- `visible_groups_with_info`
- `group`
- `group_members`
- `group_relays`
- `group_admins`

Create only the repos needed for group membership reads. Leave mutation-heavy paths in place.

**Validation:** `just precommit-quick`, group read tests.

</details>

### Phase 9: Migrate group member/data mutation operations (~800 LOC)

- [x] Completed in PR #766

<details>
<summary>Details</summary>

**Methods to move to `GroupOps`:**
- `add_members_to_group`
- `remove_members_from_group`
- `update_group_data`
- `self_demote`
- `leave_group`
- publish/merge helpers needed by these methods

**Validation:** `just precommit-quick`, `just int-test basic-messaging`.

</details>

### Phase 10: Migrate `create_group` and welcome delivery helpers (~800 LOC)

- [x] Completed in PR #769

<details>
<summary>Details</summary>

Move `create_group`, member key-package resolution, welcome preparation, welcome publish background work, and group
record finalization into session-owned group ops.

This is separated from phase 9 because it touches MDK group creation, welcome publishing, relay subscription refresh,
and push-token sharing hooks.

**Validation:** `just precommit-quick`, group creation integration tests.

</details>

### Phase 11: Migrate group media operations (~800 LOC)

- [x] Completed in PR #772

<details>
<summary>Details</summary>

Move `groups/media.rs` account-scoped operations into session-owned media/group ops while still using the current
`media_files` table. The blob/reference table split happens later in the database phase.

**Validation:** `just precommit-quick`, chat media upload/download integration tests.

</details>

### Phase 12: Migrate memberships (`accounts_groups`) (~700 LOC)

- [x] Completed in PR #764

<details>
<summary>Details</summary>

**Methods to move to `MembershipOps`:**
- `get_or_create_account_group`
- `get_visible_account_groups`
- `get_pending_account_groups`
- `accept_account_group`
- `decline_account_group`
- `mark_message_read`
- `get_last_read_message_id`
- `set_chat_pin_order`
- `archive_chat`
- `unarchive_chat`
- `mute_chat`
- `unmute_chat`
- `mark_as_left`
- `mark_as_removed`
- `get_dm_group_with_peer`

Add `AccountGroupsRepo` in this phase.

**Validation:** `just precommit-quick`, membership tests, `just int-test basic-messaging`.

</details>

### Phase 13: Migrate push account ops (~700 LOC)

- [x] Completed in PR #767

<details>
<summary>Details</summary>

Move push operations that mutate account-scoped state:
- `push_registration`
- `upsert_push_registration`
- `clear_push_registration`
- `share_local_push_token_to_joined_groups`
- `share_local_push_token_to_group`
- `remove_local_push_token_from_joined_groups`
- `remove_local_push_token_from_group`
- active-leaf token reconciliation that is scoped to a session/account

Keep shared push message parsing and notification fanout helpers outside the session unless they write account-scoped
tables.

**Validation:** `just precommit-quick`, push registration/token integration tests.

</details>

### Phase 14: Migrate key packages and chat list reads (~800 LOC)

- [x] Completed in PR #768

<details>
<summary>Details</summary>

**Methods to move:**
- `KeyPackageOps`: account-scoped publish/delete/record/cleanup operations
- `ChatListOps`: `get_chat_list`, `get_archived_chat_list`

Keep arbitrary-user key-package fetches on shared services.

**Type issue:** Chat list still uses a single SQL DB in this phase. Do not do the app-level cross-DB join until the DB
split phase.

**Validation:** `just precommit-quick`, `just int-test login-flow`, chat list integration tests.

</details>

### Phase 15: Session-scoped event dispatch (~800 LOC)

- [x] Completed in PR #770

<details>
<summary>Details</summary>

**Files to modify:**
- `src/whitenoise/event_processor/account_event_processor.rs` — resolve session and dispatch through it
- `src/whitenoise/event_processor/event_handlers/handle_mls_message.rs` — takes `Arc<AccountSession>`
- `src/whitenoise/event_processor/event_handlers/handle_giftwrap.rs` — takes `Arc<AccountSession>`
- `src/whitenoise/event_processor/event_handlers/handle_contact_list.rs` — takes `Arc<AccountSession>`

**Guidance:**
- If the session is missing because the account logged out between receipt and processing, log and discard.
- Spawned event-handler background tasks should clone `Arc<AccountSession>` for account work or a shared-services handle
  for shared work. Avoid adding new `get_instance()` usage.
- Message projection writes from event handlers should go through the account-scoped builder/repo path.

**Validation:** `just precommit-quick`, `just int-test basic-messaging`, `just int-test notification-streaming`.

</details>

### Phase 16a: Extract SharedServices while keeping singleton compatibility (~800 LOC)

- [x] In review in PR #773

<details>
<summary>Details</summary>

**Files to create:**
- `src/whitenoise/shared.rs` — `SharedServices`

**Files to modify:**
- `src/whitenoise/mod.rs` — group shared fields under `Arc<SharedServices>` while keeping
  `initialize_whitenoise()` / `get_instance()` compatibility
- `src/whitenoise/session/mod.rs` — start depending on `Arc<SharedServices>` where possible

This phase is a staging step. The global `OnceCell` can still exist so downstream call sites do not all change in the
same PR.

**Validation:** `just precommit-quick`.

</details>

### Phase 16b: Kill singleton and pass app/shared handles explicitly (~800 LOC)

- [x] Completed in PR #779

<details>
<summary>Details</summary>

**Files to modify:**
- `src/whitenoise/mod.rs` — delete `GLOBAL_WHITENOISE`, delete `get_instance()`, add `Whitenoise::new(config) -> Result<Self>`
- remaining `get_instance()` call sites — replace with passed `Arc<SharedServices>` or `Arc<AccountSession>`
- `src/cli/dispatch.rs` — `dispatch()` takes `&Whitenoise`
- `src/cli/server.rs` — constructs `Whitenoise` and passes it to the dispatch loop
- `src/bin/integration_test.rs` — constructs its own `Whitenoise`
- `src/bin/benchmark_test.rs` — constructs its own `Whitenoise`

**Validation:** `just precommit-quick`, all integration tests, `cargo test -- --test-threads=4`.

</details>

### Phase 17: BroadcastHub consolidation (~400 LOC)

- [x] Completed in PR `#765`

<details>
<summary>Details</summary>

- Extract generic `BroadcastHub<K, V>` from the keyed streaming managers.
- Replace `MessageStreamManager`, `ChatListStreamManager`, and `UserStreamManager` with thin wrappers.
- Leave `NotificationStreamManager` alone unless the generic fits naturally.

Defer `nostr_manager` namespace cleanup to a separate follow-up. It is adjacent cleanup, not required for the session
boundary.

**Validation:** `just precommit-quick`.

</details>

### Phase 18a: Database split audit + scaffolding (~600 LOC)

- [x] Completed in PR #776

<details>
<summary>Details</summary>

**Files to create:**
- `src/whitenoise/database/shared_db.rs`
- `src/whitenoise/database/account_db.rs`
- new migration directories for new DBs

**Guidance:**
- Do not move or edit already-applied migration files. Add new migration sets and a one-time legacy extraction path.
- Audit every FK and table ownership before moving data. Current cross-scope examples include `drafts` ->
  `group_information` and account tables -> `accounts`; `accounts_groups` currently does not have a FK to
  `group_information`.
- Add wrappers and migrators without moving production tables yet.

**Validation:** `just precommit-quick`, migration smoke tests.

</details>

### Phase 18b: Media blob/reference split (~800 LOC)

- [x] Completed in PR #787

<details>
<summary>Details</summary>

Create the new media shape:
- shared DB: blob cache table
- account DB: media reference table

Migrate `media_files` usage to the two-table model while keeping bytes deduplicated in the shared cache.

**Validation:** `just precommit-quick`, chat media upload/download integration tests, account deletion test that leaves
shared blobs intact when still referenced.

</details>

### Phase 18c: Move simple account tables to account DB (~800 LOC)

- [x] Completed

<details>
<summary>Details</summary>

Six tables moved out of shared into per-account DB files. Migration timeline:

| v   | Kind   | Migration                                         |
| --- | ------ | ------------------------------------------------- |
| 13  | global | `published_events.account_id → account_pubkey`    |
| 14  | global | `processed_events.account_id → account_pubkey`    |
| 15  | local  | move `account_settings`                           |
| 16  | local  | move `drafts`                                     |
| 17  | local  | move `published_key_packages`                     |
| 18  | local  | move `published_events`                           |
| 19  | local  | move account-scoped `processed_events`            |
| 20  | local  | move `account_follows`                            |
| 21  | global | drop shared `account_settings`                    |
| 22  | global | drop shared `drafts`                              |
| 23  | global | drop shared `published_key_packages`              |
| 24  | global | drop shared `published_events`                    |
| 25  | global | drop shared `account_follows`                     |
| 26  | global | purge account-scoped rows from `processed_events` |

**Unified-timeline ordering.** Globals and locals share one version space.
`Migrator::run` walks them in version order, so a "global drop" at v21 is
guaranteed to run only after the corresponding v15 local copy has been
applied for whichever account-DB pool is being migrated. Cross-scope races
that the segregated-sequence design used to allow (drop ahead of copy) are
prevented by construction — see
`m0026_purge_account_processed_events.rs:run_global` doc and the Migrator
docs in `rust_migrations/mod.rs`.

**Deferred-drop guarantee for never-logged-in accounts.** `Database::new`
runs globals on the shared pool at boot, before any per-account DB exists.
For accounts that never log in after upgrade, the per-account file is
created on first session bring-up and runs locals at that point. To stop a
boot-time global drop from racing ahead of a still-pending local copy, the
app re-runs `MIGRATOR.run(&shared.pool, None)` once `restore_sessions`
returns (see `Whitenoise::new` in `src/whitenoise/mod.rs`). For accounts
that never log in *and* never had data in the moved tables, the drop is
safe — there's nothing to lose. For accounts that had data and never log
in: it stays in shared until the account either logs in (running its
locals first) or the account row is removed entirely.

**`account_follows` schema simplification.** Old: `(account_id INT FK,
user_id INT FK)` joining shared `users` to recover the followed pubkey.
New: `(pubkey TEXT PRIMARY KEY, created_at INT, updated_at INT)` — store
the followed pubkey directly. No FK to shared `users`; richer views resolve
pubkeys to `User` rows in shared on demand via
`User::find_by_pubkeys`. Follow lists rarely exceed 1000 entries, so a
two-step pubkey-then-batch-resolve is fine.

**Event tracker rerouting.** `WhitenoiseEventTracker` now holds
`Weak<Whitenoise>` so account-scoped ops (`track_published_event`,
`track_processed_account_event`, etc.) look up the publisher's session and
write to its `account_db`. `EventTracker::global_published_event` iterates
every active session's per-account DB (1–3 accounts in practice; cost is
negligible) since shared no longer carries a global index of all
publications.

**Repos and ops.**

- `src/whitenoise/database/account/follows.rs` — `AccountFollowsRepo` now
  wraps `Arc<AccountDatabase>` + `Arc<Database>`. Methods: `follow_pubkeys`,
  `all`, `is_following`, `add` / `add_pubkey`, `remove` / `remove_pubkey`,
  `replace_all`.
- `src/whitenoise/database/published_events.rs` — `PublishedEvent::create` /
  `exists` take `&AccountDatabase` (no pubkey arg).
- `src/whitenoise/database/processed_events.rs` — split into
  `create_global` / `exists_global` / `newest_global_event_timestamp_for_kind`
  / `newest_relay_event_timestamp` (shared DB) and `create_for_account` /
  `exists_for_account` / `newest_contact_list_timestamp` (per-account DB).

**Removed APIs.** `Account::follows`, `is_following_user`, `follow_user`,
`unfollow_user` deleted (callers migrated to `session.repos.follows.*` /
`session.social()`). `Account::update_follows_from_event` now takes
`&AccountSession` so it can rewrite the per-account follow set.

**Validation:** `just precommit-quick`, upgrade migration tests.

</details>

### Phase 18d: Move membership and push account tables to account DB (~800 LOC)

- [ ] Not started

<details>
<summary>Details</summary>

Move:
- `accounts_groups`
- `push_registrations`
- `group_push_tokens`

Replace cross-DB FKs with application-level checks. Keep the account registry in shared DB.

**Validation:** `just precommit-quick`, push and group membership integration tests.

</details>

### Phase 18e: Move message projection tables and chat-list join logic (~1000 LOC)

- [ ] Not started

<details>
<summary>Details</summary>

Move:
- `aggregated_messages`
- `message_delivery_status`

Then convert `ChatListOps::get_chat_list()` / `get_archived_chat_list()` from single-SQL joins into application-level
joins across account DB and shared DB.

**Validation:** `just precommit-quick`, all messaging/chat-list integration tests, upgrade migration test.

</details>

### Phase 19: CLI crate extraction (~500 LOC of Cargo.toml + module moves)

- [x] Completed in PR #774

<details>
<summary>Details</summary>

- Move `src/cli/` -> `crates/whitenoise-cli/src/`
- Move `src/bin/wn.rs`, `src/bin/wnd.rs` -> `crates/whitenoise-cli/src/bin/`
- Update workspace `Cargo.toml`
- `whitenoise-cli` depends on `whitenoise` core crate
- CLI-specific deps (`clap`, `rpassword`, `dirs`, `indicatif`) move to CLI crate
- Retain `cli = []` as an empty feature flag in core (not removed as originally planned):
  `src/whitenoise/mod.rs` uses `#[cfg(feature = "cli")]` to select the macOS keyring store —
  CLI binaries are unsigned and must use `apple_native_keyring_store::keychain::Store` instead
  of the Protected Data store, which requires a code-signed provisioning profile. The flag
  carries no dependencies; it exists solely to gate this platform behaviour.

**Validation:** `just precommit-quick`, CLI commands work end-to-end.

</details>

## Known Type Issues and Mitigations

### 1. View lifetime + spawned tasks

Views borrow `&AccountSession`. Spawned tasks require `'static` and can't capture borrows.

**Mitigation:** Views do not spawn. All work is awaited inline. If fire-and-forget is needed, the caller (which holds
`Arc<AccountSession>`) spawns the work. This separates "what to do" (the view) from "how to schedule it" (the caller).
See the appendix for full rationale and alternatives considered.

### 2. Event handler session lookup race

An event may arrive for an account that just logged out. `self.session(pubkey)` returns Err. Handlers return early.

**Mitigation:** This is already a latent race today. Log and discard; relay replay plus processed-event idempotency
handles recovery for active accounts.

### 3. Cross-DB foreign keys

SQLite does not support cross-database FKs. Current FK constraints between account and shared tables must be dropped in
the DB split phases.

**Mitigation:** Audit all FKs in phase 18a. Replace with application-level referential integrity checks where needed.

### 4. Media table ownership

Current `media_files` mixes shared blob concerns with account/group references.

**Mitigation:** Split shared blob cache from account-scoped media references in phase 18b.

### 5. MDK is not Clone

Confirmed Send + Sync but not Clone. Store it as `Arc<MDK<...>>` on the session.

**Mitigation:** Methods take `&self.mdk` or clone the `Arc` for spawned tasks.

### 6. Signer lifetime with external signers

External signers (NIP-55/Amber) may not be available at startup even if the account row is restored.

**Mitigation:** Store the signer as an optional session slot. Account operations that require signing fail with the
existing external-signer unavailable error until `register_external_signer()` updates the session and recovers relay
subscriptions.

### 7. FRB singleton replacement

Current FRB init stores `Whitenoise` in a global. The new architecture constructs it and returns it.

**Mitigation:** `whitenoise-frb` crate (later follow-up) holds the constructed `Whitenoise` behind an FFI-safe handle.
FRB codegen wraps this. The Flutter app sees no change in its Dart API.

### 8. Existing migration immutability

Existing SQLx migrations cannot be edited once applied.

**Mitigation:** New DB migration directories get new migration files. Existing installs use an explicit extraction /
copy path into the new shared and account DBs.

## Appendix: View Pattern — Borrow Design and No-Spawn Rule

### The pattern

Operation views (`MessageOps`, `GroupOps`, `SocialOps`, etc.) borrow `&AccountSession`:

```rust
pub struct MessageOps<'a> {
    session: &'a AccountSession,
}

impl AccountSession {
    pub fn messages(&self) -> MessageOps<'_> {
        MessageOps { session: self }
    }
}
```

The caller holds `Arc<AccountSession>`. Calling `session.messages()` derefs the Arc and borrows it. The temporary
`MessageOps<'_>` lives across `.await` points — Rust is fine with this because the Arc keeps the session alive.

```rust
let session: Arc<AccountSession> = wn.session(pubkey)?;
session.messages().send_to_group(&group_id, content).await?;
```

### Why not `Arc`-owning views?

Views that own `Arc<AccountSession>` would let them spawn background tasks freely. But constructing them is awkward in
Rust — you can't get an `Arc<Self>` from `&self`, so you'd need one of:

- **An extension trait on `Arc<AccountSession>`** — works but requires a trait import at every call site.
- **`Arc::new_cyclic` with a stored `Weak<Self>`** — works but complicates session construction.
- **Explicit construction** (`MessageOps::new(session.clone())`) — works but loses the `.messages()` ergonomic.

Borrow-based views avoid all of this. `session.messages()` is just `&self` → struct. No Arc clone, no traits, no
lifetime annotation at the call site.

### The no-spawn rule

View methods do not call `tokio::spawn` internally. All work is awaited inline. This means:

```rust
impl<'a> SocialOps<'a> {
    pub async fn follow_user(&self, target: &PublicKey) -> Result<()> {
        self.session.repos.follows.add(target).await?;
        let contact_list = self.build_contact_list().await?;
        // Await the publish — don't spawn it
        self.session.ephemeral.publish(contact_list, &relays).await?;
        Ok(())
    }
}
```

If a caller needs fire-and-forget, the caller spawns — the caller already has the Arc:

```rust
let session: Arc<AccountSession> = wn.session(pubkey)?;

// Option 1: await (simple, correct)
session.social().follow_user(&target).await?;

// Option 2: fire-and-forget (caller decides, caller has the Arc)
let s = session.clone();
tokio::spawn(async move { s.social().follow_user(&target).await.ok(); });
```

### Why this is the right constraint

- **Views separate "what to do" from "how to schedule it."** Business logic (the view method) doesn't decide whether
  something runs in the foreground or background. The caller does. This makes views easier to test and reason about.
- **Background spawn decisions are visible at the call site.** Today, some `impl Whitenoise` methods quietly spawn
  background work internally. With the no-spawn rule, every `tokio::spawn` is visible to whoever calls the view.
- **The durable task runtime (future work) replaces fire-and-forget spawns.** Operations that genuinely must eventually
  succeed (publish, welcome delivery, key package maintenance) will be handled by persistent state machines, not by
  in-memory spawned tasks that die on crash. The no-spawn rule avoids building infrastructure that the task runtime
  will replace.
- **If the constraint turns out to be wrong**, switching from borrow-based views to Arc-owning views is a mechanical
  change — add an extension trait, change the struct field type. No redesign needed.

## Verification

After all phases:
- `just precommit` (full, including integration tests with Docker)
- No file over 2,000 lines
- `Whitenoise` struct has ~5 fields
- `AccountSession` owns per-account state
- No `DashMap<PublicKey, _>` on `Whitenoise`
- No `get_instance()` anywhere
- No `anyhow` dependency (from prerequisite #740)
- CLI is its own crate
- Tests run in parallel with `--test-threads=4`
