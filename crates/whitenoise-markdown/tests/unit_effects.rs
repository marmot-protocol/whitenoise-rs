//! Text effects: `{name}…{/name}` (iMessage-style).

use whitenoise_markdown::{Block, Inline};

mod common;
use common::{code, effect, parse_blocks, parse_inlines, strong, t};

// ----- Basics ----------------------------------------------------------

#[test]
fn simple_effect() {
    assert_eq!(
        parse_inlines("{big}hi{/big}"),
        vec![effect("big", vec![t("hi")])]
    );
}

#[test]
fn effect_among_text() {
    assert_eq!(
        parse_inlines("a {shake}boo{/shake} b"),
        vec![t("a "), effect("shake", vec![t("boo")]), t(" b")]
    );
}

#[test]
fn arbitrary_name() {
    assert_eq!(
        parse_inlines("{WHATEVER}x{/WHATEVER}"),
        vec![effect("WHATEVER", vec![t("x")])]
    );
}

#[test]
fn name_with_digits_and_separators() {
    assert_eq!(
        parse_inlines("{x_1-2}y{/x_1-2}"),
        vec![effect("x_1-2", vec![t("y")])]
    );
}

#[test]
fn empty_content() {
    assert_eq!(parse_inlines("{big}{/big}"), vec![effect("big", vec![])]);
}

// ----- Markdown inside --------------------------------------------------

#[test]
fn markdown_inside_effect() {
    assert_eq!(
        parse_inlines("{shake}**bold**{/shake}"),
        vec![effect("shake", vec![strong(vec![t("bold")])])]
    );
}

#[test]
fn effect_inside_emphasis() {
    // `{big}` is opaque to the surrounding emphasis pairing.
    assert_eq!(
        parse_inlines("**{big}x{/big}**"),
        vec![strong(vec![effect("big", vec![t("x")])])]
    );
}

// ----- Nesting ----------------------------------------------------------

#[test]
fn nested_different_names() {
    assert_eq!(
        parse_inlines("{big}a {shake}b{/shake} c{/big}"),
        vec![effect(
            "big",
            vec![t("a "), effect("shake", vec![t("b")]), t(" c")]
        )]
    );
}

#[test]
fn same_name_does_not_nest() {
    // An effect can't nest inside one of the same name. The outer span still
    // balances to the last `{/big}`, but the inner `{big}…{/big}` stays
    // literal inline text rather than becoming a nested Effect node.
    assert_eq!(
        parse_inlines("{big}a {big}b{/big} c{/big}"),
        vec![effect("big", vec![t("a {big}b{/big} c")])]
    );
}

#[test]
fn sibling_same_name_effects_stay_separate() {
    // Balancing (not greedy-to-last) keeps two adjacent same-name effects
    // distinct instead of merging them into one.
    assert_eq!(
        parse_inlines("{big}a{/big} b {big}c{/big}"),
        vec![
            effect("big", vec![t("a")]),
            t(" b "),
            effect("big", vec![t("c")])
        ]
    );
}

// ----- Non-triggers / literals -----------------------------------------

#[test]
fn unclosed_stays_literal() {
    assert_eq!(parse_inlines("{big}hello"), vec![t("{big}hello")]);
}

#[test]
fn mismatched_close_stays_literal() {
    assert_eq!(parse_inlines("{big}hi{/small}"), vec![t("{big}hi{/small}")]);
}

#[test]
fn set_notation_is_literal() {
    assert_eq!(parse_inlines("{1, 2, 3}"), vec![t("{1, 2, 3}")]);
}

#[test]
fn brace_with_space_is_literal() {
    assert_eq!(parse_inlines("{ big }x{/big}"), vec![t("{ big }x{/big}")]);
}

#[test]
fn escaped_braces_are_literal() {
    assert_eq!(
        parse_inlines("\\{big\\}hi\\{/big\\}"),
        vec![t("{big}hi{/big}")]
    );
}

// ----- Code is opaque ---------------------------------------------------

#[test]
fn close_inside_code_span_is_inert() {
    // The `{/big}` lives in a code span, so it does not close the effect;
    // the real close comes after.
    assert_eq!(
        parse_inlines("{big}a `{/big}` b{/big}"),
        vec![effect("big", vec![t("a "), code("{/big}"), t(" b")])]
    );
}

#[test]
fn effect_markers_in_fenced_code_block_are_literal() {
    let blocks = parse_blocks("```\n{big}x{/big}\n```");
    match &blocks[0] {
        Block::CodeBlock { content, .. } => assert_eq!(content, "{big}x{/big}\n"),
        other => panic!("expected code block, got {other:?}"),
    }
}

// ----- Serde sanity -----------------------------------------------------

#[test]
fn effect_is_a_distinct_variant() {
    let inlines = parse_inlines("{big}hi{/big}");
    assert!(matches!(inlines.as_slice(), [Inline::Effect { .. }]));
}
