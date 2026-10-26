//! Reddit listing data structures and the bookmark-shaping logic
//! (URL form, subreddit casing, tag construction).

use serde::{Deserialize, Deserializer};
use url::Url;

use crate::bookmark::Bookmark;
use crate::htmltext::{blockquote, html_to_plain};
use crate::source::BookmarkDraft;
use crate::tags::{push_prefixed, push_tag, push_tags};
use crate::urls::host_matches;

/// A Reddit "Listing" envelope (`{ "kind": "Listing", "data": { ... } }`).
#[derive(Debug, Deserialize)]
pub struct RedditListing {
    pub data: RedditListingPage,
}

/// One page of a listing: the children plus the cursor for the next page.
#[derive(Debug, Deserialize)]
pub struct RedditListingPage {
    /// Fullname to pass as `after` for the next page; `null` at the end.
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default, deserialize_with = "deserialize_lenient_children")]
    pub children: Vec<RedditListingEntry>,
}

/// Deserialize a listing's children, skipping (with a warning) any single entry that
/// doesn't match `RedditListingEntry` so one malformed item can't discard a whole page. A
/// body that isn't a JSON array of objects still fails, keeping an anti-bot HTML page
/// a genuine error. A non-empty page where *every* child fails is also an error (a schema
/// break) rather than a silently empty page.
fn deserialize_lenient_children<'de, D>(
    deserializer: D,
) -> Result<Vec<RedditListingEntry>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error as _;

    let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
    crate::source::deserialize_lenient(raw, "reddit listing entry", |count| {
        D::Error::custom(format!(
            "all {count} reddit listing child(ren) failed to deserialize — \
             the API response shape may have changed"
        ))
    })
}

/// A single entry in a listing: `kind` is `t3` (post) or `t1` (comment), and
/// `fields` holds its payload.
#[derive(Debug, Clone, Deserialize)]
pub struct RedditListingEntry {
    pub kind: RedditKind,
    #[serde(rename = "data")]
    pub fields: RedditEntryFields,
}

/// A listing entry's `kind`: Reddit's type prefix for the thing it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedditKind {
    /// `t3`.
    Post,
    /// `t1`.
    Comment,
    /// Any other thing (a subreddit, an award, ...), which never becomes a bookmark.
    Other,
}

/// From a string only. A derived enum impl would also accept a single-key map such as
/// `{"t3": null}`, letting a malformed entry through the lenient listing parse.
impl<'de> Deserialize<'de> for RedditKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "t3" => RedditKind::Post,
            "t1" => RedditKind::Comment,
            _ => RedditKind::Other,
        })
    }
}

/// The union of the fields we care about across posts and comments. Reddit
/// returns many more; `serde` ignores the rest.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct RedditEntryFields {
    /// Fullname, e.g. `t3_abc123`.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub subreddit: Option<String>,
    #[serde(default)]
    pub permalink: Option<String>,
    #[serde(default)]
    pub over_18: bool,
    /// Post/comment creation time (unix epoch seconds, as a float).
    #[serde(default)]
    pub created_utc: Option<f64>,
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
    /// True for text/self posts (whose `url` is the post's own permalink).
    #[serde(default)]
    pub is_self: bool,
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
#[derive(Debug, Clone, PartialEq)]
pub struct RedditSavedItem {
    pub fullname: String,
    pub is_comment: bool,
    pub subreddit: String,
    /// Relative permalink, e.g. `/r/rust/comments/.../`.
    pub permalink: String,
    pub over_18: bool,
    pub title: String,
    pub note: String,
    /// Post/comment creation time (unix epoch seconds), when present.
    pub created_utc: Option<f64>,
    /// Reddit author (`author:<name>` tag).
    pub author: Option<String>,
    /// Post flair text (`flair:<slug>` tag).
    pub flair: Option<String>,
    /// Media type for posts (`image`/`video` only; `type:<x>` tag).
    pub media_type: Option<String>,
}

impl RedditListingEntry {
    /// Convert a listing entry into a `RedditSavedItem`, or `None` if it is neither a
    /// post (`t3`) nor a comment (`t1`) or is missing the fields we need. `domain`
    /// is the reddit host used for the parent-thread link prepended to comments.
    ///
    /// A post/comment missing `subreddit`/`permalink` is dropped *with a warning*: every
    /// `RedditEntryFields` field is an optional default, so a renamed required subfield
    /// still deserializes (as `None`) and slips past the listing's all-fail guard — the
    /// warning keeps that silent loss visible.
    pub fn into_saved_item(self, domain: &str) -> Option<RedditSavedItem> {
        let (is_comment, wire_kind) = match self.kind {
            RedditKind::Comment => (true, "t1"),
            RedditKind::Post => (false, "t3"),
            RedditKind::Other => return None,
        };
        let fields = self.fields;
        let (subreddit, permalink) = match (fields.subreddit, fields.permalink) {
            (Some(subreddit), Some(permalink)) => (subreddit, permalink),
            _ => {
                log::warn!(
                    "skipping reddit {wire_kind} entry: missing subreddit/permalink — \
                     the Reddit response shape may have changed"
                );
                return None;
            }
        };
        let fullname = fields.name.unwrap_or_default();

        let title = if is_comment {
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

        let note = reddit_note(
            is_comment,
            fields.body.as_deref().unwrap_or_default(),
            fields.selftext.as_deref().unwrap_or_default(),
            fields.url.as_deref().unwrap_or_default(),
            fields.is_self,
            fields.link_permalink.as_deref(),
            domain,
        );

        Some(RedditSavedItem {
            fullname,
            is_comment,
            subreddit,
            permalink,
            over_18: fields.over_18,
            title,
            note,
            created_utc: fields.created_utc,
            author: fields.author.filter(|s| !s.is_empty() && s != "[deleted]"),
            flair: fields.link_flair_text.filter(|s| !s.is_empty()),
            media_type,
        })
    }
}

/// Build the Pinboard notes for a Reddit item: the quoted body (comment `body` / post
/// `selftext`) wrapped in a literal `<blockquote>`, which Pinboard renders. For comments
/// the parent-thread link is prepended outside the quote (a bare comment lacks it). For an
/// empty self-post the notes are empty (its `url` is the post's own permalink, i.e. the
/// bookmark URL — not worth duplicating); a link post keeps its external URL as the bare
/// fallback. Shared by sync and cleanup. Reddit text is already Markdown (`raw_json=1`),
/// so it is wrapped as-is, never HTML-converted.
pub(crate) fn reddit_note(
    is_comment: bool,
    body: &str,
    selftext: &str,
    url: &str,
    is_self: bool,
    link_permalink: Option<&str>,
    domain: &str,
) -> String {
    if is_comment {
        let quoted = if body.is_empty() {
            String::new()
        } else {
            blockquote(body)
        };
        match parent_thread_url(link_permalink, domain) {
            Some(thread) if quoted.is_empty() => format!("Thread: {thread}"),
            Some(thread) => format!("Thread: {thread}\n\n{quoted}"),
            None => quoted,
        }
    } else if !selftext.is_empty() {
        blockquote(selftext)
    } else if is_self {
        // Empty self-post: its `url` is its own permalink, so add nothing.
        String::new()
    } else {
        // Link post: keep the external URL.
        url.to_string()
    }
}

/// Build the parent-thread URL for a comment from its `link_permalink` (which may
/// be absolute or a relative path), relative paths resolved against `domain`.
fn parent_thread_url(link_permalink: Option<&str>, domain: &str) -> Option<String> {
    let lp = link_permalink.filter(|s| !s.is_empty())?;
    if lp.starts_with("http") {
        Some(lp.to_string())
    } else {
        Some(format!("https://{domain}{lp}"))
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

/// Per-account reddit config: the host used in bookmark/thread URLs plus the tag
/// vocabulary. `tags` are applied to every bookmark (defaulting to `["reddit"]`);
/// the `*_prefix`/conditional fields each default to a built-in value, and an empty
/// string disables that tag. `media_types` is the allowlist of post media types
/// (`image`/`video`) that get a `media_prefix` tag.
#[derive(Debug, Clone)]
pub struct RedditConfig {
    pub domain: String,
    pub tags: Vec<String>,
    pub subreddit_prefix: String,
    pub comment: String,
    pub nsfw: String,
    pub author_prefix: String,
    pub flair_prefix: String,
    pub media_prefix: String,
    pub media_types: Vec<String>,
}

impl Default for RedditConfig {
    fn default() -> Self {
        Self {
            domain: "old.reddit.com".into(),
            tags: vec!["reddit".into()],
            subreddit_prefix: "subreddit:".into(),
            comment: "reddit-comment".into(),
            nsfw: "nsfw".into(),
            author_prefix: "author:reddit:".into(),
            flair_prefix: "reddit-flair:".into(),
            media_prefix: "type:".into(),
            media_types: vec!["image".into(), "video".into()],
        }
    }
}

impl RedditSavedItem {
    /// The bookmark URL: the permalink under the configured reddit `domain`.
    pub fn bookmark_url(&self, domain: &str) -> String {
        format!("https://{domain}{}", self.permalink)
    }

    /// Build the Pinboard tag list for this item from the reddit config.
    pub fn tags(&self, cfg: &RedditConfig) -> Vec<String> {
        let mut tags = Vec::new();
        push_tags(&mut tags, &cfg.tags);
        push_prefixed(
            &mut tags,
            &cfg.subreddit_prefix,
            &cased_subreddit(&self.subreddit),
        );
        if self.is_comment {
            push_tag(&mut tags, &cfg.comment);
        }
        if self.over_18 {
            push_tag(&mut tags, &cfg.nsfw);
        }
        tags.extend(self.detail_tags(cfg).into_iter().map(|(_, tag)| tag));
        tags
    }

    /// The author, flair and media-type tags, each paired with the config prefix that
    /// marks its family. Shared with `cleanup`, which backfills a family only when the
    /// stored bookmark has no tag with that prefix.
    pub(crate) fn detail_tags<'c>(&self, cfg: &'c RedditConfig) -> Vec<(&'c str, String)> {
        let mut families: Vec<(&'c str, String)> = Vec::new();
        let mut push = |prefix: &'c str, value: &str| {
            let mut tag = Vec::new();
            push_prefixed(&mut tag, prefix, value);
            families.extend(tag.into_iter().map(|tag| (prefix, tag)));
        };
        if let Some(author) = &self.author {
            push(&cfg.author_prefix, author);
        }
        if let Some(flair) = &self.flair {
            push(&cfg.flair_prefix, &flair.to_lowercase());
        }
        if let Some(media_type) = &self.media_type {
            if cfg.media_types.iter().any(|t| t == media_type) {
                push(&cfg.media_prefix, media_type);
            }
        }
        families
    }

    /// Shape this item into a Pinboard draft using the reddit config. `None` (with a
    /// warning) if the constructed bookmark URL doesn't parse.
    pub fn into_draft(self, cfg: &RedditConfig) -> Option<BookmarkDraft> {
        let url = match Url::parse(&self.bookmark_url(&cfg.domain)) {
            Ok(url) => url,
            Err(e) => {
                log::warn!("skipping reddit item {}: bad URL: {e}", self.fullname);
                return None;
            }
        };
        // The host-agnostic dedup key, derived from the same URL.
        let dedup_key = reddit_key(&url).unwrap_or_else(|| url.to_string());
        let timestamp = self
            .created_utc
            .and_then(|s| crate::timefmt::from_unix(s as i64));
        let tags = self.tags(cfg);
        Some(BookmarkDraft {
            bookmark: Bookmark {
                url,
                title: html_to_plain(&self.title),
                note: self.note,
                tags,
                timestamp,
                public: false,
                read_later: false,
            },
            dedup_key,
        })
    }
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

/// Build a dedup key from a full Reddit URL: the path, lowercased, with query/fragment
/// and trailing slash removed. Returns `None` for non-Reddit URLs. Only the path is
/// used, so the same post saved under any reddit host (`old.`/`www.`/`m.`) maps to the
/// same key.
pub fn reddit_key(url: &Url) -> Option<String> {
    if !host_matches(url.host_str()?, "reddit.com") {
        return None;
    }
    let key = url.path().trim_end_matches('/').to_ascii_lowercase();
    (!key.is_empty()).then_some(key)
}

#[cfg(test)]
mod tests;
