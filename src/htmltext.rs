//! Shape source-provided text for Pinboard. Titles become single-line plain text
//! (entities decoded, literal angle brackets kept, whitespace collapsed); HackerNews
//! note bodies (raw Algolia HTML) become Markdown; quoted bodies are wrapped in a
//! literal `<blockquote>`, which Pinboard renders in the notes field.

use html5ever::tendril::TendrilSink;
use html5ever::{local_name, ns, ParseOpts, QualName};
use markup5ever_rcdom::{NodeData, RcDom};

/// Clean a title (`description`): decode HTML entities and collapse all internal
/// whitespace to single spaces (trimmed). HN titles are documented as HTML (HN's item
/// API, which Algolia indexes verbatim), so their entities need decoding, but neither
/// they nor GitHub's descriptions carry markup, so literal angle brackets (`Vec<String>`,
/// `vector<T>`) are preserved verbatim rather than dropped as HTML tags. Literal `<`/`>`
/// are escaped before parsing so html5ever decodes entities without treating a `<...>`
/// span as an element to strip. Not for Reddit titles, which `raw_json=1` already returns
/// unescaped. Only for source text, decoded once: `cleanup` never runs a stored Pinboard
/// title through this, since that title is already plain text and a second decode would
/// rewrite a literal `&amp;` on every run (it only collapses the whitespace).
pub fn html_to_plain(s: &str) -> String {
    let escaped = s.replace('<', "&lt;").replace('>', "&gt;");
    collapse_whitespace(&body_fragment_text(&escaped), " ")
}

/// The text of `html` parsed as the contents of a `<body>`: every text node outside a
/// `<template>`, in document order. For `html_to_plain`'s escaped input, which has no
/// tags, that is all of it.
fn body_fragment_text(html: &str) -> String {
    let body = QualName::new(None, ns!(html), local_name!("body"));
    let dom =
        html5ever::parse_fragment(RcDom::default(), ParseOpts::default(), body, vec![], false)
            .one(html);
    let mut text = String::new();
    let mut pending = vec![dom.document.clone()];
    while let Some(node) = pending.pop() {
        if let NodeData::Text { contents } = &node.data {
            text.push_str(&contents.borrow());
        }
        pending.extend(node.children.borrow().iter().rev().cloned());
    }
    text
}

/// `text`'s whitespace-separated words joined by `separator`: every run of whitespace
/// becomes one `separator`, and leading/trailing whitespace is dropped.
pub fn collapse_whitespace(text: &str, separator: &str) -> String {
    let mut collapsed = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !collapsed.is_empty() {
            collapsed.push_str(separator);
        }
        collapsed.push_str(word);
    }
    collapsed
}

/// Convert a HackerNews note body (raw HTML from Algolia `story_text`/`comment_text`)
/// to Markdown. Only ever run over HN HTML -- never over Reddit/GitHub bodies, which are
/// already Markdown/plain and would be corrupted by escaping. Returns the input verbatim
/// if the converter errors, so a single odd item never aborts the pass.
pub fn html_to_markdown(s: &str) -> String {
    htmd::convert(s).unwrap_or_else(|_| s.to_string())
}

/// Wrap quoted remote body content in a literal `<blockquote>` (Pinboard renders HTML
/// in the notes field). Used by every source.
pub fn blockquote(s: &str) -> String {
    format!("<blockquote>{s}</blockquote>")
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(html_to_plain("A normal title"), "A normal title");
        assert_eq!(html_to_plain("owner/repo"), "owner/repo");
    }

    #[test]
    fn plain_decodes_entities() {
        assert_eq!(html_to_plain("Tom &amp; Jerry"), "Tom & Jerry");
        assert_eq!(html_to_plain("it&#x27;s"), "it's");
        assert_eq!(html_to_plain("a &gt; b"), "a > b");
    }

    #[test]
    fn plain_decodes_entities_as_html_does() {
        assert_eq!(html_to_plain("AT&amp T"), "AT& T");
        assert_eq!(html_to_plain("&notit; &unknown;"), "¬it; &unknown;");
        assert_eq!(
            html_to_plain("&#0;&#xD800;&#x80;&#X41;"),
            "\u{FFFD}\u{FFFD}€A"
        );
        assert_eq!(html_to_plain("a\0b"), "ab");
        assert_eq!(html_to_plain("&lt;b&gt; &AMP; &nbsp;x"), "<b> & x");
    }

    #[test]
    fn plain_preserves_literal_angle_brackets() {
        assert_eq!(html_to_plain("Vec<String> support"), "Vec<String> support");
        assert_eq!(html_to_plain("C++ vector<T>"), "C++ vector<T>");
        assert_eq!(html_to_plain("a < b > c"), "a < b > c");
    }

    #[test]
    fn plain_does_not_strip_tag_like_text() {
        assert_eq!(html_to_plain("<b>Bold</b> title"), "<b>Bold</b> title");
    }

    #[test]
    fn plain_collapses_whitespace() {
        assert_eq!(
            html_to_plain("line one\n\n  line   two"),
            "line one line two"
        );
        assert_eq!(html_to_plain("  padded  "), "padded");
    }

    #[test]
    fn collapse_whitespace_joins_words_with_the_separator() {
        assert_eq!(collapse_whitespace("  a \t b\n\nc  ", "-"), "a-b-c");
        assert_eq!(collapse_whitespace("one", " "), "one");
        assert_eq!(collapse_whitespace(" \n ", " "), "");
    }

    #[test]
    fn plain_empty_is_empty() {
        assert_eq!(html_to_plain(""), "");
    }

    #[test]
    fn markdown_converts_paragraphs() {
        assert_eq!(html_to_markdown("<p>one</p><p>two</p>"), "one\n\ntwo");
    }

    #[test]
    fn markdown_converts_links() {
        assert_eq!(
            html_to_markdown("<a href=\"https://x.com\">x</a>"),
            "[x](https://x.com)"
        );
    }

    #[test]
    fn markdown_converts_single_paragraph() {
        assert_eq!(html_to_markdown("<p>details</p>"), "details");
    }

    #[test]
    fn markdown_decodes_entities() {
        assert_eq!(html_to_markdown("x &gt; y"), "x > y");
    }

    #[test]
    fn markdown_plain_text_passes_through() {
        // Reddit/GitHub bodies are never sent here, but document that plain text is safe.
        assert_eq!(html_to_markdown("just text"), "just text");
    }

    #[test]
    fn markdown_keeps_code_block_contents() {
        let md = html_to_markdown("<pre><code>fn main() {}</code></pre>");
        assert!(md.contains("fn main() {}"), "got: {md:?}");
    }

    fn html_escaped(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#x27;")
    }

    /// Text that is mostly entities, near-entities, markup and line breaks, which
    /// arbitrary text almost never contains. The HTML parser drops NUL anywhere and a
    /// U+FEFF (a byte-order mark) at the start of its input, so neither is drawn.
    fn entity_laden_text() -> impl Strategy<Value = String> {
        let piece = prop_oneof![
            "[^\\x00\\x{feff}]{0,4}",
            prop::sample::select(vec![
                "&", "&amp;", "&lt;", "&gt", "&quot;", "&#38;", "&#x26;", "&nbsp;", "&not",
                "&notit;", "<b>", "</b>", "\r\n", " ",
            ])
            .prop_map(String::from),
        ];
        prop::collection::vec(piece, 0..8).prop_map(|pieces| pieces.concat())
    }

    proptest! {
        #![proptest_config(crate::test_support::proptest_config())]

        /// Not idempotent on every input, by design: `&amp;amp;` decodes one level per
        /// call. Text with no `&` has no entity to decode, so a second call is a no-op.
        #[test]
        fn plain_is_idempotent_on_text_without_entities(
            text in entity_laden_text().prop_map(|text| text.replace('&', "")),
        ) {
            let once = html_to_plain(&text);
            prop_assert_eq!(html_to_plain(&once), once);
        }

        #[test]
        fn plain_undoes_exactly_one_html_escape(text in entity_laden_text()) {
            prop_assert_eq!(
                html_to_plain(&html_escaped(&text)),
                collapse_whitespace(&text, " ")
            );
        }
    }

    #[test]
    fn blockquote_wraps() {
        assert_eq!(blockquote("x"), "<blockquote>x</blockquote>");
        assert_eq!(
            blockquote("line one\nline two"),
            "<blockquote>line one\nline two</blockquote>"
        );
    }
}
