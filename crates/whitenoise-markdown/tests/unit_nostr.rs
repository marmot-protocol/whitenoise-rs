use whitenoise_markdown::{Inline, NostrEntity, NostrHrp};

mod common;
use common::{parse_inlines, t};

fn npub(b32: &str) -> Inline {
    Inline::NostrMention(NostrEntity {
        hrp: NostrHrp::Npub,
        bech32: b32.to_string(),
    })
}
fn uri(hrp: NostrHrp, b32: &str) -> Inline {
    Inline::NostrUri(NostrEntity {
        hrp,
        bech32: b32.to_string(),
    })
}

// A long-enough fake bech32 body (38 alphabet chars) so total length
// (`<hrp>1<body>`) lands inside the BIP-173 window.
const NPUB_BODY: &str = "xyq6ag2g4cd2y6h4r4ag2y3xeak0v6gxq46v9";
const NEVENT_BODY: &str = "qzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqz";

fn npub_str() -> String {
    format!("npub1{NPUB_BODY}")
}
fn nevent_str() -> String {
    format!("nevent1{NEVENT_BODY}")
}

// ----- Bare @npub mentions -------------------------------------------

#[test]
fn npub_at_start_of_paragraph() {
    let s = format!("@{}", npub_str());
    assert_eq!(parse_inlines(&s), vec![npub(&npub_str())]);
}

#[test]
fn npub_after_whitespace() {
    let s = format!("hello @{}", npub_str());
    assert_eq!(parse_inlines(&s), vec![t("hello "), npub(&npub_str())]);
}

#[test]
fn npub_after_punctuation() {
    let s = format!("(@{})", npub_str());
    assert_eq!(parse_inlines(&s), vec![t("("), npub(&npub_str()), t(")")]);
}

#[test]
fn npub_after_letter_rejected() {
    // `foo@npub1...` must NOT match — left boundary fails.
    let s = format!("foo@{}", npub_str());
    let parsed = parse_inlines(&s);
    // Should be a single text run with no NostrMention.
    assert_eq!(parsed, vec![t(&s)]);
}

#[test]
fn npub_after_digit_rejected() {
    let s = format!("a1@{}", npub_str());
    assert_eq!(parse_inlines(&s), vec![t(&s)]);
}

#[test]
fn npub_after_underscore_rejected() {
    let s = format!("a_@{}", npub_str());
    assert_eq!(parse_inlines(&s), vec![t(&s)]);
}

#[test]
fn npub_after_slash_rejected() {
    let s = format!("/@{}", npub_str());
    assert_eq!(parse_inlines(&s), vec![t(&s)]);
}

#[test]
fn at_with_no_bech32_is_literal() {
    assert_eq!(parse_inlines("@@@"), vec![t("@@@")]);
}

#[test]
fn at_with_short_bech32_rejected() {
    // `npub1abc` is < 8 chars total, fails length check.
    assert_eq!(parse_inlines("@npub1ab"), vec![t("@npub1ab")]);
}

#[test]
fn at_with_uppercase_rejected() {
    let s = format!("@NPUB1{NPUB_BODY}");
    assert_eq!(parse_inlines(&s), vec![t(&s)]);
}

#[test]
fn at_nevent_rejected_in_bare_form() {
    // Only `@npub` triggers bare-mention; `@nevent...` is just text.
    let s = format!("@{}", nevent_str());
    assert_eq!(parse_inlines(&s), vec![t(&s)]);
}

#[test]
fn npub_in_heading() {
    use whitenoise_markdown::{Block, parse};
    let s = format!("# Hi @{}", npub_str());
    let blocks = parse(&s).blocks;
    assert_eq!(
        blocks,
        vec![Block::Heading {
            level: 1,
            inlines: vec![t("Hi "), npub(&npub_str())],
        }]
    );
}

// ----- nostr:<hrp>1… URIs --------------------------------------------

#[test]
fn nostr_npub_uri() {
    let s = format!("nostr:{}", npub_str());
    assert_eq!(parse_inlines(&s), vec![uri(NostrHrp::Npub, &npub_str())]);
}

#[test]
fn nostr_nevent_uri() {
    let s = format!("nostr:{}", nevent_str());
    assert_eq!(
        parse_inlines(&s),
        vec![uri(NostrHrp::Nevent, &nevent_str())]
    );
}

#[test]
fn nostr_uri_after_letter_rejected() {
    let s = format!("foonostr:{}", npub_str());
    assert_eq!(parse_inlines(&s), vec![t(&s)]);
}

#[test]
fn nostr_uri_in_parens() {
    let s = format!("(nostr:{})", nevent_str());
    assert_eq!(
        parse_inlines(&s),
        vec![t("("), uri(NostrHrp::Nevent, &nevent_str()), t(")")]
    );
}

#[test]
fn nostr_nsec_rejected() {
    // `nsec1…` must never match.
    let s = format!("nostr:nsec1{NPUB_BODY}");
    assert_eq!(parse_inlines(&s), vec![t(&s)]);
}

#[test]
fn nostr_unknown_hrp_rejected() {
    let s = format!("nostr:nfoo1{NPUB_BODY}");
    assert_eq!(parse_inlines(&s), vec![t(&s)]);
}

#[test]
fn nostr_mixed_case_rejected() {
    let s = format!("nostr:Npub1{NPUB_BODY}");
    assert_eq!(parse_inlines(&s), vec![t(&s)]);
}

#[test]
fn nostr_terminator_punctuation() {
    // Period after the URI should NOT consume into the bech32.
    let s = format!("see nostr:{}.", nevent_str());
    assert_eq!(
        parse_inlines(&s),
        vec![t("see "), uri(NostrHrp::Nevent, &nevent_str()), t(".")]
    );
}

#[test]
fn nostr_n_alone_is_text() {
    // `n` not followed by `ostr:` is just literal text.
    assert_eq!(parse_inlines("nope"), vec![t("nope")]);
}
