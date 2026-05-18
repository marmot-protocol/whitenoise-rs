use whitenoise_markdown::{AutolinkKind, Inline, NostrHrp};

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

// ----- Bare URLs (GFM-style extended autolinks) ------------------------

#[test]
fn bare_https() {
    assert_eq!(
        parse_inlines("see https://example.com/path?q=1 ok"),
        vec![t("see "), uri("https://example.com/path?q=1"), t(" ok"),]
    );
}

#[test]
fn bare_http() {
    assert_eq!(
        parse_inlines("http://example.com"),
        vec![uri("http://example.com")]
    );
}

#[test]
fn bare_mailto() {
    assert_eq!(
        parse_inlines("ping mailto:foo@bar.com please"),
        vec![t("ping "), uri("mailto:foo@bar.com"), t(" please")]
    );
}

#[test]
fn bare_tel() {
    assert_eq!(
        parse_inlines("call tel:+15551234567 now"),
        vec![t("call "), uri("tel:+15551234567"), t(" now")]
    );
}

#[test]
fn bare_whitenoise() {
    assert_eq!(
        parse_inlines("open whitenoise://group/abc"),
        vec![t("open "), uri("whitenoise://group/abc")]
    );
}

#[test]
fn bare_whitenoise_staging() {
    assert_eq!(
        parse_inlines("open whitenoise-staging://group/abc"),
        vec![t("open "), uri("whitenoise-staging://group/abc")]
    );
}

#[test]
fn whitenoise_opaque_form_stays_text() {
    // Only the authority form (`whitenoise://`) is recognized as a URL.
    // Opaque `whitenoise:foo` (no `//`) is not a known shape and stays literal.
    assert_eq!(parse_inlines("whitenoise:foo"), vec![t("whitenoise:foo")]);
}

#[test]
fn bare_url_strips_trailing_period() {
    assert_eq!(
        parse_inlines("Visit https://example.com."),
        vec![t("Visit "), uri("https://example.com"), t(".")]
    );
}

#[test]
fn bare_url_strips_trailing_punct_cluster() {
    assert_eq!(
        parse_inlines("really?! https://example.com?!"),
        vec![t("really?! "), uri("https://example.com"), t("?!")]
    );
}

#[test]
fn bare_url_paren_balanced_kept() {
    // Wikipedia-style: the URL contains `(` so the trailing `)` is part of it.
    let s = "see https://en.wikipedia.org/wiki/Foo_(bar) ok";
    assert_eq!(
        parse_inlines(s),
        vec![
            t("see "),
            uri("https://en.wikipedia.org/wiki/Foo_(bar)"),
            t(" ok"),
        ]
    );
}

#[test]
fn bare_url_paren_unbalanced_stripped() {
    // Parenthesized prose: trailing `)` belongs to the sentence, not the URL.
    assert_eq!(
        parse_inlines("(see https://example.com)"),
        vec![t("(see "), uri("https://example.com"), t(")")]
    );
}

#[test]
fn bare_url_inside_emphasis() {
    // The closing `*` must not be absorbed into the URL.
    assert_eq!(
        parse_inlines("*https://example.com*"),
        vec![Inline::Emph(vec![uri("https://example.com")])]
    );
}

#[test]
fn word_internal_https_rejected() {
    // "xhttps://" must not become a URL; the `h` is mid-word.
    assert_eq!(
        parse_inlines("xhttps://example.com"),
        vec![t("xhttps://example.com")]
    );
}

#[test]
fn bare_url_at_start_of_line() {
    assert_eq!(
        parse_inlines("https://example.com is the site"),
        vec![uri("https://example.com"), t(" is the site")]
    );
}

#[test]
fn multiple_bare_urls() {
    assert_eq!(
        parse_inlines("a https://x.com b mailto:y@z.io c"),
        vec![
            t("a "),
            uri("https://x.com"),
            t(" b "),
            uri("mailto:y@z.io"),
            t(" c"),
        ]
    );
}

#[test]
fn non_url_h_t_m_w_bytes_stay_text() {
    // Stress the bulk-scan tripwire: lots of h/m/t/w in prose with no URL.
    let s = "the warmth of his methods matters with whitespace";
    assert_eq!(parse_inlines(s), vec![t(s)]);
}

#[test]
fn malformed_scheme_falls_through_to_text() {
    // "http:/x" — single slash, not a recognized prefix; stays literal.
    assert_eq!(parse_inlines("http:/x"), vec![t("http:/x")]);
}

#[test]
fn bare_nostr_uri_still_structured() {
    // Bare `nostr:` URIs go through the existing NostrUri path, not the new
    // generic bare-URL autolink — `nostr:` is NOT in the bare-URL scheme list.
    let s = "x nostr:nevent1qzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqz";
    let inlines = parse_inlines(s);
    assert_eq!(inlines.len(), 2);
    assert_eq!(inlines[0], t("x "));
    match &inlines[1] {
        Inline::NostrUri(e) => assert_eq!(e.hrp, NostrHrp::Nevent),
        other => panic!("expected NostrUri, got {other:?}"),
    }
}

#[test]
fn malformed_bare_nostr_stays_text() {
    // Per the documented design: `nostr:` without valid bech32 does NOT fall
    // back to a generic bare-URL autolink — it stays literal.
    assert_eq!(parse_inlines("nostr:foo"), vec![t("nostr:foo")]);
}
