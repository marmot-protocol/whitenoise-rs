use whitenoise_markdown::Inline;

mod common;
use common::{em, parse_inlines, strike, strong, t};

// ----- Emphasis: `*` ---------------------------------------------------

#[test]
fn star_emphasis() {
    assert_eq!(parse_inlines("*foo*"), vec![em(vec![t("foo")])]);
}

#[test]
fn star_strong() {
    assert_eq!(parse_inlines("**foo**"), vec![strong(vec![t("foo")])]);
}

#[test]
fn star_em_wrapping_strong() {
    // Per CommonMark spec example 444, `***foo***` → Em(Strong).
    assert_eq!(
        parse_inlines("***foo***"),
        vec![em(vec![strong(vec![t("foo")])])]
    );
}

#[test]
fn star_em_inside_strong() {
    assert_eq!(
        parse_inlines("**a *b* c**"),
        vec![strong(vec![t("a "), em(vec![t("b")]), t(" c")])]
    );
}

#[test]
fn star_with_surrounding_text() {
    assert_eq!(
        parse_inlines("foo *bar* baz"),
        vec![t("foo "), em(vec![t("bar")]), t(" baz")]
    );
}

#[test]
fn star_unmatched_is_literal() {
    assert_eq!(parse_inlines("*foo"), vec![t("*foo")]);
}

#[test]
fn star_intraword_works() {
    // `*` allows intraword emphasis.
    assert_eq!(
        parse_inlines("a*b*c"),
        vec![t("a"), em(vec![t("b")]), t("c")]
    );
}

// ----- Emphasis: `_` ---------------------------------------------------

#[test]
fn underscore_emphasis() {
    assert_eq!(parse_inlines("_foo_"), vec![em(vec![t("foo")])]);
}

#[test]
fn underscore_strong() {
    assert_eq!(parse_inlines("__foo__"), vec![strong(vec![t("foo")])]);
}

#[test]
fn underscore_intraword_does_not_emphasize() {
    // `_` does NOT support intraword emphasis (CommonMark §6.3 rule).
    assert_eq!(parse_inlines("a_b_c"), vec![t("a_b_c")]);
}

// ----- Strikethrough: `~~` --------------------------------------------

#[test]
fn strikethrough_simple() {
    assert_eq!(parse_inlines("~~foo~~"), vec![strike(vec![t("foo")])]);
}

#[test]
fn strikethrough_single_tilde_does_not_match() {
    assert_eq!(parse_inlines("~foo~"), vec![t("~foo~")]);
}

#[test]
fn strikethrough_with_text() {
    assert_eq!(
        parse_inlines("a ~~b~~ c"),
        vec![t("a "), strike(vec![t("b")]), t(" c")]
    );
}

// ----- Interactions ---------------------------------------------------

#[test]
fn em_inside_strikethrough() {
    assert_eq!(
        parse_inlines("~~*foo*~~"),
        vec![strike(vec![em(vec![t("foo")])])]
    );
}

#[test]
fn strikethrough_inside_emphasis() {
    assert_eq!(
        parse_inlines("*~~foo~~*"),
        vec![em(vec![strike(vec![t("foo")])])]
    );
}

#[test]
fn code_span_takes_priority_over_emph() {
    assert_eq!(parse_inlines("`*foo*`"), vec![Inline::Code("*foo*".into())]);
}

#[test]
fn emphasis_around_nostr_mention() {
    // *@npub1...*  → Emph wrapping NostrMention.
    use whitenoise_markdown::{NostrEntity, NostrHrp};
    let body = "xyq6ag2g4cd2y6h4r4ag2y3xeak0v6gxq46v9";
    let s = format!("*@npub1{body}*");
    let parsed = parse_inlines(&s);
    let expected = vec![em(vec![Inline::NostrMention(NostrEntity {
        hrp: NostrHrp::Npub,
        bech32: format!("npub1{body}"),
    })])];
    assert_eq!(parsed, expected);
}
