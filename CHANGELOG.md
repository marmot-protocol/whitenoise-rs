# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- CLI message reactions (react/unreact) ([jgmontoya])
- CLI message delete, retry, and reply commands ([jgmontoya])
- CLI media commands for uploading files and images ([jgmontoya])
- Chat archiving: archive/unarchive chats, list archived, subscribe to archived chat list updates ([jgmontoya])
- Relay control plane with health monitoring scaffold ([erskingardner])
- Relay observability persistence for connection metrics and failure tracking ([erskingardner])

## [v0.2.1] - 2026-03-05

### Added

- CLI with account management, group chat, messaging, follows, profile, relays, settings, user search, notifications, and streaming subscriptions ([jgmontoya])

### Fixed

- Preserve delivery status on echoed outgoing messages ([erskingardner])

## [v0.2.0] - 2026-03-05

Complete architectural rewrite from Tauri desktop/mobile app to a pure Rust library crate.

### Added

- Multi-step login API (`login_start`, `login_publish_default_relays`, `login_with_custom_relay`, `login_cancel`) that gracefully handles missing relay lists instead of crashing ([erskingardner])
- `LoginError` enum, `LoginResult` struct, and `LoginStatus` enum for structured login error handling across the FFI boundary ([erskingardner])
- Equivalent multi-step login API for external signer accounts ([erskingardner])
- Integration test scenario (`login-flow`) covering happy path, no relays, publish defaults, custom relay, and cancel flows ([erskingardner])
- KeyPackage lifecycle management with delayed cleanup: track published key packages from creation through Welcome consumption, then automatically clean up local MLS key material via a dedicated `ConsumedKeyPackageCleanup` scheduled task after a 30s quiet period ([mubarakcoded])
- Export `Language` enum from crate root for library consumers ([erskingardner])
- Search for contacts by npub or hex pubkey ([erskingardner])
- Copy npub button in settings page ([josefinalliende])
- Basic NWC support for paying invoices in messages ([a-mpch], [F3r10], [jgmontoya], [josefinalliende])
- Show invoice payments as a system message reply rather than as a reaction ([a-mpch], [jgmontoya])
- Blur QRs and hide pay button for paid invoices in messages ([a-mpch], [jgmontoya], [josefinalliende])
- Truncate invoice content in messages ([a-mpch], [jgmontoya], [josefinalliende])
- Add the ability to delete messages ([jgmontoya])
- `KeyPackageStatus` enum to detect incompatible key packages before group operations ([mubarakcoded])
- Key package ownership verification before skipping publish ([mubarakcoded])
- MLS self-update immediately after joining a group for faster group sync ([erskingardner])
- Group co-members included in user search results ([jgmontoya])
- Message delivery status tracking with retry support, replacing fire-and-forget publishing ([mubarakcoded])
- Debug mode for development troubleshooting ([dmcarrington])

### Changed

- Better handling of long messages in chat ([josefinalliende])
- User search metadata lookup rewritten as 5-tier pipeline with batched relay fetches, bounded concurrency, and NIP-65 relay discovery ([jgmontoya])
- Old key packages now rotated automatically ([dmcarrington])
- Accounts module split into focused submodules ([jgmontoya])
- Groups and users modules split into focused submodules ([dmcarrington])

### Fixed

- Login no longer crashes with "relay not found" when the user has no published relay lists ([erskingardner])
- Fix key package publish reliability: detect failed relay publishes and retry with exponential backoff so accounts are never left with zero key packages ([mubarakcoded])
- Publish evolution events before merging pending commits, with retry ([erskingardner])
- Include connected relays in fallback alongside default relays ([jgmontoya])
- Suppress NewMessage notifications for unaccepted groups ([mubarakcoded])
- Recover subscriptions after external signer re-registration ([erskingardner])
- Insertion-order reaction ordering using IndexMap ([mubarakcoded])
- Block login when any of 10002/10050/10051 relay lists are missing ([erskingardner])
- Remove user search radius cap for broader discovery ([jgmontoya])
- Fall back to NIP-65 relays for giftwrap subscriptions when inbox relay list is missing ([jgmontoya])
- Return Err for Unprocessable and PreviouslyFailed MLS results instead of silently dropping ([erskingardner])
- Remove unsubscribe-first blind window and anchor replay to account sync state ([erskingardner])
- Replace fixed sleep with subscription-gated catch-up before self-update ([erskingardner])
- Clear pending commit on publish failure ([erskingardner])
- Handle auto-committed proposals from process_message ([erskingardner])
- Make `NostrManager::with_signer` cancellation-safe ([erskingardner])
- Enforce relay filter validation and semantic event selection ([erskingardner])
- Reject malformed or missing key-package e-tags in Welcome messages ([erskingardner])
- Propagate errors from `sync_relay_urls` instead of swallowing them ([dmcarrington])
- Fail-fast admin checks for admin-only group mutations ([erskingardner])
- Pre-validate key package compatibility before adding members to groups ([erskingardner])
- Emit retry status updates to stream subscribers for message delivery ([erskingardner])

## [v0.1.0-alpha.5] - 2025-05-02

### Changed

- Migrated to Nostr MLS protocol specification ([erskingardner])
- Improved reply message display and interactions ([josefinalliende])
- Updated dependencies (blurhash, Cargo.lock) ([jgmontoya])

### Fixed

- ANSI color codes no longer leak into non-terminal log output ([johnathanCorgan])
- Android blank screen caused by mode-watcher upgrade ([erskingardner])
- Android sheet and keyboard interaction fixes ([erskingardner])

## [v0.1.0-alpha.4] - 2025-04-10

### Added

- NWC (Nostr Wallet Connect) support for paying invoices in chats ([jgmontoya])
- Delete message functionality ([jgmontoya])
- Invoice description display in messages ([jgmontoya])
- Lightning invoice QR codes ([josefinalliende])
- Media uploads with blossom integration ([erskingardner])
- Media display in chat messages ([erskingardner])
- Copy npub button in settings page ([josefinalliende])
- Profile editing with media upload ([erskingardner])
- Account switching ([erskingardner])
- Account keys management page ([erskingardner])
- Network settings page ([erskingardner])
- Search contacts by pubkey ([erskingardner])
- Docker compose for local relay and blossom server development ([justinmoon])
- Frontend test CI pipeline ([a-mpch])
- German and Italian translations, Spanish translation fixes ([josefinalliende])
- Generative avatars ([erskingardner])

### Changed

- Complete UI redesign with two-panel desktop layout ([erskingardner])
- Refactored Tauri commands into focused modules ([jgmontoya])
- Updated Rust and JavaScript dependencies ([erskingardner])
- Smarter relay loading and connection management ([erskingardner])

### Fixed

- Long messages now use proper word-break styling ([josefinalliende])
- Android keyboard avoidance and UI improvements ([erskingardner])
- Concurrency settings to prevent deadlocks ([erskingardner])
- Delete reaction now works properly ([josefinalliende])
- Account switch sidebar redirect fix ([jgmontoya])

## [v0.1.0-alpha.3] - 2025-02-20

### Added
- Basic notification system, chat times, and latest message previews. ([erskingardner])
- Simple nsec export via copy ([erskingardner])
- Invite to WhiteNoise via NIP-04 DM. ([erskingardner])

### Changed
- Messages now send on enter key press ([erskingardner])
- Improved contact metadata fetching ([erskingardner])
- Updated login page with new logo ([erskingardner])
- Enhanced toast messages design ([erskingardner])
- Updated styling for Android/iOS ([erskingardner])
- Updated to nostr-sdk v38 ([erskingardner])
- Improved build system for multiple platforms (Linux, Android, iOS, MacOS) ([erskingardner])
- Split build workflows for better efficiency ([erskingardner])

### Removed
- Removed overscroll behavior ([erskingardner])
- Disabled unimplemented chat actions ([erskingardner])

### Fixed
- Non-blocking tracing appender for stdout logging. iOS Builds now! ([justinmoon])
- Android keyboard overlaying message input ([erskingardner])
- Contact loading improvements ([erskingardner])
- Fixed infinite looping back button behavior from chat details page ([erskingardner])
- Fixed position of toasts on mobile ([erskingardner])
- Various iOS and Android styles fixes ([erskingardner])
- Fixed invite actions modal behavior for iOS and Android ([erskingardner])
- Updated modal background ([erskingardner])
- Improved group creation button behavior ([erskingardner])
- Enhanced account management text ([erskingardner])

## [v0.1.0-alpha.2] - 2025-02-08

### Added
- Replies! 💬 ([erskingardner])
- Search all of nostr for contacts, not just your contact list 🔍 ([erskingardner])
- Add metadata to Name component when available ([erskingardner])
- Improved contacts search (includes NIP-05 and npub now) ([erskingardner])

### Fixed
- Delete all now gives more context ([erskingardner])
- Fixed broken queries to delete data in database ([erskingardner])
- Fixed broken query to fetch group relays ([erskingardner])
- Fixed contact list display ([erskingardner])

## [v0.1.0-alpha.1] - 2025-02-04

### Added
- Stickers (large emoji when you post just a single emoji) ⭐ ([erskingardner])
- Reactions! ❤️ ([erskingardner])
- Added relay list with status on group info page ([erskingardner])

### Fixed
- Added more default relays to get better contact discovery ([erskingardner])
- Fixed relay bug related to publishing key packages ([erskingardner])
- Cleaned up dangling event listeners and log messaging ([erskingardner])
- Scroll conversation to bottom on new messages ([erskingardner])
- New chat window text alignment on mobile ([erskingardner])

## [v0.1.0-alpha] - 2025-02-03

### Added
- Initial release of White Noise ([erskingardner])


<!-- Contributors -->
[erskingardner]: <https://github.com/erskingardner> (nostr:npub1zuuajd7u3sx8xu92yav9jwxpr839cs0kc3q6t56vd5u9q033xmhsk6c2uc)
[justinmoon]: <https://github.com/justinmoon> (nostr:npub1zxu639qym0esxnn7rzrt48wycmfhdu3e5yvzwx7ja3t84zyc2r8qz8cx2y)
[hodlbod]: <https://github.com/staab> (nostr:npub1jlrs53pkdfjnts29kveljul2sm0actt6n8dxrrzqcersttvcuv3qdjynqn)
[dmcarrington]: <https://github.com/dmcarrington>
[josefinalliende]: <https://github.com/josefinalliende> (nostr:npub1peps0fg2us0rzrsz40we8dw069yahjvzfuyznvnq68cyf9e9cw7s8agrxw)
[jgmontoya]: <https://github.com/jgmontoya> (nostr:npub1jgm0ntzjr03wuzj5788llhed7l6fst05um4ej2r86ueaa08etv6sgd669p)
[a-mpch]: <https://github.com/a-mpch> (nostr:npub1mpchxagw3kaglylnyajzjmghdj63vly9q5eu7d62fl72f2gz8xfqk6nwkd)
[F3r10]: <https://github.com/F3r10>
[mubarakcoded]: <https://github.com/mubarakcoded> (nostr:npub1mlyye6fpsqnkuxwv3nzzf3cmrau8x6z3fhh095246me87ya0aprsun609q)
[johnathanCorgan]: <https://github.com/jcorgan>



<!-- Tags -->
[Unreleased]: https://github.com/marmot-protocol/whitenoise-rs/compare/v0.2.1...HEAD
[v0.2.1]: https://github.com/marmot-protocol/whitenoise-rs/compare/v0.2.0...v0.2.1
[v0.2.0]: https://github.com/marmot-protocol/whitenoise-rs/compare/v0.1.0-alpha.5...v0.2.0
[v0.1.0-alpha.5]: https://github.com/marmot-protocol/whitenoise-rs/compare/v0.1.0-alpha.4...v0.1.0-alpha.5
[v0.1.0-alpha.4]: https://github.com/marmot-protocol/whitenoise-rs/compare/v0.1.0-alpha.3...v0.1.0-alpha.4
[v0.1.0-alpha.3]: https://github.com/marmot-protocol/whitenoise-rs/releases/tag/v0.1.0-alpha.3
[v0.1.0-alpha.2]: https://github.com/marmot-protocol/whitenoise-rs/releases/tag/v0.1.0-alpha.2
[v0.1.0-alpha.1]: https://github.com/marmot-protocol/whitenoise-rs/releases/tag/v0.1.0-alpha.1
[v0.1.0-alpha]: https://github.com/marmot-protocol/whitenoise-rs/releases/tag/v0.1.0-alpha


<!-- Categories
`Added` for new features.
`Changed` for changes in existing functionality.
`Deprecated` for soon-to-be removed features.
`Removed` for now removed features.
`Fixed` for any bug fixes.
`Security` in case of vulnerabilities.
-->
