use whitenoise_markdown::{Block, CodeBlockKind, Inline};

mod common;
use common::{paragraph, parse_blocks};

fn indented(content: &str) -> Block {
    Block::CodeBlock {
        kind: CodeBlockKind::Indented,
        info: String::new(),
        content: content.to_string(),
    }
}

fn fenced(info: &str, content: &str) -> Block {
    Block::CodeBlock {
        kind: CodeBlockKind::Fenced,
        info: info.to_string(),
        content: content.to_string(),
    }
}

fn math(content: &str) -> Block {
    Block::MathBlock {
        content: content.to_string(),
    }
}

// ----- Indented code blocks -------------------------------------------

#[test]
fn indented_simple() {
    assert_eq!(parse_blocks("    foo"), vec![indented("foo\n")]);
}

#[test]
fn indented_multiline() {
    let input = "    foo\n    bar";
    assert_eq!(parse_blocks(input), vec![indented("foo\nbar\n")]);
}

#[test]
fn indented_internal_blank_preserved() {
    let input = "    foo\n\n    bar";
    assert_eq!(parse_blocks(input), vec![indented("foo\n\nbar\n")]);
}

#[test]
fn indented_trailing_blanks_dropped() {
    let input = "    foo\n\n\n";
    assert_eq!(parse_blocks(input), vec![indented("foo\n")]);
}

#[test]
fn indented_does_not_interrupt_paragraph() {
    let input = "foo\n    bar";
    assert_eq!(parse_blocks(input), vec![paragraph("foo\nbar")]);
}

#[test]
fn indented_starts_after_blank_after_paragraph() {
    let input = "foo\n\n    bar";
    assert_eq!(
        parse_blocks(input),
        vec![paragraph("foo"), indented("bar\n")]
    );
}

// ----- Fenced code blocks ---------------------------------------------

#[test]
fn fenced_backticks_simple() {
    let input = "```\nfoo\n```";
    assert_eq!(parse_blocks(input), vec![fenced("", "foo\n")]);
}

#[test]
fn fenced_tildes() {
    let input = "~~~\nfoo\n~~~";
    assert_eq!(parse_blocks(input), vec![fenced("", "foo\n")]);
}

#[test]
fn fenced_with_info_string() {
    let input = "```rust\nfn main(){}\n```";
    assert_eq!(parse_blocks(input), vec![fenced("rust", "fn main(){}\n")]);
}

#[test]
fn fenced_info_string_trimmed() {
    let input = "```   rust  \nx\n```";
    assert_eq!(parse_blocks(input), vec![fenced("rust", "x\n")]);
}

#[test]
fn fenced_close_must_be_at_least_as_long() {
    // Opening `~~~~` (4) cannot be closed by `~~~` (3).
    let input = "~~~~\n~~~\nfoo\n~~~~";
    assert_eq!(parse_blocks(input), vec![fenced("", "~~~\nfoo\n")]);
}

#[test]
fn fenced_backtick_in_info_rejected() {
    // A backtick in the info string disqualifies the would-be opener; the
    // line falls through to a paragraph. (The inline pass then tokenizes
    // the text and finds an inline code span — that's separate from the
    // fence-rejection rule and is the correct CommonMark behaviour.)
    let input = "```foo`bar";
    assert_eq!(
        parse_blocks(input),
        vec![Block::Paragraph {
            inlines: vec![
                Inline::Text("``".into()),
                Inline::Code("foo".into()),
                Inline::Text("bar".into()),
            ]
        }]
    );
}

#[test]
fn fenced_unterminated_runs_to_eof() {
    let input = "```\nfoo\nbar";
    assert_eq!(parse_blocks(input), vec![fenced("", "foo\nbar\n")]);
}

#[test]
fn fenced_strips_open_indent() {
    let input = "  ```\n  foo\n  ```";
    assert_eq!(parse_blocks(input), vec![fenced("", "foo\n")]);
}

#[test]
fn fenced_internal_blank_lines() {
    let input = "```\nfoo\n\nbar\n```";
    assert_eq!(parse_blocks(input), vec![fenced("", "foo\n\nbar\n")]);
}

// ----- Math blocks ----------------------------------------------------

#[test]
fn math_simple() {
    let input = "$$\nx + y\n$$";
    assert_eq!(parse_blocks(input), vec![math("x + y\n")]);
}

#[test]
fn math_opaque_content() {
    // Math content is preserved verbatim — no inline parsing.
    let input = "$$\n*not emphasis*\n$$";
    assert_eq!(parse_blocks(input), vec![math("*not emphasis*\n")]);
}

#[test]
fn math_unterminated_runs_to_eof() {
    let input = "$$\nx\n";
    assert_eq!(parse_blocks(input), vec![math("x\n")]);
}

#[test]
fn math_inline_dollars_not_opener() {
    // `$5` on its own line is not `$$`.
    assert_eq!(parse_blocks("$5"), vec![paragraph("$5")]);
}

// ----- Link reference definitions -------------------------------------

#[test]
fn refdef_alone_produces_no_block() {
    // The paragraph contained only a ref-def, so no Paragraph is emitted.
    assert_eq!(parse_blocks("[foo]: /url"), vec![]);
}

#[test]
fn refdef_with_title_alone() {
    assert_eq!(parse_blocks("[foo]: /url \"title\""), vec![]);
}

#[test]
fn refdef_followed_by_paragraph_text() {
    // After consuming the ref-def, "rest" becomes the paragraph.
    assert_eq!(parse_blocks("[foo]: /url\nrest"), vec![paragraph("rest")]);
}

#[test]
fn refdef_two_at_top() {
    assert_eq!(
        parse_blocks("[a]: /a\n[b]: /b\nbody"),
        vec![paragraph("body")]
    );
}

#[test]
fn refdef_bracketed_destination() {
    assert_eq!(parse_blocks("[foo]: <http://example.com/path>"), vec![]);
}

#[test]
fn refdef_first_label_wins_on_duplicate() {
    // Both defs are parsed; the second is ignored. We can't easily inspect
    // the ref map from outside, but both should yield no paragraph blocks.
    assert_eq!(parse_blocks("[a]: /1\n[a]: /2"), vec![]);
}

#[test]
fn refdef_invalid_destination_falls_through_to_paragraph() {
    // No destination after `]:` — not a valid ref-def, becomes paragraph.
    assert_eq!(parse_blocks("[foo]:"), vec![paragraph("[foo]:")]);
}
