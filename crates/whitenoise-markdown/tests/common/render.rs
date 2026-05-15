//! Test-only HTML renderer.
//!
//! Dumb match-per-node-kind with literal string concatenation. Not used by
//! the library; lives in `tests/common/` so spec-suite comparisons can be
//! audited end-to-end. Output mirrors the CommonMark reference renderer
//! closely enough for hand-written equivalence tests.

#![allow(dead_code, unused_imports)]

use whitenoise_markdown::{Alignment, AutolinkKind, Block, Document, Inline, ListKind};

pub fn render(doc: &Document) -> String {
    let mut out = String::new();
    render_blocks(&doc.blocks, &mut out);
    out
}

fn render_blocks(blocks: &[Block], out: &mut String) {
    for b in blocks {
        render_block(b, out);
    }
}

fn render_block(b: &Block, out: &mut String) {
    match b {
        Block::Paragraph { inlines } => {
            out.push_str("<p>");
            render_inlines(inlines, out);
            out.push_str("</p>\n");
        }
        Block::Heading { level, inlines } => {
            out.push_str(&format!("<h{level}>"));
            render_inlines(inlines, out);
            out.push_str(&format!("</h{level}>\n"));
        }
        Block::ThematicBreak => out.push_str("<hr />\n"),
        Block::CodeBlock {
            kind: _,
            info,
            content,
        } => {
            if info.is_empty() {
                out.push_str("<pre><code>");
            } else {
                let lang = info.split_whitespace().next().unwrap_or("");
                out.push_str(&format!("<pre><code class=\"language-{}\">", escape(lang)));
            }
            out.push_str(&escape(content));
            out.push_str("</code></pre>\n");
        }
        Block::BlockQuote { blocks } => {
            out.push_str("<blockquote>\n");
            render_blocks(blocks, out);
            out.push_str("</blockquote>\n");
        }
        Block::List { kind, tight, items } => {
            let (open, close): (String, &str) = match kind {
                ListKind::Bullet { .. } => ("<ul>".to_string(), "</ul>"),
                ListKind::Ordered { start, .. } if *start == 1 => ("<ol>".to_string(), "</ol>"),
                ListKind::Ordered { start, .. } => (format!("<ol start=\"{start}\">"), "</ol>"),
            };
            out.push_str(&open);
            out.push('\n');
            for item in items {
                out.push_str("<li>");
                if let Some(c) = item.checked {
                    out.push_str(&format!(
                        "<input type=\"checkbox\" disabled{}> ",
                        if c { " checked" } else { "" }
                    ));
                }
                if *tight {
                    // Tight list: render the first paragraph's inlines
                    // without <p>, then any nested non-paragraph blocks
                    // with their own tags.
                    let mut first_para_done = false;
                    for child in &item.blocks {
                        match child {
                            Block::Paragraph { inlines } if !first_para_done => {
                                render_inlines(inlines, out);
                                first_para_done = true;
                            }
                            other => render_block(other, out),
                        }
                    }
                } else {
                    out.push('\n');
                    render_blocks(&item.blocks, out);
                }
                out.push_str("</li>\n");
            }
            out.push_str(close);
            out.push('\n');
        }
        Block::Table {
            alignments,
            header,
            rows,
        } => {
            out.push_str("<table>\n<thead>\n<tr>\n");
            for (i, c) in header.iter().enumerate() {
                let align = alignments.get(i).copied().unwrap_or(Alignment::None);
                out.push_str(&format!("<th{}>", align_attr(align)));
                render_inlines(&c.inlines, out);
                out.push_str("</th>\n");
            }
            out.push_str("</tr>\n</thead>\n<tbody>\n");
            for row in rows {
                out.push_str("<tr>\n");
                for (i, c) in row.iter().enumerate() {
                    let align = alignments.get(i).copied().unwrap_or(Alignment::None);
                    out.push_str(&format!("<td{}>", align_attr(align)));
                    render_inlines(&c.inlines, out);
                    out.push_str("</td>\n");
                }
                out.push_str("</tr>\n");
            }
            out.push_str("</tbody>\n</table>\n");
        }
        Block::MathBlock { content } => {
            out.push_str("<div class=\"math\">");
            out.push_str(&escape(content));
            out.push_str("</div>\n");
        }
    }
}

fn align_attr(a: Alignment) -> &'static str {
    match a {
        Alignment::None => "",
        Alignment::Left => " align=\"left\"",
        Alignment::Center => " align=\"center\"",
        Alignment::Right => " align=\"right\"",
    }
}

fn render_inlines(inlines: &[Inline], out: &mut String) {
    for i in inlines {
        render_inline(i, out);
    }
}

fn render_inline(i: &Inline, out: &mut String) {
    match i {
        Inline::Text(s) => out.push_str(&escape(s)),
        Inline::SoftBreak => out.push('\n'),
        Inline::HardBreak => out.push_str("<br />\n"),
        Inline::Code(s) => {
            out.push_str("<code>");
            out.push_str(&escape(s));
            out.push_str("</code>");
        }
        Inline::Emph(c) => {
            out.push_str("<em>");
            render_inlines(c, out);
            out.push_str("</em>");
        }
        Inline::Strong(c) => {
            out.push_str("<strong>");
            render_inlines(c, out);
            out.push_str("</strong>");
        }
        Inline::Strikethrough(c) => {
            out.push_str("<del>");
            render_inlines(c, out);
            out.push_str("</del>");
        }
        Inline::Link {
            dest,
            title,
            children,
        } => {
            let title_attr = title
                .as_deref()
                .map(|t| format!(" title=\"{}\"", escape(t)))
                .unwrap_or_default();
            out.push_str(&format!("<a href=\"{}\"{}>", escape_attr(dest), title_attr));
            render_inlines(children, out);
            out.push_str("</a>");
        }
        Inline::Image { dest, title, alt } => {
            let title_attr = title
                .as_deref()
                .map(|t| format!(" title=\"{}\"", escape(t)))
                .unwrap_or_default();
            let mut alt_text = String::new();
            collect_text(alt, &mut alt_text);
            out.push_str(&format!(
                "<img src=\"{}\" alt=\"{}\"{} />",
                escape_attr(dest),
                escape(&alt_text),
                title_attr
            ));
        }
        Inline::Autolink { url, kind } => {
            let href = match kind {
                AutolinkKind::Uri => url.clone(),
                AutolinkKind::Email => format!("mailto:{url}"),
            };
            out.push_str(&format!("<a href=\"{}\">", escape_attr(&href)));
            out.push_str(&escape(url));
            out.push_str("</a>");
        }
        Inline::Math(s) => {
            out.push_str("<span class=\"math\">");
            out.push_str(&escape(s));
            out.push_str("</span>");
        }
        Inline::NostrMention(e) => {
            out.push_str(&format!(
                "<a class=\"nostr\" href=\"nostr:{}\">@{}</a>",
                escape_attr(&e.bech32),
                escape(&e.bech32)
            ));
        }
        Inline::NostrUri(e) => {
            out.push_str(&format!(
                "<a class=\"nostr\" href=\"nostr:{}\">{}</a>",
                escape_attr(&e.bech32),
                escape(&e.bech32)
            ));
        }
    }
}

fn collect_text(inlines: &[Inline], out: &mut String) {
    for i in inlines {
        match i {
            Inline::Text(s) => out.push_str(s),
            Inline::Code(s) => out.push_str(s),
            Inline::Emph(c)
            | Inline::Strong(c)
            | Inline::Strikethrough(c)
            | Inline::Link { children: c, .. } => collect_text(c, out),
            Inline::Image { alt: c, .. } => collect_text(c, out),
            Inline::SoftBreak | Inline::HardBreak => out.push(' '),
            Inline::Autolink { url, .. } => out.push_str(url),
            Inline::Math(s) => out.push_str(s),
            Inline::NostrMention(e) | Inline::NostrUri(e) => out.push_str(&e.bech32),
        }
    }
}

/// HTML-escape text content (`&`, `<`, `>`, `"`).
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// HTML attribute escape: same as escape() but kept distinct in case rules
/// diverge later (e.g. URL-encoding inside hrefs).
fn escape_attr(s: &str) -> String {
    escape(s)
}
