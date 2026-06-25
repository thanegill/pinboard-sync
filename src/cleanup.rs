//! `cleanup` subcommand: normalize existing Reddit bookmarks in a Pinboard
//! account. Rewrites URLs to `old.reddit.com` (unwrapping `over18` redirects),
//! normalizes tags, and — using Reddit's `/api/info` for authoritative data —
//! marks NSFW posts and replaces generic placeholder titles.

use std::collections::{BTreeSet, HashMap};

use anyhow::{anyhow, bail, Result};

use crate::cleanup_pass::{run_pass, CleanupPass, Planned};
use crate::htmltext::html_to_plain;
use crate::model::{cased_subreddit, reddit_key};
use crate::pinboard::{Bookmark, BookmarkStore};
use crate::reddit::PostInfo;
use crate::source::{host_is, host_matches, split_host_path, SourceError};

pub struct CleanupOpts {
    pub dry_run: bool,
    pub mark_nsfw: bool,
    pub fix_titles: bool,
    pub base_tag: String,
    pub subreddit_tag_prefix: String,
    /// Reddit host that URLs are rewritten to (default `old.reddit.com`).
    pub domain: String,
    /// Re-date bookmarks to the source post date (within the age cap).
    pub use_post_date: bool,
    /// Backdate age cap, in days.
    pub max_age_days: u64,
    /// Re-date posts older than the cap to "now" instead of leaving them.
    pub cleanup_stale_to_now: bool,
}

/// Authoritative per-post data from `/api/info`.
struct PostMeta {
    over_18: bool,
    title: Option<String>,
    /// Post creation time (unix epoch seconds), for `use_post_date`.
    created_utc: Option<f64>,
    /// The notes rebuilt from authoritative data — the quoted body wrapped in a
    /// `<blockquote>` (empty when there's nothing to quote). Used to retrofit existing
    /// bookmarks to the shape `sync` now writes.
    extended: String,
}

pub async fn run<P: BookmarkStore, R: PostInfo>(
    pinboard: &P,
    reddit: Option<&R>,
    opts: &CleanupOpts,
    bookmarks: &[Bookmark],
) -> Result<()> {
    let reddit_bms: Vec<_> = bookmarks
        .iter()
        .filter(|b| host_is(&b.url, "reddit.com"))
        .cloned()
        .collect();

    let info = fetch_post_info(reddit, opts, &reddit_bms).await?;
    let pass = RedditPass {
        info,
        opts,
        now: crate::timefmt::now_unix(),
    };
    let failed = run_pass(pinboard, &reddit_bms, opts.dry_run, "reddit", &pass).await;
    if failed > 0 {
        bail!("{failed} bookmark(s) failed to update");
    }
    Ok(())
}

/// Re-shapes one reddit bookmark: normalize the URL/tags, then apply the authoritative
/// `/api/info` data (NSFW marker, placeholder-title replacement, post date, rebuilt notes).
struct RedditPass<'a> {
    info: HashMap<String, PostMeta>,
    opts: &'a CleanupOpts,
    now: i64,
}

impl CleanupPass for RedditPass<'_> {
    async fn plan(&self, bm: &Bookmark) -> Result<Option<Planned>> {
        let opts = self.opts;
        let new_url = normalize_url(&bm.url, &opts.domain).unwrap_or_else(|| bm.url.clone());

        let mut tags = normalize_tags(
            &new_url,
            &bm.tag_list(),
            &opts.base_tag,
            &opts.subreddit_tag_prefix,
        );
        let post = post_fullname(&new_url).and_then(|f| self.info.get(&f));

        if opts.mark_nsfw && post.is_some_and(|p| p.over_18) && !tags.iter().any(|t| t == "nsfw") {
            tags.push("nsfw".to_string());
            tags.sort();
        }

        let mut description = html_to_plain(&bm.description);
        if opts.fix_titles && is_placeholder_title(&description) {
            if let Some(title) = post.and_then(|p| p.title.clone()) {
                description = html_to_plain(&title);
            }
        }

        let dt = crate::timefmt::cleanup_dt(
            opts.use_post_date,
            opts.max_age_days,
            opts.cleanup_stale_to_now,
            post.and_then(|p| p.created_utc).map(|s| s as i64),
            self.now,
            &bm.time,
        );

        // Reshape the notes. Only for *post* bookmarks: `/api/info` is keyed by the post
        // fullname (`post_fullname`), so a comment's own body is never fetched — leave those
        // notes alone. A non-empty rebuild from authoritative data (selftext wrapped in a
        // <blockquote>, or a link post's external URL) replaces the stored notes. Otherwise
        // (empty rebuild, or no entry this run) only drop a bare self-link an older sync
        // wrote — never wipe genuine notes.
        let extended = if is_comment_url(&new_url) {
            bm.extended.clone()
        } else if let Some(rebuilt) = post.map(|p| p.extended.clone()).filter(|e| !e.is_empty()) {
            rebuilt
        } else if is_self_link_notes(&bm.extended, &new_url) {
            String::new()
        } else {
            bm.extended.clone()
        };

        Ok(Some(Planned {
            url: new_url,
            description,
            extended,
            tags,
            dt,
        }))
    }
}

/// Batch-fetch `over_18` + `title` + `created_utc` + the rebuilt notes for every post
/// referenced by the bookmarks, keyed by fullname. Empty when none of NSFW tagging, title
/// fixing, or `use_post_date` is requested — the notes retrofit then rides on whichever of
/// those caused the `/api/info` call (it needs no extra fetch of its own).
async fn fetch_post_info<R: PostInfo>(
    reddit: Option<&R>,
    opts: &CleanupOpts,
    bookmarks: &[Bookmark],
) -> Result<HashMap<String, PostMeta>> {
    let mut map = HashMap::new();
    if !(opts.mark_nsfw || opts.fix_titles || opts.use_post_date) {
        return Ok(map);
    }
    let Some(reddit) = reddit else {
        return Ok(map);
    };

    let fullnames: Vec<String> = bookmarks
        .iter()
        .filter_map(|b| {
            let url = normalize_url(&b.url, &opts.domain).unwrap_or_else(|| b.url.clone());
            post_fullname(&url)
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if fullnames.is_empty() {
        return Ok(map);
    }

    let entries = reddit.info(&fullnames).await.map_err(|e| match e {
        SourceError::ReauthRequired(m) => {
            anyhow!("{m}\nSet a fresh REDDIT_COOKIE (reddit_session) and retry.")
        }
        SourceError::Other(e) => e,
    })?;
    for entry in entries {
        if let Some(name) = entry.fields.name.clone() {
            // Rebuild the notes from authoritative data the same way sync does, so the
            // <blockquote> shaping retrofits onto existing bookmarks. These entries are
            // always posts (`/api/info` is queried with post fullnames), so build the
            // post-shaped notes — selftext quoted, empty for an empty self-post, else the
            // external URL.
            let extended = crate::model::reddit_extended(
                false,
                "",
                entry.fields.selftext.as_deref().unwrap_or_default(),
                entry.fields.url.as_deref().unwrap_or_default(),
                entry.fields.is_self,
                None,
                &opts.domain,
            );
            map.insert(
                name,
                PostMeta {
                    over_18: entry.fields.over_18,
                    title: entry.fields.title.filter(|s| !s.is_empty()),
                    created_utc: entry.fields.created_utc,
                    extended,
                },
            );
        }
    }
    Ok(map)
}

// --- pure transforms ---------------------------------------------------------

/// Normalize a Reddit bookmark URL: unwrap an `over18/?dest=` redirect (recursively
/// URL-decoding the destination), then rewrite any reddit host to the configured
/// `domain`. Returns `Some(new)` only if it changed.
pub fn normalize_url(url: &str, domain: &str) -> Option<String> {
    let mut current = url.to_string();
    let mut changed = false;
    if let Some(dest) = over18_dest(&current) {
        current = dest;
        changed = true;
    }
    if let Some(n) = to_reddit_domain(&current, domain) {
        if n != current {
            current = n;
            changed = true;
        }
    }
    changed.then_some(current)
}

/// If `url` is an `over18` interstitial, return its decoded `dest`.
fn over18_dest(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    let marker = lower.find("reddit.com/over18")?;
    let rest = &url[marker..];
    let dpos = rest.find("dest=")?;
    Some(recursive_percent_decode(&rest[dpos + 5..]))
}

/// Rewrite any reddit host (`reddit.com` or a `*.reddit.com` subdomain) to the
/// configured `domain`, preserving the rest of the URL. Returns `None` for a
/// non-reddit host or one already equal to `domain`.
fn to_reddit_domain(url: &str, domain: &str) -> Option<String> {
    let (host, rest) = split_host_path(url);
    if host_matches(&host, "reddit.com") && host != domain {
        Some(format!("https://{domain}{rest}"))
    } else {
        None
    }
}

/// Extract the subreddit name from a Reddit URL's `/r/<sub>/` segment.
pub fn extract_subreddit(url: &str) -> Option<String> {
    let (_host, path) = split_host_path(url);
    let idx = path.find("/r/")?;
    let sub: String = path[idx + 3..]
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    (!sub.is_empty()).then_some(sub)
}

/// The post fullname (`t3_<id>`) from a permalink's `/comments/<id>/` segment.
/// Works for both post and comment permalinks (comments inherit the post id).
pub fn post_fullname(url: &str) -> Option<String> {
    let (_host, path) = split_host_path(url);
    let idx = path.find("/comments/")?;
    let id: String = path[idx + "/comments/".len()..]
        .chars()
        .take_while(|c| c.is_alphanumeric())
        .collect();
    (!id.is_empty()).then(|| format!("t3_{id}"))
}

/// Whether a reddit URL points at a comment (`/comments/<post>/<slug>/<comment>/`) rather
/// than a post. Used to skip the notes retrofit for comments, whose own body `/api/info`
/// never returns (it is keyed by the post fullname; see [`post_fullname`]).
fn is_comment_url(url: &str) -> bool {
    let (_host, path) = split_host_path(url);
    let Some(idx) = path.find("/comments/") else {
        return false;
    };
    let rest = &path[idx + "/comments/".len()..];
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    // post → [id, slug]; comment → [id, slug, comment-id].
    rest.split('/').filter(|s| !s.is_empty()).count() >= 3
}

/// Normalize a bookmark's tags: ensure the base tag, derive `subreddit:<sub>` from
/// the URL (casing per [`cased_subreddit`]), and strip bare/legacy/duplicate
/// subreddit forms. Returns a sorted, de-duplicated list.
pub fn normalize_tags(url: &str, existing: &[String], base_tag: &str, prefix: &str) -> Vec<String> {
    let mut set: BTreeSet<String> = existing.iter().cloned().collect();
    set.insert(base_tag.to_string());
    set.remove(prefix); // bare "subreddit:"
    set.remove(prefix.strip_suffix(':').unwrap_or(prefix)); // bare "subreddit"

    if let Some(raw) = extract_subreddit(url) {
        let cased = cased_subreddit(&raw);
        let forms = [
            raw.clone(),
            cased.clone(),
            raw.to_lowercase(),
            raw.to_uppercase(),
        ];
        for form in &forms {
            // Drop the bare subreddit tag in any case (but keep a literal "nsfw"),
            // and the legacy prefixed forms.
            if form != "nsfw" {
                set.remove(form);
            }
            set.remove(&format!("{prefix}{form}"));
        }
        set.insert(format!("{prefix}{cased}"));
    }
    set.into_iter().collect()
}

/// Whether `notes` is nothing but a Reddit link to the same post as `url` — the
/// duplicated self-link older syncs wrote into empty self-posts' notes. Compared by
/// [`reddit_key`], so it matches across hosts/casing; non-Reddit notes (e.g. a link
/// post's external URL) and free-text notes don't match and are left untouched.
fn is_self_link_notes(notes: &str, url: &str) -> bool {
    let key = reddit_key(notes.trim());
    key.is_some() && key == reddit_key(url)
}

/// The generic placeholder titles Reddit bookmarks often get from a browser.
pub fn is_placeholder_title(description: &str) -> bool {
    matches!(
        description.trim(),
        "Reddit - Dive into anything" | "www.reddit.com" | "reddit.com: over 18?"
    )
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn recursive_percent_decode(s: &str) -> String {
    let mut cur = s.to_string();
    for _ in 0..8 {
        if !cur.contains('%') {
            break;
        }
        let decoded = percent_decode(&cur);
        if decoded == cur {
            break;
        }
        cur = decoded;
    }
    cur
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_is_matches_reddit_hosts_only() {
        assert!(host_is("https://old.reddit.com/r/rust/", "reddit.com"));
        assert!(host_is("http://reddit.com/r/x/", "reddit.com"));
        assert!(!host_is("https://example.com/reddit.com/x", "reddit.com"));
    }

    #[test]
    fn normalize_url_rewrites_host_to_domain() {
        assert_eq!(
            normalize_url(
                "https://www.reddit.com/r/Rust/comments/abc/x/",
                "old.reddit.com"
            )
            .as_deref(),
            Some("https://old.reddit.com/r/Rust/comments/abc/x/")
        );
        assert_eq!(
            normalize_url("https://reddit.com/r/x/", "old.reddit.com").as_deref(),
            Some("https://old.reddit.com/r/x/")
        );
        assert_eq!(
            normalize_url("https://m.reddit.com/r/x/", "old.reddit.com").as_deref(),
            Some("https://old.reddit.com/r/x/")
        );
        assert_eq!(
            normalize_url("https://old.reddit.com/r/x/", "old.reddit.com"),
            None
        );
        // A non-default domain rewrites old.reddit.com too.
        assert_eq!(
            normalize_url("https://old.reddit.com/r/x/", "www.reddit.com").as_deref(),
            Some("https://www.reddit.com/r/x/")
        );
    }

    #[test]
    fn normalize_url_unwraps_over18_then_normalizes_host() {
        let url = "https://www.reddit.com/over18/?dest=https%3A%2F%2Fwww.reddit.com%2Fr%2Fx%2Fcomments%2Fa%2F";
        assert_eq!(
            normalize_url(url, "old.reddit.com").as_deref(),
            Some("https://old.reddit.com/r/x/comments/a/")
        );
    }

    #[test]
    fn extract_subreddit_from_path() {
        assert_eq!(
            extract_subreddit("https://old.reddit.com/r/AskReddit/comments/x/").as_deref(),
            Some("AskReddit")
        );
        assert_eq!(
            extract_subreddit("https://old.reddit.com/").as_deref(),
            None
        );
    }

    #[test]
    fn post_fullname_from_post_and_comment_permalinks() {
        assert_eq!(
            post_fullname("https://old.reddit.com/r/rust/comments/abc123/title/").as_deref(),
            Some("t3_abc123")
        );
        assert_eq!(
            post_fullname("https://old.reddit.com/r/rust/comments/abc123/title/def456/").as_deref(),
            Some("t3_abc123")
        );
        assert_eq!(
            post_fullname("https://old.reddit.com/r/rust/").as_deref(),
            None
        );
    }

    #[test]
    fn is_comment_url_distinguishes_comments_from_posts() {
        assert!(!is_comment_url(
            "https://old.reddit.com/r/rust/comments/abc/title/"
        ));
        assert!(is_comment_url(
            "https://old.reddit.com/r/rust/comments/abc/title/def/"
        ));
        // Query strings don't fool the segment count.
        assert!(is_comment_url(
            "https://old.reddit.com/r/rust/comments/abc/title/def/?context=3"
        ));
        assert!(!is_comment_url("https://old.reddit.com/r/rust/"));
    }

    #[test]
    fn normalize_tags_adds_base_and_subreddit_strips_legacy() {
        let existing = vec![
            "rust".to_string(),
            "subreddit:".to_string(),
            "subreddit:rust".to_string(),
        ];
        let tags = normalize_tags(
            "https://old.reddit.com/r/rust/comments/x/",
            &existing,
            "reddit",
            "subreddit:",
        );
        assert_eq!(
            tags,
            vec!["reddit".to_string(), "subreddit:rust".to_string()]
        );
    }

    #[test]
    fn normalize_tags_lowercases_all_caps_subreddit() {
        let tags = normalize_tags(
            "https://old.reddit.com/r/NEWS/comments/x/",
            &["subreddit:NEWS".to_string(), "NEWS".to_string()],
            "reddit",
            "subreddit:",
        );
        assert_eq!(
            tags,
            vec!["reddit".to_string(), "subreddit:news".to_string()]
        );
    }

    #[test]
    fn placeholder_titles_detected() {
        assert!(is_placeholder_title("Reddit - Dive into anything"));
        assert!(is_placeholder_title("  reddit.com: over 18?  "));
        assert!(!is_placeholder_title("Some real title : r/rust"));
    }

    #[test]
    fn recursive_percent_decode_handles_double_encoding() {
        assert_eq!(
            recursive_percent_decode("https%253A%252F%252Fx"),
            "https://x"
        );
    }
}

#[cfg(test)]
mod loop_tests {
    use super::*;
    use crate::pinboard::Bookmark;
    use crate::test_support::{listing_entry, FakePinboard, FakeReddit};
    use serde_json::json;

    fn opts() -> CleanupOpts {
        CleanupOpts {
            dry_run: false,
            mark_nsfw: true,
            fix_titles: true,
            base_tag: "reddit".into(),
            subreddit_tag_prefix: "subreddit:".into(),
            domain: "old.reddit.com".into(),
            use_post_date: false,
            max_age_days: 30,
            cleanup_stale_to_now: false,
        }
    }

    fn bookmark(url: &str, description: &str, tags: &str) -> Bookmark {
        Bookmark {
            url: url.into(),
            description: description.into(),
            extended: String::new(),
            tags: tags.into(),
            time: String::new(),
            shared: "no".into(),
            toread: "no".into(),
        }
    }

    #[tokio::test]
    async fn normalizes_url_tags_nsfw_and_title() {
        let pinboard = FakePinboard {
            // Carries notes + metadata that cleanup must preserve across the re-add.
            all: vec![Bookmark {
                url: "https://www.reddit.com/r/NEWS/comments/abc/x/".into(),
                description: "Reddit - Dive into anything".into(),
                extended: "original notes".into(),
                tags: String::new(),
                time: "1700000000".into(),
                shared: "yes".into(),
                toread: "yes".into(),
            }],
            ..Default::default()
        };
        let reddit = FakeReddit {
            info: vec![listing_entry(
                "t3",
                json!({ "name": "t3_abc", "subreddit": "NEWS",
                        "permalink": "/r/NEWS/comments/abc/x/", "title": "Real Title",
                        "over_18": true }),
            )],
            ..Default::default()
        };

        run(&pinboard, Some(&reddit), &opts(), &pinboard.all)
            .await
            .unwrap();

        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(
            updated[0].url,
            "https://old.reddit.com/r/NEWS/comments/abc/x/"
        );
        assert_eq!(updated[0].description, "Real Title");
        assert!(updated[0].tags.contains(&"reddit".to_string()));
        assert!(updated[0].tags.contains(&"subreddit:news".to_string()));
        assert!(updated[0].tags.contains(&"nsfw".to_string()));
        // Notes, privacy, to-read, and the original creation time are preserved.
        assert_eq!(updated[0].extended, "original notes");
        assert!(updated[0].shared);
        assert!(updated[0].toread);
        assert_eq!(updated[0].dt, "1700000000");
        // URL changed, so the old one is deleted.
        assert_eq!(
            *pinboard.deleted.borrow(),
            vec!["https://www.reddit.com/r/NEWS/comments/abc/x/".to_string()]
        );
    }

    #[tokio::test]
    async fn dates_bookmark_by_created_utc_when_use_post_date() {
        // An already-normalized bookmark: only the date should change.
        let pinboard = FakePinboard {
            all: vec![bookmark(
                "https://old.reddit.com/r/rust/comments/a/x/",
                "A real title",
                "reddit subreddit:rust",
            )],
            ..Default::default()
        };
        let reddit = FakeReddit {
            info: vec![listing_entry(
                "t3",
                json!({ "name": "t3_a", "subreddit": "rust",
                        "permalink": "/r/rust/comments/a/x/", "title": "A real title",
                        "over_18": false, "created_utc": 1_700_000_000 }),
            )],
            ..Default::default()
        };
        // A huge cap so the (old) post is always "within" it; nsfw/titles off.
        let opts = CleanupOpts {
            use_post_date: true,
            max_age_days: 1_000_000,
            mark_nsfw: false,
            fix_titles: false,
            ..opts()
        };

        run(&pinboard, Some(&reddit), &opts, &pinboard.all)
            .await
            .unwrap();

        let updated = pinboard.updated.borrow();
        assert_eq!(
            updated.len(),
            1,
            "a date-only change should still be written"
        );
        assert_eq!(updated[0].dt, "2023-11-14T22:13:20Z");
    }

    #[tokio::test]
    async fn strips_self_link_notes_from_a_self_post() {
        // An older sync left an empty self-post's notes as a link back to its own
        // permalink. No /api/info this run (nsfw/titles off), so the string-based
        // self-link drop applies. The URL is already normalized, so only the notes change.
        let pinboard = FakePinboard {
            all: vec![Bookmark {
                url: "https://old.reddit.com/r/rust/comments/a/x/".into(),
                description: "A real title".into(),
                extended: "https://www.reddit.com/r/rust/comments/a/x/".into(),
                tags: "reddit subreddit:rust".into(),
                time: "1700000000".into(),
                shared: "no".into(),
                toread: "no".into(),
            }],
            ..Default::default()
        };
        let opts = CleanupOpts {
            mark_nsfw: false,
            fix_titles: false,
            ..opts()
        };

        run(
            &pinboard,
            Some(&FakeReddit::default()),
            &opts,
            &pinboard.all,
        )
        .await
        .unwrap();

        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].extended, "");
        // URL was already normalized, so nothing is deleted.
        assert!(pinboard.deleted.borrow().is_empty());
    }

    #[tokio::test]
    async fn keeps_external_link_notes_on_a_link_post() {
        // A link post's notes hold the external URL — not the bookmark's own
        // permalink — so cleanup must preserve them while rewriting the host.
        let pinboard = FakePinboard {
            all: vec![Bookmark {
                url: "https://www.reddit.com/r/rust/comments/b/y/".into(),
                description: "A real title".into(),
                extended: "https://example.com/article".into(),
                tags: "reddit subreddit:rust".into(),
                time: "1700000000".into(),
                shared: "no".into(),
                toread: "no".into(),
            }],
            ..Default::default()
        };
        let opts = CleanupOpts {
            mark_nsfw: false,
            fix_titles: false,
            ..opts()
        };

        run(
            &pinboard,
            Some(&FakeReddit::default()),
            &opts,
            &pinboard.all,
        )
        .await
        .unwrap();

        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(
            updated[0].url,
            "https://old.reddit.com/r/rust/comments/b/y/"
        );
        assert_eq!(updated[0].extended, "https://example.com/article");
    }

    #[tokio::test]
    async fn retrofits_self_post_notes_with_blockquote() {
        // Already normalized except for the notes: an old unwrapped selftext. With
        // /api/info present, the rebuild wraps the authoritative selftext in a
        // <blockquote>; ext_changed triggers the write.
        let pinboard = FakePinboard {
            all: vec![Bookmark {
                url: "https://old.reddit.com/r/rust/comments/a/x/".into(),
                description: "A real title".into(),
                extended: "the body text".into(), // pre-blockquote notes
                tags: "reddit subreddit:rust".into(),
                time: String::new(),
                shared: "no".into(),
                toread: "no".into(),
            }],
            ..Default::default()
        };
        let reddit = FakeReddit {
            info: vec![listing_entry(
                "t3",
                json!({ "name": "t3_a", "subreddit": "rust",
                        "permalink": "/r/rust/comments/a/x/", "title": "A real title",
                        "selftext": "the body text" }),
            )],
            ..Default::default()
        };

        run(&pinboard, Some(&reddit), &opts(), &pinboard.all)
            .await
            .unwrap();

        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(
            updated[0].extended,
            "<blockquote>the body text</blockquote>"
        );
        // Notes-only change: URL unchanged, so nothing is deleted.
        assert!(pinboard.deleted.borrow().is_empty());
    }

    #[tokio::test]
    async fn leaves_comment_notes_untouched() {
        // A comment bookmark: /api/info only returns the parent *post* (post_fullname keys
        // by t3), never the comment body, so the comment's notes must be left as-is — the
        // post's selftext must not leak into the comment's notes.
        let stored = "Thread: https://old.reddit.com/r/rust/comments/a/x/\n\nmy comment";
        let pinboard = FakePinboard {
            all: vec![Bookmark {
                url: "https://old.reddit.com/r/rust/comments/a/x/c/".into(),
                description: "Parent title".into(),
                extended: stored.into(),
                tags: "reddit subreddit:rust reddit-comment".into(),
                time: String::new(),
                shared: "no".into(),
                toread: "no".into(),
            }],
            ..Default::default()
        };
        let reddit = FakeReddit {
            info: vec![listing_entry(
                "t3",
                json!({ "name": "t3_a", "subreddit": "rust",
                        "permalink": "/r/rust/comments/a/x/", "title": "Parent title",
                        "selftext": "POST BODY should not leak into the comment" }),
            )],
            ..Default::default()
        };

        run(&pinboard, Some(&reddit), &opts(), &pinboard.all)
            .await
            .unwrap();

        // Nothing changed (notes preserved, URL/title/tags already current), so no write.
        assert!(pinboard.updated.borrow().is_empty());
    }

    #[tokio::test]
    async fn skips_unchanged_and_non_reddit() {
        let pinboard = FakePinboard {
            all: vec![
                bookmark(
                    "https://old.reddit.com/r/rust/comments/a/x/",
                    "A real title",
                    "reddit subreddit:rust",
                ),
                bookmark("https://example.com/", "E", "misc"),
            ],
            ..Default::default()
        };
        let reddit = FakeReddit {
            info: vec![listing_entry(
                "t3",
                json!({ "name": "t3_a", "subreddit": "rust",
                        "permalink": "/r/rust/comments/a/x/", "title": "A real title",
                        "over_18": false }),
            )],
            ..Default::default()
        };

        run(&pinboard, Some(&reddit), &opts(), &pinboard.all)
            .await
            .unwrap();
        assert!(pinboard.updated.borrow().is_empty());
        assert!(pinboard.deleted.borrow().is_empty());
    }

    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let pinboard = FakePinboard {
            all: vec![bookmark(
                "https://www.reddit.com/r/rust/comments/a/x/",
                "T",
                "",
            )],
            ..Default::default()
        };
        let reddit = FakeReddit::default();
        let opts = CleanupOpts {
            dry_run: true,
            mark_nsfw: false,
            fix_titles: false,
            ..opts()
        };
        run(&pinboard, Some(&reddit), &opts, &pinboard.all)
            .await
            .unwrap();
        assert!(pinboard.updated.borrow().is_empty());
        assert!(pinboard.deleted.borrow().is_empty());
    }

    #[tokio::test]
    async fn logs_and_skips_a_failing_update_then_continues() {
        // Two bookmarks that both need a URL rewrite; the first one's update fails.
        let pinboard = FakePinboard {
            all: vec![
                bookmark("https://www.reddit.com/r/rust/comments/a/x/", "T", "reddit"),
                bookmark("https://www.reddit.com/r/rust/comments/b/y/", "T", "reddit"),
            ],
            fail_update_urls: ["https://old.reddit.com/r/rust/comments/a/x/".to_string()]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let reddit = FakeReddit::default();
        let opts = CleanupOpts {
            mark_nsfw: false,
            fix_titles: false,
            ..opts()
        };

        // The run reports failure (non-zero exit)...
        let err = run(&pinboard, Some(&reddit), &opts, &pinboard.all)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("1 bookmark(s) failed"));
        // ...but the second bookmark was still updated despite the first failing.
        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(
            updated[0].url,
            "https://old.reddit.com/r/rust/comments/b/y/"
        );
    }
}
