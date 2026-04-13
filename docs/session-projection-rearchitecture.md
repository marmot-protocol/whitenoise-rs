# Architecture: Session and Projection Rearchitecture

Drafted: 2026-04-10

## Overview

White Noise today runs all business logic through a single `Whitenoise` struct — a global singleton with ~25 fields and
`impl` blocks spread across 37+ files. This worked when the codebase was smaller, but it now creates three problems:

1. **"Where does this go?"** Core files have grown past 2,000 lines. Contributors can't tell where new code belongs or
   whether something similar already exists.

2. **Account scope is a convention, not a guarantee.** Every account-mutating method takes `pubkey: PublicKey` as an
   argument. The compiler can't catch a bug that passes the wrong pubkey. At least one table
   (`message_delivery_status`) already has a scope bug where sender-local state bleeds across accounts in the same
   group.

3. **The singleton blocks testing.** `Whitenoise::get_instance()` returns a process-wide static. Parallel tests with
   isolated state are impossible. Swapping a service implementation for testing is impossible.

The relay-control-plane rearchitecture solved the same class of problem at the transport layer: five incompatible
workloads were sharing one `nostr-sdk::Client`, so we split them into four dedicated relay planes. This document
proposes the same treatment for the domain layer.

## Goals

1. Every piece of state has a declared owner and a declared scope (shared, account-scoped, or operational).
2. Account-scoped mutations go through an `AccountSession` handle where identity is baked in — not through a global
   facade that accepts a pubkey argument.
3. The singleton is gone. Services are constructed with their dependencies and wired together at startup. Tests build
   the subset they need.
4. Each account's data lives in its own database file. Account deletion is `fs::remove_file`. The DashMaps that
   currently partition shared state by account pubkey at runtime disappear.
5. No file over 2,000 lines. "Where does this go?" has a mechanical answer.

## The Four State Categories

Every piece of state in White Noise belongs to exactly one of these.

### 1. MDK Authority

MLS group state, cryptographic membership, accepted application messages. Canonical, account-scoped by construction —
each account already has its own `Arc<MDK<MdkSqliteStorage>>` with its own database.

**Rule:** MDK lives on `AccountSession`. Never reached through a shared service.

### 2. Shared App State

State that is safe to share across local accounts: app settings, the public user metadata cache (`users`,
`user_relays`), relay registry (`relays`), discovery cache (`cached_graph_users`), relay observability (`relay_status`,
`relay_events`), and the shared media blob cache.

**Rule:** Shared services own writes. Sessions may read but not write.

### 3. Account-Scoped App State

Canonical state that belongs to one local account: account settings, follows, group membership/UI state
(`accounts_groups`), drafts, push registrations, published key packages, published events.

**Rule:** All mutations happen through services owned by `AccountSession`. Repositories bake `account_pubkey` into the
handle — it is not a method argument.

### 4. Projections

Derived data optimized for reads. Each projection has a **builder** (write side) and a **reader** (read side). The
builder updates the projection when source data changes; the reader queries it for the UI.

**Rule:** Every projection declares its builder, reader, scope (shared or account-scoped), and whether it is
rebuildable. Projections never mix scopes. Builders write directly to projection tables — they do not route through the
reader service.

#### How projections work in practice

Consider what happens when an MLS group message is accepted:

```
handle_mls_message (event handler, the builder)
  → writes to aggregated_messages table (account-scoped projection)
  → message_stream_manager.emit(group_id, NewMessage)
      → UI receives stream update
      → UI calls MessageService::fetch() (the reader)
```

The builder (event handler) writes directly to the projection table and emits a stream notification. The reader
(`MessageService`) owns the query API but never writes. This avoids circular dependencies.

Chat list works differently — it has no stored table:

```
handle_mls_message (event handler)
  → (writes message projection as above)
  → chat_list_stream_manager.emit(account_pubkey, NewLastMessage)
      → UI receives stream update
      → UI calls ChatListService::get_chat_list() (the reader)
      → ChatListService computes the list by joining source tables
```

The chat list "builder" is just the stream emission. The projection is computed fresh on each read from
`accounts_groups` and `aggregated_messages` (account DB) plus `group_information` and `users` (shared DB). With the
per-account DB split, this becomes an application-level join across two databases rather than a single SQL query — the
service fetches from both and assembles the result.

#### Projection map

| Projection          | Builder                                    | Reader                                                                              | Scope          | Stored? | Rebuildable?      |
| ------------------- | ------------------------------------------ | ----------------------------------------------------------------------------------- | -------------- | ------- | ----------------- |
| Aggregated messages | Event handler → account-scoped projection builder | `MessageService`                                                              | Account-scoped | Yes     | Yes (from MDK)    |
| Message delivery    | Send pipeline                              | `MessageService`                                                                    | Account-scoped | Yes     | Partially         |
| Chat list           | Stream emissions from handlers             | `ChatListService` (computes from joins)                                             | Account-scoped | No      | N/A               |
| User directory      | `handle_metadata`, `handle_relay_list`     | `UserDirectoryService`                                                              | Shared         | Yes     | Yes (from relays) |
| Discovery/search    | Discovery sync worker                      | `UserSearchService`                                                                 | Shared         | Yes     | Yes               |
| Notifications       | Event handlers                             | `NotificationStreamManager` (shared; emits account-tagged events, receivers filter) | Shared         | No      | N/A               |
| Relay observability | `RelaySession` telemetry                   | `RelayObservabilityService`                                                         | Operational    | Yes     | Yes (over time)   |

## Target Runtime Shape

```
Whitenoise (thin app facade — constructed, NOT a singleton)
├── shared: Arc<SharedServices>
│   ├── shared_db: Arc<SharedDatabase>            (shared.sqlite)
│   ├── relay_control: Arc<RelayControlPlane>     (unchanged)
│   ├── user_directory: UserDirectoryService
│   ├── search: UserSearchService
│   ├── app_settings: AppSettingsService
│   ├── media_cache: MediaCacheService             (shared blob cache)
│   ├── relay_observability: RelayObservabilityService
│   ├── event_tracker: Arc<dyn EventTracker>
│   ├── discovery_sync_worker: DiscoverySyncWorker
│   ├── secrets_store: SecretsStore
│   ├── storage: Storage
│   ├── scheduler: SchedulerHandle
│   └── stream_hubs: StreamHubs                   (generic BroadcastHub<K,V>)
│
└── accounts: AccountManager
    ├── sessions: HashMap<PublicKey, Arc<AccountSession>>
    └── pending_logins: HashMap<PublicKey, PendingLogin>

AccountSession (created on login, dropped on logout)
├── account_pubkey: PublicKey
├── shared: Arc<SharedServices>                   (read-only back-reference)
├── account_db: Arc<AccountDatabase>              (account_<pubkey>.sqlite)
├── mdk: Arc<MDK<MdkSqliteStorage>>               (MLS state, own DB per account)
├── signer: Arc<dyn NostrSigner>
├── repos: AccountRepositories                    (account-scoped DB handles)
├── inbox: AccountInboxPlane                      (owned — this IS the plane)
├── ephemeral: AccountEphemeralHandle             (scoped handle into shared infra)
├── group_relay: AccountGroupHandle               (scoped handle into shared infra)
└── cancellation: watch::Sender<bool>
```

### Key properties

**No singleton.** `Whitenoise` is constructed at startup and passed by reference. `Whitenoise::get_instance()` is
deleted. Entry points (FRB init, CLI daemon, test setup) each construct their own instance. Tests can run in parallel
with isolated state.

**Each shared service is independently constructible.** For example,
`UserDirectoryService::new(db: Arc<SharedDatabase>)`. Tests build only the services they need — no 25-field mock.

**Per-account database.** Each `AccountSession` opens its own SQLite file. Account-scoped tables live there. Shared
tables live in `shared.sqlite`. MDK keeps its existing per-account DB. Account deletion is trivial: drop the session,
delete the file.

**MDK cached on the session.** Today every call that needs MDK creates a fresh instance via
`create_mdk_for_account()`, opening a new encrypted SQLite connection each time (~20+ call sites). The session holds a
single `Arc<MDK<MdkSqliteStorage>>` for its lifetime. MDK is Send + Sync (its storage wraps rusqlite::Connection in
`Arc<Mutex<>>`).

**Scoped relay handles.** The session owns its inbox plane directly. For shared relay infrastructure (group plane,
ephemeral executor), the session holds lightweight handles that bake in identity and signer. The handle's public API
never takes a pubkey or signer — it delegates to shared connections internally. Shared relay connections are reused
across accounts (important for mobile battery), but each session's handle is forcefully scoped to the right account.

**Services are views, not owned structs.** `session.messages()` returns a lightweight `MessageOps<'_>` that borrows the
session. The view has access to the session's MDK, repos, relay handles, and shared services through the borrow. No
separate service construction, no Arc cycles, no dependency duplication. Each view lives in its own file for
organization.

**Session lifecycle.** Login creates a session, opens the account DB, activates the account's inbox plane, creates
relay handles, registers background tasks. Logout removes the session, signals cancellation, deactivates the plane,
closes the DB. All per-account state is released when the session drops.

### How the call-site shape changes

Today:

```rust
// Account scope is just another argument
whitenoise.send_message_to_group(&account, &group_id, content).await?;
whitenoise.leave_group(&account, &group_id).await?;
whitenoise.follow_user(&account, &target_pubkey).await?;

// Shared reads go through the same facade
let user = whitenoise.resolve_user(&pubkey).await?;
```

After:

```rust
// Account scope is baked into the session handle
let session = whitenoise.session(account_pubkey)?;
session.messages().send_to_group(&group_id, content).await?;
session.groups().leave(&group_id).await?;
session.social().follow_user(&target_pubkey).await?;

// Shared reads go through a separate path
let user = whitenoise.shared().user_directory().resolve(&pubkey).await?;
```

CLI dispatch changes similarly. Today, `dispatch()` takes no reference — it fetches the singleton internally. After,
the daemon constructs `Whitenoise` at startup and passes it in:

```rust
// Before: singleton fetched inside dispatch
pub async fn dispatch(req: Request) -> Response {
    let wn = Whitenoise::get_instance()?;
    match req {
        Request::SendMessage { account, group_id, content } => {
            send_message(wn, &account, &group_id, &content).await
        }
    }
}

// After: whitenoise passed in by the daemon, session carries identity
pub async fn dispatch(wn: &Whitenoise, req: Request) -> Response {
    match req {
        Request::SendMessage { account, group_id, content } => {
            let session = wn.session(account)?;
            session.messages().send_to_group(&group_id, &content).await
        }
    }
}
```

## Architectural Rules

### The Mutation Rule

Account-scoped mutations happen through `AccountSession` services. Shared services never perform account-scoped
mutations. If it writes to an account-scoped table, it lives on the session.

### The Repository Rule

Account-scoped repositories bake the account identity into the handle. With per-account databases this is natural: the
repo holds a handle to the account's DB file, not a shared DB that requires a WHERE clause.

### The Projection Ownership Rule

Every projection declares a builder and a reader. Builders write directly to projection tables. Readers own the query
API. Stream emissions are the builder's responsibility.

### The Shared Reads Rule

Sessions may read from shared services via their `Arc<SharedServices>` back-reference. They may not write to shared
state. If an account-driven action causes a shared update (e.g. an inbound message triggers a user metadata refresh),
the session calls the shared service; the shared service owns the write.

### The No Singleton Rule

No static global mutable state. Entry points construct `Whitenoise` and hold it by reference. `OnceCell` and `OnceLock`
are reserved for process-level concerns (tracing, keyring store init) — never for application state.

## Storage Layout

### Current: one database

All tables live in a single SQLite database. Account scope is enforced only by convention (WHERE clauses and caller
discipline). Account deletion requires row-level cleanup across many tables.

### Target: shared + per-account

```
data/
├── shared.sqlite                    (shared state + operational state)
│   ├── app_settings
│   ├── accounts                     (registry only)
│   ├── users
│   ├── relays
│   ├── user_relays
│   ├── cached_graph_users
│   ├── group_information
│   ├── processed_events             (global dedup)
│   ├── relay_status
│   ├── relay_events
│   └── media_blobs                  (shared blob cache: hash, file path, Blossom URL, MIME)
│
├── account_<hex_pubkey_1>.sqlite    (account-scoped state)
│   ├── account_settings
│   ├── account_follows
│   ├── accounts_groups
│   ├── drafts
│   ├── push_registrations
│   ├── group_push_tokens
│   ├── published_key_packages
│   ├── published_events
│   ├── aggregated_messages           (account-scoped message projection)
│   ├── message_delivery_status
│   ├── media_references             (per-account/group media associations, encryption refs)
│   └── processed_events             (account-scoped dedup)
│
├── account_<hex_pubkey_2>.sqlite
│   └── (same schema)
│
└── mdk_<hex_pubkey_N>/              (MDK's own per-account DB, unchanged)
```

Account deletion: drop the session, `fs::remove_file` the account database, remove the MDK directory. No row-level
cleanup. No chance of leftover state.

## API Stability

White Noise has two consumer-facing surfaces and an internal layer:

| Tier | Surface            | Stability                                             | Examples                                      |
| ---- | ------------------ | ----------------------------------------------------- | --------------------------------------------- |
| 1    | FRB cdylib exports | Stable — changes require coordinated Flutter releases | Types in `src/lib.rs` re-exports              |
| 2    | CLI wire protocol  | Stable — TUI and other tools depend on it             | `Request`/`Response` in `src/cli/protocol.rs` |
| 3    | Internal services  | Free to change                                        | Struct names, method signatures, module paths |

The rearchitecture changes Tier 3 only. Tier 1 and Tier 2 stay stable throughout. If a Tier 1 or Tier 2 change is ever
needed, it is its own separately reviewed change.

## Target Workspace Shape

The whitenoise-rs repository becomes a Cargo workspace with three crates:

```
whitenoise-rs/
├── Cargo.toml                  (workspace root)
├── crates/
│   ├── whitenoise/             (core library — all business logic, services, sessions, DB, relay control)
│   ├── whitenoise-cli/         (CLI daemon + client — dispatch, protocol, server, commands)
│   └── whitenoise-frb/         (FRB wiring — the Rust side of the Flutter bridge)
```

**`whitenoise`** is the core crate. It contains everything described in this document: `SharedServices`,
`AccountSession`, repos, relay control plane, event processor, projections. It does not depend on `clap`, `rpassword`,
or any CLI-specific deps. It does not depend on flutter_rust_bridge. It exposes a Rust API that both adapter crates
consume.

**`whitenoise-cli`** is extracted from the current `src/cli/` and `src/bin/wn.rs` / `src/bin/wnd.rs`. It depends on
`whitenoise` and adds `clap`, `rpassword`, `dirs`, and the wire protocol (`Request`/`Response` types). The TUI and
other tools consume the CLI's wire protocol over a socket. This crate is already naturally separated behind
`required-features = ["cli"]` today — making it a real crate is a mechanical move.

**`whitenoise-frb`** is extracted from the Flutter repository, where FRB wiring code currently lives alongside the Dart
app. Moving it into the whitenoise-rs workspace means all Rust code lives in one place. CI builds this crate, runs FRB
code generation, and packages the output as a Dart package that the Flutter app depends on. The Flutter repository
becomes pure Dart — no Rust source, no FRB configuration, no Cargo build steps. `whitenoise-frb` depends on
`whitenoise` and on `flutter_rust_bridge`. It constructs and holds a `Whitenoise` instance behind whatever FFI-safe
handle FRB needs — this is where the singleton replacement lives for the mobile app entry point.

## What This Is Not

**Not a rewrite.** The relay control plane, the event processor, the streaming subscription model, and the MLS/MDK
integration all stay. We are reorganizing ownership boundaries, not replacing subsystems.

**Not a new event loop.** The single-lane event processor is fine at mobile scale. Session boundaries make it easy to
add per-session concurrency later if needed, but this rearchitecture does not do so.

## Migration Approach

This will be implemented in phases, each landing independently and leaving the app working — the same approach used for
the relay control plane rearchitecture. A separate implementation plan document will detail the phase sequence,
deliverables, and validation steps.

The rough ordering:

1. Error handling cleanup (drop `anyhow`, typed errors, Rust migration hooks)
2. Session scaffolding and per-account state field moves
3. Schema fixes for account-scope bugs
4. Account repository primitives
5. Domain-by-domain migration to session services (messages first, then groups, then smaller domains)
6. Session-scoped event dispatch
7. Singleton removal and shared service extraction
8. Per-account database split
9. Adjacent cleanups (stream manager consolidation, namespace cleanup)

## Future: Durable Task Runtime

Once the session and service boundaries from this rearchitecture are in place, the next major subsystem to introduce is
a **durable task runtime** for outbound operations that must eventually succeed even across app restarts and crashes.

### The problem today

Several operations in White Noise are fire-and-forget in memory: publish a message to relays, deliver a welcome to a
new group member, publish a key package, respond to a MIP-05 token request. If the app crashes or the network drops
mid-operation, the work is lost. The current retry system (`schedule_retry` in the event processor) re-queues failed
events with exponential backoff, but that state lives entirely in memory — a process restart loses all pending retries.

### The idea

Model these operations as **deterministic state machines** with serializable state. Each operation is an enum of
states. Each state transition is a pure function of the current state plus an input (network result, timeout, etc.).
The current state is persisted to the account's database after every transition. On crash, the runtime scans the
database for incomplete operations and resumes them from their last persisted state.

```rust
pub enum PublishState {
    Pending { event: UnsignedEvent, target_relays: Vec<RelayUrl> },
    Signing { event: UnsignedEvent },
    Sending { event: Event, remaining: Vec<RelayUrl>, succeeded: Vec<RelayUrl> },
    RetryWait { event: Event, remaining: Vec<RelayUrl>, attempt: u32, resume_at: Instant },
    Complete { event: Event, accepted_by: Vec<RelayUrl> },
    Failed { event: Event, last_error: String },
}
```

Each state is serializable. On startup, the runtime reads persisted states from the account DB, finds operations in
`Sending` or `RetryWait`, and resumes them. Nothing critical stays only in memory.

### Target operations

Not every operation needs this treatment — only outbound operations where losing in-flight state is a real bug:

| Operation              | Why it needs durability                                         |
| ---------------------- | --------------------------------------------------------------- |
| Message publish        | User expects "sent" to mean sent, even if the app restarts      |
| Welcome delivery       | New member can't join if the welcome is lost mid-delivery       |
| Key package publishing | Account becomes unreachable if key packages fail silently       |
| MIP-05 token responses | Delayed responses must survive restarts                         |
| Multi-step login       | Login state already spans multiple user interactions            |
| Group sync coordinator | Post-welcome and post-reconnect commits must wait for readiness |

Inbound event processing does NOT need this — the event processor already has `processed_events` dedup, and re-fetching
from relays is the natural recovery path for missed inbound events.

### How sessions enable this

The session architecture makes durable tasks natural:

- **Per-account database** provides the persistence layer — each account's task table lives in its own DB file.
- **Scoped relay handles** provide the execution layer — the task runtime uses the session's `AccountEphemeralHandle`
  to publish, so identity and signer are baked in.
- **Session lifecycle** provides cleanup — when an account is deleted, all its pending tasks are deleted with the DB
  file. No orphaned tasks.

The runtime itself is a shared service (it schedules and drives tasks across all sessions), but the task state and
execution context are account-scoped.

### Deterministic simulation testing

Because each state machine is a pure function of `(current_state, input) → next_state`, operations can be driven with
synthetic inputs in tests: simulated network failures, simulated relay responses, simulated clock advancement. This
enables testing edge cases (partial sends, crash-during-retry, relay rejection after partial success) that are
impossible to reproduce reliably with real network tests.

This is not full-app simulation testing — it's scoped to the durable task runtime and its state machines. The rest of
the app continues to use normal async tests and integration test scenarios.

### Relationship to this rearchitecture

The durable task runtime is a **follow-up project**, not part of this rearchitecture. It is listed here because the
session architecture is a prerequisite — without per-account databases, scoped relay handles, and clean session
lifecycle, there's no natural place for durable task state to live or execute. The design doc for the durable task
runtime should be written after the session and service boundaries from this rearchitecture are in place.

## Appendix A: Pseudocode Examples

### Initialization

`Whitenoise` is constructed — not fetched from a global. Each shared service takes only its dependencies. The database
pool is created once, Arc-wrapped, and cloned into each service that needs it.

```rust
impl Whitenoise {
    pub async fn new(config: WhitenoiseConfig) -> Result<Self> {
        // Process-level init (idempotent, safe to call multiple times)
        init_tracing(&config.logs_dir);
        initialize_keyring_store();

        // Open shared database, run shared migrations
        let shared_db = Arc::new(
            SharedDatabase::open(&config.data_dir.join("shared.sqlite")).await?
        );

        // Event processing channels — created before relay control because RelayControlPlane needs a sender clone
        let (event_sender, event_receiver) = mpsc::channel(2000);
        let (shutdown_sender, shutdown_receiver) = mpsc::channel(1);

        // Construct shared services — each takes only what it needs
        let secrets_store = SecretsStore::new(&config.keyring_service_id);
        let storage = Storage::new(&config.data_dir);

        let relay_control = Arc::new(RelayControlPlane::new(
            shared_db.clone(),
            config.discovery_relays.clone(),
            event_sender.clone(),
        ));

        let user_directory = UserDirectoryService::new(shared_db.clone());
        let search = UserSearchService::new(shared_db.clone());
        let app_settings = AppSettingsService::new(shared_db.clone());
        let media_cache = MediaCacheService::new(shared_db.clone(), storage.clone());

        let shared = Arc::new(SharedServices {
            db: shared_db,
            relay_control,
            user_directory,
            search,
            app_settings,
            media_cache,
            secrets_store,
            storage,
            // ...remaining shared services...
        });

        let accounts = AccountManager::new(shared.clone(), config.clone());

        let wn = Self { config, shared, accounts, event_sender, shutdown_sender };

        wn.start_event_processing(event_receiver, shutdown_receiver);
        wn.start_scheduled_tasks();
        wn.accounts.restore_sessions().await?;

        Ok(wn)
    }
}
```

The resulting `Whitenoise` struct has ~5 fields. Each entry point (FRB, CLI daemon, tests) calls `Whitenoise::new()`
and holds the result. Tests construct their own instances with isolated databases.

### Session rehydration at startup

On launch, the account registry (in `shared.sqlite`) tells us which accounts were previously logged in. For each, we
open the per-account DB, create the MDK handle, construct relay handles, and register the session.

```rust
impl AccountManager {
    async fn restore_sessions(&self) -> Result<()> {
        let accounts = Account::all_active(&self.shared.db).await?;

        for account in accounts {
            let session = self.create_session(&account).await?;
            self.sessions.write().await.insert(account.pubkey, session.clone());

            session.inbox.activate(&account).await?;
            session.group_relay.sync_subscriptions(&active_groups).await?;
        }
        Ok(())
    }

    async fn create_session(&self, account: &Account) -> Result<Arc<AccountSession>> {
        // Open per-account database, run account migrations
        let account_db = Arc::new(AccountDatabase::open(
            &self.config.data_dir.join(format!("account_{}.sqlite", account.pubkey.to_hex())),
        ).await?);

        // MDK handle — reuses existing per-account MLS database
        let mdk = Arc::new(Account::create_mdk(
            account.pubkey, &self.config.data_dir, &self.config.keyring_service_id,
        )?);

        // Resolve signer (local keys or external NIP-55)
        let signer = self.shared.secrets_store.get_signer_for_account(&account.pubkey)?;

        // Account-scoped repos — all share the same account DB handle
        let repos = AccountRepositories::new(account_db.clone());

        // Scoped relay handles — bake in identity, delegate to shared infra
        let ephemeral = AccountEphemeralHandle::new(
            account.pubkey, signer.clone(), self.shared.relay_control.ephemeral_executor(),
        );
        let group_relay = AccountGroupHandle::new(
            account.pubkey, self.shared.relay_control.group_plane(),
        );

        // Inbox plane — fully owned by the session
        let inbox = self.shared.relay_control.create_inbox_session(
            account.pubkey, signer.clone(), &account.inbox_relays,
        ).await?;

        Ok(Arc::new(AccountSession {
            account_pubkey: account.pubkey,
            shared: self.shared.clone(),
            account_db, mdk, signer, repos, inbox, ephemeral, group_relay,
            cancellation: watch::channel(false).0,
        }))
    }
}
```

### Scoped relay handles

The session holds handles that carry identity into shared relay infrastructure. The handle's API never exposes the
pubkey or signer — callers just call methods, and the handle routes through shared connections using the right account
internally.

```rust
/// Scoped handle into the shared ephemeral executor.
/// Carries this account's identity and signer. Delegates to shared warm relay connections.
struct AccountEphemeralHandle {
    pubkey: PublicKey,
    signer: Arc<dyn NostrSigner>,
    executor: Arc<EphemeralExecutor>,
}

impl AccountEphemeralHandle {
    /// Publish using this account's signer, on shared warm connections.
    /// No pubkey or signer argument — baked in at construction.
    async fn publish(&self, event: UnsignedEvent, relays: &[RelayUrl]) -> Result<EventId> {
        self.executor.publish_scoped(&self.pubkey, event, relays, &self.signer).await
    }

    async fn fetch(&self, relays: &[RelayUrl], filter: Filter) -> Result<Vec<Event>> {
        self.executor.fetch_scoped(&self.pubkey, relays, filter).await
    }
}

/// Scoped handle into the shared group plane.
/// One shared RelaySession serves all accounts; this handle manages subscriptions for one account only.
struct AccountGroupHandle {
    pubkey: PublicKey,
    group_plane: Arc<GroupPlane>,
}

impl AccountGroupHandle {
    async fn sync_subscriptions(&self, groups: &[GroupSubscription]) -> Result<()> {
        self.group_plane.sync_for_account(&self.pubkey, groups).await
    }

    async fn deactivate(&self) -> Result<()> {
        self.group_plane.remove_account(&self.pubkey).await
    }
}
```

The relay control plane ownership split:

| Plane     | Owner          | Session interaction                                    |
| --------- | -------------- | ------------------------------------------------------ |
| Discovery | SharedServices | Purely shared, no per-account state                    |
| Group     | SharedServices | Session holds `AccountGroupHandle` (scoped handle)     |
| Inbox     | AccountSession | Session owns the plane directly                        |
| Ephemeral | SharedServices | Session holds `AccountEphemeralHandle` (scoped handle) |

Shared connections are reused across accounts. Identity is baked into the handle. The compiler enforces correct scoping
because you can't call a handle's methods without having the right handle, and you only get the right handle through a
valid session.

### Account-scoped operations (view pattern)

Services are not owned structs on the session — they are lightweight view types that borrow it.
`session.messages()` returns a `MessageOps<'_>` holding `&AccountSession`. The view has direct access to everything on
the session: MDK, repos, relay handles, shared services.

```rust
impl AccountSession {
    pub fn messages(&self) -> MessageOps<'_> { MessageOps { session: self } }
    pub fn groups(&self) -> GroupOps<'_> { GroupOps { session: self } }
    pub fn social(&self) -> SocialOps<'_> { SocialOps { session: self } }
    pub fn drafts(&self) -> DraftOps<'_> { DraftOps { session: self } }
}
```

Each view lives in its own file:

```
src/session/
├── mod.rs              (AccountSession struct, lifecycle, accessors)
├── messages.rs         (MessageOps)
├── groups.rs           (GroupOps)
├── social.rs           (SocialOps)
├── drafts.rs           (DraftOps)
├── push.rs             (PushOps)
├── key_packages.rs     (KeyPackageOps)
└── chat_list.rs        (ChatListOps)
```

A concrete example — sending a message through the view:

```rust
// src/session/messages.rs

pub struct MessageOps<'a> {
    session: &'a AccountSession,
}

impl<'a> MessageOps<'a> {
    pub async fn send_to_group(&self, group_id: &GroupId, content: &str) -> Result<EventId> {
        // Create MLS message via MDK (account-scoped, on the session)
        let mls_message = self.session.mdk.create_application_message(group_id, content.as_bytes())?;

        // Publish via scoped ephemeral handle (shared connections, account identity baked in)
        let relays = self.session.repos.memberships.group_relays(group_id).await?;
        let event_id = self.session.ephemeral.publish(mls_message, &relays).await?;

        // Record in account-scoped idempotency table
        self.session.repos.published_events.record(&event_id).await?;

        // Emit stream update so UI refreshes
        self.session.shared.stream_hubs.messages.emit(group_id, MessageUpdate::new_sent(event_id));

        Ok(event_id)
    }

    pub async fn fetch(&self, group_id: &GroupId, pagination: &PaginationOptions) -> Result<Vec<ChatMessage>> {
        // Read from account-scoped projection table
        self.session.repos.messages.fetch_aggregated(group_id, pagination).await
    }
}
```

Notice: no `account_pubkey` argument anywhere. The session carries identity. The MDK is on the session. The repos are
scoped to the account's database. The ephemeral handle signs with the account's signer. The view just coordinates — it
doesn't own state, it borrows the session's.

### Account-scoped repositories

Repos hold a handle to the per-account database. No `account_pubkey` column in per-account tables. No
`WHERE account_pubkey = ?` in queries. The database file IS the scope.

```rust
pub struct AccountRepositories {
    pub drafts: DraftsRepo,
    pub memberships: AccountGroupsRepo,
    pub messages: AccountMessagesRepo,
    pub settings: AccountSettingsRepo,
    pub follows: AccountFollowsRepo,
    pub push: PushRepo,
    pub key_packages: KeyPackageRepo,
    pub published_events: PublishedEventsRepo,
    pub delivery_status: DeliveryStatusRepo,
    pub processed_events: AccountProcessedEventsRepo,
}

impl AccountRepositories {
    pub fn new(db: Arc<AccountDatabase>) -> Self {
        Self {
            drafts: DraftsRepo { db: db.clone() },
            memberships: AccountGroupsRepo { db: db.clone() },
            messages: AccountMessagesRepo { db: db.clone() },
            settings: AccountSettingsRepo { db: db.clone() },
            follows: AccountFollowsRepo { db: db.clone() },
            push: PushRepo { db: db.clone() },
            key_packages: KeyPackageRepo { db: db.clone() },
            published_events: PublishedEventsRepo { db: db.clone() },
            delivery_status: DeliveryStatusRepo { db: db.clone() },
            processed_events: AccountProcessedEventsRepo { db: db.clone() },
        }
    }
}

/// Example repo — compare to today's Draft::save() which takes account_pubkey as an argument and WHERE-clauses on it.
pub struct DraftsRepo {
    db: Arc<AccountDatabase>,
}

impl DraftsRepo {
    pub async fn save(&self, group_id: &GroupId, content: &str) -> Result<Draft> {
        // No account_pubkey column — the DB file is the scope
        sqlx::query_as::<_, Draft>(
            "INSERT INTO drafts (mls_group_id, content, created_at, updated_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(mls_group_id) DO UPDATE SET
                 content = excluded.content, updated_at = excluded.updated_at
             RETURNING *"
        )
        .bind(group_id.as_slice())
        .bind(content)
        .bind(now_millis())
        .bind(now_millis())
        .fetch_one(&self.db.pool)
        .await
        .map_err(Into::into)
    }

    pub async fn find(&self, group_id: &GroupId) -> Result<Option<Draft>> {
        sqlx::query_as::<_, Draft>("SELECT * FROM drafts WHERE mls_group_id = ?")
            .bind(group_id.as_slice())
            .fetch_optional(&self.db.pool)
            .await
            .map_err(Into::into)
    }
}
```

### Testing with isolated state

No singleton. No `create_mock_whitenoise()`. Each test constructs the exact subset of services it needs.

```rust
#[tokio::test]
async fn test_draft_save_and_find() {
    // Just an in-memory account DB — nothing else
    let db = Arc::new(AccountDatabase::open_in_memory().await.unwrap());
    let repos = AccountRepositories::new(db);
    let group_id = GroupId::from_slice(b"test-group-00000000");

    let draft = repos.drafts.save(&group_id, "hello").await.unwrap();
    assert_eq!(draft.content, "hello");

    let found = repos.drafts.find(&group_id).await.unwrap().unwrap();
    assert_eq!(found.content, "hello");
}

#[tokio::test]
async fn test_user_directory_resolve() {
    // Just a shared DB and the one service under test
    let db = Arc::new(SharedDatabase::open_in_memory().await.unwrap());
    let user_dir = UserDirectoryService::new(db);

    user_dir.upsert_metadata(&test_pubkey, &test_metadata).await.unwrap();
    let user = user_dir.resolve(&test_pubkey).await.unwrap();
    assert_eq!(user.metadata.name, Some("Alice".to_string()));
}
```

Tests run in parallel. No shared state. No flaky failures from singleton contention.

## Appendix B: Table Ownership Map

| Table                        | Scope        | Target DB      | Owner                                       |
| ---------------------------- | ------------ | -------------- | ------------------------------------------- |
| `app_settings`               | Shared       | shared.sqlite  | AppSettingsService                          |
| `accounts`                   | Registry     | shared.sqlite  | AccountManager                              |
| `account_settings`           | Account      | account.sqlite | Session settings                            |
| `users`                      | Shared       | shared.sqlite  | UserDirectoryService                        |
| `relays`                     | Shared       | shared.sqlite  | UserDirectoryService                        |
| `user_relays`                | Shared       | shared.sqlite  | UserDirectoryService                        |
| `cached_graph_users`         | Shared       | shared.sqlite  | DiscoveryService                            |
| `account_follows`            | Account      | account.sqlite | Session social                              |
| `group_information`          | Group-shared | shared.sqlite  | GroupInfoService                            |
| `accounts_groups`            | Account      | account.sqlite | Session memberships                         |
| `drafts`                     | Account      | account.sqlite | Session drafts                              |
| `push_registrations`         | Account      | account.sqlite | Session push                                |
| `group_push_tokens`          | Account      | account.sqlite | Session push                                |
| `published_key_packages`     | Account      | account.sqlite | Session key packages                        |
| `published_events`           | Account      | account.sqlite | Session publishing                          |
| `processed_events` (global)  | Shared       | shared.sqlite  | EventTracker                                |
| `processed_events` (account) | Account      | account.sqlite | Session idempotency                         |
| `aggregated_messages`        | Account      | account.sqlite | Session messages                            |
| `message_delivery_status`    | Account      | account.sqlite | Session messages                            |
| `media_blobs`                | Shared       | shared.sqlite  | MediaCacheService                           |
| `media_references`           | Account      | account.sqlite | Session media                               |
| `relay_status`               | Operational  | shared.sqlite  | RelayObservabilityService                   |
| `relay_events`               | Operational  | shared.sqlite  | RelayObservabilityService                   |
