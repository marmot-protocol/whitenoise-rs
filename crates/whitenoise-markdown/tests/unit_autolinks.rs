use whitenoise_markdown::{AutolinkKind, Inline};

mod common;
use common::{parse_inlines, t};

fn uri(url: &str) -> Inline {
    Inline::Autolink {
        url: url.to_string(),
        kind: AutolinkKind::Uri,
    }
}
fn email(addr: &str) -> Inline {
    Inline::Autolink {
        url: addr.to_string(),
        kind: AutolinkKind::Email,
    }
}

// HTML is NOT parsed: tag-like sequences are literal text in both the
// block pass (no HTML blocks) and the inline pass (no raw-HTML inline).
// Only autolinks — `<scheme:body>` and `<email@host>` — get structured
// treatment.

// ----- URI autolinks ----------------------------------------------------

#[test]
fn autolink_https() {
    assert_eq!(
        parse_inlines("x <https://example.com/path?q=1>"),
        vec![t("x "), uri("https://example.com/path?q=1")]
    );
}

#[test]
fn autolink_http() {
    assert_eq!(
        parse_inlines("x <http://x>"),
        vec![t("x "), uri("http://x")]
    );
}

#[test]
fn autolink_with_surrounding_text() {
    assert_eq!(
        parse_inlines("see <http://x> please"),
        vec![t("see "), uri("http://x"), t(" please")]
    );
}

#[test]
fn autolink_scheme_too_short_rejected() {
    // Single-letter scheme is invalid; falls through to literal text.
    assert_eq!(parse_inlines("x <a:b>"), vec![t("x <a:b>")]);
}

#[test]
fn autolink_with_space_rejected() {
    assert_eq!(parse_inlines("x <http://x y>"), vec![t("x <http://x y>")]);
}

// ----- Email autolinks --------------------------------------------------

#[test]
fn email_simple() {
    assert_eq!(
        parse_inlines("x <a@b.com>"),
        vec![t("x "), email("a@b.com")]
    );
}

#[test]
fn email_complex_local() {
    assert_eq!(
        parse_inlines("x <foo.bar+baz@example.co.uk>"),
        vec![t("x "), email("foo.bar+baz@example.co.uk")]
    );
}

// ----- HTML-as-text -----------------------------------------------------

#[test]
fn html_open_tag_is_text() {
    assert_eq!(parse_inlines("x <span>"), vec![t("x <span>")]);
}

#[test]
fn html_close_tag_is_text() {
    assert_eq!(parse_inlines("x </span>"), vec![t("x </span>")]);
}

#[test]
fn html_self_closing_is_text() {
    assert_eq!(parse_inlines("x <br/>"), vec![t("x <br/>")]);
}

#[test]
fn html_with_attributes_is_text() {
    assert_eq!(
        parse_inlines("x <a href=\"y\">"),
        vec![t("x <a href=\"y\">")]
    );
}

#[test]
fn html_comment_is_text() {
    assert_eq!(parse_inlines("x <!-- hi -->"), vec![t("x <!-- hi -->")]);
}

#[test]
fn lt_with_no_following_match_is_text() {
    assert_eq!(parse_inlines("a < b"), vec![t("a < b")]);
}

// ----- Priority --------------------------------------------------------

#[test]
fn nostr_inside_uri_autolink_stays_text() {
    // The autolink wins; nostr scheme bytes inside `<...>` are part of the
    // URL, not a separate NostrUri token.
    let s = "x <nostr:nevent1qzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqz>";
    assert_eq!(parse_inlines(s), vec![t("x "), uri(&s[3..s.len() - 1])]);
}
