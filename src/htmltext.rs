//! Shape source-provided text for Pinboard. Titles become single-line plain text
//! (tags stripped, entities decoded, whitespace collapsed); HackerNews note bodies
//! (raw Algolia HTML) become Markdown; quoted bodies are wrapped in a literal
//! `<blockquote>`, which Pinboard renders in the notes field.

/// Clean a title (`description`): decode HTML entities and collapse all internal
/// whitespace to single spaces (trimmed). Source titles (GitHub descriptions, HN
/// titles) are plain text, not markup, so literal angle brackets (`Vec<String>`,
/// `vector<T>`) are preserved verbatim rather than dropped as HTML tags. Literal `<`/`>`
/// are escaped before parsing so scraper decodes entities without treating a `<...>`
/// span as an element to strip. The `cleanup` path runs stored Pinboard titles through
/// this too, so a stored title that happens to contain real markup is likewise kept
/// verbatim rather than stripped — the deliberate cost of treating every title as plain
/// text (see the `cleanup` tests).
pub fn html_to_plain(s: &str) -> String {
    let escaped = s.replace('<', "&lt;").replace('>', "&gt;");
    let fragment = scraper::Html::parse_fragment(&escaped);
    let text: String = fragment.root_element().text().collect();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Convert a HackerNews note body (raw HTML from Algolia `story_text`/`comment_text`)
/// to Markdown. Only ever run over HN HTML — never over Reddit/GitHub bodies, which are
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

    #[test]
    fn blockquote_wraps() {
        assert_eq!(blockquote("x"), "<blockquote>x</blockquote>");
        assert_eq!(
            blockquote("line one\nline two"),
            "<blockquote>line one\nline two</blockquote>"
        );
    }
}
