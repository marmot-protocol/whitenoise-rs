//! End-to-end golden HTML tests for richer multi-construct documents.
//!
//! These complement `tests/spec.rs` (which covers one construct per case)
//! by exercising whole-document interactions: headings + paragraphs +
//! lists + tables + code + math + nostr in realistic combinations.
//!
//! Expected strings are the renderer's *current* output captured against
//! the parser today; they exist to detect drift. They are not derived
//! from the CommonMark/GFM spec by hand — the renderer in
//! `tests/common/render.rs` is the source of truth for the HTML shape,
//! and these tests pin the round-trip.

use whitenoise_markdown::parse;

mod common;
use common::render::render;

fn check(md: &str, expected: &str) {
    let got = render(&parse(md));
    assert_eq!(got, expected, "input: {md:?}");
}

// ----- README-style document -------------------------------------------

#[test]
fn readme_like() {
    let md = concat!(
        "# Project\n",
        "\n",
        "An intro paragraph with [a link](/u) and `code`.\n",
        "\n",
        "## Notes\n",
        "\n",
        "> zero deps beyond serde.\n",
        "\n",
        "## Install\n",
        "\n",
        "```sh\n",
        "cargo add whitenoise-markdown\n",
        "```\n",
        "\n",
        "## Usage\n",
        "\n",
        "- parse\n",
        "- render\n",
        "- profit\n",
    );
    let expected = concat!(
        "<h1>Project</h1>\n",
        "<p>An intro paragraph with <a href=\"/u\">a link</a> and <code>code</code>.</p>\n",
        "<h2>Notes</h2>\n",
        "<blockquote>\n",
        "<p>zero deps beyond serde.</p>\n",
        "</blockquote>\n",
        "<h2>Install</h2>\n",
        "<pre><code class=\"language-sh\">cargo add whitenoise-markdown\n",
        "</code></pre>\n",
        "<h2>Usage</h2>\n",
        "<ul>\n",
        "<li>parse</li>\n",
        "<li>render</li>\n",
        "<li>profit</li>\n",
        "</ul>\n",
    );
    check(md, expected);
}

// ----- Blog-post with math + image + autolink --------------------------

#[test]
fn blog_post_math_image() {
    let md = concat!(
        "# Title\n",
        "\n",
        "An equation: $E = mc^2$ inline.\n",
        "\n",
        "$$\n",
        "x = \\frac{-b \\pm \\sqrt{b^2 - 4ac}}{2a}\n",
        "$$\n",
        "\n",
        "![diagram](/img/eq.png \"Derivation\")\n",
        "\n",
        "See <https://example.com> for more.\n",
    );
    let expected = concat!(
        "<h1>Title</h1>\n",
        "<p>An equation: <span class=\"math\">E = mc^2</span> inline.</p>\n",
        "<div class=\"math\">x = \\frac{-b \\pm \\sqrt{b^2 - 4ac}}{2a}\n",
        "</div>\n",
        "<p><img src=\"/img/eq.png\" alt=\"diagram\" title=\"Derivation\" /></p>\n",
        "<p>See <a href=\"https://example.com\">https://example.com</a> for more.</p>\n",
    );
    check(md, expected);
}

// ----- Nostr mention + URI + emphasis in one paragraph -----------------

#[test]
fn nostr_mixed_paragraph() {
    let md = concat!(
        "Hi @npub1xyq6ag2g4cd2y6h4r4ag2y3xeak0v6gxq46v9, ",
        "see nostr:nevent1qzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqz ",
        "for *details*.\n",
    );
    let expected = concat!(
        "<p>Hi ",
        "<a class=\"nostr\" href=\"nostr:npub1xyq6ag2g4cd2y6h4r4ag2y3xeak0v6gxq46v9\">",
        "@npub1xyq6ag2g4cd2y6h4r4ag2y3xeak0v6gxq46v9</a>, see ",
        "<a class=\"nostr\" href=\"nostr:nevent1qzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqz\">",
        "nevent1qzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqz</a> for ",
        "<em>details</em>.</p>\n",
    );
    check(md, expected);
}

// ----- Task list with inline formatting inside each item ----------------

#[test]
fn task_list_with_emphasis() {
    let md = concat!(
        "- [ ] **important** thing\n",
        "- [x] *done* thing\n",
        "- [ ] thing with `code`\n",
    );
    let expected = concat!(
        "<ul>\n",
        "<li><input type=\"checkbox\" disabled> <strong>important</strong> thing</li>\n",
        "<li><input type=\"checkbox\" disabled checked> <em>done</em> thing</li>\n",
        "<li><input type=\"checkbox\" disabled> thing with <code>code</code></li>\n",
        "</ul>\n",
    );
    check(md, expected);
}

// ----- Table with rich inline content + alignments ----------------------

#[test]
fn table_rich_inline() {
    let md = concat!(
        "| name | math | link |\n",
        "| :- | :-: | -: |\n",
        "| **bold** | $x^2$ | [home](/) |\n",
        "| *em* | $y$ | [docs](/d) |\n",
    );
    let expected = concat!(
        "<table>\n",
        "<thead>\n",
        "<tr>\n",
        "<th align=\"left\">name</th>\n",
        "<th align=\"center\">math</th>\n",
        "<th align=\"right\">link</th>\n",
        "</tr>\n",
        "</thead>\n",
        "<tbody>\n",
        "<tr>\n",
        "<td align=\"left\"><strong>bold</strong></td>\n",
        "<td align=\"center\"><span class=\"math\">x^2</span></td>\n",
        "<td align=\"right\"><a href=\"/\">home</a></td>\n",
        "</tr>\n",
        "<tr>\n",
        "<td align=\"left\"><em>em</em></td>\n",
        "<td align=\"center\"><span class=\"math\">y</span></td>\n",
        "<td align=\"right\"><a href=\"/d\">docs</a></td>\n",
        "</tr>\n",
        "</tbody>\n",
        "</table>\n",
    );
    check(md, expected);
}

// ----- Blockquote containing a list ------------------------------------

#[test]
fn blockquote_with_list() {
    let md = concat!("> Steps:\n", ">\n", "> 1. one\n", "> 2. two\n",);
    let expected = concat!(
        "<blockquote>\n",
        "<p>Steps:</p>\n",
        "<ol>\n",
        "<li>one</li>\n",
        "<li>two</li>\n",
        "</ol>\n",
        "</blockquote>\n",
    );
    check(md, expected);
}

// ----- Hard + soft breaks inside emphasis -------------------------------

#[test]
fn nested_emphasis_with_break() {
    let md = concat!("**hard  \n", "wrap** and *soft\n", "wrap*.\n",);
    let expected = "<p><strong>hard<br />\nwrap</strong> and <em>soft\nwrap</em>.</p>\n";
    check(md, expected);
}

// ----- Reference link reused in three forms (full / collapsed / shortcut)

#[test]
fn ref_links_reused() {
    let md = concat!(
        "See [home] and [home][] and [the home page][home].\n",
        "\n",
        "[home]: /h \"Home\"\n",
    );
    let expected = concat!(
        "<p>See ",
        "<a href=\"/h\" title=\"Home\">home</a> and ",
        "<a href=\"/h\" title=\"Home\">home</a> and ",
        "<a href=\"/h\" title=\"Home\">the home page</a>.</p>\n",
    );
    check(md, expected);
}

// ----- Setext heading, thematic break, then a table --------------------

#[test]
fn setext_then_thematic_then_table() {
    let md = concat!(
        "Heading\n",
        "=======\n",
        "\n",
        "***\n",
        "\n",
        "| a | b |\n",
        "| - | - |\n",
        "| 1 | 2 |\n",
    );
    let expected = concat!(
        "<h1>Heading</h1>\n",
        "<hr />\n",
        "<table>\n",
        "<thead>\n",
        "<tr>\n",
        "<th>a</th>\n",
        "<th>b</th>\n",
        "</tr>\n",
        "</thead>\n",
        "<tbody>\n",
        "<tr>\n",
        "<td>1</td>\n",
        "<td>2</td>\n",
        "</tr>\n",
        "</tbody>\n",
        "</table>\n",
    );
    check(md, expected);
}

// ----- Autolinks coexist with HTML-as-text -----------------------------

#[test]
fn autolinks_and_html_text() {
    let md = "Email <a@b.com> and URL <https://x.y> but <div> stays literal.\n";
    let expected = concat!(
        "<p>Email ",
        "<a href=\"mailto:a@b.com\">a@b.com</a> and URL ",
        "<a href=\"https://x.y\">https://x.y</a> but ",
        "&lt;div&gt; stays literal.</p>\n",
    );
    check(md, expected);
}
