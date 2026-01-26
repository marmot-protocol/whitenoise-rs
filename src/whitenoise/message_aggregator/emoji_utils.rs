use super::types::ProcessingError;

/// Validates and normalizes reaction content
pub fn validate_and_normalize_reaction(
    content: &str,
    normalize_emoji: bool,
) -> Result<String, ProcessingError> {
    match content {
        "+" => Ok("👍".to_string()), // Normalize to thumbs up
        "-" => Ok("👎".to_string()), // Normalize to thumbs down
        emoji if is_valid_emoji(emoji) => {
            if normalize_emoji {
                Ok(normalize_emoji_string(emoji))
            } else {
                Ok(emoji.to_string())
            }
        }
        _ => {
            tracing::warn!("Invalid reaction content: {}", content);
            Err(ProcessingError::InvalidReaction)
        }
    }
}

/// Checks if a string is a valid emoji or emoji sequence.
/// The entire string must be emoji-only (no mixed text like "😀ok").
pub fn is_valid_emoji(s: &str) -> bool {
    if s.is_empty() || s.len() > 50 {
        return false;
    }

    // Keycap sequences are a special case (contain ASCII digit/symbol)
    if is_keycap_sequence(s) {
        return true;
    }

    // All characters must be emoji-related, and at least one must be visible
    s.chars().all(is_emoji_related_char) && s.chars().any(is_visible_emoji_char)
}

/// Checks if a character is allowed in an emoji sequence.
/// Includes visible emoji plus modifiers (ZWJ, variation selectors, skin tones).
fn is_emoji_related_char(ch: char) -> bool {
    is_visible_emoji_char(ch) || is_emoji_modifier(ch)
}

/// Checks if a character is an emoji modifier (not visible on its own).
fn is_emoji_modifier(ch: char) -> bool {
    let code = ch as u32;
    matches!(code,
        0x200D |            // Zero width joiner
        0xFE00..=0xFE0F |   // Variation selectors
        0x1F3FB..=0x1F3FF   // Skin tone modifiers
    )
}

/// Checks for keycap sequences like 1️⃣.
/// Valid patterns: base + keycap, or base + variation selector + keycap.
fn is_keycap_sequence(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let is_keycap_base = |c: char| matches!(c, '0'..='9' | '#' | '*');

    match chars.as_slice() {
        [base, '\u{20E3}'] => is_keycap_base(*base),
        [base, '\u{FE0F}', '\u{20E3}'] => is_keycap_base(*base),
        _ => false,
    }
}

/// Checks if a character is a visible emoji (not a modifier)
fn is_visible_emoji_char(ch: char) -> bool {
    let code = ch as u32;

    matches!(code,
        // Main emoji blocks
        0x1F300..=0x1F5FF | // Misc Symbols and Pictographs (🌀-🗿)
        0x1F600..=0x1F64F | // Emoticons (😀-🙏)
        0x1F680..=0x1F6FF | // Transport and Map (🚀-🛿)
        0x1F900..=0x1F9FF | // Supplemental Symbols and Pictographs (🤀-🧿)
        0x1FA70..=0x1FAFF | // Symbols and Pictographs Extended-A (🩰-🫿)
        0x1F1E0..=0x1F1FF | // Regional indicators (flags)

        // Legacy symbol blocks with emoji
        0x2600..=0x26FF |   // Misc symbols (☀-⛿)
        0x2700..=0x27BF |   // Dingbats (✀-➿)

        // Additional emoji-capable characters
        0x231A..=0x231B |   // Watch and Hourglass
        0x23E9..=0x23F3 |   // Media controls (⏩-⏳)
        0x23F8..=0x23FA |   // Pause, record, etc.
        0x25AA..=0x25AB |   // Small squares
        0x25B6 |            // Play button
        0x25C0 |            // Reverse button
        0x25FB..=0x25FE |   // Medium squares
        0x2934..=0x2935 |   // Curved arrows
        0x2B05..=0x2B07 |   // Arrows
        0x2B1B..=0x2B1C |   // Large squares
        0x2B50 |            // Star
        0x2B55 |            // Circle
        0x3030 |            // Wavy dash
        0x303D |            // Part alternation mark
        0x3297 |            // Circled ideograph congratulation
        0x3299              // Circled ideograph secret
    )
}

/// Normalizes emoji by removing skin tone modifiers and variations.
/// Keycap sequences (e.g., 1️⃣) are preserved as-is for consistent rendering.
pub fn normalize_emoji_string(emoji: &str) -> String {
    // Preserve keycap sequences entirely
    if is_keycap_sequence(emoji) {
        return emoji.to_string();
    }

    if !emoji.contains('\u{1F3FB}')
        && !emoji.contains('\u{1F3FC}')
        && !emoji.contains('\u{1F3FD}')
        && !emoji.contains('\u{1F3FE}')
        && !emoji.contains('\u{1F3FF}')
        && !emoji.contains('\u{FE0F}')
    {
        return emoji.to_string();
    }

    // Remove skin tone modifiers and variation selectors
    let chars_to_remove = [
        '\u{1F3FB}',
        '\u{1F3FC}',
        '\u{1F3FD}',
        '\u{1F3FE}',
        '\u{1F3FF}',
        '\u{FE0F}',
    ];
    emoji
        .chars()
        .filter(|c| !chars_to_remove.contains(c))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_plus_minus() {
        assert_eq!(validate_and_normalize_reaction("+", true).unwrap(), "👍");
        assert_eq!(validate_and_normalize_reaction("-", true).unwrap(), "👎");
    }

    #[test]
    fn test_valid_emoji() {
        // Main emoji blocks
        assert!(is_valid_emoji("🌀")); // 0x1F300..=0x1F5FF Misc Symbols and Pictographs
        assert!(is_valid_emoji("😀")); // 0x1F600..=0x1F64F Emoticons
        assert!(is_valid_emoji("🚀")); // 0x1F680..=0x1F6FF Transport and Map
        assert!(is_valid_emoji("🤑")); // 0x1F900..=0x1F9FF Supplemental Symbols
        assert!(is_valid_emoji("🩰")); // 0x1FA70..=0x1FAFF Extended-A
        assert!(is_valid_emoji("🇺🇸")); // 0x1F1E0..=0x1F1FF Regional indicators (flags)

        // Legacy symbol blocks
        assert!(is_valid_emoji("☀")); // 0x2600..=0x26FF Misc symbols
        assert!(is_valid_emoji("✂")); // 0x2700..=0x27BF Dingbats

        // Additional emoji-capable characters
        assert!(is_valid_emoji("⌚")); // 0x231A..=0x231B Watch and Hourglass
        assert!(is_valid_emoji("⏩")); // 0x23E9..=0x23F3 Media controls
        assert!(is_valid_emoji("⏸")); // 0x23F8..=0x23FA Pause, record
        assert!(is_valid_emoji("▪")); // 0x25AA..=0x25AB Small squares
        assert!(is_valid_emoji("▶")); // 0x25B6 Play button
        assert!(is_valid_emoji("◀")); // 0x25C0 Reverse button
        assert!(is_valid_emoji("◻")); // 0x25FB..=0x25FE Medium squares
        assert!(is_valid_emoji("⤴")); // 0x2934..=0x2935 Curved arrows
        assert!(is_valid_emoji("⬅")); // 0x2B05..=0x2B07 Arrows
        assert!(is_valid_emoji("⬛")); // 0x2B1B..=0x2B1C Large squares
        assert!(is_valid_emoji("⭐")); // 0x2B50 Star
        assert!(is_valid_emoji("⭕")); // 0x2B55 Circle
        assert!(is_valid_emoji("〰")); // 0x3030 Wavy dash
        assert!(is_valid_emoji("〽")); // 0x303D Part alternation mark
        assert!(is_valid_emoji("㊗")); // 0x3297 Circled ideograph congratulation
        assert!(is_valid_emoji("㊙")); // 0x3299 Circled ideograph secret

        // Emoji with modifiers/combiners
        assert!(is_valid_emoji("❤️")); // With variation selector (0xFE0F)
        assert!(is_valid_emoji("👨‍👩‍👧")); // With zero width joiner (0x200D)
        assert!(is_valid_emoji("1️⃣")); // With combining enclosing keycap (0x20E3)
        assert!(is_valid_emoji("👋🏽")); // With skin tone modifier

        // Invalid cases
        assert!(!is_valid_emoji(""));
        assert!(!is_valid_emoji("not an emoji"));
        assert!(!is_valid_emoji("\u{200D}")); // ZWJ alone is not valid
        assert!(!is_valid_emoji("\u{FE0F}")); // Variation selector alone is not valid
        assert!(!is_valid_emoji("\u{20E3}")); // Keycap alone is not valid
    }

    #[test]
    fn test_normalize_emoji() {
        // Should remove skin tone modifiers
        assert_eq!(normalize_emoji_string("👋🏽"), "👋");
        assert_eq!(normalize_emoji_string("👍🏿"), "👍");

        // Should handle no modifiers
        assert_eq!(normalize_emoji_string("😀"), "😀");

        // Should remove variation selector
        assert_eq!(normalize_emoji_string("❤️"), "❤");

        // Should preserve ZWJ sequences (family emoji)
        assert_eq!(normalize_emoji_string("👨‍👩‍👧"), "👨‍👩‍👧");

        // Keycap sequences should be preserved entirely
        assert_eq!(normalize_emoji_string("1️⃣"), "1️⃣");
        assert_eq!(normalize_emoji_string("#️⃣"), "#️⃣");
        assert_eq!(normalize_emoji_string("*️⃣"), "*️⃣");

        // Flags should be unchanged
        assert_eq!(normalize_emoji_string("🇺🇸"), "🇺🇸");
    }

    #[test]
    fn test_invalid_reactions() {
        assert!(validate_and_normalize_reaction("invalid", true).is_err());
        assert!(validate_and_normalize_reaction("", true).is_err());
        assert!(
            validate_and_normalize_reaction(
                "way too long reaction string that exceeds limits",
                true
            )
            .is_err()
        );
    }

    #[test]
    fn test_rejects_mixed_emoji_and_text() {
        assert!(!is_valid_emoji("😀ok"));
        assert!(!is_valid_emoji("👍\n"));
        assert!(!is_valid_emoji("hello😀"));
        assert!(!is_valid_emoji("👍 thumbs up"));
        assert!(!is_valid_emoji("test"));
        assert!(!is_valid_emoji("123"));
    }

    #[test]
    fn test_keycap_sequence_strict() {
        // Valid keycap sequences
        assert!(is_valid_emoji("1️⃣")); // With variation selector
        assert!(is_valid_emoji("1⃣")); // Without variation selector
        assert!(is_valid_emoji("#️⃣"));
        assert!(is_valid_emoji("*️⃣"));
        assert!(is_valid_emoji("0️⃣"));
        assert!(is_valid_emoji("9️⃣"));

        // Invalid - extra characters not allowed
        assert!(!is_valid_emoji("1abc\u{20E3}"));
        assert!(!is_valid_emoji("1x\u{20E3}"));
        assert!(!is_valid_emoji("12\u{20E3}"));
        assert!(!is_valid_emoji("1\u{FE0F}x\u{20E3}"));
    }
}
