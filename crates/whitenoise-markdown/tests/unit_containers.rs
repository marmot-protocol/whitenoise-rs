use whitenoise_markdown::{Block, Inline, ListItem, ListKind};

mod common;
use common::{paragraph, parse_blocks};

fn item(checked: Option<bool>, blocks: Vec<Block>) -> ListItem {
    ListItem { blocks, checked }
}

fn bullet_list(marker: u8, tight: bool, items: Vec<ListItem>) -> Block {
    Block::List {
        kind: ListKind::Bullet { marker },
        tight,
        items,
    }
}

fn ordered_list(start: u32, delim: u8, tight: bool, items: Vec<ListItem>) -> Block {
    Block::List {
        kind: ListKind::Ordered {
            start,
            delimiter: delim,
        },
        tight,
        items,
    }
}

// ----- Block quotes -----------------------------------------------------

#[test]
fn blockquote_simple() {
    assert_eq!(
        parse_blocks("> foo"),
        vec![Block::BlockQuote {
            blocks: vec![paragraph("foo")]
        }]
    );
}

#[test]
fn blockquote_multiline_continuation() {
    let input = "> foo\n> bar";
    assert_eq!(
        parse_blocks(input),
        vec![Block::BlockQuote {
            blocks: vec![paragraph("foo\nbar")]
        }]
    );
}

#[test]
fn blockquote_lazy_continuation() {
    let input = "> foo\nbar";
    assert_eq!(
        parse_blocks(input),
        vec![Block::BlockQuote {
            blocks: vec![paragraph("foo\nbar")]
        }]
    );
}

#[test]
fn blockquote_blank_line_ends() {
    let input = "> foo\n\nbar";
    assert_eq!(
        parse_blocks(input),
        vec![
            Block::BlockQuote {
                blocks: vec![paragraph("foo")]
            },
            paragraph("bar"),
        ]
    );
}

#[test]
fn blockquote_nested() {
    let input = "> > foo";
    assert_eq!(
        parse_blocks(input),
        vec![Block::BlockQuote {
            blocks: vec![Block::BlockQuote {
                blocks: vec![paragraph("foo")]
            }]
        }]
    );
}

#[test]
fn nested_blockquote_list_then_trailing_paragraph() {
    // Regression: a list inside a nested blockquote followed by another
    // paragraph in the same nested quote — the trailing paragraph used to
    // misroute to root because the now-itemless list was left on the
    // container stack.
    let input = "> Outer quote\n\
                 >\n\
                 > > Inner quote with **bold**.\n\
                 > >\n\
                 > > - inner list item one\n\
                 > > - inner list item two\n\
                 > >   - deeply nested\n\
                 > >\n\
                 > > Trailing paragraph in inner quote.\n\
                 >\n\
                 > Back to outer.\n";

    let parsed = parse_blocks(input);
    assert_eq!(parsed.len(), 1, "exactly one outer block expected");
    let outer = match &parsed[0] {
        Block::BlockQuote { blocks } => blocks,
        other => panic!("expected outer BlockQuote, got {other:?}"),
    };
    assert_eq!(outer.len(), 3, "outer should contain 3 children");

    // Outer[0]: paragraph "Outer quote"
    assert!(matches!(&outer[0], Block::Paragraph { .. }));

    // Outer[1]: inner BlockQuote with 3 children (intro paragraph, list,
    // trailing paragraph).
    let inner = match &outer[1] {
        Block::BlockQuote { blocks } => blocks,
        other => panic!("expected inner BlockQuote, got {other:?}"),
    };
    assert_eq!(inner.len(), 3, "inner should contain 3 children");
    assert!(matches!(&inner[0], Block::Paragraph { .. }));
    let list_items = match &inner[1] {
        Block::List { items, .. } => items,
        other => panic!("expected list as inner[1], got {other:?}"),
    };
    assert_eq!(list_items.len(), 2);
    // Item 2 carries a nested list.
    assert!(
        list_items[1]
            .blocks
            .iter()
            .any(|b| matches!(b, Block::List { .. })),
        "item 2 should embed a nested list"
    );
    match &inner[2] {
        Block::Paragraph { inlines } => {
            let text: String = inlines
                .iter()
                .map(|i| match i {
                    Inline::Text(s) => s.as_str(),
                    _ => "",
                })
                .collect();
            assert_eq!(text, "Trailing paragraph in inner quote.");
        }
        other => panic!("expected trailing Paragraph as inner[2], got {other:?}"),
    }

    // Outer[2]: paragraph "Back to outer."
    match &outer[2] {
        Block::Paragraph { inlines } => {
            let text: String = inlines
                .iter()
                .map(|i| match i {
                    Inline::Text(s) => s.as_str(),
                    _ => "",
                })
                .collect();
            assert_eq!(text, "Back to outer.");
        }
        other => panic!("expected 'Back to outer.' Paragraph, got {other:?}"),
    }
}

#[test]
fn blockquote_with_atx_inside() {
    let input = "> # foo";
    assert_eq!(
        parse_blocks(input),
        vec![Block::BlockQuote {
            blocks: vec![Block::Heading {
                level: 1,
                inlines: vec![Inline::Text("foo".into())]
            }]
        }]
    );
}

#[test]
fn blockquote_indent_three_ok() {
    assert_eq!(
        parse_blocks("   > foo"),
        vec![Block::BlockQuote {
            blocks: vec![paragraph("foo")]
        }]
    );
}

// ----- Bullet lists -----------------------------------------------------

#[test]
fn bullet_simple_dash() {
    assert_eq!(
        parse_blocks("- foo"),
        vec![bullet_list(
            b'-',
            true,
            vec![item(None, vec![paragraph("foo")])]
        )]
    );
}

#[test]
fn bullet_simple_plus_and_star() {
    assert_eq!(
        parse_blocks("+ foo"),
        vec![bullet_list(
            b'+',
            true,
            vec![item(None, vec![paragraph("foo")])]
        )]
    );
    assert_eq!(
        parse_blocks("* foo"),
        vec![bullet_list(
            b'*',
            true,
            vec![item(None, vec![paragraph("foo")])]
        )]
    );
}

#[test]
fn bullet_two_items_tight() {
    let input = "- foo\n- bar";
    assert_eq!(
        parse_blocks(input),
        vec![bullet_list(
            b'-',
            true,
            vec![
                item(None, vec![paragraph("foo")]),
                item(None, vec![paragraph("bar")]),
            ]
        )]
    );
}

#[test]
fn bullet_two_items_loose_blank_between() {
    let input = "- foo\n\n- bar";
    assert_eq!(
        parse_blocks(input),
        vec![bullet_list(
            b'-',
            false,
            vec![
                item(None, vec![paragraph("foo")]),
                item(None, vec![paragraph("bar")]),
            ]
        )]
    );
}

#[test]
fn bullet_different_markers_are_separate_lists() {
    let input = "- foo\n+ bar";
    assert_eq!(
        parse_blocks(input),
        vec![
            bullet_list(b'-', true, vec![item(None, vec![paragraph("foo")])]),
            bullet_list(b'+', true, vec![item(None, vec![paragraph("bar")])]),
        ]
    );
}

#[test]
fn bullet_paragraph_continuation_inside_item() {
    // Continuation lines indented to the item content column.
    let input = "- foo\n  bar";
    assert_eq!(
        parse_blocks(input),
        vec![bullet_list(
            b'-',
            true,
            vec![item(None, vec![paragraph("foo\nbar")])]
        )]
    );
}

#[test]
fn bullet_lazy_continuation_inside_item() {
    let input = "- foo\nbar";
    assert_eq!(
        parse_blocks(input),
        vec![bullet_list(
            b'-',
            true,
            vec![item(None, vec![paragraph("foo\nbar")])]
        )]
    );
}

// ----- Ordered lists ----------------------------------------------------

#[test]
fn ordered_dot() {
    assert_eq!(
        parse_blocks("1. foo"),
        vec![ordered_list(
            1,
            b'.',
            true,
            vec![item(None, vec![paragraph("foo")])]
        )]
    );
}

#[test]
fn ordered_paren() {
    assert_eq!(
        parse_blocks("3) foo"),
        vec![ordered_list(
            3,
            b')',
            true,
            vec![item(None, vec![paragraph("foo")])]
        )]
    );
}

#[test]
fn ordered_two_items_keeps_first_start() {
    let input = "5. foo\n6. bar";
    assert_eq!(
        parse_blocks(input),
        vec![ordered_list(
            5,
            b'.',
            true,
            vec![
                item(None, vec![paragraph("foo")]),
                item(None, vec![paragraph("bar")]),
            ]
        )]
    );
}

#[test]
fn ordered_does_not_interrupt_paragraph_unless_one() {
    // "Foo\n2. bar" — a `2.` cannot interrupt a paragraph; line continues
    // the paragraph instead.
    let input = "Foo\n2. bar";
    assert_eq!(parse_blocks(input), vec![paragraph("Foo\n2. bar")]);
}

#[test]
fn ordered_one_can_interrupt_paragraph() {
    let input = "Foo\n1. bar";
    assert_eq!(
        parse_blocks(input),
        vec![
            paragraph("Foo"),
            ordered_list(1, b'.', true, vec![item(None, vec![paragraph("bar")])]),
        ]
    );
}

// ----- Task list checkboxes --------------------------------------------

#[test]
fn task_unchecked() {
    assert_eq!(
        parse_blocks("- [ ] foo"),
        vec![bullet_list(
            b'-',
            true,
            vec![item(Some(false), vec![paragraph("foo")])]
        )]
    );
}

#[test]
fn task_checked_lower() {
    assert_eq!(
        parse_blocks("- [x] foo"),
        vec![bullet_list(
            b'-',
            true,
            vec![item(Some(true), vec![paragraph("foo")])]
        )]
    );
}

#[test]
fn task_checked_upper() {
    assert_eq!(
        parse_blocks("- [X] foo"),
        vec![bullet_list(
            b'-',
            true,
            vec![item(Some(true), vec![paragraph("foo")])]
        )]
    );
}

#[test]
fn task_marker_inside_word_is_not_a_task() {
    assert_eq!(
        parse_blocks("- [no] foo"),
        vec![bullet_list(
            b'-',
            true,
            vec![item(None, vec![paragraph("[no] foo")])]
        )]
    );
}

#[test]
fn task_in_ordered_list() {
    assert_eq!(
        parse_blocks("1. [ ] foo\n1. [x] bar"),
        vec![ordered_list(
            1,
            b'.',
            true,
            vec![
                item(Some(false), vec![paragraph("foo")]),
                item(Some(true), vec![paragraph("bar")]),
            ]
        )]
    );
}

// ----- Mixed nesting ---------------------------------------------------

#[test]
fn list_inside_blockquote() {
    let input = "> - foo\n> - bar";
    assert_eq!(
        parse_blocks(input),
        vec![Block::BlockQuote {
            blocks: vec![bullet_list(
                b'-',
                true,
                vec![
                    item(None, vec![paragraph("foo")]),
                    item(None, vec![paragraph("bar")]),
                ]
            )]
        }]
    );
}

#[test]
fn nested_bullet_list() {
    let input = "- a\n  - b";
    assert_eq!(
        parse_blocks(input),
        vec![bullet_list(
            b'-',
            true,
            vec![item(
                None,
                vec![
                    paragraph("a"),
                    bullet_list(b'-', true, vec![item(None, vec![paragraph("b")])]),
                ]
            )]
        )]
    );
}

#[test]
fn nested_task_lists() {
    let input = "- [ ] a\n  - [x] b";
    assert_eq!(
        parse_blocks(input),
        vec![bullet_list(
            b'-',
            true,
            vec![item(
                Some(false),
                vec![
                    paragraph("a"),
                    bullet_list(b'-', true, vec![item(Some(true), vec![paragraph("b")])]),
                ]
            )]
        )]
    );
}
