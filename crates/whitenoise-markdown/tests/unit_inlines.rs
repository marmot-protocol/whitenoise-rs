use whitenoise_markdown::Inline;

mod common;
use common::{code, parse_inlines, t};

fn math(s: &str) -> Inline {
    Inline::Math(s.to_string())
}

// ----- Plain text + breaks --------------------------------------------

#[test]
fn plain_text() {
    assert_eq!(parse_inlines("hello world"), vec![t("hello world")]);
}

#[test]
fn soft_break_between_lines() {
    assert_eq!(
        parse_inlines("foo\nbar"),
        vec![t("foo"), Inline::SoftBreak, t("bar")]
    );
}

#[test]
fn hard_break_two_trailing_spaces() {
    assert_eq!(
        parse_inlines("foo  \nbar"),
        vec![t("foo"), Inline::HardBreak, t("bar")]
    );
}

#[test]
fn hard_break_backslash() {
    assert_eq!(
        parse_inlines("foo\\\nbar"),
        vec![t("foo"), Inline::HardBreak, t("bar")]
    );
}

// ----- Backslash escapes ----------------------------------------------

#[test]
fn escape_punct() {
    assert_eq!(parse_inlines("\\*foo\\*"), vec![t("*foo*")]);
}

#[test]
fn escape_letter_keeps_backslash() {
    // `\a` is NOT an escape; the backslash is literal.
    assert_eq!(parse_inlines("\\a"), vec![t("\\a")]);
}

#[test]
fn escape_at_end_of_input_literal() {
    assert_eq!(parse_inlines("foo\\"), vec![t("foo\\")]);
}

#[test]
fn escape_full_punctuation_set_sample() {
    // A few punctuation marks from the CommonMark escape table.
    assert_eq!(parse_inlines("\\!\\\"\\#\\$\\%"), vec![t("!\"#$%")]);
}

// ----- Code spans -----------------------------------------------------

#[test]
fn code_span_simple() {
    assert_eq!(parse_inlines("`foo`"), vec![code("foo")]);
}

#[test]
fn code_span_double_backticks() {
    assert_eq!(parse_inlines("``foo``"), vec![code("foo")]);
}

#[test]
fn code_span_keeps_inner_backtick() {
    assert_eq!(parse_inlines("``a`b``"), vec![code("a`b")]);
}

#[test]
fn code_span_strips_balanced_outer_spaces() {
    // Per CommonMark: leading + trailing space are stripped if both present
    // and the content isn't all spaces.
    assert_eq!(parse_inlines("` foo `"), vec![code("foo")]);
}

#[test]
fn code_span_keeps_unbalanced_outer_space() {
    assert_eq!(parse_inlines("` foo`"), vec![code(" foo")]);
}

#[test]
fn code_span_unmatched_run_falls_through() {
    // No matching run of length 3; backticks become literal text and the
    // single-backtick code span "foo" matches.
    assert_eq!(parse_inlines("```foo`"), vec![t("``"), code("foo")]);
}

#[test]
fn code_span_no_escapes_inside() {
    assert_eq!(parse_inlines("`\\n`"), vec![code("\\n")]);
}

// ----- Inline math ----------------------------------------------------

#[test]
fn inline_math_simple() {
    assert_eq!(parse_inlines("$x+1$"), vec![math("x+1")]);
}

#[test]
fn inline_math_money_not_matched() {
    // `$5 to $10` — opening `$` is followed by a digit (OK), but the
    // closing `$` is preceded by a space → no match.
    assert_eq!(parse_inlines("$5 to $10"), vec![t("$5 to $10")]);
}

#[test]
fn inline_math_shell_var_not_matched() {
    // No closing `$` at all.
    assert_eq!(parse_inlines("echo $PATH"), vec![t("echo $PATH")]);
}

#[test]
fn inline_math_opaque_content() {
    // Backslash is NOT an escape inside math; content is verbatim.
    assert_eq!(parse_inlines("$\\frac{1}{2}$"), vec![math("\\frac{1}{2}")]);
}

#[test]
fn inline_math_open_followed_by_space_rejected() {
    assert_eq!(parse_inlines("$ x$"), vec![t("$ x$")]);
}

// ----- Entities -------------------------------------------------------

#[test]
fn entity_named_amp() {
    assert_eq!(parse_inlines("&amp;"), vec![t("&")]);
}

#[test]
fn entity_named_unknown_passthrough() {
    assert_eq!(parse_inlines("&zzz;"), vec![t("&zzz;")]);
}

#[test]
fn entity_decimal_numeric() {
    assert_eq!(parse_inlines("&#65;"), vec![t("A")]);
}

#[test]
fn entity_hex_numeric_lower() {
    assert_eq!(parse_inlines("&#x41;"), vec![t("A")]);
}

#[test]
fn entity_hex_numeric_upper() {
    assert_eq!(parse_inlines("&#X41;"), vec![t("A")]);
}

#[test]
fn entity_zero_becomes_replacement_char() {
    assert_eq!(parse_inlines("&#0;"), vec![t("\u{FFFD}")]);
}

#[test]
fn entity_no_terminator_passthrough() {
    assert_eq!(parse_inlines("&amp"), vec![t("&amp")]);
}

#[test]
fn entity_inside_text() {
    assert_eq!(parse_inlines("a &amp; b"), vec![t("a & b")]);
}

// ----- Interactions ----------------------------------------------------

#[test]
fn code_span_takes_priority_over_emphasis() {
    // (Phase 9 will add emphasis; for now the `*`s are just text.)
    assert_eq!(parse_inlines("`*foo*`"), vec![code("*foo*")]);
}

#[test]
fn entity_inside_code_span_not_decoded() {
    assert_eq!(parse_inlines("`&amp;`"), vec![code("&amp;")]);
}
