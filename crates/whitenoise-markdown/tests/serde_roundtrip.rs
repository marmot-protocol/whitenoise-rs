//! Verify the `serde` feature on every AST type by round-tripping a
//! parsed document through JSON.

#![cfg(feature = "serde")]

use whitenoise_markdown::{Document, parse};

fn roundtrip(md: &str) {
    let doc = parse(md);
    let json = serde_json::to_string(&doc).expect("serialize");
    let back: Document = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(doc, back, "round-trip mismatch for input {md:?}");
}

#[test]
fn roundtrip_empty() {
    roundtrip("");
}

#[test]
fn roundtrip_heading_and_paragraph() {
    roundtrip("# Title\n\nA paragraph with *em*, **strong**, and `code`.");
}

#[test]
fn roundtrip_lists() {
    roundtrip("- a\n- [x] b\n  - nested\n\n1. one\n2. two");
}

#[test]
fn roundtrip_blockquote_and_code() {
    roundtrip("> quoted\n> text\n\n```rust\nfn x() {}\n```");
}

#[test]
fn roundtrip_link_image_autolink() {
    roundtrip("[a](/u \"t\") and ![alt](/img.png) and <https://x> and <a@b.com>");
}

#[test]
fn roundtrip_table_with_alignments() {
    roundtrip("| a | b | c |\n| :- | :-: | -: |\n| 1 | *2* | 3 |");
}

#[test]
fn roundtrip_math_and_html() {
    roundtrip("Inline $x+1$ and:\n\n$$\nE=mc^2\n$$\n\n<div>raw</div>");
}

#[test]
fn roundtrip_nostr_mention_and_uri() {
    let body = "xyq6ag2g4cd2y6h4r4ag2y3xeak0v6gxq46v9";
    let md = format!("Hello @npub1{body} via nostr:npub1{body}.");
    roundtrip(&md);
}

#[test]
fn roundtrip_complex_document() {
    let md = "\
# Demo

A paragraph with **bold**, *em*, ~~strike~~, and a [link](/u).

> A blockquote
> with two lines.

- [ ] task one
- [x] task two
  - nested

```rust
fn x() { 1 + 1 }
```

| h | k |
| - | - |
| 1 | 2 |
";
    roundtrip(md);
}
