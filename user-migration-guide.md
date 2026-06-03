# User Migration Guide: MDK to Darkmatter

This guide is for applications that consume `whitenoise-rs`, including the
White Noise Flutter app. It covers source changes needed after the MDK removal
and Darkmatter integration.

The app-facing goal is the same product API: accounts, groups, messages,
notifications, media, and streams still exist. The Rust type names and a few
debug/media surfaces changed because `whitenoise-rs` no longer exposes MDK
types.

## Required Migration Order

1. Update the consumer to the new `whitenoise-rs` revision.
2. Remove all imports from `whitenoise::mdk`, `mdk_core`, `mdk_storage_traits`,
   and `mdk_sqlite_storage`.
3. Replace MDK group, message, config, and error types with the WhiteNoise-owned
   Marmot types listed below.
4. Update group create/update code to use `GroupConfig` and `GroupDataUpdate`.
5. Replace ratchet-tree debug UI with group forensics.
6. Regenerate bridge bindings if the consumer uses flutter_rust_bridge or
   another generated boundary.
7. Rebuild the app and fix generated model changes at compile time.
8. Run consumer integration tests against fresh local data.

Do the type migration before chasing UI failures. Most downstream breakage is
caused by old imports and stale generated bindings.

## Rust Type Map

| Old type or path                                     | New type or path                                    | Consumer action                                                                                         |
| ---------------------------------------------------- | --------------------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| `whitenoise::mdk::GroupId`                           | `whitenoise::marmot::GroupId`                       | Update imports. Existing hex/string helper code can keep using `from_slice`, `as_slice`, and `to_vec`.  |
| `whitenoise::mdk::Group`                             | `whitenoise::marmot::Group`                         | Update imports and review fields.                                                                       |
| `whitenoise::mdk::group_types::Group`                | `whitenoise::marmot::group_types::Group`            | Update imports if code expects the `group_types` module.                                                |
| `whitenoise::mdk::GroupState`                        | `whitenoise::marmot::GroupState`                    | Add handling for `Unrecoverable`.                                                                       |
| `NostrGroupConfigData`                               | `whitenoise::marmot::GroupConfig`                   | Use `GroupConfig::new(...)`; pass `None` for `disappearing_message_secs` if the app has no setting yet. |
| `NostrGroupDataUpdate`                               | `whitenoise::marmot::GroupDataUpdate`               | Use `GroupDataUpdate::new()` and builder methods, or update struct literals.                            |
| `mdk_core::prelude::message_types::Message`          | `whitenoise::marmot::Message`                       | Update conversions from `MessageWithTokens.message`.                                                    |
| `mdk_core::media_processing::MediaProcessingOptions` | `whitenoise::MediaProcessingOptions`                | Update imports. The media method signatures still take `Option<MediaProcessingOptions>`.                |
| `RatchetTreeInfo`                                    | `whitenoise::marmot::GroupForensics`                | Replace debug UI and support output. There is no ratchet-tree equivalent.                               |
| `WhitenoiseError::Mdk*` variants                     | `WhitenoiseError::Marmot*` and key-package variants | Update error matching. Avoid matching on old MDK errors.                                                |

## Group IDs

`GroupId` is now a WhiteNoise-owned opaque byte wrapper. Keep treating it as an
MLS group identifier. It is not the Nostr routing group id.

Use these methods:

```rust
use whitenoise::marmot::GroupId;

let group_id = GroupId::from_slice(&bytes);
let bytes = group_id.as_slice();
let hex = group_id.to_string();
```

If the consumer stores group IDs as hex strings, it can keep that format. Decode
the hex to bytes and call `GroupId::from_slice`.

## Groups

Group reads still return a group projection, but the projection now belongs to
WhiteNoise:

```rust
use whitenoise::marmot::{Group, GroupState};
```

Review code that reads these fields:

- `mls_group_id` remains the MLS group id.
- `nostr_group_id` is now explicit and is used for kind `445` routing.
- `state` can now be `Unrecoverable`.
- `self_update_state` is exposed.
- `disappearing_message_secs` is exposed.
- `image_key` and `image_nonce` are `Secret` values when present.

Do not serialize `Secret` values directly. `Secret` redacts debug output and
rejects normal serialization. UI code should project the fields it needs instead
of serializing the full protocol group when image secrets may be present.

### Group State

Consumers with their own group-state enum must add an `Unrecoverable` case:

```rust
match state {
    GroupState::Active => handle_active(),
    GroupState::Inactive => handle_inactive(),
    GroupState::Pending => handle_pending(),
    GroupState::Unrecoverable => handle_unrecoverable(),
}
```

For UI, treat `Unrecoverable` as a blocked group that cannot send or mutate
until a repair flow exists. It should not be shown as a normal active group.

## Creating Groups

Replace `NostrGroupConfigData` with `GroupConfig`.

```rust
use whitenoise::marmot::GroupConfig;

let config = GroupConfig::new(
    name,
    description,
    None,        // image_hash
    None,        // image_key
    None,        // image_nonce
    relays,
    admins,
    None,        // disappearing_message_secs
);

let group = session.groups().create_group(members, config, group_type).await?;
```

Darkmatter group creation currently rejects group image fields:

- `image_hash`
- `image_key`
- `image_nonce`

Apps should create groups without an image, then leave image UI disabled until
the group-image component is implemented in `whitenoise-rs`.

## Updating Groups

Replace `NostrGroupDataUpdate` with `GroupDataUpdate`.

```rust
use whitenoise::marmot::GroupDataUpdate;

let update = GroupDataUpdate::new()
    .name(name)
    .description(description)
    .relays(relays)
    .admins(admins);

session.groups().update_group_data(&group_id, update).await?;
```

For struct literals, preserve the nested optional semantics:

```rust
let update = GroupDataUpdate {
    name: Some(name),
    description: None,
    image_hash: None,
    image_key: None,
    image_nonce: None,
    image_upload_key: None,
    relays: None,
    admins: None,
    nostr_group_id: None,
    disappearing_message_secs: None,
};
```

Use `Some(None)` to clear optional fields. Use `None` to leave a field
unchanged.

Darkmatter group image updates currently return an error. Hide or disable group
image edit actions for Darkmatter groups until the Rust API supports them.

## Messages

`MessageWithTokens` still exists, but its `message` field is now
`whitenoise::marmot::Message`.

The public message projection keeps the fields consumers normally read:

- `id`
- `pubkey`
- `kind`
- `mls_group_id`
- `created_at`
- `processed_at`
- `content`
- `tags`
- `event`
- `wrapper_event_id`
- `epoch`
- `state`

If the consumer converts `MessageWithTokens` into an app model, keep the same
field mapping for `id`, `pubkey`, `kind`, `created_at`, `content`, and `tokens`.
Update only the Rust import and any type annotations.

Message state strings remain:

- `created`
- `processed`
- `deleted`
- `epoch_invalidated`

## Debug and Support Screens

The ratchet-tree API is gone.

Replace:

```rust
let info = whitenoise.ratchet_tree_info(&account, &group_id)?;
```

with:

```rust
let info = whitenoise.group_forensics(&account, &group_id).await?;
```

The new `GroupForensics` data is redacted support data:

- `group_id`
- `epoch`
- `member_count`
- `required_app_components`
- `messages`
- `snapshots`
- `warnings`

Update UI labels, logs, test names, and clipboard output from "ratchet tree" to
"group forensics". The old debug output fields `tree_hash`, `serialized_tree`,
and `leaf_nodes` no longer exist.

## CLI and Daemon JSON Protocol

If a consumer speaks to `wnd` over the CLI JSON socket, update debug requests.

Old request method:

```json
{
  "method": "debug_ratchet_tree",
  "params": { "account": "npub...", "group_id": "..." }
}
```

New request method:

```json
{
  "method": "debug_group_forensics",
  "params": { "account": "npub...", "group_id": "..." }
}
```

The daemon still accepts `debug_ratchet_tree` as an input alias, but new clients
should send `debug_group_forensics`. Responses now contain group forensics, not
ratchet-tree data.

The CLI command changed from:

```sh
wn debug ratchet-tree <group-id>
```

to:

```sh
wn debug group-forensics <group-id>
```

The old command name is kept as an alias.

## Media

`MediaProcessingOptions` is now exported by `whitenoise-rs`:

```rust
use whitenoise::MediaProcessingOptions;
```

The chat media APIs still accept `Option<MediaProcessingOptions>`. Passing
`None` keeps default validation, EXIF sanitization, blurhash generation, and
thumbhash generation.

Group image APIs are different from chat media:

- `upload_chat_media` is the supported path for message attachments.
- `upload_group_image` returns `UnsupportedMarmotOperation` for Darkmatter
  groups.
- group creation and group update reject image metadata fields.

Consumer UI should keep chat media enabled and disable group avatar upload/edit
for this migration.

## Push Notifications

Push registration APIs still take `PushPlatform`, raw token, notification server
pubkey, and optional relay hint.

If the consumer reads `GroupPushToken` directly, update the model for two new
optional fields:

- `platform: Option<PushPlatform>`
- `token_fingerprint: Option<String>`

These fields let WhiteNoise publish Darkmatter token update and removal events
without decrypting cached tokens. Treat both fields as metadata. Do not display
raw push tokens.

`PushPlatform` string values remain lowercase:

- `apns`
- `fcm`

## Errors

Remove matches for old MDK error variants:

- `MdkSqliteStorage`
- `MdkCoreError`
- `MdkGroupImage`
- `MdkEncryptedMedia`
- `KeyPackageMissingSelfRemove`
- `GroupRejectedMember`
- `MlsMessageUnprocessable`
- `MlsMessagePreviouslyFailed`

Handle the new variants that matter to app flows:

- `MarmotStorage`
- `MarmotEngine`
- `MarmotSessionUnavailable`
- `UnsupportedMarmotOperation`
- `UnexpectedEncodingTag`
- `MissingKeyPackageRefTag`
- `InvalidKeyPackageRef`
- `KeyPackageIdentityMismatch`
- `UnsupportedKeyPackageFormat`
- `MemberKeyPackageNotFound`
- `MarmotPublishFailed`

Suggested UI handling:

- `MarmotSessionUnavailable`: ask the user to finish account setup or reconnect
  the external signer.
- `UnsupportedMarmotOperation`: hide or disable that action for now. This is
  expected for Darkmatter group image operations.
- `UnsupportedKeyPackageFormat`, `IncompatibleKeyPackage`, and
  `MemberKeyPackageNotFound`: show an invite failure and ask the peer to update
  and reopen White Noise so a current key package is published.
- `MarmotPublishFailed`: show a send/update failure with retry where the action
  is retryable.

## External Signers

External signer accounts can have an account session without a live Marmot
session. Operations that need Darkmatter protocol state may return
`MarmotSessionUnavailable`.

Consumers that support Amber or another external signer should keep calling
`register_external_signer` when the signer becomes available again. Treat
`SignerUnavailable` and `MarmotSessionUnavailable` as recoverable account-state
errors, not data corruption.

## Protocol Assumptions

Consumers should stop depending on MDK relay artifacts or legacy MLS storage.

Current Darkmatter-facing Nostr kinds are:

- key packages: kind `30443`
- welcomes: kind `444`
- group messages: kind `445`

Do not require a twin kind `443` key package. The current migration does not
need it.

Consumers should not try to read or repair old MDK SQLite storage. Missing
groups should fail as missing groups, without creating obsolete MLS storage.
For local development data from an old MDK build, prefer a clean app data
directory.

## C ABI Background Push

`src/ffi.rs` did not change in this migration. The C ABI function
`wn_collect_notifications_after_push` keeps the same signature and JSON result
shape:

```json
{
  "status": "new_data | no_data | failed",
  "notifications": [],
  "error": "only present on failure"
}
```

Native iOS notification-service code that only calls this C ABI does not need a
signature change.

## White Noise Flutter App Notes

The Flutter app has its own Rust wrapper crate in `../whitenoise/rust/src/api`
and generated flutter_rust_bridge files under `../whitenoise/lib/src/rust` and
`../whitenoise/rust/src/frb_generated.rs`.

Update these wrapper imports first:

- `rust/src/api/utils.rs`
- `rust/src/api/mod.rs`
- `rust/src/api/groups.rs`
- `rust/src/api/account_groups.rs`
- `rust/src/api/chat_list.rs`

Replace:

```rust
use whitenoise::mdk::GroupId;
use whitenoise::mdk::{Group as WhitenoiseGroup, GroupState as WhitenoiseGroupState};
use whitenoise::mdk::{NostrGroupConfigData, NostrGroupDataUpdate};
```

with:

```rust
use whitenoise::marmot::GroupId;
use whitenoise::marmot::{Group as WhitenoiseGroup, GroupState as WhitenoiseGroupState};
use whitenoise::marmot::{GroupConfig, GroupDataUpdate};
```

Then update `rust/src/api/groups.rs`:

- Convert `FlutterGroupDataUpdate` into `GroupDataUpdate`.
- Build create-group config with `GroupConfig::new(...)`.
- Add `GroupState::Unrecoverable` to the Flutter-facing enum and conversion.
- Replace `RatchetTreeInfo` and `get_ratchet_tree_info` with
  `GroupForensics` and `get_group_forensics`.
- Disable group image create/update controls or map the Rust error into a
  clear unsupported-action message.

Update Dart references after bridge generation:

- Replace `RatchetTreeInfo` with the generated group-forensics model.
- Replace calls to `crateApiGroupsGetRatchetTreeInfo` with the new generated
  group-forensics call.
- Update tests that assert `ratchet_tree` text.
- Add handling for `GroupState.unrecoverable`.
- Update mock `Group` constructors if generated fields change.

Regenerate the Flutter bridge from the Flutter app repository:

```sh
cd ../whitenoise
just regenerate
dart format .
```

Then run the app-side checks:

```sh
flutter analyze
flutter test
```

If generated files still contain `RatchetTreeInfo` or imports from
`whitenoise::mdk`, the wrapper crate was not fully migrated before generation.

## Consumer Test Checklist

Run these flows on a fresh data directory:

- create identity
- login existing identity
- publish key package
- create group without image
- invite member
- accept invite
- send message
- receive message
- add member
- remove member
- leave group
- edit group name and description
- upload, send, receive, and download chat media
- register push token
- disable notifications and verify token removal does not fail
- reconnect external signer and verify account recovery
- open debug/support screen and load group forensics

Also run negative checks:

- group image upload shows an unsupported action
- old ratchet-tree UI is gone
- no app code imports `whitenoise::mdk`
- no app code depends on kind `443` key packages
- generated bridge files contain the new group-forensics symbols
