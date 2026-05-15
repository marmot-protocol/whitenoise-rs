//! Bech32 shape validation + nostr HRP classification.
//!
//! Shape only — no checksum verification (see `PLAN.md` §4). The recognizer
//! is called from the inline tokenizer for both `@npub1…` mentions and
//! `nostr:<hrp>1…` URIs.

use crate::ast::NostrHrp;

/// Try to classify a bech32-shaped token starting at `i`. Returns the parsed
/// HRP and the **byte index after** the last bech32 char.
///
/// Validation:
/// - HRP is a run of `[a-z]+` (lowercase ASCII letters). For our use the
///   HRP is whitelisted further by the caller.
/// - A literal `1` separates HRP from data.
/// - Data is `[a-z0-9]` *excluding* `b`, `i`, `o` and `1` (the standard
///   bech32 alphabet), with at least 6 chars.
/// - Total bech32 string length 8..=90 (per BIP-173).
/// - All-lowercase: any uppercase letter rejects (mixed case forbidden).
pub(crate) fn classify_bech32(bytes: &[u8], i: usize) -> Option<(NostrHrp, usize)> {
    let total_start = i;
    let mut j = i;
    // HRP: lowercase letters.
    while j < bytes.len() && bytes[j].is_ascii_lowercase() {
        j += 1;
    }
    if j == i {
        return None;
    }
    let hrp_bytes = &bytes[i..j];
    let hrp = classify_hrp(hrp_bytes)?;
    // Separator '1'.
    if bytes.get(j) != Some(&b'1') {
        return None;
    }
    j += 1;
    let data_start = j;
    while j < bytes.len() && is_bech32_data_char(bytes[j]) {
        j += 1;
    }
    if j - data_start < 6 {
        return None;
    }
    let total_len = j - total_start;
    if !(8..=90).contains(&total_len) {
        return None;
    }
    Some((hrp, j))
}

fn is_bech32_data_char(b: u8) -> bool {
    // Standard bech32 alphabet: 0-9 a-z minus b, i, o, 1.
    matches!(
        b,
        b'0' | b'2'..=b'9'
            | b'a'
            | b'c'..=b'h'
            | b'j'..=b'n'
            | b'p'..=b'z'
    )
}

fn classify_hrp(b: &[u8]) -> Option<NostrHrp> {
    match b {
        b"npub" => Some(NostrHrp::Npub),
        b"note" => Some(NostrHrp::Note),
        b"nevent" => Some(NostrHrp::Nevent),
        b"nprofile" => Some(NostrHrp::Nprofile),
        b"naddr" => Some(NostrHrp::Naddr),
        b"nrelay" => Some(NostrHrp::Nrelay),
        // `nsec` deliberately rejected — see PLAN.md §4.
        _ => None,
    }
}

/// True if `b` is a valid character preceding a nostr `@` or `nostr:` token.
/// (The left-boundary rule.) BOF is also valid; the caller passes `None` in
/// that case.
pub(crate) fn left_boundary_ok(prev: Option<u8>) -> bool {
    match prev {
        None => true,
        Some(b) => !matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'/'),
    }
}
