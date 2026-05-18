//! Shared helpers for the integration test suite.
//!
//! Each `tests/*.rs` file is a separate crate, but `mod common;` brings this
//! module in by source — so the helpers below stay in one place. Different
//! test files use different subsets of these helpers, so we silence
//! per-crate dead-code warnings here.

#![allow(dead_code)]

pub mod render;

use whitenoise_markdown::{Block, Inline, parse};

pub fn t(s: &str) -> Inline {
    Inline::Text(s.to_string())
}
pub fn em(children: Vec<Inline>) -> Inline {
    Inline::Emph(children)
}
pub fn strong(children: Vec<Inline>) -> Inline {
    Inline::Strong(children)
}
pub fn strike(children: Vec<Inline>) -> Inline {
    Inline::Strikethrough(children)
}
pub fn code(s: &str) -> Inline {
    Inline::Code(s.to_string())
}

/// Convert a raw multi-line string into the inline-token form the inline
/// pass would emit for a paragraph or heading whose only content is plain
/// text. Newlines become `Inline::SoftBreak`; the empty string becomes an
/// empty `Vec`.
pub fn text_inlines(s: &str) -> Vec<Inline> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut first = true;
    for line in s.split('\n') {
        if !first {
            out.push(Inline::SoftBreak);
        }
        first = false;
        if !line.is_empty() {
            out.push(Inline::Text(line.to_string()));
        }
    }
    out
}

pub fn paragraph(text: &str) -> Block {
    Block::Paragraph {
        inlines: text_inlines(text),
    }
}

pub fn heading(level: u8, text: &str) -> Block {
    Block::Heading {
        level,
        inlines: text_inlines(text),
    }
}

pub fn parse_blocks(s: &str) -> Vec<Block> {
    parse(s).blocks
}

/// Parse `s` and return the inlines of the first block, which must be a
/// `Paragraph` or `Heading`.
pub fn parse_inlines(s: &str) -> Vec<Inline> {
    match parse_blocks(s).into_iter().next() {
        Some(Block::Paragraph { inlines }) => inlines,
        Some(Block::Heading { inlines, .. }) => inlines,
        _ => panic!("expected paragraph/heading block"),
    }
}
