use unicode_normalization::UnicodeNormalization;

/// Returns `true` if `c` should be considered part of a search token.
///
/// A word character is defined as either:
/// - **Alphanumeric** — via Rust's [`char::is_alphanumeric`], which follows Unicode's
///   `Alphabetic` and `Numeric` properties rather than ASCII convention. This means
///   CJK ideographs (e.g. `日`), Arabic letters, and other script characters are all
///   included.
/// - **Combining mark** (Unicode categories Mn, Mc, Me) — characters that modify a
///   preceding base character and carry no standalone meaning. Splitting a token at a
///   combining mark would corrupt the grapheme cluster; for example, the Devanagari
///   virama, Arabic diacritics, and Thai tone marks must remain attached to their
///   base letter.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || is_combining_mark(c)
}

/// Check if a character is a Unicode combining mark (categories Mn, Mc, Me).
///
/// Rust's std doesn't expose Unicode General_Category, so we check known
/// combining mark codepoint ranges from the Unicode Character Database.
/// This covers the BMP blocks used by major world scripts.
fn is_combining_mark(c: char) -> bool {
    let cp = c as u32;
    matches!(cp,
        0x0300..=0x036F   // Combining Diacritical Marks
        | 0x0483..=0x0489 // Cyrillic combining marks
        | 0x0591..=0x05BD // Hebrew marks
        | 0x05BF
        | 0x05C1..=0x05C2
        | 0x05C4..=0x05C5
        | 0x05C7
        | 0x0610..=0x061A // Arabic marks
        | 0x064B..=0x065F // Arabic diacritics
        | 0x0670
        | 0x06D6..=0x06DC
        | 0x06DF..=0x06E4
        | 0x06E7..=0x06E8
        | 0x06EA..=0x06ED
        | 0x0711          // Syriac
        | 0x0730..=0x074A
        | 0x07A6..=0x07B0 // Thaana
        | 0x0901..=0x0903 // Devanagari
        | 0x093A..=0x094F
        | 0x0951..=0x0957
        | 0x0962..=0x0963
        | 0x0981..=0x0983 // Bengali
        | 0x09BC..=0x09CD
        | 0x09D7
        | 0x09E2..=0x09E3
        | 0x0A01..=0x0A03 // Gurmukhi
        | 0x0A3C..=0x0A4D
        | 0x0A70..=0x0A71
        | 0x0A81..=0x0A83 // Gujarati
        | 0x0ABC..=0x0ACD
        | 0x0AE2..=0x0AE3
        | 0x0B01..=0x0B03 // Oriya
        | 0x0B3C..=0x0B4D
        | 0x0B56..=0x0B57
        | 0x0B82          // Tamil
        | 0x0BBE..=0x0BCD
        | 0x0BD7
        | 0x0C00..=0x0C04 // Telugu
        | 0x0C3C..=0x0C4D
        | 0x0C55..=0x0C56
        | 0x0C81..=0x0C83 // Kannada
        | 0x0CBC..=0x0CCD
        | 0x0CD5..=0x0CD6
        | 0x0D00..=0x0D03 // Malayalam
        | 0x0D3B..=0x0D4D
        | 0x0D57
        | 0x0DCA          // Sinhala
        | 0x0DCF..=0x0DDF
        | 0x0DF2..=0x0DF3
        | 0x0E31          // Thai
        | 0x0E34..=0x0E3A
        | 0x0E47..=0x0E4E
        | 0x0EB1          // Lao
        | 0x0EB4..=0x0EBC
        | 0x0EC8..=0x0ECE
        | 0x0F18..=0x0F19 // Tibetan
        | 0x0F35
        | 0x0F37
        | 0x0F39
        | 0x0F3E..=0x0F3F
        | 0x0F71..=0x0F84
        | 0x0F86..=0x0F87
        | 0x0F8D..=0x0FBC
        | 0x0FC6
        | 0x102B..=0x103E // Myanmar
        | 0x1056..=0x1059
        | 0x105E..=0x1060
        | 0x1062..=0x1064
        | 0x1067..=0x106D
        | 0x1071..=0x1074
        | 0x1082..=0x108D
        | 0x108F
        | 0x109A..=0x109D
        | 0x1DC0..=0x1DFF // Combining Diacritical Marks Supplement
        | 0x20D0..=0x20FF // Combining Diacritical Marks for Symbols
        | 0xFE20..=0xFE2F // Combining Half Marks
    )
}

/// Normalize a string for search storage and comparison.
///
/// Applies NFC normalization followed by Unicode-aware lowercasing so that:
/// - Composed and decomposed forms (e.g. `é` as U+00E9 vs `e` + U+0301) compare equal
/// - Case folding works for all scripts where SQLite's built-in `LOWER()` is a no-op
///   (Cyrillic, Greek, Turkish, Armenian, etc.)
///
/// This function is used both when storing `content_normalized` and when building the
/// LIKE pattern from the query, ensuring both sides use the same form.
pub fn normalize_for_search(s: &str) -> String {
    s.nfc().collect::<String>().to_lowercase()
}

/// Extract non-empty search tokens from a query string.
///
/// Normalizes via [`normalize_for_search`] and splits on non-word characters.
/// Returns an empty `Vec` when the query has no tokens.
pub fn query_tokens(query: &str) -> Vec<String> {
    let normalized = normalize_for_search(query);
    normalized
        .split(|c: char| !is_word_char(c))
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

/// Compute highlight spans for a query against message content.
///
/// Matches each query token in forward order against the normalized form of
/// `content`, then maps the match positions back to **char indices** into the
/// original `content` string.
///
/// Returns a `Vec` of `[start, end]` pairs (char indices, half-open: `content[start..end]`
/// is the matched substring) in the order the tokens were found.
///
/// If any token is not found, or if the normalization mapping cannot be built
/// reliably (e.g. `to_lowercase` expands a codepoint), returns an empty `Vec`.
pub fn find_highlight_spans(content: &str, query: &str) -> Vec<[usize; 2]> {
    let tokens = query_tokens(query);
    if tokens.is_empty() {
        return Vec::new();
    }

    let normalized_content = normalize_for_search(content);

    // Build a mapping: norm_char_index → orig_char_index.
    //
    // `to_lowercase` can expand one codepoint into multiple (e.g. Turkish `İ`
    // → `i` + combining dot above). When that happens the normalized string has
    // more chars than the original, and a 1-to-1 assumption would produce wrong
    // highlight offsets. We therefore walk the original string one char at a
    // time, lowercase each char individually, count how many normalized chars it
    // produces, and record the original char index for every resulting normalized
    // char. If a single original char maps to N normalized chars, all N entries
    // point back to that original char index.
    //
    // Additionally, we record the byte offset of each normalized char so we can
    // translate byte-level match positions (from `str::find`) to char indices in
    // the original string.
    //
    // Entry layout: `(norm_byte_offset, orig_char_index)`.
    // A sentinel entry at the end holds the total byte length of normalized_content
    // paired with `content.chars().count()`, allowing open-ended end-of-string
    // lookups to succeed.
    let mut norm_byte_to_orig_char: Vec<(usize, usize)> =
        Vec::with_capacity(normalized_content.len());

    let mut norm_byte = 0usize;
    for (orig_idx, orig_char) in content.chars().enumerate() {
        // Lowercase this single char. In almost all cases this is one char; for
        // the handful of codepoints that expand (Turkish dotted-I, German ß in
        // some locales, etc.) it may be two or more.
        let lowered: String = orig_char.to_lowercase().collect();
        for lc in lowered.chars() {
            norm_byte_to_orig_char.push((norm_byte, orig_idx));
            norm_byte += lc.len_utf8();
        }
    }
    // Sentinel: end-of-string position.
    norm_byte_to_orig_char.push((norm_byte, content.chars().count()));

    // Helper closure: binary-search for the orig_char_index that corresponds to
    // a given byte offset in normalized_content. Returns `None` if the offset
    // does not land on a char boundary in the mapping (should not happen for
    // well-formed UTF-8, but we return empty spans rather than wrong spans).
    let lookup = |target_byte: usize| -> Option<usize> {
        norm_byte_to_orig_char
            .iter()
            .find(|(b, _)| *b == target_byte)
            .map(|(_, orig_idx)| *orig_idx)
    };

    let mut spans = Vec::with_capacity(tokens.len());
    let mut search_byte_start = 0usize; // byte offset into normalized_content

    for token in &tokens {
        // Find the token (already normalized) in normalized_content starting
        // from the current forward position.
        let Some(match_byte_start) = normalized_content[search_byte_start..].find(token.as_str())
        else {
            // Token not found — content doesn't actually match this query.
            return Vec::new();
        };

        let abs_byte_start = search_byte_start + match_byte_start;
        let abs_byte_end = abs_byte_start + token.len();

        // Map byte offsets back to original char indices. If either lookup fails
        // we return empty spans rather than incorrect highlight ranges.
        let (Some(char_start), Some(char_end)) = (lookup(abs_byte_start), lookup(abs_byte_end))
        else {
            return Vec::new();
        };

        spans.push([char_start, char_end]);

        // Advance past this match so the next token is searched forward only.
        search_byte_start = abs_byte_end;
    }

    spans
}

/// Convert a search query into a SQLite LIKE pattern.
///
/// Normalizes the query via [`normalize_for_search`], then splits on characters
/// that are neither alphanumeric nor combining marks, and joins tokens as
/// `%tok1%tok2%...%`.
///
/// Since tokens contain only alphanumeric chars and combining marks,
/// LIKE metacharacters (`%`, `_`) are always treated as separators.
///
/// Returns `%` (match everything) when the query has no tokens.
pub fn query_to_like_pattern(query: &str) -> String {
    let tokens = query_tokens(query);

    if tokens.is_empty() {
        return "%".to_string();
    }

    format!("%{}%", tokens.join("%"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_pattern() {
        assert_eq!(
            query_to_like_pattern("marmot standard big plans"),
            "%marmot%standard%big%plans%"
        );
    }

    #[test]
    fn single_token() {
        assert_eq!(query_to_like_pattern("hello"), "%hello%");
    }

    #[test]
    fn empty_query() {
        assert_eq!(query_to_like_pattern(""), "%");
        assert_eq!(query_to_like_pattern("   "), "%");
    }

    #[test]
    fn non_alphanumeric_separators() {
        assert_eq!(query_to_like_pattern("foo-bar.baz"), "%foo%bar%baz%");
    }

    #[test]
    fn case_lowered() {
        assert_eq!(query_to_like_pattern("Hello WORLD"), "%hello%world%");
    }

    #[test]
    fn like_metacharacters_become_separators() {
        assert_eq!(query_to_like_pattern("100%"), "%100%");
        assert_eq!(query_to_like_pattern("a_b"), "%a%b%");
        assert_eq!(query_to_like_pattern("%%inject%%"), "%inject%");
    }

    #[test]
    fn cjk_characters_preserved() {
        assert_eq!(query_to_like_pattern("日本語"), "%日本語%");
        assert_eq!(query_to_like_pattern("hello 世界"), "%hello%世界%");
        assert_eq!(
            query_to_like_pattern("marmot プロトコル"),
            "%marmot%プロトコル%"
        );
    }

    #[test]
    fn all_languages_are_parsed() {
        // CJK
        assert_eq!(query_to_like_pattern("中文搜索"), "%中文搜索%");
        assert_eq!(query_to_like_pattern("日本語"), "%日本語%");
        assert_eq!(query_to_like_pattern("한국어"), "%한국어%");
        // Cyrillic
        assert_eq!(query_to_like_pattern("привет мир"), "%привет%мир%");
        // Arabic
        assert_eq!(query_to_like_pattern("مرحبا بالعالم"), "%مرحبا%بالعالم%");
        // Hebrew
        assert_eq!(query_to_like_pattern("שלום עולם"), "%שלום%עולם%");
        // Thai
        assert_eq!(query_to_like_pattern("สวัสดี"), "%สวัสดี%");
        // Greek
        assert_eq!(query_to_like_pattern("γεια σου"), "%γεια%σου%");
        // Hindi (Devanagari) — virama (U+094D) is a combining mark, stays in token
        assert_eq!(query_to_like_pattern("नमस्ते दुनिया"), "%नमस्ते%दुनिया%");
        // Mixed scripts
        assert_eq!(
            query_to_like_pattern("hello 世界 привет"),
            "%hello%世界%привет%"
        );
    }

    #[test]
    fn highlight_single_token() {
        let spans = find_highlight_spans("Hello world", "world");
        assert_eq!(spans, vec![[6, 11]]);
    }

    #[test]
    fn highlight_multiple_tokens_forward_order() {
        let spans = find_highlight_spans("big plans for the marmot colony", "big plans");
        assert_eq!(spans, vec![[0, 3], [4, 9]]);
    }

    #[test]
    fn highlight_case_insensitive() {
        let spans = find_highlight_spans("Hello World", "hello");
        assert_eq!(spans, vec![[0, 5]]);
    }

    #[test]
    fn highlight_no_match_returns_empty() {
        let spans = find_highlight_spans("Hello world", "marmot");
        assert!(spans.is_empty());
    }

    #[test]
    fn highlight_empty_query_returns_empty() {
        let spans = find_highlight_spans("Hello world", "");
        assert!(spans.is_empty());
    }

    #[test]
    fn highlight_cjk() {
        let spans = find_highlight_spans("こんにちは世界", "世界");
        assert_eq!(spans, vec![[5, 7]]);
    }

    #[test]
    fn highlight_multiword_gap_between_tokens() {
        // tokens are "big" and "colony" — they appear with other words in between
        let spans = find_highlight_spans("big marmot colony", "big colony");
        assert_eq!(spans, vec![[0, 3], [11, 17]]);
    }

    #[test]
    fn combining_marks_preserved() {
        // Devanagari virama (U+094D)
        assert!(is_word_char('\u{094D}'));
        // Arabic fatha (U+064E)
        assert!(is_word_char('\u{064E}'));
        // Thai mai ek (U+0E48)
        assert!(is_word_char('\u{0E48}'));
        // Devanagari vowel sign aa (U+093E)
        assert!(is_word_char('\u{093E}'));
        // ASCII punctuation must NOT be word chars
        assert!(!is_word_char('%'));
        assert!(!is_word_char('_'));
        assert!(!is_word_char('-'));
        assert!(!is_word_char(' '));
    }
}
