# Darkmatter Integration

## Overview

WhiteNoise uses Darkmatter crates for Marmot group state, key packages,
welcomes, MLS application messages, and Nostr transport parsing. The app owns
its persistence in this repository through `WhitenoiseMarmotStorage` and keeps
caller-facing API shapes in WhiteNoise modules.

The integration boundary is intentionally narrow:

- `src/marmot/session.rs` owns `MarmotSession`, the local wrapper around the
  Darkmatter engine.
- `src/marmot/storage/` implements the storage traits against WhiteNoise SQLite
  databases.
- `src/marmot/key_packages.rs`, `src/marmot/message.rs`,
  `src/marmot/media.rs`, and `src/marmot/push.rs` own WhiteNoise-facing codecs
  and protocol adapters.
- `src/whitenoise/session/` exposes account-scoped operations to the rest of
  the application.

## Runtime Model

Each logged-in account has one `AccountSession`. A session owns:

- the account-scoped database connection,
- the local signer when the account can sign,
- `WhitenoiseMarmotStorage`,
- an optional `MarmotSession` for Darkmatter-capable local accounts,
- relay handles and app services needed by account operations.

External signer accounts may not have a local `MarmotSession` until the account
can provide the synchronous identity proof required for current Darkmatter key
package publication. Public operations must fail clearly with the existing
WhiteNoise error types instead of manufacturing protocol state.

## Storage

WhiteNoise stores Darkmatter state in account-owned SQLite tables through
`WhitenoiseMarmotStorage`. Storage implementations live in small domain files
under `src/marmot/storage/` so protocol storage stays separate from app
projection tables.

Historical local secrets may still use the old keyring payload prefix and
keyring ID shape. Those strings are retained only so account logout and
whole-app deletion can remove old local artifacts.

## Publishing

Darkmatter engine calls return local state changes plus publish work. WhiteNoise
must publish Nostr events outside engine locks, then confirm publication back
into the session. This keeps network I/O out of the critical section and avoids
holding mutable protocol state across relay waits.

## Event Handling

Incoming Nostr events are peeled and routed by the Marmot boundary:

- key packages are current kind `30443` Darkmatter v2 events,
- welcomes are gift-wrapped kind `444` rumors,
- group application messages are kind `445` events.

Unsupported legacy protocol events are rejected or ignored according to the
handler contract. They must not create new local protocol storage.

## Error Handling

Darkmatter and storage errors are converted into `WhitenoiseError` at the
WhiteNoise boundary. Callers should see application-level failures such as
missing sessions, unsupported key packages, invalid events, or missing groups.

## Testing

Prefer tests that exercise public WhiteNoise behavior and assert projected
state, published events, or absence of legacy artifacts. Storage trait tests may
target `WhitenoiseMarmotStorage` directly when they prove persistence semantics.
