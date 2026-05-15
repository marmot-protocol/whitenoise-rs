//! `whitenoise-markdown` — a hand-written, near-zero-dependency CommonMark parser
//! that emits an abstract syntax tree.
//!
//! ## Goals
//!
//! 1. **Simplicity.** Straight-line parsing, no clever abstractions, no
//!    speculative generality.
//! 2. **Zero dependencies.** The only library dependency is optional
//!    `serde` for AST (de)serialization.
//! 3. **First-class nostr.** Two extra inline node types —
//!    [`Inline::NostrMention`] for bare `@npub1…` handles, and
//!    [`Inline::NostrUri`] for explicit `nostr:<hrp>1…` references —
//!    parsed inline alongside links and emphasis.
//!
//! See `PLAN.md` for the full design and `docs/parse-order.md` for the
//! parser's first-match-wins precedence.
//!
//! ## Architecture
//!
//! Two passes that are never fused:
//!
//! - **Pass 1 — block structure** ([`block`]): walks the input line by
//!   line, maintaining a stack of open containers (blockquote, list,
//!   list item) and at most one open leaf (paragraph, code, math, or
//!   table). Link-reference definitions are harvested at paragraph
//!   close.
//! - **Pass 2 — inline tokenization** ([`inline`]): walks the block tree
//!   and replaces each leaf's raw text with a `Vec<Inline>`. Emphasis,
//!   strikethrough, and links go through the spec's delimiter-stack +
//!   `process_emphasis` algorithm.
//!
//! ## HTML is not parsed
//!
//! Unlike CommonMark proper, this parser **does not** recognize HTML
//! blocks or raw HTML inlines. Tag-like sequences (`<div>`, `<!-- ... -->`,
//! etc.) are passed through as literal text and HTML-escaped at render
//! time. Only autolinks — `<scheme:body>` and `<email@host>` — get
//! structured treatment.
//!
//! ## Example
//!
//! ```
//! use whitenoise_markdown::{Block, Inline, parse};
//!
//! let doc = parse("# Hi *there*");
//! assert!(matches!(
//!     doc.blocks.as_slice(),
//!     [Block::Heading { level: 1, .. }]
//! ));
//! ```
//!
//! ## Serde
//!
//! All AST types implement `Serialize` and `Deserialize` when the default
//! `serde` feature is enabled. Disable it to drop the dependency entirely:
//!
//! ```toml
//! whitenoise-markdown = { version = "0.1", default-features = false }
//! ```

pub mod ast;
mod block;
mod entity;
mod inline;
mod nostr;
mod scanner;

pub use ast::{
    Alignment, AutolinkKind, Block, CodeBlockKind, Document, Inline, ListItem, ListKind,
    NostrEntity, NostrHrp, TableCell,
};

/// Parse a CommonMark document (with this crate's nostr and GFM extensions)
/// into a [`Document`].
///
/// Recognized extensions on top of CommonMark 0.31:
///
/// - GFM tables (`| h | k |\n| - | - |\n| 1 | 2 |`).
/// - GFM strikethrough (`~~foo~~`).
/// - GFM task-list items (`- [ ]`, `- [x]`).
/// - Math: inline `$…$` and block `$$ … $$` (content is opaque — recognized
///   but never parsed as LaTeX).
/// - Nostr bare mentions (`@npub1…`) and URIs (`nostr:<hrp>1…`) for the
///   whitelisted HRPs `npub`, `note`, `nevent`, `nprofile`, `naddr`,
///   `nrelay`. `nsec` is deliberately rejected — we never render private
///   keys as ergonomic anchors.
///
/// Bech32 strings are validated for *shape* only (no checksum); see
/// `PLAN.md` §4.
pub fn parse(input: &str) -> Document {
    let (blocks, refs) = block::parse_blocks(input);
    inline::parse_inlines(blocks, &refs)
}
