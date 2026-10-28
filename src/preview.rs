//! The dry-run previews' shared layout: a `[dry-run] <subject>` line, then the details
//! indented under it. Each command keeps its own labels and line format.

use std::fmt::Display;

use crate::htmltext::collapse_whitespace;
use crate::http::ELLIPSIS;
use crate::tags::Tags;

/// The indent of a preview's detail lines, under its `[dry-run] <subject>` line.
pub const INDENT: &str = "          ";

/// The line that opens one item's preview.
pub fn header(subject: impl Display) -> String {
    format!("[dry-run] {subject}")
}

/// A `label -> value` detail line, the label padded so a preview's arrows align.
pub fn field(label: &str, value: impl Display) -> String {
    format!("{INDENT}{label:<6}-> {value}")
}

/// A detail line that is not a field, such as `absorb <url>`.
pub fn detail(text: impl Display) -> String {
    format!("{INDENT}{text}")
}

/// A note as a preview shows it: `(removed)` for a write that clears it.
pub fn note(note: &str) -> String {
    if note.is_empty() {
        "(removed)".to_string()
    } else {
        note.to_string()
    }
}

/// Tags as a preview shows them, `[a b]`.
pub fn tags(tags: &Tags) -> String {
    format!("[{}]", tags.joined())
}

/// A single-line, length-bounded excerpt of (possibly multi-line) text, for dry-run output
/// only. The full text is still sent to Pinboard's `extended`.
pub fn excerpt(text: &str) -> String {
    const MAX: usize = 160;
    let one_line = collapse_whitespace(text, " ");
    if one_line.chars().count() > MAX {
        let head: String = one_line.chars().take(MAX).collect();
        format!("{head}{ELLIPSIS}")
    } else {
        one_line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpt_collapses_whitespace() {
        assert_eq!(excerpt("line1\nline2   line3"), "line1 line2 line3");
    }

    #[test]
    fn excerpt_truncates_long_text_with_ellipsis() {
        let long = "word ".repeat(100);
        let p = excerpt(&long);
        assert!(p.ends_with('…'), "{p:?}");
        assert_eq!(p.chars().count(), 161);
    }

    #[test]
    fn excerpt_keeps_text_of_exactly_the_limit_whole() {
        let limit = "é".repeat(160);
        assert_eq!(excerpt(&limit), limit);
    }
}
