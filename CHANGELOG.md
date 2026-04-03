# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Thumbhash support for media previews alongside existing blurhash, allowing clients to migrate over time ([#714] [@erskingardner])
- CLI message reactions (react/unreact) ([#564] [@jgmontoya])
- CLI message delete, retry, and reply commands ([#565] [@jgmontoya])
- CLI media commands for uploading files and images ([#566] [@jgmontoya])
- Chat archiving: archive/unarchive chats, list archived, subscribe to archived chat list updates ([#591] [@jgmontoya])
- Relay control plane with health monitoring scaffold ([#579] [@erskingardner])
- Relay observability persistence for connection metrics and failure tracking ([#580] [@erskingardner])

### Breaking

- `FileMetadata` struct now includes a `thumbhash: Option<String>` field; code using struct literal construction must be updated ([#714] [@erskingardner])

## [v0.2.1] - 2026-03-05

### Added

- CLI with account management, group chat, messaging, follows, profile, relays, settings, user search, notifications, and streaming subscriptions ([#537] [@jgmontoya])

### Fixed

- Preserve delivery status on echoed outgoing messages ([#559] [@erskingardner])

## [v0.2.0] - 2026-03-05

Complete architectural rewrite from Tauri desktop/mobile app to a pure Rust library crate.

### Added

- Multi-step login API (`login_start`, `login_publish_default_relays`, `login_with_custom_relay`, `login_cancel`) that gracefully handles missing relay lists instead of crashing ([#485] [@erskingardner])
- `LoginError` enum, `LoginResult` struct, and `LoginStatus` enum for structured login error handling across the FFI boundary ([#485] [@erskingardner])
- Equivalent multi-step login API for external signer accounts ([#485] [@erskingardner])
- Integration test scenario (`login-flow`) covering happy path, no relays, publish defaults, custom relay, and cancel flows ([#485] [@erskingardner])
- KeyPackage lifecycle management with delayed cleanup: track published key packages from creation through Welcome consumption, then automatically clean up local MLS key material via a dedicated `ConsumedKeyPackageCleanup` scheduled task after a 30s quiet period ([#477] [@mubarakcoded])
- Export `Language` enum from crate root for library consumers ([#456] [@erskingardner])
- Search for contacts by npub or hex pubkey ([#153] [@erskingardner])
- Copy npub button in settings page ([#82] [@josefinalliende])
- Basic NWC support for paying invoices in messages ([#89] [@a-mpch], [@F3r10], [@jgmontoya], [@josefinalliende])
- Show invoice payments as a system message reply rather than as a reaction ([@a-mpch], [@jgmontoya])
- Blur QRs and hide pay button for paid invoices in messages ([@a-mpch], [@jgmontoya], [@josefinalliende])
- Truncate invoice content in messages ([@a-mpch], [@jgmontoya], [@josefinalliende])
- Add the ability to delete messages ([#97] [@jgmontoya])
- `KeyPackageStatus` enum to detect incompatible key packages before group operations ([#497] [@mubarakcoded])
- Key package ownership verification before skipping publish ([#499] [@mubarakcoded])
- MLS self-update immediately after joining a group for faster group sync ([#501] [@erskingardner])
- Group co-members included in user search results ([#510] [@jgmontoya])
- Message delivery status tracking with retry support, replacing fire-and-forget publishing ([#519] [@mubarakcoded])
- Debug mode for development troubleshooting ([#528] [@dmcarrington])

### Changed

- Better handling of long messages in chat ([#86] [@josefinalliende])
- User search metadata lookup rewritten as 5-tier pipeline with batched relay fetches, bounded concurrency, and NIP-65 relay discovery ([#470] [@jgmontoya])
- Old key packages now rotated automatically ([#469] [@dmcarrington])
- Accounts module split into focused submodules ([#517] [@jgmontoya])
- Groups and users modules split into focused submodules ([#541] [@dmcarrington])

### Fixed

- Login no longer crashes with "relay not found" when the user has no published relay lists ([#485] [@erskingardner])
- Fix key package publish reliability: detect failed relay publishes and retry with exponential backoff so accounts are never left with zero key packages ([#486] [@mubarakcoded])
- Publish evolution events before merging pending commits, with retry ([#505] [@erskingardner])
- Include connected relays in fallback alongside default relays ([#506] [@jgmontoya])
- Suppress NewMessage notifications for unaccepted groups ([#507] [@mubarakcoded])
- Recover subscriptions after external signer re-registration ([#512] [@erskingardner])
- Insertion-order reaction ordering using IndexMap ([#513] [@mubarakcoded])
- Block login when any of 10002/10050/10051 relay lists are missing ([#515] [@erskingardner])
- Remove user search radius cap for broader discovery ([#516] [@jgmontoya])
- Fall back to NIP-65 relays for giftwrap subscriptions when inbox relay list is missing ([#518] [@jgmontoya])
- Return Err for Unprocessable and PreviouslyFailed MLS results instead of silently dropping ([#525] [@erskingardner])
- Remove unsubscribe-first blind window and anchor replay to account sync state ([#524] [@erskingardner])
- Replace fixed sleep with subscription-gated catch-up before self-update ([#526] [@erskingardner])
- Clear pending commit on publish failure ([@erskingardner])
- Handle auto-committed proposals from process_message ([#504] [@erskingardner])
- Make `NostrManager::with_signer` cancellation-safe ([#538] [@erskingardner])
- Enforce relay filter validation and semantic event selection ([#540] [@erskingardner])
- Reject malformed or missing key-package e-tags in Welcome messages ([#539] [@erskingardner])
- Propagate errors from `sync_relay_urls` instead of swallowing them ([#545] [@dmcarrington])
- Fail-fast admin checks for admin-only group mutations ([#547] [@erskingardner])
- Pre-validate key package compatibility before adding members to groups ([#548] [@erskingardner])
- Emit retry status updates to stream subscribers for message delivery ([#556] [@erskingardner])

## [v0.1.0-alpha.5] - 2025-05-02

### Changed

- Migrated to Nostr MLS protocol specification ([#163] [@erskingardner])
- Improved reply message display and interactions ([#162] [@josefinalliende])
- Updated dependencies (blurhash, Cargo.lock) ([#165] [@jgmontoya])

### Fixed

- ANSI color codes no longer leak into non-terminal log output ([#167] [@johnathanCorgan])
- Android blank screen caused by mode-watcher upgrade ([@erskingardner])
- Android sheet and keyboard interaction fixes ([@erskingardner])

## [v0.1.0-alpha.4] - 2025-04-10

### Added

- NWC (Nostr Wallet Connect) support for paying invoices in chats ([#89] [@jgmontoya])
- Delete message functionality ([#97] [@jgmontoya])
- Invoice description display in messages ([#96] [@jgmontoya])
- Lightning invoice QR codes ([#95] [@josefinalliende])
- Media uploads with blossom integration ([#108] [@erskingardner])
- Media display in chat messages ([#127] [@erskingardner])
- Copy npub button in settings page ([#82] [@josefinalliende])
- Profile editing with media upload ([#139] [@erskingardner])
- Account switching ([#140] [@erskingardner])
- Account keys management page ([#145] [@erskingardner])
- Network settings page ([#152] [@erskingardner])
- Search contacts by pubkey ([#153] [@erskingardner])
- Docker compose for local relay and blossom server development ([#79] [@justinmoon])
- Frontend test CI pipeline ([#88] [@a-mpch])
- German and Italian translations, Spanish translation fixes ([#160] [@josefinalliende])
- Generative avatars ([@erskingardner])

### Changed

- Complete UI redesign with two-panel desktop layout ([#128] [@erskingardner])
- Refactored Tauri commands into focused modules ([#101] [@jgmontoya])
- Updated Rust and JavaScript dependencies ([#113] [@erskingardner])
- Smarter relay loading and connection management ([@erskingardner])

### Fixed

- Long messages now use proper word-break styling ([#86] [@josefinalliende])
- Android keyboard avoidance and UI improvements ([#136] [@erskingardner])
- Concurrency settings to prevent deadlocks ([@erskingardner])
- Delete reaction now works properly ([#110] [@josefinalliende])
- Account switch sidebar redirect fix ([#107] [@jgmontoya])

## [v0.1.0-alpha.3] - 2025-02-20

### Added
- Basic notification system, chat times, and latest message previews. ([@erskingardner])
- Simple nsec export via copy ([@erskingardner])
- Invite to WhiteNoise via NIP-04 DM. ([@erskingardner])

### Changed
- Messages now send on enter key press ([@erskingardner])
- Improved contact metadata fetching ([@erskingardner])
- Updated login page with new logo ([@erskingardner])
- Enhanced toast messages design ([@erskingardner])
- Updated styling for Android/iOS ([@erskingardner])
- Updated to nostr-sdk v38 ([@erskingardner])
- Improved build system for multiple platforms (Linux, Android, iOS, MacOS) ([@erskingardner])
- Split build workflows for better efficiency ([@erskingardner])

### Removed
- Removed overscroll behavior ([@erskingardner])
- Disabled unimplemented chat actions ([@erskingardner])

### Fixed
- Non-blocking tracing appender for stdout logging. iOS Builds now! ([#72] [@justinmoon])
- Android keyboard overlaying message input ([@erskingardner])
- Contact loading improvements ([@erskingardner])
- Fixed infinite looping back button behavior from chat details page ([@erskingardner])
- Fixed position of toasts on mobile ([@erskingardner])
- Various iOS and Android styles fixes ([@erskingardner])
- Fixed invite actions modal behavior for iOS and Android ([@erskingardner])
- Updated modal background ([@erskingardner])
- Improved group creation button behavior ([@erskingardner])
- Enhanced account management text ([@erskingardner])

## [v0.1.0-alpha.2] - 2025-02-08

### Added
- Replies! ([@erskingardner])
- Search all of nostr for contacts, not just your contact list ([@erskingardner])
- Add metadata to Name component when available ([@erskingardner])
- Improved contacts search (includes NIP-05 and npub now) ([@erskingardner])

### Fixed
- Delete all now gives more context ([@erskingardner])
- Fixed broken queries to delete data in database ([@erskingardner])
- Fixed broken query to fetch group relays ([@erskingardner])
- Fixed contact list display ([@erskingardner])

## [v0.1.0-alpha.1] - 2025-02-04

### Added
- Stickers (large emoji when you post just a single emoji) ([@erskingardner])
- Reactions! ([@erskingardner])
- Added relay list with status on group info page ([@erskingardner])

### Fixed
- Added more default relays to get better contact discovery ([@erskingardner])
- Fixed relay bug related to publishing key packages ([@erskingardner])
- Cleaned up dangling event listeners and log messaging ([@erskingardner])
- Scroll conversation to bottom on new messages ([@erskingardner])
- New chat window text alignment on mobile ([@erskingardner])

## [v0.1.0-alpha] - 2025-02-03

### Added
- Initial release of White Noise ([@erskingardner])


<!-- Contributors -->
[@erskingardner]: <https://github.com/erskingardner> (nostr:npub1zuuajd7u3sx8xu92yav9jwxpr839cs0kc3q6t56vd5u9q033xmhsk6c2uc)
[@justinmoon]: <https://github.com/justinmoon> (nostr:npub1zxu639qym0esxnn7rzrt48wycmfhdu3e5yvzwx7ja3t84zyc2r8qz8cx2y)
[@hodlbod]: <https://github.com/staab> (nostr:npub1jlrs53pkdfjnts29kveljul2sm0actt6n8dxrrzqcersttvcuv3qdjynqn)
[@dmcarrington]: <https://github.com/dmcarrington>
[@josefinalliende]: <https://github.com/josefinalliende> (nostr:npub1peps0fg2us0rzrsz40we8dw069yahjvzfuyznvnq68cyf9e9cw7s8agrxw)
[@jgmontoya]: <https://github.com/jgmontoya> (nostr:npub1jgm0ntzjr03wuzj5788llhed7l6fst05um4ej2r86ueaa08etv6sgd669p)
[@a-mpch]: <https://github.com/a-mpch> (nostr:npub1mpchxagw3kaglylnyajzjmghdj63vly9q5eu7d62fl72f2gz8xfqk6nwkd)
[@F3r10]: <https://github.com/F3r10>
[@mubarakcoded]: <https://github.com/mubarakcoded> (nostr:npub1mlyye6fpsqnkuxwv3nzzf3cmrau8x6z3fhh095246me87ya0aprsun609q)
[@johnathanCorgan]: <https://github.com/jcorgan>


<!-- PRs -->
[#72]: https://github.com/marmot-protocol/whitenoise-rs/pull/72
[#79]: https://github.com/marmot-protocol/whitenoise-rs/pull/79
[#82]: https://github.com/marmot-protocol/whitenoise-rs/pull/82
[#86]: https://github.com/marmot-protocol/whitenoise-rs/pull/86
[#88]: https://github.com/marmot-protocol/whitenoise-rs/pull/88
[#89]: https://github.com/marmot-protocol/whitenoise-rs/pull/89
[#95]: https://github.com/marmot-protocol/whitenoise-rs/pull/95
[#96]: https://github.com/marmot-protocol/whitenoise-rs/pull/96
[#97]: https://github.com/marmot-protocol/whitenoise-rs/pull/97
[#101]: https://github.com/marmot-protocol/whitenoise-rs/pull/101
[#107]: https://github.com/marmot-protocol/whitenoise-rs/pull/107
[#108]: https://github.com/marmot-protocol/whitenoise-rs/pull/108
[#110]: https://github.com/marmot-protocol/whitenoise-rs/pull/110
[#113]: https://github.com/marmot-protocol/whitenoise-rs/pull/113
[#127]: https://github.com/marmot-protocol/whitenoise-rs/pull/127
[#128]: https://github.com/marmot-protocol/whitenoise-rs/pull/128
[#136]: https://github.com/marmot-protocol/whitenoise-rs/pull/136
[#139]: https://github.com/marmot-protocol/whitenoise-rs/pull/139
[#140]: https://github.com/marmot-protocol/whitenoise-rs/pull/140
[#145]: https://github.com/marmot-protocol/whitenoise-rs/pull/145
[#152]: https://github.com/marmot-protocol/whitenoise-rs/pull/152
[#153]: https://github.com/marmot-protocol/whitenoise-rs/pull/153
[#160]: https://github.com/marmot-protocol/whitenoise-rs/pull/160
[#162]: https://github.com/marmot-protocol/whitenoise-rs/pull/162
[#163]: https://github.com/marmot-protocol/whitenoise-rs/pull/163
[#165]: https://github.com/marmot-protocol/whitenoise-rs/pull/165
[#167]: https://github.com/marmot-protocol/whitenoise-rs/pull/167
[#456]: https://github.com/marmot-protocol/whitenoise-rs/pull/456
[#469]: https://github.com/marmot-protocol/whitenoise-rs/pull/469
[#470]: https://github.com/marmot-protocol/whitenoise-rs/pull/470
[#477]: https://github.com/marmot-protocol/whitenoise-rs/pull/477
[#485]: https://github.com/marmot-protocol/whitenoise-rs/pull/485
[#486]: https://github.com/marmot-protocol/whitenoise-rs/pull/486
[#497]: https://github.com/marmot-protocol/whitenoise-rs/pull/497
[#499]: https://github.com/marmot-protocol/whitenoise-rs/pull/499
[#501]: https://github.com/marmot-protocol/whitenoise-rs/pull/501
[#504]: https://github.com/marmot-protocol/whitenoise-rs/pull/504
[#505]: https://github.com/marmot-protocol/whitenoise-rs/pull/505
[#506]: https://github.com/marmot-protocol/whitenoise-rs/pull/506
[#507]: https://github.com/marmot-protocol/whitenoise-rs/pull/507
[#510]: https://github.com/marmot-protocol/whitenoise-rs/pull/510
[#512]: https://github.com/marmot-protocol/whitenoise-rs/pull/512
[#513]: https://github.com/marmot-protocol/whitenoise-rs/pull/513
[#515]: https://github.com/marmot-protocol/whitenoise-rs/pull/515
[#516]: https://github.com/marmot-protocol/whitenoise-rs/pull/516
[#517]: https://github.com/marmot-protocol/whitenoise-rs/pull/517
[#518]: https://github.com/marmot-protocol/whitenoise-rs/pull/518
[#519]: https://github.com/marmot-protocol/whitenoise-rs/pull/519
[#524]: https://github.com/marmot-protocol/whitenoise-rs/pull/524
[#525]: https://github.com/marmot-protocol/whitenoise-rs/pull/525
[#526]: https://github.com/marmot-protocol/whitenoise-rs/pull/526
[#528]: https://github.com/marmot-protocol/whitenoise-rs/pull/528
[#537]: https://github.com/marmot-protocol/whitenoise-rs/pull/537
[#538]: https://github.com/marmot-protocol/whitenoise-rs/pull/538
[#539]: https://github.com/marmot-protocol/whitenoise-rs/pull/539
[#540]: https://github.com/marmot-protocol/whitenoise-rs/pull/540
[#541]: https://github.com/marmot-protocol/whitenoise-rs/pull/541
[#545]: https://github.com/marmot-protocol/whitenoise-rs/pull/545
[#547]: https://github.com/marmot-protocol/whitenoise-rs/pull/547
[#548]: https://github.com/marmot-protocol/whitenoise-rs/pull/548
[#556]: https://github.com/marmot-protocol/whitenoise-rs/pull/556
[#559]: https://github.com/marmot-protocol/whitenoise-rs/pull/559
[#564]: https://github.com/marmot-protocol/whitenoise-rs/pull/564
[#565]: https://github.com/marmot-protocol/whitenoise-rs/pull/565
[#566]: https://github.com/marmot-protocol/whitenoise-rs/pull/566
[#579]: https://github.com/marmot-protocol/whitenoise-rs/pull/579
[#580]: https://github.com/marmot-protocol/whitenoise-rs/pull/580
[#591]: https://github.com/marmot-protocol/whitenoise-rs/pull/591
[#714]: https://github.com/marmot-protocol/whitenoise-rs/pull/714


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
