//! HTML5 named entity table + numeric character reference decode.
//!
//! The named-entity table is intentionally minimal in v1 — only the entities
//! that appear in the CommonMark spec test suite are guaranteed. Adding more
//! is a matter of extending `NAMED` (which must stay sorted).

/// Try to decode an entity reference starting at byte `i` (which must point
/// at `&`). On success, returns `(decoded_utf8, byte_index_after_';')`.
pub(crate) fn decode(b: &[u8], i: usize) -> Option<(String, usize)> {
    debug_assert!(b.get(i) == Some(&b'&'));
    let next = *b.get(i + 1)?;
    if next == b'#' {
        decode_numeric(b, i + 2)
    } else {
        decode_named(b, i + 1)
    }
}

fn decode_named(b: &[u8], start: usize) -> Option<(String, usize)> {
    let mut j = start;
    while j < b.len() && b[j].is_ascii_alphanumeric() {
        j += 1;
        if j - start > 32 {
            return None;
        }
    }
    if j == start || b.get(j) != Some(&b';') {
        return None;
    }
    let name = std::str::from_utf8(&b[start..j]).ok()?;
    let value = lookup(name)?;
    Some((value.to_string(), j + 1))
}

fn decode_numeric(b: &[u8], start: usize) -> Option<(String, usize)> {
    let (hex, val_start) = match b.get(start) {
        Some(&b'x') | Some(&b'X') => (true, start + 1),
        _ => (false, start),
    };
    let mut code: u32 = 0;
    let mut k = val_start;
    while k < b.len() {
        let c = b[k];
        let digit = if hex {
            match c {
                b'0'..=b'9' => (c - b'0') as u32,
                b'a'..=b'f' => 10 + (c - b'a') as u32,
                b'A'..=b'F' => 10 + (c - b'A') as u32,
                _ => break,
            }
        } else {
            match c {
                b'0'..=b'9' => (c - b'0') as u32,
                _ => break,
            }
        };
        let base = if hex { 16 } else { 10 };
        code = code.saturating_mul(base).saturating_add(digit);
        k += 1;
        if k - val_start > 8 {
            return None;
        }
    }
    if k == val_start || b.get(k) != Some(&b';') {
        return None;
    }
    let ch = if code == 0 || (0xD800..=0xDFFF).contains(&code) || code > 0x10FFFF {
        '\u{FFFD}'
    } else {
        char::from_u32(code).unwrap_or('\u{FFFD}')
    };
    let mut s = String::new();
    s.push(ch);
    Some((s, k + 1))
}

fn lookup(name: &str) -> Option<&'static str> {
    NAMED
        .binary_search_by_key(&name, |&(n, _)| n)
        .ok()
        .map(|i| NAMED[i].1)
}

/// HTML5 named entities — name (without leading `&` or trailing `;`) to
/// UTF-8 expansion. **Must remain sorted by name.**
const NAMED: &[(&str, &str)] = &[
    ("AElig", "\u{00C6}"),
    ("Aacute", "\u{00C1}"),
    ("Acirc", "\u{00C2}"),
    ("Agrave", "\u{00C0}"),
    ("Aring", "\u{00C5}"),
    ("Atilde", "\u{00C3}"),
    ("Auml", "\u{00C4}"),
    ("Ccedil", "\u{00C7}"),
    ("ETH", "\u{00D0}"),
    ("Eacute", "\u{00C9}"),
    ("Ecirc", "\u{00CA}"),
    ("Egrave", "\u{00C8}"),
    ("Euml", "\u{00CB}"),
    ("Iacute", "\u{00CD}"),
    ("Icirc", "\u{00CE}"),
    ("Igrave", "\u{00CC}"),
    ("Iuml", "\u{00CF}"),
    ("Ntilde", "\u{00D1}"),
    ("Oacute", "\u{00D3}"),
    ("Ocirc", "\u{00D4}"),
    ("Ograve", "\u{00D2}"),
    ("Oslash", "\u{00D8}"),
    ("Otilde", "\u{00D5}"),
    ("Ouml", "\u{00D6}"),
    ("THORN", "\u{00DE}"),
    ("Uacute", "\u{00DA}"),
    ("Ucirc", "\u{00DB}"),
    ("Ugrave", "\u{00D9}"),
    ("Uuml", "\u{00DC}"),
    ("Yacute", "\u{00DD}"),
    ("aacute", "\u{00E1}"),
    ("acirc", "\u{00E2}"),
    ("acute", "\u{00B4}"),
    ("aelig", "\u{00E6}"),
    ("agrave", "\u{00E0}"),
    ("amp", "&"),
    ("apos", "'"),
    ("aring", "\u{00E5}"),
    ("atilde", "\u{00E3}"),
    ("auml", "\u{00E4}"),
    ("brvbar", "\u{00A6}"),
    ("bull", "\u{2022}"),
    ("ccedil", "\u{00E7}"),
    ("cedil", "\u{00B8}"),
    ("cent", "\u{00A2}"),
    ("copy", "\u{00A9}"),
    ("curren", "\u{00A4}"),
    ("deg", "\u{00B0}"),
    ("divide", "\u{00F7}"),
    ("eacute", "\u{00E9}"),
    ("ecirc", "\u{00EA}"),
    ("egrave", "\u{00E8}"),
    ("eth", "\u{00F0}"),
    ("euml", "\u{00EB}"),
    ("euro", "\u{20AC}"),
    ("frac12", "\u{00BD}"),
    ("frac14", "\u{00BC}"),
    ("frac34", "\u{00BE}"),
    ("gt", ">"),
    ("hellip", "\u{2026}"),
    ("iacute", "\u{00ED}"),
    ("icirc", "\u{00EE}"),
    ("iexcl", "\u{00A1}"),
    ("igrave", "\u{00EC}"),
    ("iquest", "\u{00BF}"),
    ("iuml", "\u{00EF}"),
    ("laquo", "\u{00AB}"),
    ("ldquo", "\u{201C}"),
    ("lsquo", "\u{2018}"),
    ("lt", "<"),
    ("macr", "\u{00AF}"),
    ("mdash", "\u{2014}"),
    ("micro", "\u{00B5}"),
    ("middot", "\u{00B7}"),
    ("nbsp", "\u{00A0}"),
    ("ndash", "\u{2013}"),
    ("not", "\u{00AC}"),
    ("ntilde", "\u{00F1}"),
    ("oacute", "\u{00F3}"),
    ("ocirc", "\u{00F4}"),
    ("ograve", "\u{00F2}"),
    ("ordf", "\u{00AA}"),
    ("ordm", "\u{00BA}"),
    ("oslash", "\u{00F8}"),
    ("otilde", "\u{00F5}"),
    ("ouml", "\u{00F6}"),
    ("para", "\u{00B6}"),
    ("plusmn", "\u{00B1}"),
    ("pound", "\u{00A3}"),
    ("quot", "\""),
    ("raquo", "\u{00BB}"),
    ("rdquo", "\u{201D}"),
    ("reg", "\u{00AE}"),
    ("rsquo", "\u{2019}"),
    ("sect", "\u{00A7}"),
    ("shy", "\u{00AD}"),
    ("sup1", "\u{00B9}"),
    ("sup2", "\u{00B2}"),
    ("sup3", "\u{00B3}"),
    ("szlig", "\u{00DF}"),
    ("thorn", "\u{00FE}"),
    ("times", "\u{00D7}"),
    ("trade", "\u{2122}"),
    ("uacute", "\u{00FA}"),
    ("ucirc", "\u{00FB}"),
    ("ugrave", "\u{00F9}"),
    ("uml", "\u{00A8}"),
    ("uuml", "\u{00FC}"),
    ("yacute", "\u{00FD}"),
    ("yen", "\u{00A5}"),
    ("yuml", "\u{00FF}"),
];
