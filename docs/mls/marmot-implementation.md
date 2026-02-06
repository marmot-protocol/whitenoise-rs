# Marmot Implementation Notes

## Overview

Whitenoise implements the Marmot protocol, which brings MLS group messaging to the Nostr protocol. This document outlines our implementation approach and key design decisions.

## Marmot Specification

- [Official Specification](https://github.com/marmot-protocol/marmot)
- **Purpose**: Enable secure group messaging on Nostr using MLS

## Key Components

### Event Types

The following Nostr event kinds are used for MLS functionality:

```
Kind::MlsKeyPackage (443) - MLS key packages for group joining
Kind::MlsWelcome (444) - MLS welcome messages for new group members
Kind::MlsGroupMessage (445) - MLS application messages within groups
Kind::GiftWrap (1059) - Encrypted MLS protocol messages (Welcome messages, etc.)
Kind::InboxRelays (10050) - Inbox relay announcements
Kind::MlsKeyPackageRelays (10051) - Key package relay announcements
```

## Nostr Integration Points

### Event Processing

MLS events are routed through the main event processor:

```rust
// From src/whitenoise/event_processor/mod.rs
Kind::MlsGroupMessage => {
    whitenoise.handle_mls_message(&account, event.clone()).await
}
```

### Subscription Management

MLS messages are received via account-specific subscriptions that include the account's salted pubkey hash in the subscription ID format: `{hashed_pubkey}_{subscription_type}`

### Storage Integration

MLS state is stored using the `mdk-sqlite-storage` crate, integrated with our main SQLite database.

## Security Considerations

1. **Forward Secrecy**: MLS provides automatic forward secrecy through epoch key rotation
2. **Post-Compromise Security**: Regular key updates ensure recovery from key compromise
3. **Authentication**: All group members are authenticated via their Nostr public keys
4. **Transport Security**: Messages are encrypted both at the MLS layer and via Nostr's encryption
5. **Metadata Protection**: Messages published to nostr relays don't provide any identifying information

## Testing

MLS functionality is tested through:
- Unit tests in individual modules
- Integration tests in `src/integration_tests/`
- Full system testing via `just int-test`
