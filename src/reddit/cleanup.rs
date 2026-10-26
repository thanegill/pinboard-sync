//! `cleanup` subcommand: normalize existing Reddit bookmarks in a Pinboard
//! account. Rewrites URLs to `old.reddit.com` (unwrapping `over18` redirects),
//! normalizes tags, and — using Reddit's `/api/info` for authoritative data —
//! marks NSFW posts and replaces generic placeholder titles.

use std::collections::{BTreeSet, HashMap};

use anyhow::Result;

use crate::bookmark::{AccountState, Bookmark, BookmarkStore};
use crate::cleanup_pass::{run_pass, CleanupPass, Plan};
use crate::htmltext::html_to_plain;
use crate::reddit::model::{cased_subreddit, reddit_key, RedditConfig};
use crate::reddit::PostInfo;
use crate::source::{host_matches, push_prefixed, push_tags, SourceError, UrlExt};
use crate::timefmt::DateOpts;
use url::Url;

pub struct RedditCleanupOpts {
    pub mark_nsfw: bool,
    pub fix_titles: bool,
    /// The account's tag vocabulary and domain, the same ones `sync` writes with: URLs are
    /// rewritten to `domain`, and `nsfw` is the NSFW marker tag (empty disables it).
    pub config: RedditConfig,
    /// Whether and how to re-date bookmarks by the source post date.
    pub dates: DateOpts,
}

/// Authoritative per-post data from `/api/info`.
struct RedditPostMeta {
    over_18: bool,
    title: Option<String>,
    /// Post creation time (unix epoch seconds), for `use_post_date`.
    created_utc: Option<f64>,
    /// The notes rebuilt from authoritative data — the quoted body wrapped in a
    /// `<blockquote>` (empty when there's nothing to quote). Used to retrofit existing
    /// bookmarks to the shape `sync` now writes.
    note: String,
    /// The raw `selftext`, i.e. the rebuilt notes before the `<blockquote>` wrapping —
    /// the shape an older sync wrote. Lets the retrofit recognize its own past output.
    selftext: String,
}

pub async fn run<S: BookmarkStore + AccountState, R: PostInfo>(
    store: &S,
    reddit: Option<&R>,
    opts: &RedditCleanupOpts,
) -> Result<(), SourceError> {
    // Taken from the run's live view, so an earlier source's writes are already reflected.
    let reddit_bms: Vec<Bookmark> = store
        .snapshot()
        .into_iter()
        .filter(|bookmark| bookmark.url.host_is("reddit.com"))
        .collect();

    let info = fetch_post_info(reddit, opts, &reddit_bms).await?;
    let pass = RedditCleanupPass { info, opts };
    run_pass(store, &reddit_bms, "reddit", opts.dates, &pass)
        .await
        .into_result()
}

/// Re-shapes one reddit bookmark: normalize the URL/tags, then apply the authoritative
/// `/api/info` data (NSFW marker, placeholder-title replacement, post date, rebuilt notes).
struct RedditCleanupPass<'a> {
    info: HashMap<String, RedditPostMeta>,
    opts: &'a RedditCleanupOpts,
}

impl CleanupPass for RedditCleanupPass<'_> {
    async fn plan(&self, bookmark: &Bookmark) -> Result<Plan, SourceError> {
        let opts = self.opts;
        // The stored URL is already a parsed `Url` (and a reddit one — the pass is filtered
        // to those); normalize it, falling back to the original when nothing changed.
        let new_url = normalize_url(&bookmark.url, &opts.config.domain)
            .unwrap_or_else(|| bookmark.url.clone());

        let mut tags = normalize_tags(
            &new_url,
            &bookmark.tags,
            &opts.config.tags,
            &opts.config.subreddit_prefix,
            &opts.config.nsfw,
        );
        let post = post_fullname(&new_url).and_then(|f| self.info.get(&f));

        if opts.mark_nsfw
            && !opts.config.nsfw.is_empty()
            && post.is_some_and(|p| p.over_18)
            && !tags.iter().any(|t| t == &opts.config.nsfw)
        {
            tags.push(opts.config.nsfw.clone());
            tags.sort();
        }

        let mut title = html_to_plain(&bookmark.title);
        if opts.fix_titles && is_placeholder_title(&title) {
            if let Some(real_title) = post.and_then(|p| p.title.clone()) {
                title = html_to_plain(&real_title);
            }
        }

        // Reshape the notes. Only for *post* bookmarks: `/api/info` is keyed by the post
        // fullname (`post_fullname`), so a comment's own body is never fetched — leave those
        // notes alone. When we have authoritative data, retrofit the rebuild (selftext in a
        // <blockquote>, or a link post's external URL) onto the stored notes *only* when
        // those notes are ones the tool itself wrote (empty, the current rebuild, the raw
        // pre-<blockquote> selftext, or a bare self-link an older sync left) — a hand-written
        // annotation on a manually-saved bookmark is preserved. Without an entry this run,
        // only drop a bare self-link; never wipe genuine notes.
        let note = if is_comment_url(&new_url) {
            bookmark.note.clone()
        } else if let Some(post) = post {
            if note_is_tool_generated(&bookmark.note, &post.note, &post.selftext, &new_url) {
                post.note.clone()
            } else {
                bookmark.note.clone()
            }
        } else if is_self_link_notes(&bookmark.note, &new_url) {
            String::new()
        } else {
            bookmark.note.clone()
        };

        // `/api/info` is keyed by the post fullname, so `post` for a comment bookmark is the
        // *parent thread's* meta; its `created_utc` is the thread-creation time, not the
        // comment's. Never re-date a comment by it -- leave a None candidate so the driver
        // keeps the comment's stored timestamp (as with its notes above).
        let timestamp = if is_comment_url(&new_url) {
            None
        } else {
            post.and_then(|p| p.created_utc)
                .and_then(|s| crate::timefmt::from_unix(s as i64))
        };

        Ok(Plan::Bookmark(Bookmark {
            url: new_url,
            title,
            note,
            tags,
            timestamp,
            public: bookmark.public,
            read_later: bookmark.read_later,
        }))
    }
}

/// Batch-fetch `over_18` + `title` + `created_utc` + the rebuilt notes for every post
/// referenced by the bookmarks, keyed by fullname. Empty when none of NSFW tagging, title
/// fixing, or `use_post_date` is requested — the notes retrofit then rides on whichever of
/// those caused the `/api/info` call (it needs no extra fetch of its own).
async fn fetch_post_info<R: PostInfo>(
    reddit: Option<&R>,
    opts: &RedditCleanupOpts,
    bookmarks: &[Bookmark],
) -> Result<HashMap<String, RedditPostMeta>, SourceError> {
    let mut map = HashMap::new();
    if !(opts.mark_nsfw || opts.fix_titles || opts.dates.use_post_date) {
        return Ok(map);
    }
    let Some(reddit) = reddit else {
        return Ok(map);
    };

    let fullnames: Vec<String> = bookmarks
        .iter()
        .filter_map(|bookmark| {
            let url = normalize_url(&bookmark.url, &opts.config.domain)
                .unwrap_or_else(|| bookmark.url.clone());
            post_fullname(&url)
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if fullnames.is_empty() {
        return Ok(map);
    }

    let entries = reddit.info(&fullnames).await?;
    for entry in entries {
        if let Some(name) = entry.fields.name.clone() {
            // Rebuild the notes from authoritative data the same way sync does, so the
            // <blockquote> shaping retrofits onto existing bookmarks. These entries are
            // always posts (`/api/info` is queried with post fullnames), so build the
            // post-shaped notes — selftext quoted, empty for an empty self-post, else the
            // external URL.
            let selftext = entry.fields.selftext.clone().unwrap_or_default();
            let note = crate::reddit::model::reddit_note(
                false,
                "",
                &selftext,
                entry.fields.url.as_deref().unwrap_or_default(),
                entry.fields.is_self,
                None,
                &opts.config.domain,
            );
            map.insert(
                name,
                RedditPostMeta {
                    over_18: entry.fields.over_18,
                    title: entry.fields.title.filter(|s| !s.is_empty()),
                    created_utc: entry.fields.created_utc,
                    note,
                    selftext,
                },
            );
        }
    }
    Ok(map)
}

// --- pure transforms ---------------------------------------------------------

/// Normalize a Reddit bookmark URL: unwrap an `over18/?dest=` redirect, then rewrite
/// any reddit host to the configured `domain`. Returns `Some(new)` only if it changed.
pub fn normalize_url(url: &Url, domain: &str) -> Option<Url> {
    let mut current = url.clone();
    let mut changed = false;
    if let Some(dest) = over18_dest(&current) {
        current = dest;
        changed = true;
    }
    if let Some(rewritten) = to_reddit_domain(&current, domain) {
        current = rewritten;
        changed = true;
    }
    changed.then_some(current)
}

/// If `url` is a reddit `over18` interstitial, return its `dest` target (the `url`
/// crate percent-decodes the query value).
fn over18_dest(url: &Url) -> Option<Url> {
    if !host_matches(url.host_str()?, "reddit.com")
        || !url.path().trim_start_matches('/').starts_with("over18")
    {
        return None;
    }
    let dest = url.query_pairs().find(|(key, _)| key == "dest")?.1;
    Url::parse(&dest).ok()
}

/// Rewrite any reddit host (`reddit.com` or a `*.reddit.com` subdomain) to the
/// configured `domain` (over https), preserving the rest of the URL. Returns `None`
/// for a non-reddit host or one already equal to `domain`.
fn to_reddit_domain(url: &Url, domain: &str) -> Option<Url> {
    let host = url.host_str()?;
    if !(host_matches(host, "reddit.com") && host != domain) {
        return None;
    }
    let mut rewritten = url.clone();
    rewritten.set_scheme("https").ok()?;
    rewritten.set_host(Some(domain)).ok()?;
    Some(rewritten)
}

/// Extract the subreddit name from a Reddit URL's `/r/<sub>/` segment.
pub fn extract_subreddit(url: &Url) -> Option<String> {
    let (_, after) = url.path().split_once("/r/")?;
    let sub: String = after
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    (!sub.is_empty()).then_some(sub)
}

/// The post fullname (`t3_<id>`) from a permalink's `/comments/<id>/` segment.
/// Works for both post and comment permalinks (comments inherit the post id).
pub fn post_fullname(url: &Url) -> Option<String> {
    let (_, after) = url.path().split_once("/comments/")?;
    let id: String = after.chars().take_while(|c| c.is_alphanumeric()).collect();
    (!id.is_empty()).then(|| format!("t3_{id}"))
}

/// Whether a reddit URL points at a comment (`/comments/<post>/<slug>/<comment>/`) rather
/// than a post. Used to skip the notes retrofit for comments, whose own body `/api/info`
/// never returns (it is keyed by the post fullname; see [`post_fullname`]).
fn is_comment_url(url: &Url) -> bool {
    let Some((_, rest)) = url.path().split_once("/comments/") else {
        return false;
    };
    // `Url::path` excludes any query/fragment.
    // post → [id, slug]; comment → [id, slug, comment-id].
    rest.split('/').filter(|s| !s.is_empty()).count() >= 3
}

/// Normalize a bookmark's tags: ensure the base tags, derive `subreddit:<sub>` from
/// the URL (casing per [`cased_subreddit`]), and strip bare/legacy/duplicate
/// subreddit forms. An empty base tag or prefix adds no tag, as in sync. Returns a
/// sorted, de-duplicated list.
pub fn normalize_tags(
    url: &Url,
    existing: &[String],
    base_tags: &[String],
    prefix: &str,
    nsfw_tag: &str,
) -> Vec<String> {
    let mut set: BTreeSet<String> = existing.iter().cloned().collect();
    let mut ensured = Vec::new();
    push_tags(&mut ensured, base_tags);
    set.remove(prefix); // bare "subreddit:"
    set.remove(prefix.strip_suffix(':').unwrap_or(prefix)); // bare "subreddit"

    // With the subreddit tag disabled, a bare tag named like the subreddit is the user's.
    if let Some(raw) = extract_subreddit(url).filter(|_| !prefix.is_empty()) {
        let cased = cased_subreddit(&raw);
        let forms = [
            raw.clone(),
            cased.clone(),
            raw.to_lowercase(),
            raw.to_uppercase(),
        ];
        for form in &forms {
            // Drop the bare subreddit tag in any case (but keep the NSFW marker, which
            // nothing re-adds; a base tag named like the subreddit, e.g. r/reddit, comes
            // back from `ensured`), and the legacy prefixed forms.
            if form != nsfw_tag {
                set.remove(form);
            }
            set.remove(&format!("{prefix}{form}"));
        }
        push_prefixed(&mut ensured, prefix, &cased);
    }
    set.extend(ensured);
    set.into_iter().collect()
}

/// Whether a post bookmark's stored `note` is one the tool itself wrote — so the
/// authoritative `rebuilt` notes may safely replace it — rather than a hand-written
/// annotation to preserve. Recognized tool shapes: empty, the current rebuild, the raw
/// `selftext` from before the `<blockquote>` retrofit, or a bare self-link an older sync
/// left in an empty self-post.
fn note_is_tool_generated(note: &str, rebuilt: &str, selftext: &str, url: &Url) -> bool {
    let trimmed = note.trim();
    trimmed.is_empty()
        || note == rebuilt
        || (!selftext.trim().is_empty() && trimmed == selftext.trim())
        || is_self_link_notes(note, url)
}

/// Whether `notes` is nothing but a Reddit link to the same post as `url` — the
/// duplicated self-link older syncs wrote into empty self-posts' notes. Compared by
/// [`reddit_key`], so it matches across hosts/casing; non-Reddit notes (e.g. a link
/// post's external URL) and free-text notes don't match and are left untouched.
fn is_self_link_notes(notes: &str, url: &Url) -> bool {
    let notes_key = Url::parse(notes.trim()).ok();
    let notes_key = notes_key.as_ref().and_then(reddit_key);
    notes_key.is_some() && notes_key == reddit_key(url)
}

/// The generic placeholder titles Reddit bookmarks often get from a browser.
pub fn is_placeholder_title(title: &str) -> bool {
    matches!(
        title.trim(),
        "Reddit - Dive into anything" | "www.reddit.com" | "reddit.com: over 18?"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::url;

    fn norm(u: &str, domain: &str) -> Option<String> {
        normalize_url(&url(u), domain).map(String::from)
    }

    #[test]
    fn host_is_matches_reddit_hosts_only() {
        assert!(url("https://old.reddit.com/r/rust/").host_is("reddit.com"));
        assert!(url("http://reddit.com/r/x/").host_is("reddit.com"));
        assert!(!url("https://example.com/reddit.com/x").host_is("reddit.com"));
    }

    #[test]
    fn normalize_url_rewrites_host_to_domain() {
        assert_eq!(
            norm(
                "https://www.reddit.com/r/Rust/comments/abc/x/",
                "old.reddit.com"
            )
            .as_deref(),
            Some("https://old.reddit.com/r/Rust/comments/abc/x/")
        );
        assert_eq!(
            norm("https://reddit.com/r/x/", "old.reddit.com").as_deref(),
            Some("https://old.reddit.com/r/x/")
        );
        assert_eq!(
            norm("https://m.reddit.com/r/x/", "old.reddit.com").as_deref(),
            Some("https://old.reddit.com/r/x/")
        );
        assert_eq!(norm("https://old.reddit.com/r/x/", "old.reddit.com"), None);
        // A non-default domain rewrites old.reddit.com too.
        assert_eq!(
            norm("https://old.reddit.com/r/x/", "www.reddit.com").as_deref(),
            Some("https://www.reddit.com/r/x/")
        );
    }

    #[test]
    fn normalize_url_unwraps_over18_then_normalizes_host() {
        let u = "https://www.reddit.com/over18/?dest=https%3A%2F%2Fwww.reddit.com%2Fr%2Fx%2Fcomments%2Fa%2F";
        assert_eq!(
            norm(u, "old.reddit.com").as_deref(),
            Some("https://old.reddit.com/r/x/comments/a/")
        );
    }

    #[test]
    fn extract_subreddit_from_path() {
        assert_eq!(
            extract_subreddit(&url("https://old.reddit.com/r/AskReddit/comments/x/")).as_deref(),
            Some("AskReddit")
        );
        assert_eq!(
            extract_subreddit(&url("https://old.reddit.com/")).as_deref(),
            None
        );
    }

    #[test]
    fn post_fullname_from_post_and_comment_permalinks() {
        assert_eq!(
            post_fullname(&url("https://old.reddit.com/r/rust/comments/abc123/title/")).as_deref(),
            Some("t3_abc123")
        );
        assert_eq!(
            post_fullname(&url(
                "https://old.reddit.com/r/rust/comments/abc123/title/def456/"
            ))
            .as_deref(),
            Some("t3_abc123")
        );
        assert_eq!(
            post_fullname(&url("https://old.reddit.com/r/rust/")).as_deref(),
            None
        );
    }

    #[test]
    fn is_comment_url_distinguishes_comments_from_posts() {
        assert!(!is_comment_url(&url(
            "https://old.reddit.com/r/rust/comments/abc/title/"
        )));
        assert!(is_comment_url(&url(
            "https://old.reddit.com/r/rust/comments/abc/title/def/"
        )));
        // Query strings don't fool the segment count.
        assert!(is_comment_url(&url(
            "https://old.reddit.com/r/rust/comments/abc/title/def/?context=3"
        )));
        assert!(!is_comment_url(&url("https://old.reddit.com/r/rust/")));
    }

    #[test]
    fn normalize_tags_adds_base_and_subreddit_strips_legacy() {
        let existing = vec![
            "rust".to_string(),
            "subreddit:".to_string(),
            "subreddit:rust".to_string(),
        ];
        let tags = normalize_tags(
            &url("https://old.reddit.com/r/rust/comments/x/"),
            &existing,
            &["reddit".to_string()],
            "subreddit:",
            "nsfw",
        );
        assert_eq!(
            tags,
            vec!["reddit".to_string(), "subreddit:rust".to_string()]
        );
    }

    #[test]
    fn normalize_tags_lowercases_all_caps_subreddit() {
        let tags = normalize_tags(
            &url("https://old.reddit.com/r/NEWS/comments/x/"),
            &["subreddit:NEWS".to_string(), "NEWS".to_string()],
            &["reddit".to_string()],
            "subreddit:",
            "nsfw",
        );
        assert_eq!(
            tags,
            vec!["reddit".to_string(), "subreddit:news".to_string()]
        );
    }

    #[test]
    fn normalize_tags_keeps_base_tag_for_subreddit_named_like_it() {
        let tags = normalize_tags(
            &url("https://old.reddit.com/r/reddit/comments/x/"),
            &[],
            &["reddit".to_string()],
            "subreddit:",
            "nsfw",
        );
        assert_eq!(
            tags,
            vec!["reddit".to_string(), "subreddit:reddit".to_string()]
        );
    }

    #[test]
    fn normalize_tags_ensures_and_keeps_every_base_tag() {
        let tags = normalize_tags(
            &url("https://old.reddit.com/r/rust/comments/x/"),
            &["rust".to_string()],
            &[
                "reddit".to_string(),
                "rust".to_string(),
                "saved".to_string(),
            ],
            "subreddit:",
            "nsfw",
        );
        assert_eq!(tags, vec!["reddit", "rust", "saved", "subreddit:rust"]);
    }

    #[test]
    fn normalize_tags_adds_no_empty_tag_when_the_base_tag_is_disabled() {
        let tags = normalize_tags(
            &url("https://old.reddit.com/r/rust/comments/x/"),
            &[],
            &[String::new()],
            "subreddit:",
            "nsfw",
        );
        assert_eq!(tags, vec!["subreddit:rust".to_string()]);
    }

    #[test]
    fn normalize_tags_adds_and_strips_no_subreddit_tag_when_the_prefix_is_disabled() {
        let tags = normalize_tags(
            &url("https://old.reddit.com/r/rust/comments/x/"),
            &[],
            &["reddit".to_string()],
            "",
            "nsfw",
        );
        assert_eq!(tags, vec!["reddit".to_string()]);

        let tags = normalize_tags(
            &url("https://old.reddit.com/r/rust/comments/x/"),
            &["rust".to_string()],
            &["reddit".to_string()],
            "",
            "nsfw",
        );
        assert_eq!(tags, vec!["reddit".to_string(), "rust".to_string()]);

        let tags = normalize_tags(
            &url("https://old.reddit.com/r/nsfw/comments/x/"),
            &["nsfw".to_string()],
            &["reddit".to_string()],
            "",
            "nsfw",
        );
        assert_eq!(tags, vec!["nsfw".to_string(), "reddit".to_string()]);
    }

    #[test]
    fn placeholder_titles_detected() {
        assert!(is_placeholder_title("Reddit - Dive into anything"));
        assert!(is_placeholder_title("  reddit.com: over 18?  "));
        assert!(!is_placeholder_title("Some real title : r/rust"));
    }
}

#[cfg(test)]
mod loop_tests;
