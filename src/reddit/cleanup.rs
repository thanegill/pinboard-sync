//! `cleanup` subcommand: normalize existing Reddit bookmarks in a Pinboard
//! account. Rewrites URLs to the account's `reddit_domain` (unwrapping `over18`
//! redirects) unless they are on another configured account's,
//! normalizes tags, and -- using Reddit's `/api/info` for authoritative data --
//! marks NSFW posts, replaces generic placeholder titles, and backfills the author,
//! flair and media-type tags sync writes.

use std::collections::{BTreeSet, HashMap};

use anyhow::Result;

use crate::bookmark::{AccountState, Bookmark, BookmarkStore};
use crate::cleanup_pass::{run_pass, CleanupPass, Plan};
use crate::config::own_then_configured;
use crate::htmltext::collapse_whitespace;
use crate::keys::RedditKeys;
use crate::reddit::model::{
    cased_subreddit, created_at, over18_dest, RedditConfig, RedditKind, RedditSavedItem,
};
use crate::reddit::PostInfo;
use crate::source::{SourceError, UrlKey};
use crate::tags::{push_prefixed, push_tags, push_unique};
use crate::timefmt::DateOpts;
use crate::urls::{host_matches, UrlExt};
use url::Url;

pub struct RedditCleanupOpts {
    pub mark_nsfw: bool,
    pub fix_titles: bool,
    /// The account's tag vocabulary and domain, the same ones `sync` writes with: a
    /// reddit.com URL on no configured account's domain is moved to `domain`, and `nsfw`
    /// is the NSFW marker tag (empty disables it).
    pub config: RedditConfig,
    /// Whether and how to re-date bookmarks by the source post date.
    pub dates: DateOpts,
    /// Every configured account's config, in config order. A bookmark already on one of
    /// their domains is left there (see [`RedditCleanupOpts::natural_url`]) and shaped with
    /// that account's tags ([`RedditCleanupOpts::config_for`]).
    pub configured: Vec<RedditConfig>,
}

impl RedditCleanupOpts {
    /// Whether the pass reads `/api/info`: for NSFW tagging, title fixing or
    /// `use_post_date`. The notes retrofit and the tag backfill ride on that read.
    pub fn needs_post_info(&self) -> bool {
        self.mark_nsfw || self.fix_titles || self.dates.use_post_date
    }

    /// The account's own domain, then every configured one.
    fn domains(&self) -> impl Iterator<Item = &str> {
        own_then_configured(&self.config, &self.configured).map(|account| account.domain.as_str())
    }

    /// The config to shape a bookmark at `url` with: the account's own when `url` is on its
    /// domain, else that of the first configured account whose domain it is on, else the
    /// account's own. A bookmark stays on its owner's domain, so the host names its owner.
    fn config_for(&self, url: &Url) -> &RedditConfig {
        self.configs_on(url).next().unwrap_or(&self.config)
    }

    /// The account's own config, then every configured one, whose domain is `url`'s host:
    /// the accounts that can shape a bookmark at `url`, depending on which one runs.
    fn configs_on<'s: 'u, 'u>(
        &'s self,
        url: &'u Url,
    ) -> impl Iterator<Item = &'s RedditConfig> + 'u {
        own_then_configured(&self.config, &self.configured).filter(move |account| {
            url.host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case(&account.domain))
        })
    }

    /// The bare tags [`normalize_tags`] keeps at `url` even when named like the subreddit
    /// or the subreddit prefix: `owner`'s NSFW marker, which nothing re-adds without an
    /// `/api/info` answer, and every tag an account that can shape the bookmark adds to it
    /// bare (its base tags, NSFW marker and, on a comment, comment tag), which cleanup run
    /// as that account adds back (the NSFW marker only with an `/api/info` answer).
    fn kept_tags<'s>(&'s self, url: &Url, owner: &'s RedditConfig) -> Vec<&'s str> {
        let is_comment = is_comment_url(url);
        let sharing = self.configs_on(url).flat_map(move |account| {
            account
                .tags
                .iter()
                .chain(std::iter::once(&account.nsfw))
                .chain(is_comment.then_some(&account.comment))
                .map(String::as_str)
        });
        std::iter::once(owner.nsfw.as_str())
            .chain(sharing)
            .collect()
    }

    /// Whether `url` is on the account's own or another configured `reddit_domain`.
    fn is_configured_host(&self, url: &Url) -> bool {
        url.host_str().is_some_and(|host| {
            self.domains()
                .any(|domain| host.eq_ignore_ascii_case(domain))
        })
    }

    /// [`RedditKeys`] on every configured domain, as every account's sync keys a save.
    fn reddit_keys(&self) -> RedditKeys {
        RedditKeys::new(self.domains())
    }

    /// Where this pass moves `url` before collapsing copies: `over18` unwrapped, and a
    /// reddit.com host rewritten to the account's domain unless it is already some
    /// account's `reddit_domain`. A bookmark on another account's mirror stays put, so
    /// that `cleanup` run as either account leaves it where it is instead of moving it
    /// back and forth.
    fn natural_url(&self, url: &Url) -> Url {
        let unwrapped = unwrap_over18(url);
        if self.is_configured_host(&unwrapped) {
            return unwrapped;
        }
        normalize_url(&unwrapped, &self.config.domain).unwrap_or(unwrapped)
    }
}

/// The `cleanup reddit` choices that are not account settings: the cookie flag, and which
/// `/api/info`-backed fixes to make.
pub struct RedditCleanupOverrides {
    pub cookie: Option<String>,
    pub mark_nsfw: bool,
    pub fix_titles: bool,
}

/// Authoritative per-post data from `/api/info`.
struct RedditPostMeta {
    over_18: bool,
    title: Option<String>,
    /// Post creation time (unix epoch seconds), for `use_post_date`.
    created_utc: Option<f64>,
    /// The notes rebuilt from authoritative data -- the quoted body wrapped in a
    /// `<blockquote>` (empty when there's nothing to quote). Used to retrofit existing
    /// bookmarks to the shape `sync` writes.
    note: String,
    /// The raw `selftext`, i.e. the rebuilt notes before the `<blockquote>` wrapping --
    /// the shape an older sync wrote. Lets the retrofit recognize its own past output.
    selftext: String,
}

/// What `/api/info` said about the bookmarks' posts and comments.
#[derive(Default)]
struct RedditInfo {
    /// Per-post data, keyed by `t3_` fullname.
    posts: HashMap<String, RedditPostMeta>,
    /// Each post and comment shaped as `sync` shapes it, for the tags drawn from its own
    /// data, keyed by `t3_`/`t1_` fullname.
    items: HashMap<String, RedditSavedItem>,
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
        .filter(|bookmark| {
            bookmark.url.host_is("reddit.com") || opts.is_configured_host(&bookmark.url)
        })
        .collect();

    let info = fetch_post_info(reddit, opts, &reddit_bms).await?;
    let keys = opts.reddit_keys();
    let pass = RedditCleanupPass {
        info,
        opts,
        collapse_hosts: collapse_hosts(&reddit_bms, opts, &keys),
        keys,
    };
    run_pass(store, &reddit_bms, "reddit", opts.dates, &pass)
        .await
        .into_result()
}

/// Re-shapes one reddit bookmark: normalize the URL/tags, then apply the authoritative
/// `/api/info` data (NSFW marker, placeholder-title replacement, post date, rebuilt notes,
/// and the author/flair/media tags missing from the bookmark).
struct RedditCleanupPass<'a> {
    info: RedditInfo,
    opts: &'a RedditCleanupOpts,
    /// From [`RedditCleanupOpts::reddit_keys`].
    keys: RedditKeys,
    /// From [`collapse_hosts`].
    collapse_hosts: HashMap<String, String>,
}

impl CleanupPass for RedditCleanupPass<'_> {
    async fn plan(&self, bookmark: &Bookmark) -> Result<Plan, SourceError> {
        let opts = self.opts;
        let mut new_url = opts.natural_url(&bookmark.url);
        if let Some(host) = self
            .keys
            .dedup_key(&new_url)
            .and_then(|key| self.collapse_hosts.get(&key))
        {
            new_url = with_host(&new_url, host).unwrap_or(new_url);
        }

        let owner = opts.config_for(&new_url);
        let mut tags = normalize_tags(
            &new_url,
            &bookmark.tags,
            &owner.tags,
            &owner.subreddit_prefix,
            &opts.kept_tags(&new_url, owner),
        );
        let post = post_fullname(&new_url).and_then(|f| self.info.posts.get(&f));

        if opts.mark_nsfw && post.is_some_and(|p| p.over_18) {
            push_unique(&mut tags, &owner.nsfw);
        }

        let own_fullname = if is_comment_url(&new_url) {
            push_unique(&mut tags, &owner.comment);
            comment_fullname(&new_url)
        } else {
            post_fullname(&new_url)
        };
        if let Some(item) = own_fullname.and_then(|f| self.info.items.get(&f)) {
            for (prefix, tag) in item.detail_tags(owner) {
                if !tags.iter().any(|t| t.starts_with(prefix)) {
                    tags.push(tag);
                }
            }
        }
        tags.sort();

        let mut title = collapse_whitespace(&bookmark.title, " ");
        if opts.fix_titles && is_placeholder_title(&title) {
            if let Some(real_title) = post.and_then(|p| p.title.clone()) {
                title = collapse_whitespace(&real_title, " ");
            }
        }

        // Reshape the notes. Only for *post* bookmarks: `post` is keyed by the post fullname
        // (`post_fullname`), and a comment's own entry is used only for its tags, so leave
        // those notes alone. When we have authoritative data, retrofit the rebuild (selftext
        // in a <blockquote>, or a link post's external URL) onto the stored notes *only* when
        // those notes are ones the tool itself wrote (empty, the current rebuild, the raw
        // pre-<blockquote> selftext, or a bare self-link an older sync left) -- a
        // hand-written annotation on a manually-saved bookmark is preserved. Without an entry
        // this run, only drop a bare self-link; never wipe genuine notes.
        let note = if is_comment_url(&new_url) {
            bookmark.note.clone()
        } else if let Some(post) = post {
            if note_is_tool_generated(
                &bookmark.note,
                &post.note,
                &post.selftext,
                &new_url,
                &self.keys,
            ) {
                post.note.clone()
            } else {
                bookmark.note.clone()
            }
        } else if is_self_link_notes(&bookmark.note, &new_url, &self.keys) {
            String::new()
        } else {
            bookmark.note.clone()
        };

        // `post` is keyed by the post fullname, so for a comment bookmark it is the
        // *parent thread's* meta; its `created_utc` is the thread-creation time, not the
        // comment's. Never re-date a comment by it -- leave a None candidate so the driver
        // keeps the comment's stored timestamp (as with its notes above).
        let timestamp = if is_comment_url(&new_url) {
            None
        } else {
            created_at(post.and_then(|p| p.created_utc))
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
/// referenced by the bookmarks, and each post and comment bookmark's own entry for its
/// tags. Empty unless [`RedditCleanupOpts::needs_post_info`].
async fn fetch_post_info<R: PostInfo>(
    reddit: Option<&R>,
    opts: &RedditCleanupOpts,
    bookmarks: &[Bookmark],
) -> Result<RedditInfo, SourceError> {
    let mut info = RedditInfo::default();
    if !opts.needs_post_info() {
        return Ok(info);
    }
    let Some(reddit) = reddit else {
        return Ok(info);
    };

    let fullnames: Vec<String> = bookmarks
        .iter()
        .flat_map(|bookmark| {
            let url = normalize_url(&bookmark.url, &opts.config.domain)
                .unwrap_or_else(|| bookmark.url.clone());
            [post_fullname(&url), comment_fullname(&url)]
        })
        .flatten()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if fullnames.is_empty() {
        return Ok(info);
    }

    let entries = reddit.info(&fullnames).await?;
    for entry in entries {
        if let (RedditKind::Post, Some(name)) = (entry.kind, entry.fields.name.clone()) {
            // Rebuild the notes from authoritative data the same way sync does, so the
            // <blockquote> shaping retrofits onto existing bookmarks: selftext quoted,
            // empty for an empty self-post, else the external URL.
            let selftext = entry.fields.selftext.clone().unwrap_or_default();
            let note = crate::reddit::model::post_note(
                &selftext,
                entry.fields.url.as_deref().unwrap_or_default(),
                entry.fields.is_self,
            );
            info.posts.insert(
                name,
                RedditPostMeta {
                    over_18: entry.fields.over_18,
                    title: entry.fields.title.clone().filter(|s| !s.is_empty()),
                    created_utc: entry.fields.created_utc,
                    note,
                    selftext,
                },
            );
        }
        if let Some(item) = entry.into_saved_item(&opts.config.domain) {
            info.items.insert(item.fullname.clone(), item);
        }
    }
    Ok(info)
}

/// For each post or comment stored on more than one host once each copy has its
/// [`RedditCleanupOpts::natural_url`], the host the copies collapse onto, keyed by `keys`:
/// the earliest account domain in config order, so the copies meet on one URL, where the
/// driver merges them, whichever account cleanup runs as.
fn collapse_hosts(
    bookmarks: &[Bookmark],
    opts: &RedditCleanupOpts,
    keys: &RedditKeys,
) -> HashMap<String, String> {
    let mut hosts_by_key: HashMap<String, BTreeSet<String>> = HashMap::new();
    for bookmark in bookmarks {
        let url = opts.natural_url(&bookmark.url);
        let (Some(key), Some(host)) = (keys.dedup_key(&url), url.host_str()) else {
            continue;
        };
        hosts_by_key
            .entry(key)
            .or_default()
            .insert(host.to_ascii_lowercase());
    }
    let rank = |host: &String| {
        let position = opts
            .configured
            .iter()
            .position(|account| account.domain.eq_ignore_ascii_case(host));
        (position.unwrap_or(usize::MAX), host.clone())
    };
    hosts_by_key
        .into_iter()
        .filter(|(_, hosts)| hosts.len() > 1)
        .filter_map(|(key, hosts)| Some((key, hosts.into_iter().min_by_key(rank)?)))
        .collect()
}

// --- pure transforms ---------------------------------------------------------

/// Normalize a Reddit bookmark URL: unwrap `over18/?dest=` redirects, then rewrite
/// any reddit host to the configured `domain`. Returns `Some(new)` only if it changed.
pub fn normalize_url(url: &Url, domain: &str) -> Option<Url> {
    let unwrapped = unwrap_over18(url);
    let normalized = to_reddit_domain(&unwrapped, domain).unwrap_or(unwrapped);
    (normalized != *url).then_some(normalized)
}

/// `url` with every `over18/?dest=` redirect unwrapped.
fn unwrap_over18(url: &Url) -> Url {
    let mut current = url.clone();
    while let Some(dest) = over18_dest(&current) {
        current = dest;
    }
    current
}

/// Rewrite any reddit host (`reddit.com` or a `*.reddit.com` subdomain) to the
/// configured `domain` (over https), preserving the rest of the URL. Returns `None`
/// for a non-reddit host or one already equal to `domain`.
fn to_reddit_domain(url: &Url, domain: &str) -> Option<Url> {
    let host = url.host_str()?;
    if !(host_matches(host, "reddit.com") && host != domain) {
        return None;
    }
    with_host(url, domain)
}

/// `url` over https on `host`, or `None` when it is already there.
fn with_host(url: &Url, host: &str) -> Option<Url> {
    if url.scheme() == "https" && url.host_str() == Some(host) {
        return None;
    }
    let mut rewritten = url.clone();
    rewritten.set_scheme("https").ok()?;
    rewritten.set_host(Some(host)).ok()?;
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
/// than a post. Decides the `reddit-comment` tag, which fullname a bookmark's own tags come
/// from, and skips the notes retrofit for comments, whose post-keyed data (see
/// [`post_fullname`]) is the parent thread's.
fn is_comment_url(url: &Url) -> bool {
    let Some((_, rest)) = url.path().split_once("/comments/") else {
        return false;
    };
    // `Url::path` excludes any query/fragment.
    // post -> [id, slug]; comment -> [id, slug, comment-id].
    rest.split('/').filter(|s| !s.is_empty()).count() >= 3
}

/// The comment fullname (`t1_<id>`) from a comment permalink's third segment after
/// `/comments/`, or `None` for a post permalink.
fn comment_fullname(url: &Url) -> Option<String> {
    let (_, rest) = url.path().split_once("/comments/")?;
    let id = rest.split('/').filter(|s| !s.is_empty()).nth(2)?;
    id.chars()
        .all(char::is_alphanumeric)
        .then(|| format!("t1_{id}"))
}

/// Normalize a bookmark's tags: ensure the base tags, derive `subreddit:<sub>` from
/// the URL (casing per [`cased_subreddit`]), and strip bare/legacy/duplicate
/// subreddit forms, except a bare one in `kept`. An empty base tag or prefix adds no
/// tag, as in sync. Returns a sorted, de-duplicated list.
pub fn normalize_tags(
    url: &Url,
    existing: &[String],
    base_tags: &[String],
    prefix: &str,
    kept: &[&str],
) -> Vec<String> {
    let mut set: BTreeSet<String> = existing.iter().cloned().collect();
    let mut ensured = Vec::new();
    push_tags(&mut ensured, base_tags);
    let bare_prefixes = [prefix, prefix.strip_suffix(':').unwrap_or(prefix)];
    for bare_prefix in bare_prefixes {
        if !kept.contains(&bare_prefix) {
            set.remove(bare_prefix);
        }
    }

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
            // Drop the bare subreddit tag in any case (but keep a `kept` one; a base tag
            // named like the subreddit, e.g. r/reddit, comes back from `ensured`), and the
            // legacy prefixed forms.
            if !kept.contains(&form.as_str()) {
                set.remove(form);
            }
            set.remove(&format!("{prefix}{form}"));
        }
        push_prefixed(&mut ensured, prefix, &cased);
    }
    set.extend(ensured);
    set.into_iter().collect()
}

/// Whether a post bookmark's stored `note` is one the tool itself wrote -- so the
/// authoritative `rebuilt` notes may safely replace it -- rather than a hand-written
/// annotation to preserve. Recognized tool shapes: empty, the current rebuild, the raw
/// `selftext` from before the `<blockquote>` retrofit, or a bare self-link an older sync
/// left in an empty self-post.
fn note_is_tool_generated(
    note: &str,
    rebuilt: &str,
    selftext: &str,
    url: &Url,
    keys: &RedditKeys,
) -> bool {
    let trimmed = note.trim();
    trimmed.is_empty()
        || note == rebuilt
        || (!selftext.trim().is_empty() && trimmed == selftext.trim())
        || is_self_link_notes(note, url, keys)
}

/// Whether `notes` is nothing but a Reddit link to the same post as `url` -- the duplicated
/// self-link older syncs wrote into empty self-posts' notes. Compared by `keys` (on every
/// configured `reddit_domain`), so it matches across hosts/casing; non-Reddit notes (e.g.
/// a link post's external URL) and free-text notes don't match and are left untouched.
fn is_self_link_notes(notes: &str, url: &Url, keys: &RedditKeys) -> bool {
    let notes_key = Url::parse(notes.trim()).ok();
    let notes_key = notes_key
        .as_ref()
        .and_then(|notes_url| keys.dedup_key(notes_url));
    notes_key.is_some() && notes_key == keys.dedup_key(url)
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
    use crate::test_support::{strings, url};

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
        let existing = strings(&["rust", "subreddit:", "subreddit:rust"]);
        let tags = normalize_tags(
            &url("https://old.reddit.com/r/rust/comments/x/"),
            &existing,
            &strings(&["reddit"]),
            "subreddit:",
            &["nsfw"],
        );
        assert_eq!(tags, strings(&["reddit", "subreddit:rust"]));
    }

    #[test]
    fn normalize_tags_lowercases_all_caps_subreddit() {
        let tags = normalize_tags(
            &url("https://old.reddit.com/r/NEWS/comments/x/"),
            &strings(&["subreddit:NEWS", "NEWS"]),
            &strings(&["reddit"]),
            "subreddit:",
            &["nsfw"],
        );
        assert_eq!(tags, strings(&["reddit", "subreddit:news"]));
    }

    #[test]
    fn normalize_tags_keeps_base_tag_for_subreddit_named_like_it() {
        let tags = normalize_tags(
            &url("https://old.reddit.com/r/reddit/comments/x/"),
            &[],
            &strings(&["reddit"]),
            "subreddit:",
            &["nsfw"],
        );
        assert_eq!(tags, strings(&["reddit", "subreddit:reddit"]));
    }

    #[test]
    fn normalize_tags_ensures_and_keeps_every_base_tag() {
        let tags = normalize_tags(
            &url("https://old.reddit.com/r/rust/comments/x/"),
            &strings(&["rust"]),
            &strings(&["reddit", "rust", "saved"]),
            "subreddit:",
            &["nsfw"],
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
            &["nsfw"],
        );
        assert_eq!(tags, strings(&["subreddit:rust"]));
    }

    #[test]
    fn normalize_tags_adds_and_strips_no_subreddit_tag_when_the_prefix_is_disabled() {
        let tags = normalize_tags(
            &url("https://old.reddit.com/r/rust/comments/x/"),
            &[],
            &strings(&["reddit"]),
            "",
            &["nsfw"],
        );
        assert_eq!(tags, strings(&["reddit"]));

        let tags = normalize_tags(
            &url("https://old.reddit.com/r/rust/comments/x/"),
            &strings(&["rust"]),
            &strings(&["reddit"]),
            "",
            &["nsfw"],
        );
        assert_eq!(tags, strings(&["reddit", "rust"]));

        let tags = normalize_tags(
            &url("https://old.reddit.com/r/nsfw/comments/x/"),
            &strings(&["nsfw"]),
            &strings(&["reddit"]),
            "",
            &["nsfw"],
        );
        assert_eq!(tags, strings(&["nsfw", "reddit"]));
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
