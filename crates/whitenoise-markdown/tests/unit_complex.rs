//! Structural AST tests for multi-construct interactions.
//!
//! The existing `unit_*.rs` files exercise each construct in isolation.
//! This file targets the seams *between* constructs — nested containers,
//! leaf blocks inside list items, hard breaks crossing emphasis, inline
//! math next to nostr mentions, link refs reused across paragraphs, etc.
//! Every expected value is derived from the rules already pinned by
//! sibling unit tests; nothing here introduces new spec interpretations.

use whitenoise_markdown::{
    Alignment, AutolinkKind, Block, CodeBlockKind, Inline, ListItem, ListKind, NostrEntity,
    NostrHrp, TableCell, parse,
};

mod common;
use common::{code, em, paragraph, parse_blocks, parse_inlines, strong, t};

fn math(s: &str) -> Inline {
    Inline::Math(s.to_string())
}
fn link(dest: &str, title: Option<&str>, children: Vec<Inline>) -> Inline {
    Inline::Link {
        dest: dest.to_string(),
        title: title.map(|s| s.to_string()),
        children,
    }
}
fn image(dest: &str, alt: Vec<Inline>) -> Inline {
    Inline::Image {
        dest: dest.to_string(),
        title: None,
        alt,
    }
}
fn npub(b32: &str) -> Inline {
    Inline::NostrMention(NostrEntity {
        hrp: NostrHrp::Npub,
        bech32: b32.to_string(),
    })
}
fn nuri(hrp: NostrHrp, b32: &str) -> Inline {
    Inline::NostrUri(NostrEntity {
        hrp,
        bech32: b32.to_string(),
    })
}
fn item(checked: Option<bool>, blocks: Vec<Block>) -> ListItem {
    ListItem { blocks, checked }
}
fn bullet(marker: u8, tight: bool, items: Vec<ListItem>) -> Block {
    Block::List {
        kind: ListKind::Bullet { marker },
        tight,
        items,
    }
}
fn ordered(start: u32, delim: u8, tight: bool, items: Vec<ListItem>) -> Block {
    Block::List {
        kind: ListKind::Ordered {
            start,
            delimiter: delim,
        },
        tight,
        items,
    }
}
fn fenced(info: &str, content: &str) -> Block {
    Block::CodeBlock {
        kind: CodeBlockKind::Fenced,
        info: info.to_string(),
        content: content.to_string(),
    }
}
fn indented(content: &str) -> Block {
    Block::CodeBlock {
        kind: CodeBlockKind::Indented,
        info: String::new(),
        content: content.to_string(),
    }
}

// A long-enough fake bech32 body so total length lands in the BIP-173 window.
const NPUB_BODY: &str = "xyq6ag2g4cd2y6h4r4ag2y3xeak0v6gxq46v9";
const NEVENT_BODY: &str = "qzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqz";

fn npub_str() -> String {
    format!("npub1{NPUB_BODY}")
}
fn nevent_str() -> String {
    format!("nevent1{NEVENT_BODY}")
}

// =========================================================================
// Deeply nested containers
// =========================================================================

#[test]
fn list_inside_blockquote_inside_list() {
    // Outer bullet item contains a blockquote which contains another
    // bullet list. Each level's continuation markers must be honoured.
    let input = "- > - inner";
    assert_eq!(
        parse_blocks(input),
        vec![bullet(
            b'-',
            true,
            vec![item(
                None,
                vec![Block::BlockQuote {
                    blocks: vec![bullet(
                        b'-',
                        true,
                        vec![item(None, vec![paragraph("inner")])],
                    )],
                }],
            )],
        )],
    );
}

#[test]
fn blockquote_inside_list_with_lazy_continuation() {
    // `> foo` opens a blockquote inside the bullet item; the next line is
    // a lazy continuation of the inner paragraph.
    let input = "- > foo\n  bar";
    assert_eq!(
        parse_blocks(input),
        vec![bullet(
            b'-',
            true,
            vec![item(
                None,
                vec![Block::BlockQuote {
                    blocks: vec![paragraph("foo\nbar")],
                }],
            )],
        )],
    );
}

#[test]
fn three_levels_of_blockquote() {
    let input = "> > > deep";
    assert_eq!(
        parse_blocks(input),
        vec![Block::BlockQuote {
            blocks: vec![Block::BlockQuote {
                blocks: vec![Block::BlockQuote {
                    blocks: vec![paragraph("deep")]
                }]
            }],
        }],
    );
}

#[test]
fn nested_ordered_inside_bullet() {
    let input = "- a\n  1. one\n  2. two";
    assert_eq!(
        parse_blocks(input),
        vec![bullet(
            b'-',
            true,
            vec![item(
                None,
                vec![
                    paragraph("a"),
                    ordered(
                        1,
                        b'.',
                        true,
                        vec![
                            item(None, vec![paragraph("one")]),
                            item(None, vec![paragraph("two")]),
                        ],
                    ),
                ],
            )],
        )],
    );
}

// =========================================================================
// Leaf blocks living inside list items
// =========================================================================

#[test]
fn fenced_code_inside_list_item() {
    let input = "- before\n\n  ```rust\n  fn x(){}\n  ```\n\n  after";
    assert_eq!(
        parse_blocks(input),
        vec![bullet(
            b'-',
            false,
            vec![item(
                None,
                vec![
                    paragraph("before"),
                    fenced("rust", "fn x(){}\n"),
                    paragraph("after"),
                ],
            )],
        )],
    );
}

#[test]
fn indented_code_inside_list_item() {
    // 2 spaces for the item, 4 more for the indented code (= 6 total).
    let input = "- text\n\n      x = 1";
    assert_eq!(
        parse_blocks(input),
        vec![bullet(
            b'-',
            false,
            vec![item(None, vec![paragraph("text"), indented("x = 1\n")])],
        )],
    );
}

#[test]
fn heading_inside_list_item() {
    let input = "- # title\n- body";
    assert_eq!(
        parse_blocks(input),
        vec![bullet(
            b'-',
            true,
            vec![
                item(
                    None,
                    vec![Block::Heading {
                        level: 1,
                        inlines: vec![t("title")],
                    }],
                ),
                item(None, vec![paragraph("body")]),
            ],
        )],
    );
}

// =========================================================================
// Tight / loose interactions across nesting
// =========================================================================

#[test]
fn outer_loose_inner_tight() {
    // Blank line separates the two outer items → outer loose. The inner
    // bullet list has no internal blanks → tight.
    let input = "- a\n  - x\n  - y\n\n- b";
    assert_eq!(
        parse_blocks(input),
        vec![bullet(
            b'-',
            false,
            vec![
                item(
                    None,
                    vec![
                        paragraph("a"),
                        bullet(
                            b'-',
                            true,
                            vec![
                                item(None, vec![paragraph("x")]),
                                item(None, vec![paragraph("y")]),
                            ],
                        ),
                    ],
                ),
                item(None, vec![paragraph("b")]),
            ],
        )],
    );
}

// =========================================================================
// Hard / soft breaks across inline constructs
// =========================================================================

#[test]
fn hard_break_inside_emphasis_in_list_item() {
    // Trailing two spaces produce a HardBreak; the emphasis spans the break.
    let input = "- *a  \n  b*";
    assert_eq!(
        parse_blocks(input),
        vec![bullet(
            b'-',
            true,
            vec![item(
                None,
                vec![Block::Paragraph {
                    inlines: vec![em(vec![t("a"), Inline::HardBreak, t("b")])],
                }],
            )],
        )],
    );
}

#[test]
fn backslash_hardbreak_inside_blockquote() {
    let input = "> foo\\\n> bar";
    assert_eq!(
        parse_blocks(input),
        vec![Block::BlockQuote {
            blocks: vec![Block::Paragraph {
                inlines: vec![t("foo"), Inline::HardBreak, t("bar")],
            }],
        }],
    );
}

// =========================================================================
// Inline-pass combinations on a single paragraph
// =========================================================================

#[test]
fn link_with_emphasis_code_and_nostr_in_label() {
    // Code spans, emphasis, and nostr mentions all parse inside link text.
    let s = format!("[*x* `y` @{}](/u)", npub_str());
    assert_eq!(
        parse_inlines(&s),
        vec![link(
            "/u",
            None,
            vec![
                em(vec![t("x")]),
                t(" "),
                code("y"),
                t(" "),
                npub(&npub_str()),
            ],
        )],
    );
}

#[test]
fn inline_math_then_nostr_uri_then_emphasis() {
    let s = format!("$E=mc^2$ see nostr:{} for *more*", nevent_str());
    assert_eq!(
        parse_inlines(&s),
        vec![
            math("E=mc^2"),
            t(" see "),
            nuri(NostrHrp::Nevent, &nevent_str()),
            t(" for "),
            em(vec![t("more")]),
        ],
    );
}

#[test]
fn autolink_and_nostr_uri_share_paragraph() {
    let s = format!("<https://example.com> and nostr:{}", npub_str());
    assert_eq!(
        parse_inlines(&s),
        vec![
            Inline::Autolink {
                url: "https://example.com".into(),
                kind: AutolinkKind::Uri,
            },
            t(" and "),
            nuri(NostrHrp::Npub, &npub_str()),
        ],
    );
}

#[test]
fn entity_inside_emphasis_decoded() {
    // Entities decode normally inside emphasis content (not inside code).
    assert_eq!(parse_inlines("*a &amp; b*"), vec![em(vec![t("a & b")])],);
}

#[test]
fn strong_wrapping_emphasis_wrapping_strikethrough() {
    assert_eq!(
        parse_inlines("**a *b ~~c~~ d* e**"),
        vec![strong(vec![
            t("a "),
            em(vec![t("b "), Inline::Strikethrough(vec![t("c")]), t(" d"),]),
            t(" e"),
        ])],
    );
}

// =========================================================================
// Image alt + nested inline content
// =========================================================================

#[test]
fn image_alt_with_code_and_emphasis() {
    // Inline constructs inside image alt parse normally; only the *rendered*
    // alt text flattens them — that's a renderer concern, not the AST.
    assert_eq!(
        parse_inlines("![*alt* `code`](/i.png)"),
        vec![image(
            "/i.png",
            vec![em(vec![t("alt")]), t(" "), code("code")],
        )],
    );
}

// =========================================================================
// Reference links across the document
// =========================================================================

#[test]
fn ref_link_used_in_two_paragraphs() {
    let input = "see [foo]\n\nalso [foo]\n\n[foo]: /u \"t\"";
    assert_eq!(
        parse_blocks(input),
        vec![
            Block::Paragraph {
                inlines: vec![t("see "), link("/u", Some("t"), vec![t("foo")])],
            },
            Block::Paragraph {
                inlines: vec![t("also "), link("/u", Some("t"), vec![t("foo")])],
            },
        ],
    );
}

#[test]
fn ref_link_defined_after_use() {
    let input = "[later]\n\n[later]: /late";
    assert_eq!(
        parse_blocks(input),
        vec![Block::Paragraph {
            inlines: vec![link("/late", None, vec![t("later")])],
        }],
    );
}

// =========================================================================
// Setext + table interactions
// =========================================================================

#[test]
fn setext_heading_then_table() {
    let input = "Title\n=====\n\n| a |\n| - |\n| 1 |";
    assert_eq!(
        parse_blocks(input),
        vec![
            Block::Heading {
                level: 1,
                inlines: vec![t("Title")],
            },
            Block::Table {
                alignments: vec![Alignment::None],
                header: vec![TableCell {
                    inlines: vec![t("a")]
                }],
                rows: vec![vec![TableCell {
                    inlines: vec![t("1")]
                }]],
            },
        ],
    );
}

#[test]
fn table_cells_have_mixed_inline_content() {
    let body = NPUB_BODY;
    let input = format!(
        "| name | math | code | who |\n| - | - | - | - |\n| **bold** | $x$ | `c` | @npub1{body} |"
    );
    let blocks = parse_blocks(&input);
    let Block::Table { rows, .. } = &blocks[0] else {
        panic!("expected table");
    };
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row[0].inlines, vec![strong(vec![t("bold")])]);
    assert_eq!(row[1].inlines, vec![math("x")]);
    assert_eq!(row[2].inlines, vec![code("c")]);
    assert_eq!(row[3].inlines, vec![npub(&format!("npub1{body}"))]);
}

// =========================================================================
// Heading interrupting paragraph (incl. inside blockquote)
// =========================================================================

#[test]
fn heading_interrupts_paragraph_inside_blockquote() {
    let input = "> foo\n> # bar";
    assert_eq!(
        parse_blocks(input),
        vec![Block::BlockQuote {
            blocks: vec![
                paragraph("foo"),
                Block::Heading {
                    level: 1,
                    inlines: vec![t("bar")],
                },
            ],
        }],
    );
}

// =========================================================================
// Multi-item list with task checkboxes interleaved with regular items
// =========================================================================

#[test]
fn task_and_regular_items_in_one_list() {
    let input = "- [ ] todo\n- regular\n- [x] done";
    assert_eq!(
        parse_blocks(input),
        vec![bullet(
            b'-',
            true,
            vec![
                item(Some(false), vec![paragraph("todo")]),
                item(None, vec![paragraph("regular")]),
                item(Some(true), vec![paragraph("done")]),
            ],
        )],
    );
}

// =========================================================================
// Whole-document smoke: a mixed doc parses without panicking and yields
// the expected sequence of top-level block kinds.
// =========================================================================

#[test]
fn mixed_document_block_kinds() {
    // Each section is followed by a heading so the boundary between blocks
    // is unambiguous and order-stable. The list and blockquote sit between
    // distinct H2s instead of bumping into each other.
    let input = concat!(
        "# Project\n",
        "\n",
        "An intro paragraph.\n",
        "\n",
        "## Notes\n",
        "\n",
        "> zero deps beyond serde.\n",
        "\n",
        "## Install\n",
        "\n",
        "```sh\n",
        "cargo add whitenoise-markdown\n",
        "```\n",
        "\n",
        "## Usage\n",
        "\n",
        "- parse\n",
        "- render\n",
        "- profit\n",
    );
    let blocks = parse(input).blocks;
    let kinds: Vec<&'static str> = blocks
        .iter()
        .map(|b| match b {
            Block::Paragraph { .. } => "P",
            Block::Heading { level: 1, .. } => "H1",
            Block::Heading { level: 2, .. } => "H2",
            Block::Heading { .. } => "H",
            Block::ThematicBreak => "TB",
            Block::CodeBlock { .. } => "Code",
            Block::BlockQuote { .. } => "BQ",
            Block::List { .. } => "L",
            Block::Table { .. } => "T",
            Block::MathBlock { .. } => "M",
        })
        .collect();
    assert_eq!(kinds, vec!["H1", "P", "H2", "BQ", "H2", "Code", "H2", "L"]);
}
