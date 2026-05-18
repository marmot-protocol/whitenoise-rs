//! Pass 2: inline tokenization.
//!
//! Walks the block tree and replaces each leaf block's single raw
//! `Inline::Text` with a tokenized `Vec<Inline>`. Phase 5 covers text,
//! backslash escapes, code spans, inline math, entity references, and
//! hard/soft line breaks. Emphasis, links, autolinks, raw HTML, and nostr
//! nodes are added in later phases.

use std::collections::HashMap;

use crate::ast::{
    AutolinkKind, Block, Document, Inline, ListItem, NostrEntity, NostrHrp, TableCell,
};
use crate::block::LinkRef;
use crate::entity;
use crate::nostr;
use crate::scanner;

/// ASCII bytes that have a dedicated arm in `tokenize`'s dispatch match.
/// Keep in sync with the explicit match arms — used as the exit condition
/// for the plain-byte fast-scan in the `_` wildcard arm.
///
/// `h`, `m`, `t`, `w` are tripwires for bare-URL schemes (`http(s)://`,
/// `mailto:`, `tel:`, `whitenoise:`); the bulk-scan rescue below keeps the
/// fast path for the overwhelmingly common case of ordinary prose containing
/// those letters.
const INLINE_SPECIAL: [bool; 128] = {
    let mut t = [false; 128];
    let chars = b"\\`$&[!*_~]<@nhmtw\n";
    let mut k = 0;
    while k < chars.len() {
        t[chars[k] as usize] = true;
        k += 1;
    }
    t
};

pub(crate) fn parse_inlines(blocks: Vec<Block>, refs: &HashMap<String, LinkRef>) -> Document {
    Document {
        blocks: walk_blocks(blocks, refs),
    }
}

fn walk_blocks(blocks: Vec<Block>, refs: &HashMap<String, LinkRef>) -> Vec<Block> {
    blocks.into_iter().map(|b| walk(b, refs)).collect()
}

// `refs` is threaded through here so Phase 8 (links) can resolve reference
// labels; emphasis (Phase 9) etc. don't use it.
fn walk(block: Block, refs: &HashMap<String, LinkRef>) -> Block {
    match block {
        Block::Paragraph { inlines } => Block::Paragraph {
            inlines: tokenize(&extract_raw(inlines), refs),
        },
        Block::Heading { level, inlines } => Block::Heading {
            level,
            inlines: tokenize(&extract_raw(inlines), refs),
        },
        Block::BlockQuote { blocks } => Block::BlockQuote {
            blocks: walk_blocks(blocks, refs),
        },
        Block::List { kind, tight, items } => Block::List {
            kind,
            tight,
            items: items
                .into_iter()
                .map(|item| ListItem {
                    blocks: walk_blocks(item.blocks, refs),
                    checked: item.checked,
                })
                .collect(),
        },
        Block::Table {
            alignments,
            header,
            rows,
        } => Block::Table {
            alignments,
            header: header
                .into_iter()
                .map(|cell| TableCell {
                    inlines: tokenize(&extract_raw(cell.inlines), refs),
                })
                .collect(),
            rows: rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|cell| TableCell {
                            inlines: tokenize(&extract_raw(cell.inlines), refs),
                        })
                        .collect()
                })
                .collect(),
        },
        // Code blocks, HTML blocks, math blocks, thematic breaks don't
        // tokenize here.
        b => b,
    }
}

fn extract_raw(inlines: Vec<Inline>) -> String {
    match inlines.into_iter().next() {
        Some(Inline::Text(s)) => s,
        _ => String::new(),
    }
}

#[derive(Debug, Clone, Copy)]
struct BracketDelim {
    /// `b'['` link opener, `b'!'` image opener, `b'*'`/`b'_'`/`b'~'`
    /// emphasis-or-strikethrough run.
    kind: u8,
    /// Index in `out` of the Text node holding this delimiter's
    /// characters. For brackets: a placeholder Text("[" / "!["). For
    /// runs: a Text containing the run's chars.
    out_pos: usize,
    /// Index in `bytes` of the leading character (for brackets, the `[`).
    input_pos: usize,
    /// True while the delimiter can still participate in matching.
    active: bool,
    /// For runs: original run length (frozen for rule-of-three).
    orig_len: usize,
    /// For runs: remaining length.
    len: usize,
    /// For runs: can this run open / close emphasis?
    can_open: bool,
    can_close: bool,
}

impl BracketDelim {
    fn bracket(kind: u8, out_pos: usize, input_pos: usize) -> Self {
        Self {
            kind,
            out_pos,
            input_pos,
            active: true,
            orig_len: 0,
            len: 0,
            can_open: false,
            can_close: false,
        }
    }
}

/// Tokenize the raw paragraph/heading text. First-match-wins.
pub(crate) fn tokenize(raw: &str, refs: &HashMap<String, LinkRef>) -> Vec<Inline> {
    let bytes = raw.as_bytes();
    let mut out: Vec<Inline> = Vec::new();
    // Entity decoding only ever shrinks bytes (e.g. `&amp;` → `&`), so
    // `raw.len()` is a safe upper bound. Pre-sizing avoids the doubling
    // reallocation chain (4→8→16→…) as the first text run accumulates.
    let mut buf = String::with_capacity(raw.len());
    let mut delims: Vec<BracketDelim> = Vec::new();
    let mut i = 0;

    // Try a "consume an Inline or fall back to a literal byte" recognizer.
    // Used for the recognizers whose only failure mode is "didn't match —
    // treat the lead byte as text". Keeps the dispatch table readable.
    macro_rules! try_or_literal {
        ($lit:literal, $try:expr, $wrap:expr) => {
            match $try {
                Some((v, end)) => {
                    flush_text(&mut out, &mut buf, &delims);
                    out.push($wrap(v));
                    i = end;
                }
                None => {
                    buf.push($lit);
                    i += 1;
                }
            }
        };
    }

    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\\' => {
                if i + 1 < bytes.len() {
                    let next = bytes[i + 1];
                    if next == b'\n' {
                        flush_text(&mut out, &mut buf, &delims);
                        out.push(Inline::HardBreak);
                        i += 2;
                        // Skip leading whitespace on next line (paragraph
                        // continuations should already have it stripped,
                        // but be defensive).
                        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                            i += 1;
                        }
                        continue;
                    }
                    if scanner::is_ascii_punct(next) {
                        buf.push(next as char);
                        i += 2;
                        continue;
                    }
                }
                buf.push('\\');
                i += 1;
            }
            b'`' => try_or_literal!('`', try_code_span(bytes, i), Inline::Code),
            b'$' => try_or_literal!('$', try_inline_math(bytes, i), Inline::Math),
            b'&' => match entity::decode(bytes, i) {
                Some((decoded, end)) => {
                    // Entities decode straight into the text buffer (no
                    // flush) so adjacent literal bytes stay coalesced.
                    buf.push_str(&decoded);
                    i = end;
                }
                None => {
                    buf.push('&');
                    i += 1;
                }
            },
            b'[' => {
                flush_text(&mut out, &mut buf, &delims);
                let out_pos = out.len();
                out.push(Inline::Text("[".to_string()));
                delims.push(BracketDelim::bracket(b'[', out_pos, i));
                i += 1;
            }
            b'!' if bytes.get(i + 1) == Some(&b'[') => {
                flush_text(&mut out, &mut buf, &delims);
                let out_pos = out.len();
                out.push(Inline::Text("![".to_string()));
                delims.push(BracketDelim::bracket(b'!', out_pos, i + 1));
                i += 2;
            }
            b'*' | b'_' | b'~' => {
                flush_text(&mut out, &mut buf, &delims);
                let (run_len, can_open, can_close) = classify_delim_run(bytes, i, c);
                let out_pos = out.len();
                out.push(Inline::Text(
                    std::str::from_utf8(&bytes[i..i + run_len])
                        .unwrap()
                        .to_string(),
                ));
                delims.push(BracketDelim {
                    kind: c,
                    out_pos,
                    input_pos: i,
                    active: true,
                    orig_len: run_len,
                    len: run_len,
                    can_open,
                    can_close,
                });
                i += run_len;
            }
            b']' => {
                flush_text(&mut out, &mut buf, &delims);
                if let Some(end) = try_close_bracket(bytes, i, &mut out, &mut delims, refs) {
                    i = end;
                } else {
                    buf.push(']');
                    i += 1;
                }
            }
            b'<' => {
                if let Some((url, end)) = try_uri_autolink(bytes, i) {
                    flush_text(&mut out, &mut buf, &delims);
                    out.push(Inline::Autolink {
                        url,
                        kind: AutolinkKind::Uri,
                    });
                    i = end;
                } else if let Some((url, end)) = try_email_autolink(bytes, i) {
                    flush_text(&mut out, &mut buf, &delims);
                    out.push(Inline::Autolink {
                        url,
                        kind: AutolinkKind::Email,
                    });
                    i = end;
                } else {
                    // Unrecognized `<` — emit as literal text. HTML is NOT
                    // parsed; tag-like sequences pass through unchanged.
                    buf.push('<');
                    i += 1;
                }
            }
            b'@' => try_or_literal!('@', try_nostr_mention(bytes, i), Inline::NostrMention),
            b'n' => {
                if let Some((entity, end)) = try_nostr_uri(bytes, i) {
                    flush_text(&mut out, &mut buf, &delims);
                    out.push(Inline::NostrUri(entity));
                    i = end;
                } else if let Some((entity, end)) = try_nostr_bare_mention(bytes, i) {
                    flush_text(&mut out, &mut buf, &delims);
                    out.push(Inline::NostrMention(entity));
                    i = end;
                } else {
                    buf.push('n');
                    i += 1;
                }
            }
            b'h' | b'm' | b't' | b'w' => {
                if let Some((url, end)) = try_bare_url(bytes, i) {
                    flush_text(&mut out, &mut buf, &delims);
                    out.push(Inline::Autolink {
                        url,
                        kind: AutolinkKind::Uri,
                    });
                    i = end;
                } else {
                    buf.push(c as char);
                    i += 1;
                }
            }
            b'\n' => {
                let trailing = trailing_space_count(&buf);
                let hard = trailing >= 2;
                // Strip any trailing spaces/tabs from the buffer (they're
                // either the hard-break signal or just paragraph-internal
                // trailing whitespace, neither of which we want in the
                // emitted text).
                while buf.ends_with(' ') || buf.ends_with('\t') {
                    buf.pop();
                }
                flush_text(&mut out, &mut buf, &delims);
                if hard {
                    out.push(Inline::HardBreak);
                } else {
                    out.push(Inline::SoftBreak);
                }
                i += 1;
                // Skip leading whitespace on the next line (paragraph
                // continuations should already have it stripped).
                while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                    i += 1;
                }
            }
            _ => {
                // Fast path: c is ASCII and not in any special arm above
                // (otherwise the match would have caught it). Bulk-scan
                // forward over the run of contiguous ASCII non-special bytes
                // and append them in one push_str, instead of paying
                // per-byte from_utf8 + push_str + dispatch overhead.
                if c < 0x80 {
                    let start = i;
                    i += 1;
                    while i < bytes.len() {
                        let cc = bytes[i];
                        if cc >= 0x80 {
                            break;
                        }
                        if INLINE_SPECIAL[cc as usize] {
                            // `n` is in INLINE_SPECIAL only as a tripwire for
                            // the `nostr:` URI scheme and bare `npub1…`
                            // mentions; the vast majority of `n` bytes in
                            // prose aren't either. Replicate the cheap
                            // discriminator here so those bytes stay in the
                            // bulk run instead of bouncing out to dispatch
                            // and back.
                            if cc == b'n'
                                && bytes.get(i + 1..i + 6) != Some(b"ostr:")
                                && bytes.get(i + 1..i + 5) != Some(b"pub1")
                            {
                                i += 1;
                                continue;
                            }
                            // Same rescue for `h`/`m`/`t`/`w` — they're
                            // tripwires for bare-URL schemes and most occur
                            // mid-word in prose.
                            if matches!(cc, b'h' | b'm' | b't' | b'w')
                                && !looks_like_bare_url_start(bytes, i)
                            {
                                i += 1;
                                continue;
                            }
                            break;
                        }
                        i += 1;
                    }
                    buf.push_str(std::str::from_utf8(&bytes[start..i]).unwrap());
                } else {
                    let len = utf8_char_len(c);
                    let end = (i + len).min(bytes.len());
                    buf.push_str(std::str::from_utf8(&bytes[i..end]).unwrap_or(""));
                    i = end;
                }
            }
        }
    }
    flush_text(&mut out, &mut buf, &delims);
    // Pair emphasis / strong / strikethrough delim runs at the top level.
    process_emphasis(&mut out, &mut delims, 0);
    // Any remaining unclosed bracket openers are orphans — their `[` /
    // `![` placeholder Text nodes stay literal, but they no longer block
    // coalescing.
    coalesce_text_runs(&mut out);
    out
}

fn coalesce_text_runs(items: &mut Vec<Inline>) {
    // O(n) in-place compaction: `Vec::remove(i+1)` shifts the tail on every
    // merge, so the prior loop was O(n × m) for m adjacent-Text merges (which
    // emphasis pairing produces in bulk after dropping placeholder Texts).
    // `dedup_by` walks the slice once with a read/write cursor; the closure's
    // two `&mut Inline` come from distinct slice positions, so extracting two
    // disjoint `&mut String` and folding `later` into `earlier` is sound.
    items.dedup_by(|later, earlier| {
        let Inline::Text(later_s) = later else {
            return false;
        };
        let Inline::Text(earlier_s) = earlier else {
            return false;
        };
        earlier_s.push_str(later_s);
        true
    });
}

/// Flush the text buffer into `out`. Coalesces with the previous Text node
/// **unless** that node is the placeholder for the most recent open bracket
/// delimiter — in that case we push a fresh Text so absorb_link can later
/// drain the link's children separately from the placeholder.
fn flush_text(out: &mut Vec<Inline>, buf: &mut String, delims: &[BracketDelim]) {
    if buf.is_empty() {
        return;
    }
    let last_idx = out.len().wrapping_sub(1);
    let blocked = delims.last().is_some_and(|d| d.out_pos == last_idx);
    if !blocked && let Some(Inline::Text(prev)) = out.last_mut() {
        prev.push_str(buf);
        buf.clear();
        return;
    }
    // `mem::take` would reset `buf` to capacity 0, forcing the next text run
    // to grow from scratch. Preserve the existing capacity so a paragraph
    // with N inline delimiters costs ~1 alloc total instead of ~N growth
    // chains.
    let cap = buf.capacity();
    out.push(Inline::Text(std::mem::replace(
        buf,
        String::with_capacity(cap),
    )));
}

fn trailing_space_count(buf: &str) -> usize {
    buf.bytes().rev().take_while(|&b| b == b' ').count()
}

/// Try to consume a code span starting at byte `i` (pointing at `` ` ``).
/// Returns the (normalized content, end-index-just-past-closing-run).
fn try_code_span(bytes: &[u8], i: usize) -> Option<(String, usize)> {
    let mut j = i;
    while j < bytes.len() && bytes[j] == b'`' {
        j += 1;
    }
    let run_len = j - i;
    let mut k = j;
    while k < bytes.len() {
        if bytes[k] == b'`' {
            let mut m = k;
            while m < bytes.len() && bytes[m] == b'`' {
                m += 1;
            }
            if m - k == run_len {
                let content = normalize_code_span(&bytes[j..k]);
                return Some((content, m));
            }
            k = m;
        } else {
            k += 1;
        }
    }
    None
}

fn normalize_code_span(b: &[u8]) -> String {
    // 1. Replace newlines with single spaces. `scanner::lines` already
    //    normalized `\r\n` / bare `\r` to `\n` before the block pass, so
    //    only `\n` can appear here.
    let mut s = String::with_capacity(b.len());
    let mut prev_was_nl = false;
    let src = std::str::from_utf8(b).unwrap_or("");
    for c in src.chars() {
        if c == '\n' {
            if !prev_was_nl {
                s.push(' ');
            }
            prev_was_nl = true;
        } else {
            s.push(c);
            prev_was_nl = false;
        }
    }
    // 2. If the result begins AND ends with a space (and isn't all spaces),
    //    strip one space from each end.
    if s.len() >= 2 && s.starts_with(' ') && s.ends_with(' ') && s.bytes().any(|b| b != b' ') {
        s = s[1..s.len() - 1].to_string();
    }
    s
}

/// Try to consume a single-`$` inline math span. Boundary rule: opening `$`
/// must not be followed by whitespace; closing `$` must not be preceded by
/// whitespace.
fn try_inline_math(bytes: &[u8], i: usize) -> Option<(String, usize)> {
    debug_assert_eq!(bytes[i], b'$');
    // Reject `$$` (block math) and dollar runs of 2+.
    if bytes.get(i + 1) == Some(&b'$') {
        return None;
    }
    let next = *bytes.get(i + 1)?;
    if next == b' ' || next == b'\t' || next == b'\n' {
        return None;
    }
    // Scan for a closing `$` not preceded by whitespace, not preceded by
    // backslash escape.
    let mut k = i + 1;
    while k < bytes.len() {
        let c = bytes[k];
        if c == b'\\' && k + 1 < bytes.len() {
            // Escape inside math is opaque, but we still need to skip an
            // escaped `$` for boundary scanning.
            k += 2;
            continue;
        }
        if c == b'$' {
            // Check it isn't `$$`.
            if bytes.get(k + 1) == Some(&b'$') {
                k += 1;
                continue;
            }
            let prev = bytes[k - 1];
            if prev == b' ' || prev == b'\t' || prev == b'\n' {
                // Not a valid close; keep searching.
                k += 1;
                continue;
            }
            let content = std::str::from_utf8(&bytes[i + 1..k]).ok()?.to_string();
            return Some((content, k + 1));
        }
        k += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Links + images (delimiter-stack closer)
// ---------------------------------------------------------------------------

/// Handle a `]` byte at position `i`. Returns the byte index past the close
/// on success, or `None` if the `]` should be emitted as literal text.
fn try_close_bracket(
    bytes: &[u8],
    i: usize,
    out: &mut Vec<Inline>,
    delims: &mut Vec<BracketDelim>,
    refs: &HashMap<String, LinkRef>,
) -> Option<usize> {
    // Find the most recent active opener (`[` or `![`).
    let opener_idx = delims
        .iter()
        .rposition(|d| d.kind == b'[' || d.kind == b'!')?;
    let opener = delims[opener_idx];

    if !opener.active {
        // Inactive opener: drop it and emit literal `]`.
        delims.remove(opener_idx);
        return None;
    }

    // 1. Inline link: `](dest "title")`
    if bytes.get(i + 1) == Some(&b'(')
        && let Some((dest, title, end)) = parse_inline_link_suffix(bytes, i + 1)
    {
        absorb_link(out, delims, opener_idx, dest, title);
        return Some(end);
    }

    // 2. Full reference link: `][label]`
    if bytes.get(i + 1) == Some(&b'[')
        && let Some((label_raw, end)) = parse_ref_label(bytes, i + 1)
    {
        if !label_raw.trim().is_empty()
            && let Some(def) = refs.get(&crate::block::normalize_label(&label_raw))
        {
            let (dest, title) = (def.dest.clone(), def.title.clone());
            absorb_link(out, delims, opener_idx, dest, title);
            return Some(end);
        }
        // Empty `[]` after `]` — collapsed reference: use the link text as
        // the label.
        if label_raw.is_empty() {
            let label = label_text(bytes, opener.input_pos + 1, i);
            if let Some(def) = refs.get(&crate::block::normalize_label(&label)) {
                let (dest, title) = (def.dest.clone(), def.title.clone());
                absorb_link(out, delims, opener_idx, dest, title);
                return Some(end);
            }
        }
    }

    // 3. Shortcut reference: just `]`. Use the link text as label.
    let label = label_text(bytes, opener.input_pos + 1, i);
    if !label.is_empty()
        && let Some(def) = refs.get(&crate::block::normalize_label(&label))
    {
        let (dest, title) = (def.dest.clone(), def.title.clone());
        absorb_link(out, delims, opener_idx, dest, title);
        return Some(i + 1);
    }

    // No match — drop the opener and emit literal `]`.
    delims.remove(opener_idx);
    None
}

/// Parse `(dest "title")` starting at the `(`.
fn parse_inline_link_suffix(b: &[u8], i: usize) -> Option<(String, Option<String>, usize)> {
    debug_assert_eq!(b.get(i), Some(&b'('));
    let mut j = i + 1;
    j = skip_ws_and_one_nl(b, j);
    let (dest, j2) = crate::block::parse_link_destination(b, j).unwrap_or((String::new(), j));
    j = j2;
    j = skip_ws_and_one_nl(b, j);
    // Optional title.
    let mut title = None;
    if let Some(&c) = b.get(j)
        && matches!(c, b'"' | b'\'' | b'(')
        && let Some((t, ne)) = crate::block::parse_link_title(b, j)
    {
        title = Some(t);
        j = ne;
        j = skip_ws_and_one_nl(b, j);
    }
    if b.get(j) != Some(&b')') {
        return None;
    }
    Some((dest, title, j + 1))
}

fn skip_ws_and_one_nl(b: &[u8], mut j: usize) -> usize {
    while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
        j += 1;
    }
    if j < b.len() && b[j] == b'\n' {
        j += 1;
        while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
            j += 1;
        }
    }
    j
}

/// Parse `[label]` starting at `[`. Returns (raw label, end-after-`]`).
fn parse_ref_label(b: &[u8], i: usize) -> Option<(String, usize)> {
    debug_assert_eq!(b.get(i), Some(&b'['));
    let mut j = i + 1;
    let start = j;
    while j < b.len() && b[j] != b']' {
        if b[j] == b'\\' && j + 1 < b.len() {
            j += 2;
            continue;
        }
        if b[j] == b'[' {
            return None;
        }
        j += 1;
        if j - start > 999 {
            return None;
        }
    }
    if b.get(j) != Some(&b']') {
        return None;
    }
    let label = std::str::from_utf8(&b[start..j]).ok()?.to_string();
    Some((label, j + 1))
}

/// Best-effort label text for shortcut/collapsed reference resolution: the
/// raw bytes between `[` and `]` from the source. Backslash-escaped `]`
/// inside is allowed.
fn label_text(b: &[u8], start: usize, end: usize) -> String {
    let mut out = String::new();
    let src = std::str::from_utf8(&b[start..end]).unwrap_or("");
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(&next) = chars.peek()
        {
            out.push(next);
            chars.next();
            continue;
        }
        out.push(c);
    }
    out
}

/// Replace the placeholder + intermediate inlines with a single Link/Image
/// inline; deactivate earlier `[` openers (no nested links).
fn absorb_link(
    out: &mut Vec<Inline>,
    delims: &mut Vec<BracketDelim>,
    opener_idx: usize,
    dest: String,
    title: Option<String>,
) {
    // First, pair any emphasis runs strictly inside the link before
    // wrapping — emphasis inside link text DOES get processed (per spec).
    process_emphasis(out, delims, opener_idx + 1);
    let opener = delims[opener_idx];
    let kind_is_image = opener.kind == b'!';
    // Children = items after the placeholder.
    let children: Vec<Inline> = out.drain(opener.out_pos + 1..).collect();
    // Drop the placeholder Text("[" / "![").
    out.pop();
    if kind_is_image {
        out.push(Inline::Image {
            dest,
            title,
            alt: children,
        });
    } else {
        out.push(Inline::Link {
            dest,
            title,
            children,
        });
    }
    // Pop all delimiters from opener_idx onward (any inner unclosed openers
    // are now part of the link's children).
    delims.truncate(opener_idx);
    // Prevent nested links: deactivate any earlier `[` opener.
    if !kind_is_image {
        for d in delims.iter_mut() {
            if d.kind == b'[' {
                d.active = false;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Autolinks + raw HTML
// ---------------------------------------------------------------------------

/// Schemes recognized as bare (unbracketed) URLs.
///
/// **Order matters:** `whitenoise-staging://` must come before `whitenoise://`
/// because the `find(..)` lookup is first-match-wins, and the latter is a
/// strict prefix of the former. With them in the wrong order
/// `whitenoise-staging://x` would parse as `whitenoise:` + literal
/// `-staging://x`.
const BARE_URL_SCHEMES: &[&[u8]] = &[
    b"https://",
    b"http://",
    b"mailto:",
    b"tel:",
    b"whitenoise-staging://",
    b"whitenoise://",
];

/// True if the bytes starting at `i` begin with one of the recognized bare-URL
/// scheme prefixes. Used by the bulk-scan tripwire to keep ordinary text
/// (every `h`/`m`/`t`/`w` byte in prose) on the fast path.
fn looks_like_bare_url_start(bytes: &[u8], i: usize) -> bool {
    BARE_URL_SCHEMES
        .iter()
        .any(|s| bytes.get(i..i + s.len()) == Some(s))
}

/// Try to consume a bare URL (no surrounding `<>`) starting at `i`. Matches
/// `https://`, `http://`, `mailto:`, `tel:`, or `whitenoise:` followed by a
/// non-empty run of non-whitespace, non-`<` bytes. Trailing punctuation is
/// stripped per the GFM extended-autolink rules (`.,;:!?*_~` always; `)` only
/// when it would unbalance the URL body).
fn try_bare_url(bytes: &[u8], i: usize) -> Option<(String, usize)> {
    let prev = if i == 0 { None } else { Some(bytes[i - 1]) };
    if !nostr::left_boundary_ok(prev) {
        return None;
    }
    // If the previous byte is `<`, the angle-bracket autolink form was tried
    // and rejected (e.g. body contained a space). Per existing convention,
    // the whole `<…>` token stays literal — don't rescue part of it as a
    // bare URL.
    if prev == Some(b'<') {
        return None;
    }
    let scheme = BARE_URL_SCHEMES
        .iter()
        .find(|s| bytes.get(i..i + s.len()) == Some(**s))?;
    let body_start = i + scheme.len();
    let mut j = body_start;
    while j < bytes.len() {
        let c = bytes[j];
        if c == b' '
            || c == b'\t'
            || c == b'\n'
            || c == b'\r'
            || c == b'<'
            || c == b'>'
            || c < 0x20
            || c == 0x7f
        {
            break;
        }
        j += 1;
    }
    if j == body_start {
        return None;
    }
    j = trim_trailing_punct(bytes, body_start, j);
    if j == body_start {
        return None;
    }
    let url = std::str::from_utf8(&bytes[i..j]).ok()?.to_string();
    Some((url, j))
}

/// Trim trailing punctuation from a bare-URL body per GFM:
/// - Always strip `.`, `,`, `;`, `:`, `!`, `?`, `*`, `_`, `~`.
/// - Strip `)` only when the body has more `)` than `(` (so balanced parens
///   inside the URL — e.g. Wikipedia disambiguation links — are kept).
fn trim_trailing_punct(bytes: &[u8], start: usize, mut end: usize) -> usize {
    while end > start {
        let c = bytes[end - 1];
        match c {
            b'.' | b',' | b';' | b':' | b'!' | b'?' | b'*' | b'_' | b'~' => end -= 1,
            b')' => {
                let opens = bytes[start..end].iter().filter(|&&b| b == b'(').count();
                let closes = bytes[start..end].iter().filter(|&&b| b == b')').count();
                if closes > opens {
                    end -= 1;
                } else {
                    return end;
                }
            }
            _ => return end,
        }
    }
    end
}

/// `<scheme:body>` — scheme is `[A-Za-z][A-Za-z0-9+.-]{1,31}`, body has no
/// `<`, `>`, control chars, or whitespace. Returns the URL (without the
/// surrounding `<>`).
fn try_uri_autolink(bytes: &[u8], i: usize) -> Option<(String, usize)> {
    debug_assert_eq!(bytes[i], b'<');
    let mut j = i + 1;
    if j >= bytes.len() || !bytes[j].is_ascii_alphabetic() {
        return None;
    }
    let scheme_start = j;
    j += 1;
    while j < bytes.len()
        && (bytes[j].is_ascii_alphanumeric()
            || bytes[j] == b'+'
            || bytes[j] == b'.'
            || bytes[j] == b'-')
        && (j - scheme_start) < 32
    {
        j += 1;
    }
    let scheme_len = j - scheme_start;
    if !(2..=32).contains(&scheme_len) {
        return None;
    }
    if bytes.get(j) != Some(&b':') {
        return None;
    }
    j += 1;
    while j < bytes.len() {
        let c = bytes[j];
        if c == b'>' {
            break;
        }
        if c == b'<' || c == b' ' || c == b'\t' || c == b'\n' || c < 0x20 || c == 0x7f {
            return None;
        }
        j += 1;
    }
    if bytes.get(j) != Some(&b'>') {
        return None;
    }
    let url = std::str::from_utf8(&bytes[scheme_start..j])
        .ok()?
        .to_string();
    Some((url, j + 1))
}

/// `<email@host>` per CommonMark §6.4.
fn try_email_autolink(bytes: &[u8], i: usize) -> Option<(String, usize)> {
    debug_assert_eq!(bytes[i], b'<');
    let mut j = i + 1;
    let local_start = j;
    while j < bytes.len() && is_email_local_char(bytes[j]) {
        j += 1;
    }
    if j == local_start {
        return None;
    }
    if bytes.get(j) != Some(&b'@') {
        return None;
    }
    j += 1;
    // host: label ('.' label)*; label is alnum + optional internal hyphens,
    // 1..=63 chars.
    loop {
        let label_start = j;
        if j >= bytes.len() || !bytes[j].is_ascii_alphanumeric() {
            return None;
        }
        j += 1;
        while j < bytes.len()
            && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'-')
            && (j - label_start) < 63
        {
            j += 1;
        }
        // Last char must not be `-`.
        if bytes[j - 1] == b'-' {
            return None;
        }
        if bytes.get(j) == Some(&b'.') {
            j += 1;
            continue;
        }
        break;
    }
    if bytes.get(j) != Some(&b'>') {
        return None;
    }
    let url = std::str::from_utf8(&bytes[local_start..j])
        .ok()?
        .to_string();
    Some((url, j + 1))
}

fn is_email_local_char(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'.' | b'!'
                | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'/'
                | b'='
                | b'?'
                | b'^'
                | b'_'
                | b'`'
                | b'{'
                | b'|'
                | b'}'
                | b'~'
                | b'-'
        )
}

/// Try to consume an `@npub1…` bare-mention starting at byte `i` (which
/// must point at `@`). Returns the parsed entity and the byte index just
/// past the bech32. Bare-mention only accepts `npub`; other HRPs require
/// the explicit `nostr:` prefix.
fn try_nostr_mention(bytes: &[u8], i: usize) -> Option<(NostrEntity, usize)> {
    debug_assert_eq!(bytes[i], b'@');
    let prev = if i == 0 { None } else { Some(bytes[i - 1]) };
    if !nostr::left_boundary_ok(prev) {
        return None;
    }
    let (hrp, end) = nostr::classify_bech32(bytes, i + 1)?;
    if hrp != NostrHrp::Npub {
        return None;
    }
    let bech32 = std::str::from_utf8(&bytes[i + 1..end]).ok()?.to_string();
    Some((NostrEntity { hrp, bech32 }, end))
}

/// Try to consume a bare `npub1…` mention starting at byte `i` (which must
/// point at `n`). Same shape rules as `try_nostr_mention` but without the
/// `@` prefix. Restricted to the `npub` HRP — other HRPs require the
/// explicit `@` or `nostr:` prefix to avoid false-positive matches on
/// running text that happens to start with `note1…` / `nevent1…`.
fn try_nostr_bare_mention(bytes: &[u8], i: usize) -> Option<(NostrEntity, usize)> {
    debug_assert_eq!(bytes[i], b'n');
    let prev = if i == 0 { None } else { Some(bytes[i - 1]) };
    if !nostr::left_boundary_ok(prev) {
        return None;
    }
    // Don't let the bare form rescue a prefix that already declined: if the
    // previous byte is `@` or `:` then `@npub1…` or `nostr:npub1…` was tried
    // and rejected (left boundary on the prefix), so the trailing bech32
    // must stay literal too.
    if matches!(prev, Some(b'@') | Some(b':')) {
        return None;
    }
    let (hrp, end) = nostr::classify_bech32(bytes, i)?;
    if hrp != NostrHrp::Npub {
        return None;
    }
    let bech32 = std::str::from_utf8(&bytes[i..end]).ok()?.to_string();
    Some((NostrEntity { hrp, bech32 }, end))
}

/// Try to consume a `nostr:<hrp>1…` URI starting at byte `i` (which must
/// point at `n`). Returns the parsed entity and the byte index just past
/// the bech32.
fn try_nostr_uri(bytes: &[u8], i: usize) -> Option<(NostrEntity, usize)> {
    debug_assert_eq!(bytes[i], b'n');
    if bytes.get(i + 1..i + 6) != Some(b"ostr:") {
        return None;
    }
    let prev = if i == 0 { None } else { Some(bytes[i - 1]) };
    if !nostr::left_boundary_ok(prev) {
        return None;
    }
    let (hrp, end) = nostr::classify_bech32(bytes, i + 6)?;
    let bech32 = std::str::from_utf8(&bytes[i + 6..end]).ok()?.to_string();
    Some((NostrEntity { hrp, bech32 }, end))
}

// ---------------------------------------------------------------------------
// Emphasis / strong / strikethrough — the spec's process_emphasis algorithm.
// ---------------------------------------------------------------------------

fn classify_delim_run(bytes: &[u8], i: usize, ch: u8) -> (usize, bool, bool) {
    let mut j = i;
    while j < bytes.len() && bytes[j] == ch {
        j += 1;
    }
    let len = j - i;
    let prev = if i == 0 { None } else { Some(bytes[i - 1]) };
    let next = bytes.get(j).copied();
    let prev_is_ws = prev.is_none_or(is_ascii_ws_for_flank);
    let next_is_ws = next.is_none_or(is_ascii_ws_for_flank);
    let prev_is_punct = prev.is_some_and(scanner::is_ascii_punct);
    let next_is_punct = next.is_some_and(scanner::is_ascii_punct);
    let left_flanking = !next_is_ws && (!next_is_punct || prev_is_ws || prev_is_punct);
    let right_flanking = !prev_is_ws && (!prev_is_punct || next_is_ws || next_is_punct);
    let (can_open, can_close) = match ch {
        b'_' => (
            left_flanking && (!right_flanking || prev_is_punct),
            right_flanking && (!left_flanking || next_is_punct),
        ),
        // `*` and `~`: flanking is enough.
        _ => (left_flanking, right_flanking),
    };
    (len, can_open, can_close)
}

fn is_ascii_ws_for_flank(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C)
}

/// Walk the delimiter stack and pair up `*` / `_` / `~` runs, replacing the
/// matched portion with `Emph` / `Strong` / `Strikethrough` inlines.
fn process_emphasis(out: &mut Vec<Inline>, delims: &mut Vec<BracketDelim>, stack_bottom: usize) {
    // For each (delim_char, can_open, mod3) we track an "openers_bottom"
    // index — earlier than this we won't search for an opener again.
    use std::collections::HashMap;
    let mut openers_bottom: HashMap<(u8, bool, usize), usize> = HashMap::new();

    let mut closer_idx = stack_bottom;
    while closer_idx < delims.len() {
        let closer = delims[closer_idx];
        if !is_run(closer.kind) || !closer.can_close || !closer.active {
            closer_idx += 1;
            continue;
        }
        let key = (closer.kind, closer.can_open, closer.orig_len % 3);
        let bottom = *openers_bottom.get(&key).unwrap_or(&stack_bottom);

        // Walk back to find a compatible opener.
        let mut opener_pos: Option<usize> = None;
        let mut k = closer_idx;
        while k > bottom {
            k -= 1;
            let opener = &delims[k];
            // Brackets terminate the search? Per spec we only look back at
            // delimiters; bracket openers between us are handled elsewhere.
            // A bracket "blocks" emphasis pairing across it.
            if opener.kind == b'[' || opener.kind == b'!' {
                if opener.active {
                    break;
                }
                continue;
            }
            if !is_run(opener.kind) || !opener.active || opener.kind != closer.kind {
                continue;
            }
            if !opener.can_open {
                continue;
            }
            // Strikethrough only pairs runs of length ≥ 2 on both sides.
            if opener.kind == b'~' && (opener.len < 2 || closer.len < 2) {
                continue;
            }
            // Rule of three (`*` and `_`; not `~` per the plan).
            if opener.kind != b'~' {
                let both_can = opener.can_close || closer.can_open;
                let sum_is_mod3 = (opener.orig_len + closer.orig_len).is_multiple_of(3);
                let both_mod3 =
                    opener.orig_len.is_multiple_of(3) && closer.orig_len.is_multiple_of(3);
                if both_can && sum_is_mod3 && !both_mod3 {
                    continue;
                }
            }
            opener_pos = Some(k);
            break;
        }

        if let Some(opener_idx) = opener_pos {
            let opener = delims[opener_idx];
            // Strikethrough always consumes 2; emphasis takes 2 when both
            // sides are ≥ 2 (strong), else 1 (emph).
            let strong = opener.len >= 2 && closer.len >= 2;
            let n = if closer.kind == b'~' || strong { 2 } else { 1 };

            // Drain children = out items strictly between opener.out_pos and
            // closer.out_pos.
            let drain_start = opener.out_pos + 1;
            let drain_end = closer.out_pos;
            let children: Vec<Inline> = out.drain(drain_start..drain_end).collect();
            // After this drain, all out_pos > drain_start shift by
            // -(drain_end - drain_start).
            let shift_a = drain_end - drain_start;

            // Trim n chars from the opener's Text (right end) and closer's
            // Text (left end). The closer's out_pos has shifted by shift_a.
            let new_closer_out_pos = closer.out_pos - shift_a;

            let opener_empty = trim_run_text(out, opener.out_pos, n, /*from_right*/ true);
            let closer_empty = trim_run_text(out, new_closer_out_pos, n, /*from_right*/ false);

            // Insert the wrapped node directly after the opener Text. If the
            // opener is now empty, replace it; otherwise insert after.
            let wrap_pos = if opener_empty {
                out.remove(opener.out_pos);
                opener.out_pos
            } else {
                opener.out_pos + 1
            };
            let wrapped = match closer.kind {
                b'~' => Inline::Strikethrough(children),
                _ if strong => Inline::Strong(children),
                _ => Inline::Emph(children),
            };
            out.insert(wrap_pos, wrapped);

            // Now bookkeeping: closer position in out changes too.
            // After insertion at wrap_pos, items at index ≥ wrap_pos shift
            // by +1.
            let shift_b = 1usize;

            if closer_empty {
                // Closer's text was at new_closer_out_pos; after the insert
                // its index is new_closer_out_pos + shift_b - (opener_empty
                // ? 1 : 0). Compute precisely:
                let closer_pos_now = if opener_empty {
                    new_closer_out_pos - 1 + shift_b
                } else {
                    new_closer_out_pos + shift_b
                };
                out.remove(closer_pos_now);
            }

            // Update delim out_pos for survivors; remove all delims strictly
            // between opener and closer; possibly drop opener and closer too.
            // First, trim opener/closer lengths.
            delims[opener_idx].len -= n;
            delims[closer_idx].len -= n;
            let drop_opener = delims[opener_idx].len == 0;
            let drop_closer = delims[closer_idx].len == 0;

            // Net shift for delims strictly after closer_idx — derived once,
            // applied during the in-place compaction below. shift_a items
            // drained at drain_start; shift_b inserted at wrap_pos; one
            // removal each for opener_empty / closer_empty.
            let net: isize = -(shift_a as isize) + (shift_b as isize)
                - (if drop_opener { 1 } else { 0 })
                - (if drop_closer { 1 } else { 0 });

            // In-place compaction with a write index — avoids the per-match
            // Vec::with_capacity(delims.len()) allocation the prior code paid
            // every emphasis pairing. BracketDelim is Copy, so the move is a
            // memcpy; w ≤ r at all times, so reads from delims[r] are never
            // clobbered by writes to delims[w].
            let len = delims.len();
            let mut w = 0;
            for r in 0..len {
                if r > opener_idx && r < closer_idx {
                    continue;
                }
                if (r == opener_idx && drop_opener) || (r == closer_idx && drop_closer) {
                    continue;
                }
                let mut d = delims[r];
                if r == closer_idx {
                    d.out_pos = if drop_opener {
                        new_closer_out_pos - 1 + shift_b
                    } else {
                        new_closer_out_pos + shift_b
                    };
                } else if r > closer_idx {
                    d.out_pos = (d.out_pos as isize + net) as usize;
                }
                delims[w] = d;
                w += 1;
            }
            delims.truncate(w);

            // After a match, the delim list has been rewritten. Restart
            // from the opener's index — if the opener survived, the next
            // loop iteration will see it (and skip because it's a left-
            // flanking opener, not a closer); if it didn't survive, the
            // index now points at the first delim that came after the
            // closer.
            closer_idx = opener_idx;
            continue;
        } else {
            // No opener found.
            openers_bottom.insert(key, closer_idx);
            if !closer.can_open {
                // Drop this closer (it can't be an opener for later
                // closers); but we keep its text in `out` as literal.
                delims.remove(closer_idx);
                continue;
            }
            closer_idx += 1;
        }
    }
}

fn is_run(k: u8) -> bool {
    matches!(k, b'*' | b'_' | b'~')
}

/// Trim `n` characters from a run's Text node at `out[pos]`. If
/// `from_right` is true, trim from the end; otherwise from the front.
/// Returns true if the Text became empty (caller is responsible for
/// removing it).
fn trim_run_text(out: &mut [Inline], pos: usize, n: usize, from_right: bool) -> bool {
    if let Inline::Text(s) = &mut out[pos] {
        if from_right {
            for _ in 0..n {
                s.pop();
            }
        } else {
            let byte_end = s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len());
            s.drain(..byte_end);
        }
        s.is_empty()
    } else {
        false
    }
}

fn utf8_char_len(first_byte: u8) -> usize {
    if first_byte < 0xC0 {
        // ASCII or stray continuation byte: advance 1.
        1
    } else if first_byte < 0xE0 {
        2
    } else if first_byte < 0xF0 {
        3
    } else {
        4
    }
}
