# Login & Relay Error Improvements

Plan for addressing relay connection failures during login and providing structured errors to the Flutter app.

**Related issues:**
- [whitenoise#46](https://github.com/marmot-protocol/whitenoise/issues/46) - Login error when user has no relay lists
- [whitenoise#143](https://github.com/marmot-protocol/whitenoise/issues/143) - Add error states for login screen

**Branch:** `improve-login-relay-errors`

---

## Problem Summary

### 1. Login crashes when user has no relay lists (issue #46)

When a user logs in with an nsec that has no published relay lists (kind 10002/10050/10051), login fails with:

```text
Nostr manager error: Client Error: relay not found
```

The root cause is an ordering issue in the login flow. `setup_relays_for_existing_account()` calls `fetch_existing_relays()` which uses nostr-sdk's `client.fetch_events_from()`. This method requires relay URLs to already be in the client's relay pool. But `ensure_relays_connected()` -- which adds relays to the pool -- only runs later in `activate_account()`.

**Key symbols in the call chain:**
- `setup_relays_for_existing_account()` → `fetch_existing_relays()` → `client.fetch_events_from()` — the failing path
- `ensure_relays_connected()` — adds relays to the client pool (called too late, inside `activate_account()`)
- `create_identity()` → `setup_relays_for_new_account()` — NOT affected (never calls `fetch_events_from`)

There is also a secondary failure path: if the NIP-65 fetch succeeds and returns non-default relays (e.g. `wss://relay.snort.social`), the code then tries to fetch Inbox/KeyPackage lists FROM those NIP-65 relays, which are not in the pool.

Note: `create_identity()` does NOT have this bug because `setup_relays_for_new_account()` only writes to the DB and spawns background publishes -- it never calls `fetch_events_from`.

### 2. All errors arrive in Flutter as opaque strings (issue #143)

The Flutter app receives all Rust errors through the FRB bridge's `ApiError` enum. For login errors specifically, `WhitenoiseError` gets converted to `ApiError::Whitenoise { message: error.to_string() }`, which produces strings like `"Nostr manager error: Client Error: relay not found"`. The Flutter login hook (`use_login_with_nsec.dart`) catches everything in a generic try/catch and shows `"Oh no! An error occurred, please try again."`.

The Flutter app cannot distinguish between invalid key format, network failures, relay list not found, timeout, etc., because all errors are just `ApiError::Whitenoise` with a `.message` string.

---

## Current Architecture

### Error flow: Rust crate -> FRB bridge -> Flutter

```text
WhitenoiseError (whitenoise-rs)
  |
  | .to_string()
  v
ApiError::Whitenoise { message: String }  (Flutter app's rust/src/api/error.rs)
  |
  | flutter_rust_bridge
  v
Dart ApiError.whitenoise(message: String)  (Flutter app)
  |
  | catch (e) { ... }
  v
"Oh no! An error occurred, please try again."
```

### Login flow sequence

```text
login(nsec)
  1. Keys::parse(nsec)                           -- can fail: InvalidKey
  2. create_base_account_with_private_key(keys)   -- DB + keychain
  3. setup_relays_for_existing_account(account)   -- FAILS HERE
     a. load_default_relays() from DB
     b. fetch NIP-65 from default relays          -- fetch_events_from (needs pool)
     c. fetch Inbox from NIP-65 relays            -- fetch_events_from (needs pool)
     d. fetch KeyPackage from NIP-65 relays       -- fetch_events_from (needs pool)
     e. publish any relay lists that used defaults
  4. activate_account(account)                    -- NEVER REACHED
     a. ensure_relays_connected()                 -- adds relays to pool
     b. setup subscriptions
     c. setup key package
```

### Three relay list types

| Kind  | Type         | Purpose                              |
|-------|--------------|--------------------------------------|
| 10002 | NIP-65       | General read/write relays (outbox)   |
| 10050 | Inbox        | Receiving giftwrapped MLS messages   |
| 10051 | KeyPackage   | MLS key package storage/discovery    |

All three are **replaceable events** -- publishing a new one overwrites the previous one.

---

## Decisions

### Decision 1: How to handle "no relay list found" -- Option C: Multi-step login with user choice

When a user logs in with an nsec and we can't find their relay lists on the network, we return a structured result to Flutter so the UI can present the user with a choice:

1. **Publish relay lists to default relays** -- the app publishes all three relay list events (10002/10050/10051) with the default relay URLs, then continues login.
2. **Provide a custom relay URL** -- the user enters a relay URL where their lists can be found. The app fetches relay lists from that relay. If found, continue login with those relays. If not found, return to the choice screen.

Login does NOT proceed to key package publishing or account activation until relay lists are resolved. The outbox model requires published relay lists for key package discoverability, so we cannot skip this step.

`create_identity()` remains unchanged -- it auto-publishes all three relay lists with defaults since it's creating a brand new Nostr identity with no prior state to overwrite.

### Decision 2: Return a `LoginResult` struct

`login()` (and the new multi-step API) returns a `LoginResult` struct instead of a bare `Account`. This struct communicates both the account state and whether relay lists need resolution.

### Decision 3: Use a `LoginError` enum

Create a dedicated `LoginError` enum for login-specific failures. This gives Flutter a focused set of errors to match on rather than parsing generic `WhitenoiseError` strings.

### Decision 4: Both login paths

Both `login()` (nsec) and `login_with_external_signer()` must be handled. They share the same underlying relay fetch bug and need the same multi-step flow.

### Decision 5: Key packages require relay lists

We cannot publish MLS key packages until relay lists are resolved. Key packages are published to the account's key package relays (kind 10051), and other users discover them via the outbox model. Without published relay lists, the key packages are undiscoverable.

### Decision 6: Custom relay is a search hint only

When the user provides a custom relay URL and we find their relay lists there, we use the discovered relay lists as-is. The custom URL is just a search hint -- we don't add it to their relay lists unless it's already in the discovered NIP-65 event.

### Decision 7: Cancel cleans up fully

If the user cancels/backs out after `login_start` has created a partial account, we do a full cleanup: delete the account record from the DB and remove the keychain entry. Clean slate if they retry.

---

## New API Design

### Multi-step login API

The login flow becomes a series of discrete steps that Flutter calls sequentially. In the **happy path** (relay lists found on the network), `login_start` does the entire login in one call.

```rust
/// Step 1: Start login. In the happy path, this completes the full login.
/// If relay lists are not found, returns NeedsRelayLists status.
pub async fn login_start(
    &self,
    nsec_or_hex_privkey: String,
) -> core::result::Result<LoginResult, LoginError>;

/// Step 2a: User chose to publish default relay lists.
/// Publishes all three relay list events with defaults, then completes login.
pub async fn login_publish_default_relays(
    &self,
    pubkey: &PublicKey,
) -> core::result::Result<LoginResult, LoginError>;

/// Step 2b: User provided a custom relay URL to search for their lists.
/// Fetches relay lists from that URL. If found, completes login.
/// If not found, returns NeedsRelayLists again so Flutter can re-prompt.
pub async fn login_with_custom_relay(
    &self,
    pubkey: &PublicKey,
    relay_url: RelayUrl,
) -> core::result::Result<LoginResult, LoginError>;

/// Cancel: User backed out. Cleans up partial account state.
pub async fn login_cancel(
    &self,
    pubkey: &PublicKey,
) -> core::result::Result<(), LoginError>;
```

### LoginResult

```rust
pub enum LoginStatus {
    /// Login completed successfully. Account is fully activated.
    Complete,
    /// Relay lists were not found. User must choose how to proceed.
    NeedsRelayLists,
}

pub struct LoginResult {
    pub account: Account,
    pub status: LoginStatus,
}
```

### LoginError

```rust
pub enum LoginError {
    /// The provided private key is not valid (bad format, bad encoding, etc.)
    InvalidKeyFormat(String),
    /// Could not connect to any relay to fetch or publish data.
    NoRelayConnections,
    /// The operation timed out (e.g. relay fetch took too long).
    Timeout,
    /// No partial login in progress for this pubkey (e.g. calling
    /// login_publish_default_relays without a prior login_start).
    NoLoginInProgress,
    /// An internal error that doesn't fit the above categories.
    Internal(String),
}
```

`LoginError` will also implement `From<...>` for the underlying error types it wraps (key parse errors, nostr-sdk errors, etc.) and will have a corresponding `WhitenoiseError::Login(LoginError)` variant for propagation through the existing error infrastructure.

### External signer login

`login_with_external_signer` needs an equivalent multi-step flow:

```rust
pub async fn login_external_signer_start(
    &self,
    pubkey: PublicKey,
    signer: NostrSigner,
) -> core::result::Result<LoginResult, LoginError>;

pub async fn login_external_signer_publish_default_relays(
    &self,
    pubkey: &PublicKey,
) -> core::result::Result<LoginResult, LoginError>;

pub async fn login_external_signer_with_custom_relay(
    &self,
    pubkey: &PublicKey,
    relay_url: RelayUrl,
) -> core::result::Result<LoginResult, LoginError>;

// login_cancel is shared -- works for both login paths.
```

### Login flow diagrams

**Happy path (relay lists found):**
```text
Flutter                          whitenoise-rs
  |                                   |
  |-- login_start(nsec) ------------->|
  |                                   |-- parse keys
  |                                   |-- create account in DB + keychain
  |                                   |-- ensure default relays connected
  |                                   |-- fetch NIP-65 from defaults -> FOUND
  |                                   |-- fetch Inbox from NIP-65 relays -> FOUND
  |                                   |-- fetch KeyPackage from NIP-65 relays -> FOUND
  |                                   |-- activate_account (subscriptions, key package)
  |<-- LoginResult { Complete } ------|
  |                                   |
  done
```

**No relay lists found -- user picks defaults:**
```text
Flutter                          whitenoise-rs
  |                                   |
  |-- login_start(nsec) ------------->|
  |                                   |-- parse keys
  |                                   |-- create account in DB + keychain
  |                                   |-- ensure default relays connected
  |                                   |-- fetch NIP-65 from defaults -> EMPTY
  |                                   |-- (skip Inbox/KeyPackage fetch)
  |<-- LoginResult { NeedsRelayLists }|
  |                                   |
  |  [show UI: "use defaults?" or     |
  |   "enter relay URL"]              |
  |                                   |
  |-- login_publish_default_relays -->|
  |                                   |-- publish 10002/10050/10051 with defaults
  |                                   |-- activate_account (subscriptions, key package)
  |<-- LoginResult { Complete } ------|
  |                                   |
  done
```

**No relay lists found -- user provides custom relay:**
```text
Flutter                          whitenoise-rs
  |                                   |
  |-- login_start(nsec) ------------->|
  |                                   |-- (same as above, returns NeedsRelayLists)
  |<-- LoginResult { NeedsRelayLists }|
  |                                   |
  |  [user enters relay URL]          |
  |                                   |
  |-- login_with_custom_relay ------->|
  |                                   |-- ensure custom relay connected
  |                                   |-- fetch NIP-65 from custom relay
  |                                   |-- if FOUND:
  |                                   |    -- fetch Inbox from NIP-65 relays
  |                                   |    -- fetch KeyPackage from NIP-65 relays
  |                                   |    -- activate_account
  |<-- LoginResult { Complete } ------|
  |                                   |
  done
  
  OR if not found on custom relay:
  
  |<-- LoginResult { NeedsRelayLists }|
  |                                   |
  |  [show UI again: relay not found] |
  |  ...                              |
```

**User cancels:**
```text
Flutter                          whitenoise-rs
  |                                   |
  |-- login_start(nsec) ------------->|
  |<-- LoginResult { NeedsRelayLists }|
  |                                   |
  |  [user taps back/cancel]          |
  |                                   |
  |-- login_cancel(pubkey) ---------->|
  |                                   |-- delete account from DB
  |                                   |-- remove key from keychain
  |<-- Ok(()) -----------------------|
  |                                   |
  done (back to login screen)
```

---

## Implementation Plan

### Phase 1: Core types and relay connection fix (whitenoise-rs)

**Files to create/modify:**

1. **Create `LoginError` enum and `LoginResult` struct**
   - Add to `src/whitenoise/accounts.rs` (alongside existing `AccountError`)
   - Add `WhitenoiseError::Login(LoginError)` variant in `src/whitenoise/error.rs`
   - Re-export `LoginError`, `LoginResult`, `LoginStatus` from `src/lib.rs`

2. **Fix `fetch_existing_relays` relay connection bug**
   - In `accounts.rs:1064`, add `self.nostr.ensure_relays_connected(&source_relay_urls).await?` before calling `fetch_user_relays`
   - This fixes the immediate crash for both `login` and `login_with_external_signer`

3. **Refactor relay discovery to detect "not found" without crashing**
   - Extract the relay discovery logic from `setup_relays_for_existing_account` into a method that returns a clear result: relays found or not found
   - When NIP-65 fetch returns empty, do NOT proceed to fetch Inbox/KeyPackage (they'd also be empty), return early with a "not found" status

### Phase 2: Multi-step login API (whitenoise-rs)

**Files to modify:**

1. **Add `login_start` method** (`accounts.rs`)
   - Parse keys, create base account, attempt relay discovery
   - If relay lists found: complete full login (activate, subscriptions, key package), return `LoginResult { status: Complete }`
   - If relay lists not found: leave account in partial state, return `LoginResult { status: NeedsRelayLists }`

2. **Add `login_publish_default_relays` method** (`accounts.rs`)
   - Validate a partial login is in progress for this pubkey
   - Set up default relays for all three types, publish all three relay list events
   - Complete the login (activate, subscriptions, key package)
   - Return `LoginResult { status: Complete }`

3. **Add `login_with_custom_relay` method** (`accounts.rs`)
   - Validate a partial login is in progress for this pubkey
   - Connect to the custom relay, fetch NIP-65 list
   - If found: fetch Inbox/KeyPackage from discovered NIP-65 relays, complete login
   - If not found: return `LoginResult { status: NeedsRelayLists }`

4. **Add `login_cancel` method** (`accounts.rs`)
   - Delete the partial account from DB
   - Remove the private key from the keychain
   - Clean up any relay connections that were added

5. **Track partial login state**
   - Need a way to know whether a pubkey has a partial login in progress
   - Options: a flag on the Account record, or an in-memory set of "pending login" pubkeys
   - An in-memory `DashSet<PublicKey>` on Whitenoise is simplest and avoids DB schema changes. If the app crashes mid-login, the partial account in the DB is harmless (a subsequent login attempt will overwrite it via `INSERT ... ON CONFLICT DO UPDATE`).

6. **Add equivalent external signer methods**
   - `login_external_signer_start`, `login_external_signer_publish_default_relays`, `login_external_signer_with_custom_relay`
   - These share most logic with the nsec path but use the external signer for publishing

7. **Old `login` and `login_with_external_signer` methods**
   - Leave them in place during development so nothing breaks while the new flow is being built
   - Remove them once the new multi-step API is fully implemented, tested, and wired up in the Flutter app

### Phase 3: Update tests (whitenoise-rs)

1. **Unit tests for `LoginError` and `LoginResult`**
   - Error display messages, From conversions
   - Follows the pattern in existing `error.rs` tests

2. **Unit tests for the multi-step flow**
   - Happy path: login_start finds relays, returns Complete
   - No relays: login_start returns NeedsRelayLists
   - Publish defaults: login_publish_default_relays completes login
   - Custom relay found: login_with_custom_relay completes login
   - Custom relay not found: login_with_custom_relay returns NeedsRelayLists
   - Cancel: login_cancel cleans up state
   - Invalid state transitions (e.g. login_publish_default_relays without login_start)

3. **Integration tests** (if Docker available)
   - Full login flow with relay discovery
   - Login with no relay lists -> publish defaults -> complete

### Phase 4: FRB bridge updates (Flutter repo)

This phase is in the Flutter repo (`marmot-protocol/whitenoise`). Changes needed:

1. **Update `rust/src/api/error.rs`**
   - Add `LoginError` variants to `ApiError` or create a parallel `ApiLoginError` enum
   - Map `WhitenoiseError::Login(LoginError)` to the specific API error variants

2. **Update `rust/src/api/accounts.rs`**
   - Add FRB-annotated functions for `login_start`, `login_publish_default_relays`, `login_with_custom_relay`, `login_cancel`
   - Create a `FlutterLoginResult` struct with FRB annotations

3. **Update `lib/hooks/use_login_with_nsec.dart`**
   - Replace `login()` call with `login_start()`
   - Handle `NeedsRelayLists` status by navigating to a relay choice screen
   - Match on specific error types for error messages

4. **Create relay choice UI**
   - New screen/dialog shown when `NeedsRelayLists` is returned
   - Two options: "Use default relays" or "Enter relay URL"
   - Wire up to `login_publish_default_relays` and `login_with_custom_relay`

5. **Add localized error strings**
   - Update `lib/l10n/app_en.arb` with error messages for each `LoginError` variant

---

## Files Changed Summary (whitenoise-rs only)

| File | Changes |
|------|---------|
| `src/whitenoise/accounts.rs` | Add `LoginError`, `LoginResult`, `LoginStatus`. Add multi-step login methods. Fix relay connection in `fetch_existing_relays`. Refactor relay discovery. |
| `src/whitenoise/error.rs` | Add `WhitenoiseError::Login(LoginError)` variant. |
| `src/whitenoise/mod.rs` | Add `pending_logins: DashSet<PublicKey>` field to `Whitenoise` struct. |
| `src/lib.rs` | Re-export `LoginError`, `LoginResult`, `LoginStatus`. |
