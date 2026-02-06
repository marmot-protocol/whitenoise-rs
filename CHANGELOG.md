# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Export `Language` enum from crate root for library consumers ([erskingardner])
- Search for contacts by npub or hex pubkey ([erskingardner])
- Copy npub button in settings page([josefinalliende])
- Basic NWC support for paying invoices in messages ([a-mpch], [F3r10], [jgmontoya], [josefinalliende])
- Show invoice payments as a system message reply rather than as a reaction ([a-mpch], [jgmontoya])
- Blur QRs and hide pay button for paid invoices in messages ([a-mpch], [jgmontoya], [josefinalliende])
- Truncate invoice content in messages ([a-mpch], [jgmontoya], [josefinalliende])
- Add the ability to delete messages ([jgmontoya])

### Changed

- Better handling of long messages in chat ([josefinalliende])

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



<!-- Tags -->
[Unreleased]: https://github.com/marmot-protocol/whitenoise-rs/compare/v0.1.0-alpha.3...HEAD
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
