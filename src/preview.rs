//! The dry-run previews' shared layout: a `[dry-run] <subject>` line, then the details
//! indented under it. Each command keeps its own labels and line format.

use std::fmt::Display;

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
