use whitenoise_markdown::{Block, Inline};

mod common;
use common::{heading, paragraph, parse_blocks};

// ----- ATX headings ------------------------------------------------------

#[test]
fn atx_h1_simple() {
    assert_eq!(parse_blocks("# foo"), vec![heading(1, "foo")]);
}

#[test]
fn atx_all_levels() {
    let input = "# 1\n## 2\n### 3\n#### 4\n##### 5\n###### 6\n";
    assert_eq!(
        parse_blocks(input),
        vec![
            heading(1, "1"),
            heading(2, "2"),
            heading(3, "3"),
            heading(4, "4"),
            heading(5, "5"),
            heading(6, "6"),
        ]
    );
}

#[test]
fn atx_seven_hashes_is_paragraph() {
    assert_eq!(parse_blocks("####### foo"), vec![paragraph("####### foo")]);
}

#[test]
fn atx_requires_space_after_hashes() {
    assert_eq!(parse_blocks("#foo"), vec![paragraph("#foo")]);
}

#[test]
fn atx_empty_heading() {
    assert_eq!(parse_blocks("#"), vec![heading(1, "")]);
    assert_eq!(parse_blocks("## "), vec![heading(2, "")]);
}

#[test]
fn atx_strips_trailing_hashes() {
    assert_eq!(parse_blocks("# foo ##"), vec![heading(1, "foo")]);
    assert_eq!(parse_blocks("### bar ###"), vec![heading(3, "bar")]);
}

#[test]
fn atx_keeps_internal_hash_when_followed_by_more() {
    assert_eq!(parse_blocks("# foo # bar #"), vec![heading(1, "foo # bar")]);
}

#[test]
fn atx_trailing_hashes_must_be_preceded_by_space() {
    assert_eq!(parse_blocks("# foo#"), vec![heading(1, "foo#")]);
}

#[test]
fn atx_only_hashes_collapses_to_empty() {
    assert_eq!(parse_blocks("## ##"), vec![heading(2, "")]);
}

#[test]
fn atx_allows_up_to_three_leading_spaces() {
    assert_eq!(parse_blocks("   # foo"), vec![heading(1, "foo")]);
}

#[test]
fn atx_four_leading_spaces_is_indented_code() {
    use whitenoise_markdown::CodeBlockKind;
    assert_eq!(
        parse_blocks("    # foo"),
        vec![Block::CodeBlock {
            kind: CodeBlockKind::Indented,
            info: String::new(),
            content: "# foo\n".to_string(),
        }]
    );
}

#[test]
fn atx_interrupts_paragraph() {
    let input = "Foo\n# bar\nBaz";
    assert_eq!(
        parse_blocks(input),
        vec![paragraph("Foo"), heading(1, "bar"), paragraph("Baz"),]
    );
}

#[test]
fn atx_trims_inner_whitespace() {
    assert_eq!(parse_blocks("#   foo  "), vec![heading(1, "foo")]);
}

// ----- Setext headings ---------------------------------------------------

#[test]
fn setext_h1_equals() {
    assert_eq!(parse_blocks("foo\n==="), vec![heading(1, "foo")]);
}

#[test]
fn setext_h2_dashes() {
    assert_eq!(parse_blocks("foo\n---"), vec![heading(2, "foo")]);
}

#[test]
fn setext_promotes_multiline_paragraph() {
    let input = "foo\nbar\n===";
    assert_eq!(parse_blocks(input), vec![heading(1, "foo\nbar")]);
}

#[test]
fn setext_underline_can_be_indented_up_to_three() {
    assert_eq!(parse_blocks("foo\n   ==="), vec![heading(1, "foo")]);
}

#[test]
fn setext_underline_indented_four_is_paragraph_continuation() {
    // Per CommonMark §4.8, leading whitespace is stripped from paragraph
    // continuation lines; a 4-indent `===` is NOT a setext underline (which
    // requires ≤3 indent) but is allowed to lazy-continue.
    assert_eq!(parse_blocks("foo\n    ==="), vec![paragraph("foo\n===")]);
}

#[test]
fn setext_underline_with_trailing_whitespace_ok() {
    assert_eq!(parse_blocks("foo\n===   "), vec![heading(1, "foo")]);
}

#[test]
fn setext_underline_after_blank_does_not_apply() {
    let input = "foo\n\n===";
    assert_eq!(
        parse_blocks(input),
        vec![paragraph("foo"), paragraph("===")]
    );
}

#[test]
fn setext_underline_no_open_paragraph_dashes_become_thematic_break() {
    assert_eq!(parse_blocks("---"), vec![Block::ThematicBreak]);
}

#[test]
fn setext_underline_dashes_take_priority_over_thematic_break() {
    // "foo\n---" must be a setext heading, not paragraph + thematic break.
    assert_eq!(parse_blocks("foo\n---"), vec![heading(2, "foo")]);
}

// ----- Thematic break ----------------------------------------------------

#[test]
fn tb_dashes_stars_underscores() {
    assert_eq!(parse_blocks("---"), vec![Block::ThematicBreak]);
    assert_eq!(parse_blocks("***"), vec![Block::ThematicBreak]);
    assert_eq!(parse_blocks("___"), vec![Block::ThematicBreak]);
}

#[test]
fn tb_with_spaces_between() {
    assert_eq!(parse_blocks(" - - -"), vec![Block::ThematicBreak]);
    assert_eq!(parse_blocks("* * *"), vec![Block::ThematicBreak]);
}

#[test]
fn tb_more_than_three_chars() {
    assert_eq!(parse_blocks("--------"), vec![Block::ThematicBreak]);
}

#[test]
fn tb_mixed_chars_not_a_break() {
    assert_eq!(parse_blocks("-*-"), vec![paragraph("-*-")]);
}

#[test]
fn tb_only_two_chars_is_paragraph() {
    assert_eq!(parse_blocks("--"), vec![paragraph("--")]);
}

#[test]
fn tb_indented_four_is_code_block() {
    use whitenoise_markdown::CodeBlockKind;
    assert_eq!(
        parse_blocks("    ---"),
        vec![Block::CodeBlock {
            kind: CodeBlockKind::Indented,
            info: String::new(),
            content: "---\n".to_string(),
        }]
    );
}

#[test]
fn tb_after_paragraph_closes_paragraph() {
    let input = "foo\n\n***\nbar";
    assert_eq!(
        parse_blocks(input),
        vec![paragraph("foo"), Block::ThematicBreak, paragraph("bar"),]
    );
}

// ----- Paragraph behaviour ----------------------------------------------

#[test]
fn paragraph_joins_consecutive_lines() {
    assert_eq!(parse_blocks("foo\nbar"), vec![paragraph("foo\nbar")]);
}

#[test]
fn paragraph_split_by_blank_line() {
    assert_eq!(
        parse_blocks("foo\n\nbar"),
        vec![paragraph("foo"), paragraph("bar")]
    );
}

#[test]
fn paragraph_strips_up_to_three_leading_spaces_per_continuation() {
    assert_eq!(parse_blocks("foo\n   bar"), vec![paragraph("foo\nbar")]);
}

#[test]
fn paragraph_trailing_spaces_become_hard_break() {
    // Two or more trailing spaces before a newline → HardBreak.
    assert_eq!(
        parse_blocks("foo   \nbar"),
        vec![Block::Paragraph {
            inlines: vec![
                Inline::Text("foo".into()),
                Inline::HardBreak,
                Inline::Text("bar".into()),
            ]
        }]
    );
}

#[test]
fn empty_input_is_empty_doc() {
    assert!(parse_blocks("").is_empty());
}

#[test]
fn only_blank_lines_is_empty_doc() {
    assert!(parse_blocks("\n\n\n").is_empty());
}

#[test]
fn line_endings_normalized() {
    assert_eq!(
        parse_blocks("foo\r\nbar\rbaz"),
        vec![paragraph("foo\nbar\nbaz")]
    );
}
