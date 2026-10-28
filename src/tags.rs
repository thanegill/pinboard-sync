//! A bookmark's tag list: the [`Tags`] every source shapes its configured tags into,
//! where an empty tag means "disabled" and whitespace and commas can't survive (Pinboard
//! splits the tag string on whitespace and forbids commas).

use std::collections::BTreeSet;
use std::ops::Deref;

/// A bookmark's tags, in order. Pinboard keeps them case-sensitively and stores them as
/// one space-separated string. Read like a slice; every method that adds a tag skips an
/// empty one (a disabled tag).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tags(Vec<String>);

impl Tags {
    /// The tags in Pinboard's space-separated tag string.
    pub fn parse(stored: &str) -> Self {
        Self(stored.split_whitespace().map(String::from).collect())
    }

    /// Pinboard's space-separated form, the inverse of [`Tags::parse`], with each tag once
    /// in first-seen order: the set [`Tags::differs_from`] compares, so a repeat can't make
    /// a write, or the URL budget it is measured against, differ from that set.
    pub fn joined(&self) -> String {
        let mut seen = BTreeSet::new();
        self.0
            .iter()
            .filter(|tag| seen.insert(tag.as_str()))
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Append `tag` unless it is empty (empty = the tag is disabled).
    pub fn push(&mut self, tag: &str) {
        if !tag.is_empty() {
            self.0.push(tag.to_string());
        }
    }

    /// [`Tags::push`] every tag in `tags`. Used to seed a draft's tags from a source's
    /// configurable base `tags` list.
    pub fn push_all(&mut self, tags: &[String]) {
        for tag in tags {
            self.push(tag);
        }
    }

    /// Append [`prefixed`]`(prefix, value)`, if there is one.
    pub fn push_prefixed(&mut self, prefix: &str, value: &str) {
        self.0.extend(prefixed(prefix, value));
    }

    /// Append `tag` unless it is empty or already here. Empty tags are skipped for
    /// parity with [`Tags::push`]: stored tags derive via `split_whitespace` and so can
    /// never contain `""`, so emitting one would make a cleanup plan diff against
    /// storage on every run and never converge.
    pub fn push_unique(&mut self, tag: &str) {
        if !tag.is_empty() && !self.has(tag) {
            self.0.push(tag.to_string());
        }
    }

    /// [`Tags::push_unique`] each tag in `add`, in order.
    pub fn extend_unique(&mut self, add: &[String]) {
        for tag in add {
            self.push_unique(tag);
        }
    }

    /// Keep only the tags `keep` accepts.
    pub fn retain(&mut self, keep: impl FnMut(&String) -> bool) {
        self.0.retain(keep);
    }

    /// Sort the tags, for a plan whose order is canonical.
    pub fn sort(&mut self) {
        self.0.sort();
    }

    /// Whether `tag` is one of the tags.
    pub fn has(&self, tag: &str) -> bool {
        self.0.iter().any(|held| held == tag)
    }

    /// Whether the two lists differ as sets (order- and duplicate-insensitive).
    pub fn differs_from(&self, other: &Tags) -> bool {
        self.0.iter().collect::<BTreeSet<_>>() != other.0.iter().collect::<BTreeSet<_>>()
    }
}

impl Deref for Tags {
    type Target = [String];

    fn deref(&self) -> &[String] {
        &self.0
    }
}

#[cfg(test)]
impl From<Vec<String>> for Tags {
    fn from(tags: Vec<String>) -> Self {
        Self(tags)
    }
}

impl FromIterator<String> for Tags {
    fn from_iter<I: IntoIterator<Item = String>>(tags: I) -> Self {
        Self(tags.into_iter().collect())
    }
}

#[cfg(test)]
impl<T> PartialEq<Vec<T>> for Tags
where
    String: PartialEq<T>,
{
    fn eq(&self, other: &Vec<T>) -> bool {
        self.0 == *other
    }
}

#[cfg(test)]
impl<T, const N: usize> PartialEq<[T; N]> for Tags
where
    String: PartialEq<T>,
{
    fn eq(&self, other: &[T; N]) -> bool {
        self.0 == *other
    }
}

/// `prefix + value`, with each internal run of whitespace and commas in `value`
/// collapsed to `-` (Pinboard tags can't contain either -- the API splits on whitespace,
/// and its docs forbid commas). `None` if `prefix` is empty or `value` is nothing but
/// whitespace and commas.
pub fn prefixed(prefix: &str, value: &str) -> Option<String> {
    let value = crate::htmltext::collapse_whitespace(&value.replace(',', " "), "-");
    (!prefix.is_empty() && !value.is_empty()).then(|| format!("{prefix}{value}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::strings;

    fn tags(tags: &[&str]) -> Tags {
        strings(tags).into()
    }

    #[test]
    fn joined_keeps_each_tag_once_in_first_seen_order() {
        assert_eq!(tags(&["b", "a", "b", "c", "a"]).joined(), "b a c");
    }

    #[test]
    fn push_prefixed_slugs_internal_whitespace() {
        let mut tags = Tags::default();
        tags.push_prefixed("lang:", "Jupyter Notebook");
        tags.push_prefixed("x:", "  spaced  out ");
        tags.push_prefixed("y:", "   "); // all whitespace -> skipped
        tags.push_prefixed("", "v"); // empty prefix -> skipped
        assert_eq!(tags, ["lang:Jupyter-Notebook", "x:spaced-out"]);
    }

    #[test]
    fn push_prefixed_slugs_commas_like_whitespace() {
        let mut tags = Tags::default();
        tags.push_prefixed("f:", "Help, Question");
        tags.push_prefixed("g:", "a,,b");
        tags.push_prefixed("h:", " , "); // nothing but separators -> skipped
        assert_eq!(tags, ["f:Help-Question", "g:a-b"]);
    }

    #[test]
    fn push_skips_a_disabled_tag() {
        let mut held = Tags::default();
        held.push("reddit");
        held.push("");
        held.push_all(&strings(&["", "x"]));
        assert_eq!(held, ["reddit", "x"]);
    }

    #[test]
    fn extend_unique_skips_empty_and_duplicate_tags() {
        let mut acc = tags(&["reddit"]);
        // An empty base tag (config `tags = [""]`) must not add a literal "" -- stored
        // tags can never contain one, so it would diff against storage forever.
        acc.extend_unique(&strings(&["", "reddit", "rust"]));
        assert_eq!(acc, ["reddit", "rust"]);
    }

    #[test]
    fn has_matches_a_whole_tag_case_sensitively() {
        let held = tags(&["reddit", "subreddit:rust"]);
        assert!(held.has("reddit"), "{held:?}");
        assert!(!held.has("Reddit"), "{held:?}");
        assert!(!held.has("subreddit"), "{held:?}");
        assert!(!held.has(""), "{held:?}");
    }

    #[test]
    fn differs_from_is_set_based() {
        let differs = |a: &[&str], b: &[&str]| tags(a).differs_from(&tags(b));
        assert!(
            !differs(&["reddit"], &["reddit", "reddit"]),
            "a duplicate on one side"
        );
        assert!(!differs(&["a", "b"], &["b", "a"]), "another order");
        assert!(
            differs(&["reddit"], &["reddit", "rust"]),
            "an extra tag on one side"
        );
        assert!(differs(&["a", "a"], &["a", "b"]), "a different tag");
    }

    #[test]
    fn a_repeated_tag_is_written_once_at_its_first_place() {
        assert_eq!(Tags::parse("a b a").joined(), "a b");
        let mut held = tags(&["b", "a"]);
        held.push("b");
        held.push_all(&strings(&["c", "a"]));
        assert_eq!(held.joined(), "b a c");
    }

    #[test]
    fn sort_orders_the_written_tags() {
        let mut held = Tags::parse("b c a b");
        held.sort();
        assert_eq!(held.joined(), "a b c");
    }

    #[test]
    fn retain_drops_every_copy_of_a_tag() {
        let mut held = Tags::parse("a b a c");
        held.retain(|tag| tag != "a");
        assert_eq!(held.joined(), "b c");
    }

    #[test]
    fn equality_sees_order() {
        assert_ne!(tags(&["a", "b"]), tags(&["b", "a"]));
        assert_eq!(tags(&["a", "b"]), tags(&["a", "b"]));
    }

    #[test]
    fn parse_and_joined_round_trip_pinboards_tag_string() {
        let parsed = Tags::parse("  reddit  subreddit:rust ");
        assert_eq!(parsed, ["reddit", "subreddit:rust"]);
        assert_eq!(parsed.joined(), "reddit subreddit:rust");
    }
}
