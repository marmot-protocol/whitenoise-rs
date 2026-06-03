# Session Projection Rearchitecture

This document describes the current target architecture for account sessions,
Marmot protocol state, and app projections.

## Principles

1. Account identity is session-scoped. Public APIs may still accept an account
   pubkey for caller ergonomics, but implementation code should move quickly to
   an `AccountSession`.
2. Protocol state belongs to the Marmot boundary. Application services consume
   projected WhiteNoise types rather than Darkmatter crate types directly.
3. Storage is owned here. `WhitenoiseMarmotStorage` implements the Darkmatter
   storage traits against WhiteNoise SQLite tables.
4. Network publication happens outside protocol locks. Engine calls can produce
   publish work; relay I/O must not run while mutable protocol state is held.
5. Unsupported legacy protocol data is not a fallback source of truth. Public
   reads answer from WhiteNoise projections or return clear errors.

## Runtime Shape

`Whitenoise` owns shared process-wide services: shared database, Nostr manager,
media storage, settings, stream broadcasters, and background task wiring.

Each logged-in account is represented by an `AccountSession`:

```text
Whitenoise
  shared services
  sessions
    AccountSession
      account metadata
      account database
      signer, when locally available
      WhitenoiseMarmotStorage
      MarmotSession, when Darkmatter-capable
      relay handles
      account-scoped service facades
```

Session facades such as `session.groups()`, `session.messages()`,
`session.key_packages()`, and `session.push()` keep account identity implicit
inside the session. This avoids passing account pubkeys through deep call chains
and prevents cross-account storage access.

## Storage Model

WhiteNoise keeps three kinds of state separate:

- Shared app state in the shared database.
- Account app projections in each account database.
- Marmot protocol state in `WhitenoiseMarmotStorage`.

Projection tables store the app-facing view: group membership, group
information, chat list inputs, aggregated messages, push-token metadata, media
metadata, and key-package publication bookkeeping.

Protocol storage stores the durable data required by the Darkmatter engine:
groups, key material references, outbound publish intents, received-message
witness data, capabilities, welcomes, and convergence policy data.

Historical local storage identifiers may still be recognized by deletion
paths. That recognition is cleanup-only and must not create or reopen old
protocol runtimes.

## Event Flow

1. Relay events enter `nostr_manager` and are forwarded to the event processor.
2. The event processor resolves the target account session.
3. Marmot transport code peels and validates protocol events.
4. `MarmotSession` mutates protocol state and returns publish/projection
   effects.
5. WhiteNoise persists app projections and emits stream updates.
6. Any publish work is sent through relay publishers outside the engine lock.
7. Successful publication is confirmed back into the Marmot session.

## Projection Rules

App code should prefer projected state over protocol internals:

- Chat lists read account-group rows, group information, aggregated messages,
  and notification metadata.
- Message APIs read/write raw Marmot message projections and aggregated
  message rows.
- Group-information repair may use confirmed Marmot group projections.
- Push and media flows use WhiteNoise-owned codecs and projection tables.

If a public read cannot answer from projections and there is no confirmed
Marmot state, it should fail clearly. Silent repair from unsupported legacy
state is not part of the target design.

## Locking

The protocol engine requires mutable access. Keep lock scopes tight:

- validate inputs before locking when possible,
- call the engine while holding the session lock,
- collect effects,
- release the lock,
- publish to relays,
- reacquire only to confirm publication or persist protocol follow-up state.

Long-running database reads that do not need mutable engine state should use
the storage/projection layer directly.

## Testing

Tests should assert public behavior first: returned values, projected rows,
published Nostr events, stream updates, and absence of unintended legacy
artifacts. Storage-trait tests may target `WhitenoiseMarmotStorage` directly
when they prove durable protocol behavior.
