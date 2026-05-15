use whitenoise_markdown::{Block, Inline, parse};

mod common;
use common::{parse_blocks, parse_inlines, t};

fn link(dest: &str, title: Option<&str>, children: Vec<Inline>) -> Inline {
    Inline::Link {
        dest: dest.to_string(),
        title: title.map(|t| t.to_string()),
        children,
    }
}
fn image(dest: &str, title: Option<&str>, alt: Vec<Inline>) -> Inline {
    Inline::Image {
        dest: dest.to_string(),
        title: title.map(|t| t.to_string()),
        alt,
    }
}

// ----- Inline links ---------------------------------------------------

#[test]
fn inline_link_simple() {
    assert_eq!(
        parse_inlines("[foo](/url)"),
        vec![link("/url", None, vec![t("foo")])]
    );
}

#[test]
fn inline_link_with_title() {
    assert_eq!(
        parse_inlines("[foo](/url \"t\")"),
        vec![link("/url", Some("t"), vec![t("foo")])]
    );
}

#[test]
fn inline_link_bracketed_dest() {
    assert_eq!(
        parse_inlines("[foo](<https://x>)"),
        vec![link("https://x", None, vec![t("foo")])]
    );
}

#[test]
fn inline_link_empty_dest() {
    assert_eq!(
        parse_inlines("[foo]()"),
        vec![link("", None, vec![t("foo")])]
    );
}

#[test]
fn inline_link_with_text_around() {
    assert_eq!(
        parse_inlines("see [foo](/u) ok"),
        vec![t("see "), link("/u", None, vec![t("foo")]), t(" ok")]
    );
}

#[test]
fn inline_link_with_emphasis_in_text() {
    // Emphasis inside link text IS processed.
    assert_eq!(
        parse_inlines("[*foo*](/u)"),
        vec![link("/u", None, vec![Inline::Emph(vec![t("foo")])])]
    );
}

#[test]
fn inline_link_with_code_span_in_text() {
    assert_eq!(
        parse_inlines("[`code`](/u)"),
        vec![link("/u", None, vec![Inline::Code("code".into())])]
    );
}

#[test]
fn unmatched_bracket_falls_through() {
    assert_eq!(parse_inlines("[foo"), vec![t("[foo")]);
}

#[test]
fn unmatched_close_bracket_is_text() {
    assert_eq!(parse_inlines("foo]"), vec![t("foo]")]);
}

#[test]
fn no_matching_paren_falls_through() {
    // `[foo](/url` — never closes; no link, all literal.
    assert_eq!(parse_inlines("[foo](/url"), vec![t("[foo](/url")]);
}

// ----- Images ---------------------------------------------------------

#[test]
fn image_simple() {
    assert_eq!(
        parse_inlines("![alt](/img.png)"),
        vec![image("/img.png", None, vec![t("alt")])]
    );
}

#[test]
fn image_with_title() {
    assert_eq!(
        parse_inlines("![alt](/img.png \"caption\")"),
        vec![image("/img.png", Some("caption"), vec![t("alt")])]
    );
}

#[test]
fn image_in_text() {
    assert_eq!(
        parse_inlines("see ![alt](/img) ok"),
        vec![t("see "), image("/img", None, vec![t("alt")]), t(" ok")]
    );
}

// ----- Reference links (full / collapsed / shortcut) ----------------

#[test]
fn full_ref_link() {
    let input = "[foo][bar]\n\n[bar]: /url \"t\"";
    assert_eq!(
        parse_blocks(input),
        vec![Block::Paragraph {
            inlines: vec![link("/url", Some("t"), vec![t("foo")])]
        }]
    );
}

#[test]
fn collapsed_ref_link() {
    let input = "[foo][]\n\n[foo]: /url";
    assert_eq!(
        parse_blocks(input),
        vec![Block::Paragraph {
            inlines: vec![link("/url", None, vec![t("foo")])]
        }]
    );
}

#[test]
fn shortcut_ref_link() {
    let input = "[foo]\n\n[foo]: /url";
    assert_eq!(
        parse_blocks(input),
        vec![Block::Paragraph {
            inlines: vec![link("/url", None, vec![t("foo")])]
        }]
    );
}

#[test]
fn ref_link_case_insensitive() {
    let input = "[Foo]\n\n[foo]: /url";
    assert_eq!(
        parse_blocks(input),
        vec![Block::Paragraph {
            inlines: vec![link("/url", None, vec![t("Foo")])]
        }]
    );
}

#[test]
fn missing_ref_falls_through_to_text() {
    let input = "[foo][bar]";
    let blocks = parse(input).blocks;
    assert_eq!(
        blocks,
        vec![Block::Paragraph {
            inlines: vec![t("[foo][bar]")]
        }]
    );
}

// ----- Nested-link prevention ----------------------------------------

#[test]
fn no_nested_links() {
    // `[a [b](/u1) c](/u2)` — the inner `[b](/u1)` link absorbs and
    // deactivates the outer `[`, so the outer text becomes literal.
    let input = "[a [b](/u1) c](/u2)";
    let parsed = parse_inlines(input);
    // Expect: literal "[a ", inner link, literal " c](/u2)"
    assert_eq!(
        parsed,
        vec![t("[a "), link("/u1", None, vec![t("b")]), t(" c](/u2)"),]
    );
}

// ----- Image alt with nested link ------------------------------------

#[test]
fn image_alt_can_contain_link_text() {
    // Inside an image, links DO render (per spec); the outer image
    // wraps everything.
    assert_eq!(
        parse_inlines("![see [a](/u)](/img)"),
        vec![image(
            "/img",
            None,
            vec![t("see "), link("/u", None, vec![t("a")])]
        )]
    );
}
