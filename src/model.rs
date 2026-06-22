//! Reddit listing data structures and the bookmark-shaping logic
//! (URL form, subreddit casing, tag construction).

use serde::Deserialize;

/// A Reddit "Listing" envelope (`{ "kind": "Listing", "data": { ... } }`).
#[derive(Debug, Deserialize)]
pub struct RedditListing {
    pub data: ListingPage,
}

/// One page of a listing: the children plus the cursor for the next page.
#[derive(Debug, Deserialize)]
pub struct ListingPage {
    /// Fullname to pass as `after` for the next page; `null` at the end.
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub children: Vec<ListingEntry>,
}

/// A single entry in a listing: `kind` is `t3` (post) or `t1` (comment), and
/// `fields` holds its payload.
#[derive(Debug, Clone, Deserialize)]
pub struct ListingEntry {
    pub kind: String,
    #[serde(rename = "data")]
    pub fields: EntryFields,
}

/// The union of the fields we care about across posts and comments. Reddit
/// returns many more; `serde` ignores the rest.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct EntryFields {
    /// Fullname, e.g. `t3_abc123`.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub subreddit: Option<String>,
    #[serde(default)]
    pub permalink: Option<String>,
    #[serde(default)]
    pub over_18: bool,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub link_flair_text: Option<String>,
    // Post (t3)
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub selftext: Option<String>,
    #[serde(default)]
    pub post_hint: Option<String>,
    #[serde(default)]
    pub is_video: bool,
    // Comment (t1)
    #[serde(default)]
    pub link_title: Option<String>,
    #[serde(default)]
    pub link_permalink: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
}

/// A normalized saved item ready to become a Pinboard bookmark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedItem {
    pub fullname: String,
    pub is_comment: bool,
    pub subreddit: String,
    /// Relative permalink, e.g. `/r/rust/comments/.../`.
    pub permalink: String,
    pub over_18: bool,
    pub description: String,
    pub extended: String,
    /// Reddit author (`author:<name>` tag).
    pub author: Option<String>,
    /// Post flair text (`flair:<slug>` tag).
    pub flair: Option<String>,
    /// Media type for posts (`image`/`video` only; `type:<x>` tag).
    pub media_type: Option<String>,
}

impl ListingEntry {
    /// Convert a listing entry into a `SavedItem`, or `None` if it is neither a
    /// post (`t3`) nor a comment (`t1`) or is missing the fields we need.
    pub fn into_saved_item(self) -> Option<SavedItem> {
        let is_comment = match self.kind.as_str() {
            "t1" => true,
            "t3" => false,
            _ => return None,
        };
        let fields = self.fields;
        let subreddit = fields.subreddit?;
        let permalink = fields.permalink?;
        let fullname = fields.name.unwrap_or_default();

        let description = if is_comment {
            fields
                .link_title
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("Comment in r/{subreddit}"))
        } else {
            fields
                .title
                .unwrap_or_else(|| format!("Post in r/{subreddit}"))
        };

        // Media-type tag, posts only and only for image/video content.
        let media_type = if is_comment {
            None
        } else {
            post_media_type(fields.is_video, fields.post_hint.as_deref())
        };

        let extended = if is_comment {
            // Prepend a link to the parent thread, which a bare comment lacks.
            let body = fields.body.unwrap_or_default();
            match parent_thread_url(fields.link_permalink.as_deref()) {
                Some(url) if body.is_empty() => format!("Thread: {url}"),
                Some(url) => format!("Thread: {url}\n\n{body}"),
                None => body,
            }
        } else {
            // Self-posts carry their text in `selftext`; link posts point elsewhere.
            let selftext = fields.selftext.unwrap_or_default();
            if selftext.is_empty() {
                fields.url.unwrap_or_default()
            } else {
                selftext
            }
        };

        Some(SavedItem {
            fullname,
            is_comment,
            subreddit,
            permalink,
            over_18: fields.over_18,
            description,
            extended,
            author: fields.author.filter(|s| !s.is_empty()),
            flair: fields.link_flair_text.filter(|s| !s.is_empty()),
            media_type,
        })
    }
}

/// Build the parent-thread URL for a comment from its `link_permalink` (which may
/// be absolute or a relative path), normalized to `old.reddit.com`.
fn parent_thread_url(link_permalink: Option<&str>) -> Option<String> {
    let lp = link_permalink.filter(|s| !s.is_empty())?;
    if lp.starts_with("http") {
        Some(lp.to_string())
    } else {
        Some(format!("https://old.reddit.com{lp}"))
    }
}

/// The media type of a post — `image` or `video` — or `None` for text/link
/// posts, which don't get a `type:` tag.
fn post_media_type(is_video: bool, post_hint: Option<&str>) -> Option<String> {
    if is_video {
        return Some("video".to_string());
    }
    match post_hint {
        Some("image") => Some("image".to_string()),
        Some("hosted:video") | Some("rich:video") => Some("video".to_string()),
        _ => None,
    }
}

impl SavedItem {
    /// The bookmark URL: the `old.reddit.com` permalink.
    pub fn bookmark_url(&self) -> String {
        format!("https://old.reddit.com{}", self.permalink)
    }

    /// Build the Pinboard tag list for this item.
    pub fn tags(&self, base_tag: &str, subreddit_prefix: &str) -> Vec<String> {
        let mut tags = vec![
            base_tag.to_string(),
            format!("{}{}", subreddit_prefix, cased_subreddit(&self.subreddit)),
        ];
        if self.is_comment {
            tags.push("reddit-comment".to_string());
        }
        if self.over_18 {
            tags.push("nsfw".to_string());
        }
        if let Some(author) = &self.author {
            tags.push(format!("author:reddit:{author}"));
        }
        if let Some(flair) = &self.flair {
            let slug = tag_slug(flair);
            if !slug.is_empty() {
                tags.push(format!("reddit-flair:{slug}"));
            }
        }
        if let Some(media_type) = &self.media_type {
            tags.push(format!("type:{media_type}"));
        }
        tags
    }
}

/// Slugify free-form text (e.g. flair) into a space-free, lowercase tag value.
fn tag_slug(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase()
}

/// Normalize a subreddit name for tagging: lowercase it, keeping the original
/// case only when it is multi-word camel/Pascal case — i.e. it has an internal
/// lowercase→uppercase boundary (`AskReddit`, `ExplainLikeImFive`). Single words
/// and all-caps lowercase: `rust`/`Rust`/`NEWS` → `rust`/`rust`/`news`.
pub fn cased_subreddit(subreddit: &str) -> String {
    let multiword_camel = subreddit
        .chars()
        .zip(subreddit.chars().skip(1))
        .any(|(a, b)| a.is_lowercase() && b.is_uppercase());
    if multiword_camel {
        subreddit.to_string()
    } else {
        subreddit.to_lowercase()
    }
}

/// Build a dedup key from a Reddit permalink path or a full Reddit URL: the
/// path, lowercased, with any query/fragment and trailing slash removed. Returns
/// `None` for non-Reddit URLs. Only the path is used, so the same post saved
/// under any reddit host (`old.`/`www.`/`m.`/none) maps to the same key.
pub fn reddit_key(path_or_url: &str) -> Option<String> {
    let path = if path_or_url.starts_with('/') {
        path_or_url
    } else {
        let after_scheme = path_or_url
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(path_or_url);
        let (host, path) = match after_scheme.find('/') {
            Some(i) => (&after_scheme[..i], &after_scheme[i..]),
            None => (after_scheme, "/"),
        };
        let host_owned = host.to_ascii_lowercase();
        // Strip any userinfo and port before checking the host.
        let host = host_owned.rsplit('@').next().unwrap_or(&host_owned);
        let host = host.split(':').next().unwrap_or(host);
        if host != "reddit.com" && !host.ends_with(".reddit.com") {
            return None;
        }
        path
    };
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let key = path.trim_end_matches('/').to_ascii_lowercase();
    if key.is_empty() {
        None
    } else {
        Some(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn casing_preserves_multiword_camel() {
        assert_eq!(cased_subreddit("AskReddit"), "AskReddit");
        assert_eq!(cased_subreddit("ExplainLikeImFive"), "ExplainLikeImFive");
        assert_eq!(cased_subreddit("IAmA"), "IAmA");
    }

    #[test]
    fn casing_lowercases_single_word_and_all_caps() {
        assert_eq!(cased_subreddit("rust"), "rust");
        assert_eq!(cased_subreddit("NEWS"), "news");
        assert_eq!(cased_subreddit("WTF"), "wtf");
        // Single-word title case is lowercased (only multi-word camel is kept).
        assert_eq!(cased_subreddit("Python"), "python");
        assert_eq!(cased_subreddit("Rust"), "rust");
    }

    fn item(kind: &str, fields: serde_json::Value) -> SavedItem {
        let entry: ListingEntry =
            serde_json::from_value(serde_json::json!({ "kind": kind, "data": fields })).unwrap();
        entry.into_saved_item().unwrap()
    }

    #[test]
    fn post_url_and_tags() {
        let it = item(
            "t3",
            serde_json::json!({
                "name": "t3_abc",
                "subreddit": "rust",
                "permalink": "/r/rust/comments/abc/title/",
                "title": "A title",
                "url": "https://example.com",
            }),
        );
        assert!(!it.is_comment);
        assert_eq!(
            it.bookmark_url(),
            "https://old.reddit.com/r/rust/comments/abc/title/"
        );
        assert_eq!(it.description, "A title");
        // A plain link post gets no `type:` tag (only image/video do).
        assert_eq!(
            it.tags("reddit", "subreddit:"),
            vec!["reddit", "subreddit:rust"]
        );
    }

    #[test]
    fn post_tags_include_author_flair_and_type() {
        let it = item(
            "t3",
            serde_json::json!({
                "name": "t3_abc", "subreddit": "rust", "permalink": "/r/rust/comments/abc/x/",
                "title": "T", "author": "alice", "link_flair_text": "Help Wanted",
                "post_hint": "image"
            }),
        );
        let tags = it.tags("reddit", "subreddit:");
        assert!(tags.contains(&"author:reddit:alice".to_string()));
        assert!(tags.contains(&"reddit-flair:help-wanted".to_string()));
        assert!(tags.contains(&"type:image".to_string()));
    }

    #[test]
    fn text_and_link_posts_get_no_type_tag_and_comment_links_its_parent_thread() {
        // A self/text post gets no `type:` tag.
        let post = item(
            "t3",
            serde_json::json!({
                "name": "t3_a", "subreddit": "rust", "permalink": "/r/rust/comments/a/x/",
                "title": "T", "is_self": true, "selftext": "hi"
            }),
        );
        assert!(!post
            .tags("reddit", "subreddit:")
            .iter()
            .any(|t| t.starts_with("type:")));

        let comment = item(
            "t1",
            serde_json::json!({
                "name": "t1_b", "subreddit": "rust", "permalink": "/r/rust/comments/a/x/b/",
                "link_title": "Parent", "link_permalink": "/r/rust/comments/a/x/", "body": "my reply"
            }),
        );
        assert_eq!(
            comment.extended,
            "Thread: https://old.reddit.com/r/rust/comments/a/x/\n\nmy reply"
        );
        // Comments get no `type:` tag.
        assert!(!comment
            .tags("reddit", "subreddit:")
            .iter()
            .any(|t| t.starts_with("type:")));
    }

    #[test]
    fn comment_gets_reddit_comment_tag_and_link_title() {
        let it = item(
            "t1",
            serde_json::json!({
                "name": "t1_xyz",
                "subreddit": "NEWS",
                "permalink": "/r/NEWS/comments/abc/title/xyz/",
                "link_title": "The story",
                "body": "my comment",
            }),
        );
        assert!(it.is_comment);
        assert_eq!(it.description, "The story");
        assert_eq!(it.extended, "my comment");
        // All-caps subreddit lowercased.
        assert_eq!(
            it.tags("reddit", "subreddit:"),
            vec!["reddit", "subreddit:news", "reddit-comment"]
        );
    }

    #[test]
    fn over_18_adds_nsfw_tag() {
        let it = item(
            "t3",
            serde_json::json!({
                "name": "t3_abc",
                "subreddit": "gonewild",
                "permalink": "/r/gonewild/comments/abc/x/",
                "title": "x",
                "over_18": true,
            }),
        );
        assert!(it
            .tags("reddit", "subreddit:")
            .contains(&"nsfw".to_string()));
    }

    #[test]
    fn reddit_key_matches_across_subdomains_and_query_and_trailing_slash() {
        let want = "/r/rust/comments/abc/title";
        // A saved item's relative permalink.
        assert_eq!(
            reddit_key("/r/rust/comments/abc/title/").as_deref(),
            Some(want)
        );
        // The same post bookmarked under any host / case / query / no trailing slash.
        for url in [
            "https://old.reddit.com/r/rust/comments/abc/title/",
            "https://www.reddit.com/r/Rust/comments/abc/Title/",
            "http://m.reddit.com/r/rust/comments/abc/title",
            "https://reddit.com/r/rust/comments/abc/title/?utm_source=x",
        ] {
            assert_eq!(reddit_key(url).as_deref(), Some(want), "url: {url}");
        }
    }

    #[test]
    fn reddit_key_rejects_non_reddit_and_empty() {
        assert_eq!(reddit_key("https://example.com/r/rust/comments/abc/"), None);
        // "reddit.com" only in the path of another host must not match.
        assert_eq!(reddit_key("https://example.com/reddit.com/x"), None);
        assert_eq!(reddit_key("https://www.reddit.com/"), None);
    }

    #[test]
    fn non_post_non_comment_kinds_are_skipped() {
        let entry: ListingEntry = serde_json::from_value(serde_json::json!({
            "kind": "t5",
            "data": { "subreddit": "rust", "permalink": "/r/rust/" }
        }))
        .unwrap();
        assert!(entry.into_saved_item().is_none());
    }

    #[test]
    fn deserializes_a_listing_with_after_and_mixed_children() {
        let listing: RedditListing = serde_json::from_str(
            r#"{
                "kind": "Listing",
                "data": {
                    "after": "t3_next",
                    "children": [
                        { "kind": "t3", "data": { "name": "t3_a", "subreddit": "rust",
                          "permalink": "/r/rust/a/", "title": "A" } },
                        { "kind": "t1", "data": { "name": "t1_b", "subreddit": "rust",
                          "permalink": "/r/rust/a/b/", "link_title": "A", "body": "hi",
                          "over_18": true } }
                    ]
                }
            }"#,
        )
        .unwrap();
        assert_eq!(listing.data.after.as_deref(), Some("t3_next"));
        assert_eq!(listing.data.children.len(), 2);
        let items: Vec<_> = listing
            .data
            .children
            .into_iter()
            .filter_map(ListingEntry::into_saved_item)
            .collect();
        assert_eq!(items.len(), 2);
        assert!(items[1].is_comment);
        assert!(items[1].over_18);
    }
}
