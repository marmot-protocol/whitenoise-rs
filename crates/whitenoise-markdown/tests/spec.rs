//! End-to-end "spec-style" tests: parse → render → compare against a
//! hand-written expected HTML string. These are not the full CommonMark
//! spec JSON suite (which would be checked into `tests/fixtures/` per
//! PLAN.md §6 and isn't bundled with this checkout), but they exercise
//! the same parse → render path and pin the renderer's output for
//! representative cases across every block + inline construct.

use whitenoise_markdown::parse;

mod common;
use common::render::render;

fn check(md: &str, expected: &str) {
    let got = render(&parse(md));
    assert_eq!(got, expected, "input: {md:?}");
}

// ----- Headings ----------------------------------------------------------

#[test]
fn atx_h1() {
    check("# Hello", "<h1>Hello</h1>\n");
}

#[test]
fn setext_h2() {
    check("Hello\n---", "<h2>Hello</h2>\n");
}

// ----- Paragraphs + breaks ----------------------------------------------

#[test]
fn paragraph_with_soft_break() {
    check("a\nb", "<p>a\nb</p>\n");
}

#[test]
fn paragraph_with_hard_break() {
    check("a  \nb", "<p>a<br />\nb</p>\n");
}

// ----- Code -------------------------------------------------------------

#[test]
fn fenced_code_no_info() {
    check("```\nx\n```", "<pre><code>x\n</code></pre>\n");
}

#[test]
fn indented_code() {
    check("    x", "<pre><code>x\n</code></pre>\n");
}

#[test]
fn inline_code_escapes_lt() {
    check("`x<y`", "<p><code>x&lt;y</code></p>\n");
}

// ----- Emphasis + strong + strikethrough --------------------------------

#[test]
fn emphasis_combined() {
    check(
        "*em* **strong** ~~strike~~",
        "<p><em>em</em> <strong>strong</strong> <del>strike</del></p>\n",
    );
}

// ----- Links + images + autolinks --------------------------------------

#[test]
fn inline_link_with_title() {
    check("[a](/u \"t\")", "<p><a href=\"/u\" title=\"t\">a</a></p>\n");
}

#[test]
fn image_alt_extracts_text() {
    check(
        "![alt](/i.png)",
        "<p><img src=\"/i.png\" alt=\"alt\" /></p>\n",
    );
}

#[test]
fn email_autolink() {
    check(
        "see <a@b.com>",
        "<p>see <a href=\"mailto:a@b.com\">a@b.com</a></p>\n",
    );
}

// ----- Block quote + lists ----------------------------------------------

#[test]
fn blockquote_with_paragraph() {
    check(
        "> hi\n> there",
        "<blockquote>\n<p>hi\nthere</p>\n</blockquote>\n",
    );
}

#[test]
fn ordered_list() {
    check("1. a\n2. b", "<ol>\n<li>a</li>\n<li>b</li>\n</ol>\n");
}

#[test]
fn task_list_mixed() {
    let html = render(&parse("- [ ] a\n- [x] b"));
    assert!(html.contains("<input type=\"checkbox\" disabled>"));
    assert!(html.contains("<input type=\"checkbox\" disabled checked>"));
}

// ----- Table ------------------------------------------------------------

#[test]
fn table_alignments_in_html() {
    let html = render(&parse("| a | b | c |\n| :- | :-: | -: |\n| 1 | 2 | 3 |"));
    assert!(html.contains("<th align=\"left\">a</th>"));
    assert!(html.contains("<th align=\"center\">b</th>"));
    assert!(html.contains("<th align=\"right\">c</th>"));
}

// ----- Math + thematic break --------------------------------------------

#[test]
fn math_inline_and_block() {
    check(
        "$x+1$\n\n$$\nx\n$$",
        "<p><span class=\"math\">x+1</span></p>\n<div class=\"math\">x\n</div>\n",
    );
}

#[test]
fn thematic_break() {
    check("***", "<hr />\n");
}

#[test]
fn html_is_literal_text() {
    // HTML is NOT parsed; tags are escaped just like any other text.
    check("<div>hi</div>", "<p>&lt;div&gt;hi&lt;/div&gt;</p>\n");
    check(
        "see <span>x</span>",
        "<p>see &lt;span&gt;x&lt;/span&gt;</p>\n",
    );
}

// ----- Nostr ------------------------------------------------------------

#[test]
fn nostr_uri_renders_as_anchor() {
    let body = "xyq6ag2g4cd2y6h4r4ag2y3xeak0v6gxq46v9";
    let md = format!("nostr:npub1{body}");
    let html = render(&parse(&md));
    assert!(html.contains(&format!("href=\"nostr:npub1{body}\"")));
}
