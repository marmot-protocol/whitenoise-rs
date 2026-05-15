//! Pass 1: block structure.
//!
//! State is an open-container stack plus an optional open leaf. Each line is
//! processed in three phases:
//!
//! 1. **Match open containers** top-to-bottom, consuming each one's
//!    continuation marker (`>` for block quotes, indent for list items).
//! 2. **Open new containers** on the remainder (`>`, list markers).
//! 3. **Classify the remainder** as a leaf-block starter or as content for
//!    the current open leaf.

use std::collections::HashMap;

use crate::ast::{Alignment, Block, CodeBlockKind, Inline, ListItem, ListKind, TableCell};
use crate::scanner;

#[derive(Debug, Clone)]
#[allow(dead_code)] // populated in Phase 3 (link reference definitions)
pub(crate) struct LinkRef {
    pub dest: String,
    pub title: Option<String>,
}

pub(crate) fn parse_blocks(input: &str) -> (Vec<Block>, HashMap<String, LinkRef>) {
    let mut p = BlockParser::new();
    for line in scanner::lines(input) {
        p.feed(line);
    }
    p.finish();
    (p.root, p.refs)
}

// ---------------------------------------------------------------------------
// Open-block representation
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Container {
    BlockQuote {
        children: Vec<Block>,
    },
    List {
        kind: ListKind,
        tight: bool,
        last_blank: bool,
        items: Vec<ListItem>,
    },
    ListItem {
        children: Vec<Block>,
        indent: usize,
    },
}

#[derive(Debug)]
enum Leaf {
    Paragraph(String),
    /// 4-space-indented code block. `pending_blanks` is the number of blank
    /// lines seen since the last indented content line; they are flushed
    /// when more content arrives, or discarded on close.
    IndentedCode {
        content: String,
        pending_blanks: usize,
    },
    /// Fenced code block opened by ` ``` ` or `~~~`.
    FencedCode {
        fence: u8,
        fence_len: usize,
        /// Column indent of the opening fence; subtracted from each content
        /// line's leading whitespace when storing.
        indent: usize,
        info: String,
        content: String,
    },
    /// `$$ ... $$` math block. Structurally a fenced code block whose fence
    /// byte is `$` and whose `fence_len` is implicitly 2.
    MathBlock {
        indent: usize,
        content: String,
    },
    /// GFM table. Cells are stored as raw strings; the inline pass
    /// tokenizes each cell at walk time.
    Table {
        alignments: Vec<Alignment>,
        header: Vec<String>,
        rows: Vec<Vec<String>>,
    },
}

struct BlockParser {
    refs: HashMap<String, LinkRef>,
    root: Vec<Block>,
    containers: Vec<Container>,
    leaf: Option<Leaf>,
}

#[derive(Debug, Clone, Copy)]
struct Cursor {
    col: usize,
    off: usize,
}

impl BlockParser {
    fn new() -> Self {
        Self {
            refs: HashMap::new(),
            root: Vec::new(),
            containers: Vec::new(),
            leaf: None,
        }
    }

    // -------- top-level line dispatch --------

    fn feed(&mut self, line: &str) {
        let bytes = line.as_bytes();
        let (cursor, matched_depth) = self.match_open_containers(bytes);
        let remainder = &bytes[cursor.off..];
        let blank = scanner::is_blank(remainder);

        // A fenced code block / math block stays open across container-
        // prefix failures and blank lines until its own close condition
        // fires.
        let in_verbatim_leaf = matches!(
            self.leaf,
            Some(Leaf::FencedCode { .. }) | Some(Leaf::MathBlock { .. })
        );

        if blank {
            match &mut self.leaf {
                Some(Leaf::IndentedCode { pending_blanks, .. }) => {
                    *pending_blanks += 1;
                }
                Some(Leaf::FencedCode { content, .. } | Leaf::MathBlock { content, .. }) => {
                    content.push('\n');
                }
                Some(Leaf::Table { .. }) => {
                    self.close_leaf();
                }
                _ => self.close_leaf(),
            }
            self.mark_lists_blank();
            return;
        }

        if matched_depth < self.containers.len() {
            if self.leaf.is_some() && self.can_lazy_continue(bytes) {
                let stripped = strip_paragraph_indent(line);
                self.append_paragraph(stripped);
                return;
            }
            self.close_to_depth(matched_depth);
        }

        // While a verbatim leaf (fenced code / math) is open, the line is
        // either its closing fence or another content line; no other
        // recognizers run.
        if in_verbatim_leaf && self.leaf.is_some() {
            self.feed_verbatim_line(bytes, cursor);
            return;
        }

        // While a Table leaf is open, try to consume the line as another
        // row; on failure close the table and fall through to normal
        // classification.
        if matches!(self.leaf, Some(Leaf::Table { .. })) {
            let rest = std::str::from_utf8(&bytes[cursor.off..]).unwrap_or("");
            if rest.contains('|') {
                if let Some(Leaf::Table { header, rows, .. }) = &mut self.leaf {
                    let mut cells = split_table_row(rest);
                    cells.resize(header.len(), String::new());
                    rows.push(cells);
                }
                return;
            }
            self.close_leaf();
            // Fall through to classify this line as a fresh block.
        }

        // Open new containers / classify the remainder. Each iteration may
        // open one new container or emit a leaf and stop.
        let mut col = cursor.col;
        let mut off = cursor.off;
        loop {
            // First skip any leading spaces visible at this layer (≤3 cols
            // counts as leaf-context indent; ≥4 means indented code).
            let (sub_col, sub_off) = scanner::measure_indent(&bytes[off..]);
            let probe_col = col + sub_col;
            let probe_off = off + sub_off;

            if sub_col < 4 {
                let rest = &bytes[probe_off..];

                // Leaf-block starters that beat container openers.
                if !rest.is_empty() {
                    if let Some((level, text)) = parse_atx_heading(rest) {
                        self.close_leaf();
                        self.push_block(Block::Heading {
                            level,
                            inlines: vec![Inline::Text(text)],
                        });
                        return;
                    }
                    if let Some((fence, fence_len, info)) = parse_fence_open(rest) {
                        self.close_leaf();
                        self.leaf = Some(Leaf::FencedCode {
                            fence,
                            fence_len,
                            indent: sub_col,
                            info,
                            content: String::new(),
                        });
                        return;
                    }
                    if is_math_fence(rest) {
                        self.close_leaf();
                        self.leaf = Some(Leaf::MathBlock {
                            indent: sub_col,
                            content: String::new(),
                        });
                        return;
                    }
                    if matches!(self.leaf, Some(Leaf::Paragraph(_)))
                        && let Some(level) = parse_setext_underline(rest)
                    {
                        let Some(Leaf::Paragraph(raw)) = self.leaf.take() else {
                            unreachable!()
                        };
                        let raw = self.harvest_ref_defs(raw);
                        if raw.is_empty() {
                            // All paragraph text was ref-defs — nothing to
                            // promote; emit no heading, fall through as if
                            // the paragraph were never here.
                            return;
                        }
                        self.push_block(Block::Heading {
                            level,
                            inlines: vec![Inline::Text(raw)],
                        });
                        return;
                    }
                    if is_thematic_break(rest) {
                        self.close_leaf();
                        self.push_block(Block::ThematicBreak);
                        return;
                    }

                    // GFM table reclassification: if the open leaf is a
                    // single-line paragraph and this line is a delimiter
                    // row with matching column count, promote.
                    if let Some(Leaf::Paragraph(prev)) = &self.leaf
                        && !prev.contains('\n')
                    {
                        let rest_str = std::str::from_utf8(rest).unwrap_or("");
                        if let Some(alignments) = parse_table_delim_row(rest_str) {
                            let header = split_table_row(prev);
                            if header.len() == alignments.len() {
                                self.leaf = Some(Leaf::Table {
                                    alignments,
                                    header,
                                    rows: Vec::new(),
                                });
                                return;
                            }
                        }
                    }

                    // Container openers.
                    if rest[0] == b'>'
                        && let Some((nc, no)) = try_open_blockquote(bytes, probe_col, probe_off)
                    {
                        self.close_leaf();
                        self.containers.push(Container::BlockQuote {
                            children: Vec::new(),
                        });
                        col = nc;
                        off = no;
                        continue;
                    }
                    if let Some(open) =
                        try_open_list_marker(bytes, probe_col, probe_off, self.leaf.is_some())
                    {
                        self.close_leaf();
                        self.ensure_list_open(open.kind);
                        self.containers.push(Container::ListItem {
                            children: Vec::new(),
                            indent: open.indent,
                        });
                        col = open.col_after;
                        off = open.off_after;
                        continue;
                    }
                }
            }

            // Indented code block: ≥4 leading spaces AND not a paragraph
            // continuation. If a paragraph is open, this is lazy paragraph
            // continuation (handled below as a paragraph append).
            if sub_col >= 4 && !matches!(self.leaf, Some(Leaf::Paragraph(_))) {
                // Append to existing indented code or open a new one.
                let stripped = indented_strip(line, off);
                if let Some(Leaf::IndentedCode {
                    content,
                    pending_blanks,
                }) = &mut self.leaf
                {
                    for _ in 0..*pending_blanks {
                        content.push('\n');
                    }
                    *pending_blanks = 0;
                    content.push_str(stripped);
                    content.push('\n');
                } else {
                    self.close_leaf();
                    let mut content = String::new();
                    content.push_str(stripped);
                    content.push('\n');
                    self.leaf = Some(Leaf::IndentedCode {
                        content,
                        pending_blanks: 0,
                    });
                }
                return;
            }

            // Anything past this point is paragraph content.
            self.append_paragraph_from(line, off);
            return;
        }
    }

    /// Feed a line to an already-open fenced code / math block / HTML block.
    /// Either it's the matching closing fence (in which case the block is
    /// closed) or another verbatim content line.
    fn feed_verbatim_line(&mut self, bytes: &[u8], cursor: Cursor) {
        let rest = &bytes[cursor.off..];
        match self.leaf.as_mut().unwrap() {
            Leaf::FencedCode {
                fence,
                fence_len,
                indent,
                info: _,
                content,
            } => {
                if is_close_fence(rest, *fence, *fence_len) {
                    self.close_leaf();
                    return;
                }
                let stripped = strip_n_leading_spaces(rest, *indent);
                content.push_str(stripped);
                content.push('\n');
            }
            Leaf::MathBlock { indent, content } => {
                if is_math_close(rest) {
                    self.close_leaf();
                    return;
                }
                let stripped = strip_n_leading_spaces(rest, *indent);
                content.push_str(stripped);
                content.push('\n');
            }
            _ => unreachable!(),
        }
    }

    fn finish(&mut self) {
        self.close_to_depth(0);
    }

    // -------- container matching --------

    fn match_open_containers(&mut self, bytes: &[u8]) -> (Cursor, usize) {
        let mut col = 0;
        let mut off = 0;
        for (i, c) in self.containers.iter().enumerate() {
            match c {
                Container::BlockQuote { .. } => {
                    // ≤3 leading spaces then '>'.
                    let (sub_col, sub_off) = scanner::measure_indent(&bytes[off..]);
                    let probe_off = off + sub_off;
                    if sub_col >= 4 || probe_off >= bytes.len() || bytes[probe_off] != b'>' {
                        return (Cursor { col, off }, i);
                    }
                    col += sub_col + 1;
                    off = probe_off + 1;
                    // Optional one space/tab after '>'.
                    if off < bytes.len() {
                        match bytes[off] {
                            b' ' => {
                                off += 1;
                                col += 1;
                            }
                            b'\t' => {
                                col += 4 - (col % 4);
                                off += 1;
                            }
                            _ => {}
                        }
                    }
                }
                Container::List { .. } => {
                    // Lists themselves consume nothing; their open ListItem
                    // (if any) handles indent matching.
                }
                Container::ListItem { indent, .. } => {
                    let needed = *indent;
                    if col >= needed {
                        continue;
                    }
                    // Check whether the rest of the line is blank — a blank
                    // line matches every list item.
                    if scanner::is_blank(&bytes[off..]) {
                        return (Cursor { col, off }, i + 1);
                    }
                    let (new_col, consumed) = consume_cols(&bytes[off..], col, needed);
                    if new_col < needed {
                        return (Cursor { col, off }, i);
                    }
                    col = new_col;
                    off += consumed;
                }
            }
        }
        (Cursor { col, off }, self.containers.len())
    }

    // -------- leaf handling --------

    fn append_paragraph_from(&mut self, full_line: &str, container_off: usize) {
        // Per-line trailing whitespace is preserved so the inline pass can
        // detect the 2-space-then-newline hard-break signal; it strips its
        // own trailing whitespace on close. Leading whitespace IS stripped
        // here (paragraph "raw content" per CommonMark §4.8).
        let after_containers = if container_off <= full_line.len() {
            &full_line[container_off..]
        } else {
            ""
        };
        let stripped = strip_paragraph_indent(after_containers);
        self.append_paragraph(stripped);
    }

    fn append_paragraph(&mut self, text: &str) {
        // A non-blank line that adds content to a list resets last_blank
        // and (if last_blank was set) marks the list loose.
        self.consume_list_blank();

        // GFM table reclassification: if the open paragraph is a single
        // line and the incoming line is a valid delimiter row with a
        // matching column count, promote the paragraph into a Table.
        if let Some(Leaf::Paragraph(prev)) = &self.leaf
            && !prev.contains('\n')
            && let Some(alignments) = parse_table_delim_row(text)
        {
            let header = split_table_row(prev);
            if header.len() == alignments.len() {
                self.leaf = Some(Leaf::Table {
                    alignments,
                    header,
                    rows: Vec::new(),
                });
                return;
            }
        }

        match &mut self.leaf {
            Some(Leaf::Paragraph(buf)) => {
                buf.push('\n');
                buf.push_str(text);
            }
            Some(_) => {
                // A non-paragraph leaf is open (indented code, etc.). Close
                // it and start a fresh paragraph.
                self.close_leaf();
                self.leaf = Some(Leaf::Paragraph(text.to_string()));
            }
            None => self.leaf = Some(Leaf::Paragraph(text.to_string())),
        }
    }

    fn close_leaf(&mut self) {
        let Some(leaf) = self.leaf.take() else {
            return;
        };
        match leaf {
            Leaf::Paragraph(raw) => {
                let remaining = self.harvest_ref_defs(raw);
                if !remaining.is_empty() {
                    self.push_block(Block::Paragraph {
                        inlines: vec![Inline::Text(remaining)],
                    });
                }
            }
            Leaf::IndentedCode {
                content,
                pending_blanks: _,
            } => {
                self.push_block(Block::CodeBlock {
                    kind: CodeBlockKind::Indented,
                    info: String::new(),
                    content,
                });
            }
            Leaf::FencedCode { info, content, .. } => {
                self.push_block(Block::CodeBlock {
                    kind: CodeBlockKind::Fenced,
                    info,
                    content,
                });
            }
            Leaf::MathBlock { content, .. } => {
                self.push_block(Block::MathBlock { content });
            }
            Leaf::Table {
                alignments,
                header,
                rows,
            } => {
                let header_cells = header
                    .into_iter()
                    .map(|s| TableCell {
                        inlines: vec![Inline::Text(s)],
                    })
                    .collect();
                let body_rows = rows
                    .into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|s| TableCell {
                                inlines: vec![Inline::Text(s)],
                            })
                            .collect()
                    })
                    .collect();
                self.push_block(Block::Table {
                    alignments,
                    header: header_cells,
                    rows: body_rows,
                });
            }
        }
    }

    // -------- container stack manipulation --------

    fn push_block(&mut self, block: Block) {
        self.consume_list_blank();
        match self.containers.last_mut() {
            None => self.root.push(block),
            Some(Container::BlockQuote { children }) => children.push(block),
            Some(Container::ListItem { children, .. }) => children.push(block),
            Some(Container::List { .. }) => {
                // Block-level content directly inside a List with no open
                // item is impossible: opening a list always pushes an item.
                // Defensive: append to root.
                self.root.push(block);
            }
        }
    }

    fn close_to_depth(&mut self, depth: usize) {
        self.close_leaf();
        while self.containers.len() > depth {
            let c = self.containers.pop().unwrap();
            self.close_container(c);
        }
    }

    fn close_container(&mut self, c: Container) {
        match c {
            Container::BlockQuote { children } => {
                let block = Block::BlockQuote { blocks: children };
                self.push_block(block);
            }
            Container::ListItem { mut children, .. } => {
                let checked = detect_task_marker(&mut children);
                let item = ListItem {
                    blocks: children,
                    checked,
                };
                // Push into the enclosing list.
                if let Some(Container::List { items, .. }) = self.containers.last_mut() {
                    items.push(item);
                }
            }
            Container::List {
                kind, tight, items, ..
            } => {
                let block = Block::List { kind, tight, items };
                self.push_block(block);
            }
        }
    }

    fn ensure_list_open(&mut self, new_kind: ListKind) {
        // If the top container is a compatible List (same marker kind +
        // delimiter), do nothing — the new ListItem joins that list.
        // Otherwise close any existing list and open a fresh one.
        if let Some(Container::List { kind, .. }) = self.containers.last() {
            if lists_compatible(*kind, new_kind) {
                return;
            }
            // Different list kind: close the existing one (and its item if open).
            // The previous item was already closed (we entered ensure_list_open
            // after close_leaf and matched_depth handling); but defensively
            // close down.
            let depth = self.containers.len() - 1;
            self.close_to_depth(depth);
        }
        // After potential close, the top might no longer be a List.
        self.containers.push(Container::List {
            kind: new_kind,
            tight: true,
            last_blank: false,
            items: Vec::new(),
        });
    }

    // -------- last_blank bookkeeping (loose vs tight) --------

    fn mark_lists_blank(&mut self) {
        for c in self.containers.iter_mut() {
            if let Container::List { last_blank, .. } = c {
                *last_blank = true;
            }
        }
    }

    fn consume_list_blank(&mut self) {
        for c in self.containers.iter_mut() {
            if let Container::List {
                last_blank, tight, ..
            } = c
                && *last_blank
            {
                *tight = false;
                *last_blank = false;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Container opening predicates
// ---------------------------------------------------------------------------

fn try_open_blockquote(bytes: &[u8], col: usize, off: usize) -> Option<(usize, usize)> {
    let (sub_col, sub_off) = scanner::measure_indent(&bytes[off..]);
    if sub_col >= 4 {
        return None;
    }
    let probe = off + sub_off;
    if probe >= bytes.len() || bytes[probe] != b'>' {
        return None;
    }
    let mut new_off = probe + 1;
    let mut new_col = col + sub_col + 1;
    if new_off < bytes.len() {
        match bytes[new_off] {
            b' ' => {
                new_off += 1;
                new_col += 1;
            }
            b'\t' => {
                new_col += 4 - (new_col % 4);
                new_off += 1;
            }
            _ => {}
        }
    }
    Some((new_col, new_off))
}

#[derive(Debug)]
struct OpenListMarker {
    kind: ListKind,
    indent: usize,
    col_after: usize,
    off_after: usize,
}

fn try_open_list_marker(
    bytes: &[u8],
    col: usize,
    off: usize,
    interrupting_paragraph: bool,
) -> Option<OpenListMarker> {
    let (sub_col, sub_off) = scanner::measure_indent(&bytes[off..]);
    if sub_col >= 4 {
        return None;
    }
    let marker_off = off + sub_off;
    let marker_col = col + sub_col;
    if marker_off >= bytes.len() {
        return None;
    }
    let b = bytes[marker_off];

    let (kind, marker_end) = match b {
        b'-' | b'+' | b'*' => (ListKind::Bullet { marker: b }, marker_off + 1),
        d if d.is_ascii_digit() => {
            // Up to 9 digits then '.' or ')'.
            let mut i = marker_off;
            while i < bytes.len() && bytes[i].is_ascii_digit() && i - marker_off < 9 {
                i += 1;
            }
            if i >= bytes.len() || (bytes[i] != b'.' && bytes[i] != b')') {
                return None;
            }
            let start: u32 = std::str::from_utf8(&bytes[marker_off..i])
                .ok()?
                .parse()
                .ok()?;
            let delim = bytes[i];
            // Ordered list can only interrupt a paragraph if start == 1.
            if interrupting_paragraph && start != 1 {
                return None;
            }
            (
                ListKind::Ordered {
                    start,
                    delimiter: delim,
                },
                i + 1,
            )
        }
        _ => return None,
    };

    // After the marker, require space/tab or end-of-line.
    if marker_end < bytes.len() && bytes[marker_end] != b' ' && bytes[marker_end] != b'\t' {
        return None;
    }
    // Bullet lists interrupting a paragraph: the remainder must be non-empty
    // (CommonMark §5.2: a list-item that interrupts a paragraph must not be
    // empty).
    if interrupting_paragraph
        && (marker_end >= bytes.len() || scanner::is_blank(&bytes[marker_end..]))
    {
        return None;
    }

    // Compute indent: column after marker + at most 4 whitespace cols.
    let marker_width = marker_end - marker_off;
    let after_marker_col = marker_col + marker_width;
    let (ws_col, ws_off) = scanner::measure_indent(&bytes[marker_end..]);

    let (col_after, off_after);
    if marker_end >= bytes.len() || scanner::is_blank(&bytes[marker_end..]) {
        // Empty item: indent = marker col + width + 1 (one space).
        col_after = after_marker_col + 1;
        off_after = bytes.len();
    } else if ws_col >= 5 {
        // 5+ spaces of whitespace after marker → the content is treated as
        // indented code inside the item; use the minimum (one-space) indent.
        col_after = after_marker_col + 1;
        off_after = marker_end + 1;
    } else {
        col_after = after_marker_col + ws_col;
        off_after = marker_end + ws_off;
    }
    Some(OpenListMarker {
        kind,
        // Continuation indent is measured in *absolute* columns from the
        // start of the line.
        indent: col_after,
        col_after,
        off_after,
    })
}

fn lists_compatible(a: ListKind, b: ListKind) -> bool {
    match (a, b) {
        (ListKind::Bullet { marker: x }, ListKind::Bullet { marker: y }) => x == y,
        (ListKind::Ordered { delimiter: x, .. }, ListKind::Ordered { delimiter: y, .. }) => x == y,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Link reference definitions (harvested at paragraph close).
// ---------------------------------------------------------------------------

impl BlockParser {
    /// Strip leading `[label]: dest [title]` definitions from `raw` and
    /// register them. Returns the remaining paragraph text (possibly empty).
    fn harvest_ref_defs(&mut self, raw: String) -> String {
        let mut rest = raw.as_str();
        while let Some((label, def, consumed)) = parse_one_ref_def(rest) {
            let normalized = normalize_label(&label);
            if !normalized.is_empty() {
                self.refs.entry(normalized).or_insert(def);
            }
            rest = &rest[consumed..];
            // A ref-def is followed by a newline (or EOI); skip it.
            if rest.starts_with('\n') {
                rest = &rest[1..];
            }
        }
        rest.to_string()
    }
}

/// Try to parse one ref-def from the front of `s`. Returns
/// `(raw_label, link_ref, bytes_consumed_up_to_but_not_including_trailing_nl)`.
fn parse_one_ref_def(s: &str) -> Option<(String, LinkRef, usize)> {
    let b = s.as_bytes();
    // Optional ≤3 leading spaces.
    let mut i = 0;
    let mut col = 0;
    while col < 4 && i < b.len() && b[i] == b' ' {
        i += 1;
        col += 1;
    }
    if col >= 4 || i >= b.len() || b[i] != b'[' {
        return None;
    }
    // Label: `[ ... ]`, possibly with escaped `]`, max ~999 chars per spec.
    let label_start = i + 1;
    let mut j = label_start;
    while j < b.len() && b[j] != b']' {
        if b[j] == b'\\' && j + 1 < b.len() {
            j += 2;
            continue;
        }
        if b[j] == b'\n' {
            // Labels may span lines in spec; for v1 keep within a single
            // logical buffer (the paragraph already joined lines with '\n')
            // — treat newlines as label content.
        }
        j += 1;
    }
    if j >= b.len() || b[j] != b']' {
        return None;
    }
    let raw_label = std::str::from_utf8(&b[label_start..j]).ok()?.to_string();
    if raw_label.trim().is_empty() {
        return None;
    }
    i = j + 1;
    if i >= b.len() || b[i] != b':' {
        return None;
    }
    i += 1;
    // Skip optional spaces/tabs (and at most one newline) before destination.
    i = skip_ws_and_one_newline(b, i);
    if i >= b.len() {
        return None;
    }
    let (dest, ni) = parse_link_destination(b, i)?;
    i = ni;
    // Optional title. Title must be preceded by whitespace (or be on the
    // next line); if title parse fails we still keep the def (no-title).
    let after_dest = i;
    let i_after_ws = skip_inline_ws(b, after_dest);
    let saw_nl = i_after_ws < b.len() && b[i_after_ws] == b'\n';
    let i_after_ws_nl = if saw_nl { i_after_ws + 1 } else { i_after_ws };
    let i_title_start = skip_inline_ws(b, i_after_ws_nl);
    let mut title = None;
    let mut end = i;
    if i_title_start < b.len()
        && matches!(b[i_title_start], b'"' | b'\'' | b'(')
        && let Some((t, te)) = parse_link_title(b, i_title_start)
    {
        // After title, only whitespace allowed before end-of-line / EOI.
        let after_t = skip_inline_ws(b, te);
        if after_t >= b.len() || b[after_t] == b'\n' {
            title = Some(t);
            end = after_t;
        }
    }
    if title.is_none() {
        // After destination, only whitespace allowed before end-of-line / EOI.
        let after = skip_inline_ws(b, after_dest);
        if after < b.len() && b[after] != b'\n' {
            return None;
        }
        end = after;
    }

    Some((raw_label, LinkRef { dest, title }, end))
}

fn skip_inline_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    i
}

fn skip_ws_and_one_newline(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    if i < b.len() && b[i] == b'\n' {
        i += 1;
        while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
            i += 1;
        }
    }
    i
}

/// Parse a link destination starting at `i`. Returns the unescaped
/// destination and the byte index after it.
pub(crate) fn parse_link_destination(b: &[u8], i: usize) -> Option<(String, usize)> {
    if i >= b.len() {
        return None;
    }
    if b[i] == b'<' {
        // `<...>` form: no unescaped `<`, `>` or newline.
        let start = i + 1;
        let mut j = start;
        let mut out_bytes: Vec<u8> = Vec::new();
        while j < b.len() && b[j] != b'>' {
            if b[j] == b'<' || b[j] == b'\n' {
                return None;
            }
            if b[j] == b'\\' && j + 1 < b.len() && scanner::is_ascii_punct(b[j + 1]) {
                out_bytes.push(b[j + 1]);
                j += 2;
                continue;
            }
            out_bytes.push(b[j]);
            j += 1;
        }
        if j >= b.len() {
            return None;
        }
        Some((String::from_utf8(out_bytes).ok()?, j + 1))
    } else {
        // Bare form: no whitespace, balanced parens, no control chars.
        let start = i;
        let mut j = start;
        let mut depth: i32 = 0;
        let mut out_bytes: Vec<u8> = Vec::new();
        while j < b.len() {
            let c = b[j];
            if c == b' ' || c == b'\t' || c == b'\n' {
                break;
            }
            if c < 0x20 || c == 0x7f {
                break;
            }
            if c == b'\\' && j + 1 < b.len() && scanner::is_ascii_punct(b[j + 1]) {
                out_bytes.push(b[j + 1]);
                j += 2;
                continue;
            }
            if c == b'(' {
                depth += 1;
            }
            if c == b')' {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            out_bytes.push(c);
            j += 1;
        }
        if start == j || depth != 0 {
            return None;
        }
        Some((String::from_utf8(out_bytes).ok()?, j))
    }
}

/// Parse a link title (`"..."`, `'...'`, or `(...)`). Returns the unescaped
/// title and the byte index after the closing quote.
pub(crate) fn parse_link_title(b: &[u8], i: usize) -> Option<(String, usize)> {
    if i >= b.len() {
        return None;
    }
    let (open, close) = match b[i] {
        b'"' => (b'"', b'"'),
        b'\'' => (b'\'', b'\''),
        b'(' => (b'(', b')'),
        _ => return None,
    };
    let mut j = i + 1;
    let mut out_bytes: Vec<u8> = Vec::new();
    while j < b.len() {
        let c = b[j];
        if c == b'\\' && j + 1 < b.len() && scanner::is_ascii_punct(b[j + 1]) {
            out_bytes.push(b[j + 1]);
            j += 2;
            continue;
        }
        if c == close {
            return Some((String::from_utf8(out_bytes).ok()?, j + 1));
        }
        // Disallow unescaped opening quote of same family inside parens form.
        if open == b'(' && c == b'(' {
            return None;
        }
        out_bytes.push(c);
        j += 1;
    }
    None
}

/// Lowercase ASCII + collapse internal whitespace (runs of space / tab /
/// newline) into a single space. Trim leading + trailing whitespace.
pub(crate) fn normalize_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut prev_ws = true; // suppresses leading whitespace
    for c in label.chars() {
        if c == ' ' || c == '\t' || c == '\n' {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            prev_ws = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------------------
// Lazy continuation
// ---------------------------------------------------------------------------

impl BlockParser {
    /// True if the line could safely extend a paragraph as a lazy
    /// continuation — i.e. it's not a thematic break, ATX heading,
    /// blockquote marker, fenced/indented code (Phase 3+), HTML block
    /// (Phase 4+), or list-marker that would interrupt a paragraph.
    ///
    /// A list marker that *cannot* interrupt a paragraph (e.g. `2.`) still
    /// blocks lazy continuation when the deepest open list is of compatible
    /// kind — because in that case the marker opens a sibling item, not a
    /// paragraph continuation.
    fn can_lazy_continue(&self, bytes: &[u8]) -> bool {
        let (col, off) = scanner::measure_indent(bytes);
        if col >= 4 {
            return true;
        }
        let rest = &bytes[off..];
        if rest.is_empty() {
            return false;
        }
        if parse_atx_heading(rest).is_some() {
            return false;
        }
        if is_thematic_break(rest) {
            return false;
        }
        if rest[0] == b'>' {
            return false;
        }
        if parse_fence_open(rest).is_some() {
            return false;
        }
        if is_math_fence(rest) {
            return false;
        }
        if try_open_list_marker(bytes, 0, 0, true).is_some() {
            return false;
        }
        if let Some(open) = try_open_list_marker(bytes, 0, 0, false)
            && let Some(k) = self.deepest_open_list()
            && lists_compatible(k, open.kind)
        {
            return false;
        }
        true
    }

    fn deepest_open_list(&self) -> Option<ListKind> {
        self.containers.iter().rev().find_map(|c| match c {
            Container::List { kind, .. } => Some(*kind),
            _ => None,
        })
    }
}

// ---------------------------------------------------------------------------
// Task-list marker detection (run at list-item close).
// ---------------------------------------------------------------------------

fn detect_task_marker(children: &mut [Block]) -> Option<bool> {
    let first = children.first_mut()?;
    let Block::Paragraph { inlines } = first else {
        return None;
    };
    let Some(Inline::Text(text)) = inlines.first_mut() else {
        return None;
    };
    let bytes = text.as_bytes();
    if bytes.len() >= 4 && bytes[0] == b'[' && bytes[2] == b']' && bytes[3] == b' ' {
        let inner = bytes[1];
        let checked = match inner {
            b' ' => Some(false),
            b'x' | b'X' => Some(true),
            _ => None,
        };
        if let Some(c) = checked {
            *text = text[4..].to_string();
            return Some(c);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Helpers shared with Phase 1
// ---------------------------------------------------------------------------

fn consume_cols(line: &[u8], start_col: usize, target_col: usize) -> (usize, usize) {
    let mut col = start_col;
    let mut off = 0;
    while col < target_col && off < line.len() {
        match line[off] {
            b' ' => {
                col += 1;
                off += 1;
            }
            b'\t' => {
                col += 4 - (col % 4);
                off += 1;
            }
            _ => break,
        }
    }
    (col, off)
}

/// Strip all leading whitespace from a paragraph line. For both the first
/// line (which the leaf-classifier has already consumed ≤3 cols of indent
/// for) and lazy continuations (which can have 4+ cols of indent), the
/// paragraph's "raw content" excludes leading whitespace per CommonMark §4.8.
fn strip_paragraph_indent(line: &str) -> &str {
    line.trim_start_matches([' ', '\t'])
}

fn parse_atx_heading(bytes: &[u8]) -> Option<(u8, String)> {
    if bytes.is_empty() || bytes[0] != b'#' {
        return None;
    }
    let mut hash_count = 0;
    while hash_count < bytes.len() && bytes[hash_count] == b'#' {
        hash_count += 1;
    }
    if hash_count == 0 || hash_count > 6 {
        return None;
    }
    let after = &bytes[hash_count..];
    if !after.is_empty() && after[0] != b' ' && after[0] != b'\t' {
        return None;
    }
    let level = hash_count as u8;
    let mut start = 0;
    while start < after.len() && (after[start] == b' ' || after[start] == b'\t') {
        start += 1;
    }
    let mut end = after.len();
    while end > start && (after[end - 1] == b' ' || after[end - 1] == b'\t') {
        end -= 1;
    }
    let core = &after[start..end];
    let stripped = strip_atx_closing(core);
    let s = std::str::from_utf8(stripped).expect("ascii-aligned slice");
    Some((level, s.to_string()))
}

fn strip_atx_closing(core: &[u8]) -> &[u8] {
    let mut i = core.len();
    while i > 0 && core[i - 1] == b'#' {
        i -= 1;
    }
    let hashes = core.len() - i;
    if hashes == 0 {
        return core;
    }
    if i == 0 {
        return &core[..0];
    }
    if core[i - 1] != b' ' && core[i - 1] != b'\t' {
        return core;
    }
    let mut j = i;
    while j > 0 && (core[j - 1] == b' ' || core[j - 1] == b'\t') {
        j -= 1;
    }
    &core[..j]
}

fn parse_setext_underline(bytes: &[u8]) -> Option<u8> {
    if bytes.is_empty() {
        return None;
    }
    let ch = bytes[0];
    if ch != b'=' && ch != b'-' {
        return None;
    }
    let mut i = 0;
    while i < bytes.len() && bytes[i] == ch {
        i += 1;
    }
    while i < bytes.len() {
        if bytes[i] != b' ' && bytes[i] != b'\t' {
            return None;
        }
        i += 1;
    }
    Some(if ch == b'=' { 1 } else { 2 })
}

/// Parse a fenced-code-block opener: 3+ `` ` `` or `~`, optionally followed
/// by an info string. Returns `(fence_char, fence_len, info_string)`.
fn parse_fence_open(bytes: &[u8]) -> Option<(u8, usize, String)> {
    if bytes.is_empty() {
        return None;
    }
    let ch = bytes[0];
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let mut i = 0;
    while i < bytes.len() && bytes[i] == ch {
        i += 1;
    }
    if i < 3 {
        return None;
    }
    let info_bytes = &bytes[i..];
    // Info string rules:
    //  - For backtick fences, the info string must not contain a backtick.
    //  - Trim leading and trailing whitespace.
    if ch == b'`' && info_bytes.contains(&b'`') {
        return None;
    }
    let trimmed = trim_ascii_ws(info_bytes);
    let info = std::str::from_utf8(trimmed).ok()?.to_string();
    Some((ch, i, info))
}

fn is_close_fence(bytes: &[u8], fence: u8, fence_len: usize) -> bool {
    // ≤3 leading spaces, then a run of `fence` of length ≥ fence_len, then
    // only whitespace.
    let (col, off) = scanner::measure_indent(bytes);
    if col >= 4 {
        return false;
    }
    let rest = &bytes[off..];
    let mut i = 0;
    while i < rest.len() && rest[i] == fence {
        i += 1;
    }
    if i < fence_len {
        return false;
    }
    rest[i..].iter().all(|&b| b == b' ' || b == b'\t')
}

/// `$$` followed by only whitespace (and not `$$$`). Caller is expected to
/// have already stripped any ≤3-column indent.
fn is_math_fence(bytes: &[u8]) -> bool {
    bytes.len() >= 2
        && bytes[0] == b'$'
        && bytes[1] == b'$'
        && bytes.get(2) != Some(&b'$')
        && bytes[2..].iter().all(|&b| b == b' ' || b == b'\t')
}

fn is_math_close(bytes: &[u8]) -> bool {
    let (col, off) = scanner::measure_indent(bytes);
    col < 4 && is_math_fence(&bytes[off..])
}

fn trim_ascii_ws(b: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = b.len();
    while start < end && (b[start] == b' ' || b[start] == b'\t') {
        start += 1;
    }
    while end > start && (b[end - 1] == b' ' || b[end - 1] == b'\t') {
        end -= 1;
    }
    &b[start..end]
}

/// For an indented code line, strip up to 4 leading columns of whitespace
/// from `line` starting at `off`.
fn indented_strip(line: &str, off: usize) -> &str {
    let bytes = line.as_bytes();
    let mut col = 0;
    let mut i = off;
    while col < 4 && i < bytes.len() {
        match bytes[i] {
            b' ' => {
                col += 1;
                i += 1;
            }
            b'\t' => {
                col += 4 - (col % 4);
                i += 1;
                if col > 4 {
                    // Tab overshoots; for simplicity we still advance past
                    // it (loses some indent fidelity, acceptable in v1).
                }
            }
            _ => break,
        }
    }
    &line[i..]
}

/// Strip up to `n` leading spaces (column-counted) from a verbatim content
/// line — used for fenced code / math block content to remove the indent of
/// the opening fence.
fn strip_n_leading_spaces(bytes: &[u8], n: usize) -> &str {
    let mut col = 0;
    let mut i = 0;
    while col < n && i < bytes.len() && bytes[i] == b' ' {
        col += 1;
        i += 1;
    }
    std::str::from_utf8(&bytes[i..]).expect("ascii-aligned slice")
}

// ---------------------------------------------------------------------------
// GFM tables
// ---------------------------------------------------------------------------

/// Try to parse `line` as a table delimiter row. On success returns the
/// per-column alignments.
fn parse_table_delim_row(line: &str) -> Option<Vec<Alignment>> {
    if !line.contains('|') && !line.contains('-') {
        return None;
    }
    let cells = split_table_row(line);
    if cells.is_empty() {
        return None;
    }
    let mut alignments = Vec::with_capacity(cells.len());
    for cell in &cells {
        let trimmed = cell.trim();
        let bytes = trimmed.as_bytes();
        if bytes.is_empty() {
            return None;
        }
        let left = bytes[0] == b':';
        let right = *bytes.last().unwrap() == b':';
        let inner_start = if left { 1 } else { 0 };
        let inner_end = if right { bytes.len() - 1 } else { bytes.len() };
        if inner_start > inner_end {
            return None;
        }
        let inner = &bytes[inner_start..inner_end];
        if inner.is_empty() || !inner.iter().all(|&b| b == b'-') {
            return None;
        }
        alignments.push(match (left, right) {
            (true, true) => Alignment::Center,
            (true, false) => Alignment::Left,
            (false, true) => Alignment::Right,
            _ => Alignment::None,
        });
    }
    Some(alignments)
}

/// Split a table row on `|`. Handles `\|` as a literal `|` inside a cell.
/// Strips a single optional leading and trailing `|` (with their adjacent
/// whitespace).
fn split_table_row(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();

    // Find content range, ignoring leading/trailing whitespace.
    let mut lo = 0;
    while lo < bytes.len() && (bytes[lo] == b' ' || bytes[lo] == b'\t') {
        lo += 1;
    }
    let mut hi = bytes.len();
    while hi > lo && (bytes[hi - 1] == b' ' || bytes[hi - 1] == b'\t') {
        hi -= 1;
    }
    // Strip a leading `|`.
    if lo < hi && bytes[lo] == b'|' {
        lo += 1;
    }
    // Strip an unescaped trailing `|`.
    if hi > lo && bytes[hi - 1] == b'|' && (hi < 2 || bytes[hi - 2] != b'\\') {
        hi -= 1;
    }

    let mut cells: Vec<String> = Vec::new();
    let mut cur_bytes: Vec<u8> = Vec::new();
    let mut k = lo;
    while k < hi {
        let c = bytes[k];
        if c == b'\\' && k + 1 < hi && bytes[k + 1] == b'|' {
            cur_bytes.push(b'|');
            k += 2;
            continue;
        }
        if c == b'|' {
            let cell = String::from_utf8(std::mem::take(&mut cur_bytes)).unwrap_or_default();
            cells.push(cell.trim().to_string());
            k += 1;
            continue;
        }
        cur_bytes.push(c);
        k += 1;
    }
    let cell = String::from_utf8(cur_bytes).unwrap_or_default();
    cells.push(cell.trim().to_string());
    cells
}

fn is_thematic_break(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let ch = bytes[0];
    if ch != b'-' && ch != b'*' && ch != b'_' {
        return false;
    }
    let mut count = 0;
    for &b in bytes {
        if b == ch {
            count += 1;
        } else if b != b' ' && b != b'\t' {
            return false;
        }
    }
    count >= 3
}
