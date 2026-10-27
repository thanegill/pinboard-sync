//! A bookmark's tag list: the helpers every source shapes its configured tags through,
//! where an empty tag means "disabled" and whitespace and commas can't survive (Pinboard
//! splits the tag string on whitespace and forbids commas).

use std::collections::BTreeSet;

/// Append `key` to `tags` unless it is empty (empty = the tag is disabled).
pub fn push_tag(tags: &mut Vec<String>, key: &str) {
    if !key.is_empty() {
        tags.push(key.to_string());
    }
}

/// Append every non-empty key in `keys` to `tags` (via [`push_tag`]). Used to seed a
/// draft's tags from a source's configurable base `tags` list.
pub fn push_tags(tags: &mut Vec<String>, keys: &[String]) {
    for key in keys {
        push_tag(tags, key);
    }
}

/// `prefix + value`, with each internal run of whitespace and commas in `value`
/// collapsed to `-` (Pinboard tags can't contain either — the API splits on whitespace,
/// and its docs forbid commas). `None` if `prefix` is empty or `value` is nothing but
/// whitespace and commas.
pub fn prefixed(prefix: &str, value: &str) -> Option<String> {
    let value = crate::htmltext::collapse_whitespace(&value.replace(',', " "), "-");
    (!prefix.is_empty() && !value.is_empty()).then(|| format!("{prefix}{value}"))
}

/// Append [`prefixed`]`(prefix, value)` to `tags`, if there is one.
pub fn push_prefixed(tags: &mut Vec<String>, prefix: &str, value: &str) {
    tags.extend(prefixed(prefix, value));
}

/// Append `tag` unless it is empty or already in `tags`. Empty tags are skipped for
/// parity with [`push_tag`] (empty = the tag is disabled): stored tags derive via
/// `split_whitespace` and so can never contain `""`, so emitting one would make a
/// cleanup plan diff against storage on every run and never converge.
pub fn push_unique(tags: &mut Vec<String>, tag: &str) {
    if !tag.is_empty() && !tags.iter().any(|held| held == tag) {
        tags.push(tag.to_string());
    }
}

/// [`push_unique`] each tag in `add`, in order.
pub fn extend_unique(tags: &mut Vec<String>, add: &[String]) {
    for tag in add {
        push_unique(tags, tag);
    }
}

/// Whether two tag lists differ as sets (order- and duplicate-insensitive).
pub fn tags_differ(a: &[String], b: &[String]) -> bool {
    a.iter().collect::<BTreeSet<_>>() != b.iter().collect::<BTreeSet<_>>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::strings as tags;

    #[test]
    fn push_prefixed_slugs_internal_whitespace() {
        let mut tags = Vec::new();
        push_prefixed(&mut tags, "lang:", "Jupyter Notebook");
        push_prefixed(&mut tags, "x:", "  spaced  out ");
        push_prefixed(&mut tags, "y:", "   "); // all whitespace → skipped
        push_prefixed(&mut tags, "", "v"); // empty prefix → skipped
        assert_eq!(tags, vec!["lang:Jupyter-Notebook", "x:spaced-out"]);
    }

    #[test]
    fn push_prefixed_slugs_commas_like_whitespace() {
        let mut tags = Vec::new();
        push_prefixed(&mut tags, "f:", "Help, Question");
        push_prefixed(&mut tags, "g:", "a,,b");
        push_prefixed(&mut tags, "h:", " , "); // nothing but separators → skipped
        assert_eq!(tags, vec!["f:Help-Question", "g:a-b"]);
    }

    #[test]
    fn extend_unique_skips_empty_and_duplicate_tags() {
        let mut acc = tags(&["reddit"]);
        // An empty base tag (config `tags = [""]`) must not add a literal "" — stored
        // tags can never contain one, so it would diff against storage forever.
        extend_unique(&mut acc, &tags(&["", "reddit", "rust"]));
        assert_eq!(acc, tags(&["reddit", "rust"]));
    }

    #[test]
    fn tags_differ_is_set_based() {
        // A duplicate on one side must not read as a difference.
        assert!(!tags_differ(
            &tags(&["reddit"]),
            &tags(&["reddit", "reddit"])
        ));
        // Order-insensitive.
        assert!(!tags_differ(&tags(&["a", "b"]), &tags(&["b", "a"])));
        // Genuinely different sets still differ.
        assert!(tags_differ(&tags(&["reddit"]), &tags(&["reddit", "rust"])));
        assert!(tags_differ(&tags(&["a", "a"]), &tags(&["a", "b"])));
    }
}
