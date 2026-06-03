# Session Projection Implementation Plan

This plan tracks the remaining work for the session/projection architecture
after moving protocol state to Darkmatter and WhiteNoise-owned storage.

## Current Baseline

- `AccountSession` is the account-scoped entry point for group, message,
  key-package, push, and relay operations.
- `WhitenoiseMarmotStorage` implements protocol storage traits in this
  repository.
- Local Darkmatter-capable accounts open a `MarmotSession`.
- Public reads should answer from WhiteNoise projections or confirmed Marmot
  projections.
- Unsupported legacy protocol artifacts are cleanup-only.

## Phase 1: Tighten Session Boundaries

1. Keep public `Whitenoise` methods as thin account resolution wrappers.
2. Move implementation logic into session facades when an operation requires
   account identity.
3. Avoid accepting both `Account` and account pubkey in lower layers.
4. Keep signer access session-owned.

Done when new account-scoped behavior can be implemented without adding shared
singleton access or cross-account database calls.

## Phase 2: Finish Projection Ownership

1. Route chat list, group information, message aggregation, media metadata, and
   push-token reads through projection repositories.
2. Treat confirmed Marmot group/message projections as the repair source for
   missing app rows.
3. Return `GroupNotFound`, `MarmotSessionUnavailable`, or invalid-event errors
   when neither projected nor confirmed Marmot state exists.
4. Keep projection writes idempotent so replayed relay events and startup sync
   can safely reprocess events.

Done when public reads no longer need protocol-engine access unless the method
is explicitly a protocol mutation.

## Phase 3: Keep Protocol Locks Small

1. Validate inputs and fetch app rows before locking `MarmotSession`.
2. Call Darkmatter through `MarmotSession`.
3. Collect publish/projection effects.
4. Release the lock before relay I/O.
5. Confirm successful publication in a short follow-up lock.

Done when no relay publish or long database scan runs while mutable protocol
state is held.

## Phase 4: Remove Legacy Surface Area

1. Keep historical keyring/storage identifiers only inside deletion or database
   key decoding paths.
2. Do not add test helpers that manufacture unsupported protocol state.
3. Keep docs, CLI output, FFI names, and public types on Marmot/Darkmatter or
   WhiteNoise-owned vocabulary.
4. Delete cleanup-only historical support when product policy allows old local
   data to be ignored entirely.

Done when source scans find no active legacy protocol crate names or runtime
facades outside explicit cleanup identifiers.

## Phase 5: Integration Validation

1. Run `just precommit-quick` after each substantial source slice.
2. Run `just docker-up` and `just int-test` when Docker is healthy.
3. Run `just precommit` before considering the branch ready.
4. If Docker is unhealthy, record the exact command that hung or failed and do
   not claim integration coverage was refreshed.

Done when both the quick gate and Docker-backed integration gate pass from a
clean service state.

## Stop Rules

Stop and document the blocker instead of adding a workaround if:

- Darkmatter lacks a required storage trait or exporter API.
- External signer accounts cannot produce required identity proofs.
- A protocol mutation requires holding engine state across relay I/O.
- App projections cannot preserve witness data needed by convergence policy.
- Integration tests require unsupported legacy protocol interoperability before
  a deliberate data migration policy exists.
