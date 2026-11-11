//! `cleanup reddit`: moves a bookmark to the account's `reddit_domain` (unwrapping
//! `over18`) unless it is on another configured account's, normalizes its tags, and from
//! `/api/info` marks NSFW posts, fixes placeholder titles and backfills the author, flair
//! and media-type tags sync writes.
//!
//! While Reddit's edge blocks `/api/info` (#89), the saved listing stands in: the bookmarks
//! it covers get the same fixes from the same entries, and the rest only what needs no
//! metadata, as for a post `/api/info` no longer knows.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::Result;
use log::warn;

use crate::bookmark::{AccountState, Bookmark, BookmarkStore};
use crate::cleanup_pass::{run_pass, CleanupPass, Plan};
use crate::config::own_then_configured;
use crate::htmltext::collapse_whitespace;
use crate::keys::RedditKeys;
use crate::reddit::model::{
    cased_subreddit, created_at, unwrap_over18, RedditConfig, RedditKind, RedditListingEntry,
    RedditSavedItem,
};
use crate::reddit::PostInfo;
use crate::secrets::SecretSpec;
use crate::source::{DedupKey, Halt, SourceError, UrlKey};
use crate::tags::Tags;
use crate::timefmt::DateOpts;
use crate::urls::{host_matches, UrlExt};
use url::Url;

pub struct RedditCleanupOpts {
    pub mark_nsfw: bool,
    pub fix_titles: bool,
    /// The account's tags and domain, the ones `sync` writes with: a reddit.com URL on no
    /// configured account's domain moves to `domain`.
    pub config: RedditConfig,
    pub dates: DateOpts,
    /// Every configured account's config, in config order. A bookmark on one of their
    /// domains stays there ([`Self::natural_url`]) and takes its account's tags
    /// ([`Self::config_for`]), whichever account runs.
    pub configured: Vec<RedditConfig>,
}

impl RedditCleanupOpts {
    /// Whether the pass reads `/api/info`: for NSFW tagging, title fixing or
    /// `use_post_date`. The notes and tag backfill ride on that read.
    pub const fn needs_post_info(&self) -> bool {
        self.mark_nsfw || self.fix_titles || self.dates.use_post_date
    }

    /// The account's own domain, then every configured one.
    fn domains(&self) -> impl Iterator<Item = &str> {
        own_then_configured(&self.config, &self.configured).map(|account| account.domain.as_str())
    }

    /// The config to shape a bookmark at `url` with: the first of [`Self::configs_on`],
    /// else the account's own. A bookmark stays on its owner's domain, which names it.
    fn config_for(&self, url: &Url) -> &RedditConfig {
        self.configs_on(url).next().unwrap_or(&self.config)
    }

    /// The account's own config, then every configured one, on `url`'s host: whichever
    /// runs, one of these shapes the bookmark.
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
    /// or its prefix: `owner`'s NSFW marker, which nothing re-adds without an `/api/info`
    /// answer, and every bare tag an account that can shape the bookmark adds (base tags,
    /// NSFW marker and, on a comment, comment tag), which cleanup run as that account adds
    /// back, the NSFW marker only with an `/api/info` answer.
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

    /// [`RedditKeys`] on every configured domain, as each account's sync keys a save.
    fn reddit_keys(&self) -> RedditKeys {
        RedditKeys::new(self.domains())
    }

    /// Where the pass moves `url` before collapsing copies: `over18` unwrapped, and a
    /// reddit.com host moved to the account's domain unless it is some account's
    /// `reddit_domain`, so passes run as different accounts never move it back and forth.
    fn natural_url(&self, url: &Url) -> Url {
        let unwrapped = unwrap_over18(url);
        if self.is_configured_host(&unwrapped) {
            return unwrapped;
        }
        normalize_url(&unwrapped, &self.config.domain).unwrap_or(unwrapped)
    }
}

/// The `cleanup reddit` flags that aren't account settings.
pub struct RedditCleanupOverrides {
    pub cookie: Option<String>,
    pub mark_nsfw: bool,
    pub fix_titles: bool,
}

/// Every fix on and no cookie flag: what `cleanup --all` runs.
impl Default for RedditCleanupOverrides {
    fn default() -> Self {
        Self {
            cookie: None,
            mark_nsfw: true,
            fix_titles: true,
        }
    }
}

/// Per-post data from `/api/info`.
struct RedditPostMeta {
    over_18: bool,
    title: Option<String>,
    /// Epoch seconds, for `use_post_date`.
    created_utc: Option<f64>,
    /// The notes `sync` would write, to retrofit onto a bookmark the tool wrote.
    note: String,
    /// The `selftext` unquoted: a legacy shape of the tool's notes.
    selftext: String,
}

/// What `/api/info` said about the bookmarks' posts and comments.
#[derive(Default)]
struct RedditInfo {
    /// Per-post data, keyed by `t3_` fullname.
    posts: HashMap<String, RedditPostMeta>,
    /// Each post and comment as `sync` shapes it, for its own tags, keyed by fullname.
    items: HashMap<String, RedditSavedItem>,
}

pub async fn run<S: BookmarkStore + AccountState, R: PostInfo>(
    store: &S,
    reddit: Option<&R>,
    opts: &RedditCleanupOpts,
) -> Result<(), SourceError> {
    let reddit_bms = store.snapshot(|bookmark| {
        bookmark.url.host_is("reddit.com") || opts.is_configured_host(&bookmark.url)
    });

    let info = fetch_post_info(reddit, opts, &reddit_bms).await?;
    let keys = opts.reddit_keys();
    let pass = RedditCleanupPass {
        store,
        info,
        opts,
        collapse_hosts: collapse_hosts(&reddit_bms, opts, &keys),
        keys,
    };
    run_pass(store, &reddit_bms, "reddit", opts.dates, &pass)
        .await
        .into_result()
}

/// Re-shapes one Reddit bookmark: its URL and tags, then the `/api/info` data (NSFW marker,
/// placeholder title, post date, notes, and missing author/flair/media tags).
struct RedditCleanupPass<'a, S> {
    /// Knows a note it stored cut.
    store: &'a S,
    info: RedditInfo,
    opts: &'a RedditCleanupOpts,
    /// From [`RedditCleanupOpts::reddit_keys`].
    keys: RedditKeys,
    /// From [`collapse_hosts`].
    collapse_hosts: HashMap<DedupKey, String>,
}

impl<S: BookmarkStore> CleanupPass for RedditCleanupPass<'_, S> {
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
            tags.push(&owner.nsfw);
        }

        if is_comment_url(&new_url) {
            tags.push(&owner.comment);
        }
        if let Some(item) = own_fullname(&new_url).and_then(|f| self.info.items.get(&f)) {
            for detail in item.detail_tags(owner) {
                if !tags.iter().any(|t| t.starts_with(detail.family)) {
                    tags.push(&detail.tag);
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

        // Only a post's notes are rebuilt, and only when the tool wrote them; a comment's
        // entry feeds only its tags. Without an entry this run, only a bare self-link is
        // dropped, so a hand-written note is never wiped.
        let note = if is_comment_url(&new_url) {
            bookmark.note.clone()
        } else if let Some(post) = post {
            if note_is_tool_generated(
                &bookmark.note,
                self.store.kept_of_cut(&bookmark.note),
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

        // For a comment `post` is the parent thread's, so its `created_utc` is not the
        // comment's: a `None` candidate keeps the stored date.
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

/// The `/api/info` data for every post the bookmarks reference, and each bookmark's own
/// entry for its tags, with the saved listing standing in for what a block left unanswered
/// ([`fall_back_to_saved`]). Empty unless [`RedditCleanupOpts::needs_post_info`].
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
            let url = lookup_url(bookmark, opts);
            [post_fullname(&url), comment_fullname(&url)]
        })
        .flatten()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if fullnames.is_empty() {
        return Ok(info);
    }

    let answer = reddit.info(&fullnames).await?;
    let entries = match answer.blocked {
        None => answer.entries,
        Some(block) => {
            fall_back_to_saved(reddit, block, &fullnames, answer.entries, bookmarks, opts).await?
        }
    };
    for entry in entries {
        if let (RedditKind::Post, Some(name)) = (entry.kind, entry.fields.name.clone()) {
            let selftext = entry.fields.selftext.clone().unwrap_or_default();
            let note = entry.fields.post_note();
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

/// The URL a bookmark is looked up by.
fn lookup_url(bookmark: &Bookmark, opts: &RedditCleanupOpts) -> Url {
    normalize_url(&bookmark.url, &opts.config.domain).unwrap_or_else(|| bookmark.url.clone())
}

/// `answered`, what `/api/info` said before `block`, plus the saved listing's entries for
/// the `fullnames` it left unanswered, logging how many bookmarks each covered. A failed
/// listing read fails the pass with its own error, so a refused cookie still fires the
/// hook; with no username to read it by, the pass fails with the block.
async fn fall_back_to_saved<R: PostInfo>(
    reddit: &R,
    block: String,
    fullnames: &[String],
    answered: Vec<RedditListingEntry>,
    bookmarks: &[Bookmark],
    opts: &RedditCleanupOpts,
) -> Result<Vec<RedditListingEntry>, SourceError> {
    let Some(saved) = reddit.saved().await else {
        warn!(
            "{block} No Reddit username is set ({}), so the saved listing can't stand in \
             for it.",
            SecretSpec::REDDIT_USERNAME.settings_without_flag()
        );
        return Err(Halt::Blocked(block).into());
    };
    let saved = saved.inspect_err(|_| {
        warn!("{block} Reading the saved listing in its place failed too.");
    })?;

    let from_info: HashSet<String> = answered
        .iter()
        .filter_map(|entry| entry.fields.name.clone())
        .collect();
    let unanswered: HashSet<&str> = fullnames
        .iter()
        .map(String::as_str)
        .filter(|fullname| !from_info.contains(*fullname))
        .collect();
    let from_saved: Vec<RedditListingEntry> = saved
        .into_iter()
        .filter(|entry| {
            entry
                .fields
                .name
                .as_deref()
                .is_some_and(|name| unanswered.contains(name))
        })
        .collect();
    let saved_names: HashSet<String> = from_saved
        .iter()
        .filter_map(|entry| entry.fields.name.clone())
        .collect();

    let (mut by_info, mut by_saved, mut url_only) = (0, 0, 0);
    for bookmark in bookmarks {
        let url = lookup_url(bookmark, opts);
        let names: Vec<String> = [post_fullname(&url), comment_fullname(&url)]
            .into_iter()
            .flatten()
            .collect();
        if names.iter().any(|name| saved_names.contains(name)) {
            by_saved += 1;
        } else if names.iter().any(|name| from_info.contains(name)) {
            by_info += 1;
        } else if !names.is_empty() {
            url_only += 1;
        }
    }
    let before_block = if by_info > 0 {
        format!(", after /api/info answered for {by_info}")
    } else {
        String::new()
    };
    warn!(
        "{block} The saved listing stood in for it: {by_saved} bookmark(s) took their \
         metadata from it and {url_only} got URL-only normalization{before_block}."
    );
    Ok(answered.into_iter().chain(from_saved).collect())
}

/// For each post or comment still on several hosts after
/// [`RedditCleanupOpts::natural_url`], the host its copies collapse onto: the earliest
/// account domain in config order, so they meet on one URL, where the driver merges them,
/// whichever account runs.
fn collapse_hosts(
    bookmarks: &[Bookmark],
    opts: &RedditCleanupOpts,
    keys: &RedditKeys,
) -> HashMap<DedupKey, String> {
    let mut hosts_by_key: HashMap<DedupKey, BTreeSet<String>> = HashMap::new();
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

/// `url` with `over18` unwrapped and a reddit.com host moved to `domain`, or `None` when
/// that changes nothing.
pub fn normalize_url(url: &Url, domain: &str) -> Option<Url> {
    let unwrapped = unwrap_over18(url);
    let normalized = to_reddit_domain(&unwrapped, domain).unwrap_or(unwrapped);
    (normalized != *url).then_some(normalized)
}

/// `url` over https on `domain` when its host is reddit.com or a subdomain, and not
/// `domain`, else `None`.
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

/// The `<sub>` of `/r/<sub>/`.
pub fn extract_subreddit(url: &Url) -> Option<String> {
    let (_, after) = url.path().split_once("/r/")?;
    let sub: String = after
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    (!sub.is_empty()).then_some(sub)
}

/// The post fullname (`t3_<id>`) of a post or comment permalink.
pub fn post_fullname(url: &Url) -> Option<String> {
    let (_, after) = url.path().split_once("/comments/")?;
    let id: String = after.chars().take_while(|c| c.is_alphanumeric()).collect();
    (!id.is_empty()).then(|| format!("t3_{id}"))
}

/// Whether `url` is a comment (`/comments/<post>/<slug>/<comment>/`). A comment gets the
/// comment tag and its own fullname's tags, and no notes retrofit: its post-keyed data is
/// the parent thread's.
fn is_comment_url(url: &Url) -> bool {
    let Some((_, rest)) = url.path().split_once("/comments/") else {
        return false;
    };
    // post -> [id, slug]; comment -> [id, slug, comment-id].
    rest.split('/').filter(|s| !s.is_empty()).count() >= 3
}

/// The comment's fullname for a comment permalink, else the post's.
fn own_fullname(url: &Url) -> Option<String> {
    if is_comment_url(url) {
        comment_fullname(url)
    } else {
        post_fullname(url)
    }
}

/// A comment permalink's fullname (`t1_<id>`), or `None` for a post.
fn comment_fullname(url: &Url) -> Option<String> {
    let (_, rest) = url.path().split_once("/comments/")?;
    let id = rest.split('/').filter(|s| !s.is_empty()).nth(2)?;
    id.chars()
        .all(char::is_alphanumeric)
        .then(|| format!("t1_{id}"))
}

/// The tags with the base tags ensured, the `subreddit:<sub>` tag derived from the URL
/// ([`cased_subreddit`]), and other subreddit forms stripped, except a bare one in `kept`.
/// An empty base tag or prefix adds nothing, as in sync. Sorted.
pub fn normalize_tags<'e>(
    url: &Url,
    existing: impl IntoIterator<Item = &'e String>,
    base_tags: &[String],
    prefix: &str,
    kept: &[&str],
) -> Tags {
    let mut set: BTreeSet<String> = existing.into_iter().cloned().collect();
    let mut ensured = Tags::default();
    ensured.push_all(base_tags);
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
            // A base tag named like the subreddit (r/reddit) comes back from `ensured`.
            if !kept.contains(&form.as_str()) {
                set.remove(form);
            }
            set.remove(&format!("{prefix}{form}"));
        }
        ensured.push_prefixed(prefix, &cased);
    }
    set.extend(ensured.iter().cloned());
    set.into_iter().collect()
}

/// Whether a post's stored `note` is the tool's, so `rebuilt` may replace it, rather than
/// the user's: empty, `rebuilt` or a cut of it (`kept_of_cut`, the part the store kept,
/// starts `rebuilt`), or a legacy shape of the tool's, the bare `selftext` or a self-link.
fn note_is_tool_generated(
    note: &str,
    kept_of_cut: Option<&str>,
    rebuilt: &str,
    selftext: &str,
    url: &Url,
    keys: &RedditKeys,
) -> bool {
    let trimmed = note.trim();
    trimmed.is_empty()
        || note == rebuilt
        || kept_of_cut.is_some_and(|kept| rebuilt.starts_with(kept))
        || (!selftext.trim().is_empty() && trimmed == selftext.trim())
        || is_self_link_notes(note, url, keys)
}

/// Whether `notes` is only a link to the post at `url`, by `keys`, so across hosts and
/// case: the self-link the tool once wrote into an empty self-post.
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
    use proptest::prelude::*;

    use super::*;
    use crate::keys::HackerNewsKeys;
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

    /// The configured accounts' `reddit_domain`s: two reddit.com hosts and a mirror.
    const DOMAINS: [&str; 3] = ["old.reddit.com", "www.reddit.com", "redlib.example"];

    /// A post, comment or subreddit URL on any Reddit host or configured mirror, in any
    /// scheme and case, with or without a trailing slash, a query or a fragment.
    fn reddit_url() -> impl Strategy<Value = String> {
        let path = prop_oneof![
            ("[A-Za-z0-9_]{3,21}", "[a-z0-9]{5,7}", "[a-z0-9_]{1,30}")
                .prop_map(|(sub, id, slug)| format!("/r/{sub}/comments/{id}/{slug}")),
            ("[A-Za-z0-9_]{3,21}", "[a-z0-9]{5,7}", "[a-z0-9]{7}")
                .prop_map(|(sub, id, comment)| format!("/r/{sub}/comments/{id}/x/{comment}")),
            "[A-Za-z0-9_]{3,21}".prop_map(|sub| format!("/r/{sub}")),
            "[a-z0-9]{5,7}".prop_map(|id| format!("/comments/{id}")),
        ];
        (
            prop::sample::select(vec!["http", "https"]),
            prop::sample::select(vec![
                "reddit.com",
                "www.reddit.com",
                "old.reddit.com",
                "m.reddit.com",
                "OLD.Reddit.com",
                "redlib.example",
            ]),
            path,
            prop::sample::select(vec!["", "/"]),
            prop::sample::select(vec!["", "?utm_source=share", "?context=3#thing"]),
        )
            .prop_map(|(scheme, host, path, slash, tail)| {
                format!("{scheme}://{host}{path}{slash}{tail}")
            })
    }

    /// [`reddit_url`], or that URL behind one or two `over18` interstitials.
    fn stored_reddit_url() -> impl Strategy<Value = String> {
        (reddit_url(), 0..3usize).prop_map(|(mut stored, wraps)| {
            for _ in 0..wraps {
                let dest: String =
                    url::form_urlencoded::byte_serialize(stored.as_bytes()).collect();
                stored = format!("https://www.reddit.com/over18/?dest={dest}");
            }
            stored
        })
    }

    /// `cleanup reddit` run as the account on `own`, with one on each of [`DOMAINS`].
    fn opts_on(own: &str) -> RedditCleanupOpts {
        let account = |domain: &str| RedditConfig {
            domain: domain.into(),
            ..RedditConfig::default()
        };
        RedditCleanupOpts {
            mark_nsfw: false,
            fix_titles: false,
            config: account(own),
            dates: crate::test_support::NO_DATING,
            configured: DOMAINS.iter().map(|domain| account(domain)).collect(),
        }
    }

    /// Asserts that `keys` keys `stored` as it does the URL `plan` moves it to: its
    /// [`RedditCleanupOpts::natural_url`], then onto `collapse_onto` if its copies on
    /// several hosts collapse there.
    fn assert_keys_survive_cleanup(
        keys: &impl UrlKey,
        opts: &RedditCleanupOpts,
        stored: &str,
        collapse_onto: Option<&str>,
    ) -> Result<(), TestCaseError> {
        let stored = url(stored);
        prop_assert!(keys.dedup_key(&stored).is_some(), "{} has no key", stored);
        let natural = opts.natural_url(&stored);
        let moved = collapse_onto
            .and_then(|host| with_host(&natural, host))
            .unwrap_or(natural);
        prop_assert_eq!(
            keys.dedup_key(&moved),
            keys.dedup_key(&stored),
            "{} -> {}",
            stored,
            moved
        );
        Ok(())
    }

    proptest! {
        #![proptest_config(crate::test_support::proptest_config())]

        #[test]
        fn a_reddit_url_keys_the_same_once_cleaned_up(
            stored in reddit_url(),
            own in prop::sample::select(DOMAINS.to_vec()),
            collapse_onto in proptest::option::of(prop::sample::select(DOMAINS.to_vec())),
        ) {
            let opts = opts_on(own);
            assert_keys_survive_cleanup(&opts.reddit_keys(), &opts, &stored, collapse_onto)?;
        }

        #[test]
        fn an_over18_url_keys_the_same_once_cleaned_up(
            stored in stored_reddit_url(),
            own in prop::sample::select(DOMAINS.to_vec()),
            collapse_onto in proptest::option::of(prop::sample::select(DOMAINS.to_vec())),
        ) {
            let opts = opts_on(own);
            assert_keys_survive_cleanup(&opts.reddit_keys(), &opts, &stored, collapse_onto)?;
        }

        #[test]
        fn hackernews_keys_a_reddit_article_the_same_once_cleaned_up(
            stored in stored_reddit_url(),
            own in prop::sample::select(DOMAINS.to_vec()),
            collapse_onto in proptest::option::of(prop::sample::select(DOMAINS.to_vec())),
        ) {
            let opts = opts_on(own);
            let keys = HackerNewsKeys::new(opts.reddit_keys());
            assert_keys_survive_cleanup(&keys, &opts, &stored, collapse_onto)?;
        }
    }
}

#[cfg(test)]
mod loop_tests;
