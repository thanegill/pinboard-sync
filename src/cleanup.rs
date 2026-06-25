//! `cleanup` subcommand: normalize existing Reddit bookmarks in a Pinboard
//! account. Rewrites URLs to `old.reddit.com` (unwrapping `over18` redirects),
//! normalizes tags, and — using Reddit's `/api/info` for authoritative data —
//! marks NSFW posts and replaces generic placeholder titles.

use std::collections::{BTreeSet, HashMap};

use anyhow::{anyhow, bail, Result};
use log::{debug, error, info};

use crate::model::{cased_subreddit, reddit_key};
use crate::pinboard::{apply_update, Bookmark, BookmarkStore, BookmarkUpdate};
use crate::reddit::PostInfo;
use crate::source::{host_matches, split_host_path, SourceError};

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
}

pub async fn run<P: BookmarkStore, R: PostInfo>(
    pinboard: &P,
    reddit: Option<&R>,
    opts: &CleanupOpts,
    bookmarks: &[Bookmark],
) -> Result<()> {
    let reddit_bms: Vec<_> = bookmarks
        .iter()
        .filter(|b| is_reddit_url(&b.url))
        .cloned()
        .collect();
    info!(
        "scanning {} reddit bookmark(s){}",
        reddit_bms.len(),
        if opts.dry_run { " (dry run)" } else { "" }
    );

    let info = fetch_post_info(reddit, opts, &reddit_bms).await?;

    let now = crate::timefmt::now_unix();
    let mut changed = 0usize;
    let mut failed = 0usize;
    let mut wrote = false;
    for bm in &reddit_bms {
        let normalized = normalize_url(&bm.url, &opts.domain);
        let url_changed = normalized.is_some();
        let new_url = normalized.unwrap_or_else(|| bm.url.clone());

        let mut tags = normalize_tags(
            &new_url,
            &bm.tag_list(),
            &opts.base_tag,
            &opts.subreddit_tag_prefix,
        );
        let post = post_fullname(&new_url).and_then(|f| info.get(&f));

        let wants_nsfw =
            opts.mark_nsfw && post.is_some_and(|p| p.over_18) && !tags.iter().any(|t| t == "nsfw");
        if wants_nsfw {
            tags.push("nsfw".to_string());
            tags.sort();
        }

        let mut description = bm.description.clone();
        if opts.fix_titles && is_placeholder_title(&description) {
            if let Some(title) = post.and_then(|p| p.title.clone()) {
                description = title;
            }
        }

        // The creation date to write: source post date within the cap, else preserve
        // (or "now" when stale and cleanup_stale_to_now). Bound to a `String` here so it
        // outlives the borrow in `BookmarkUpdate` below.
        let dt = crate::timefmt::cleanup_dt(
            opts.use_post_date,
            opts.max_age_days,
            opts.cleanup_stale_to_now,
            post.and_then(|p| p.created_utc).map(|s| s as i64),
            now,
            &bm.time,
        );

        // Older syncs wrote an empty self-post's notes as a link back to its own
        // permalink — a duplicate of the bookmark URL. Drop that self-link.
        let extended = if is_self_link_notes(&bm.extended, &new_url) {
            String::new()
        } else {
            bm.extended.clone()
        };

        let old_tags: BTreeSet<String> = bm.tag_list().into_iter().collect();
        let new_tags: BTreeSet<String> = tags.iter().cloned().collect();
        let tags_changed = old_tags != new_tags;
        let desc_changed = description != bm.description;
        let ext_changed = extended != bm.extended;
        let date_changed = dt != bm.time;

        if !(url_changed || tags_changed || desc_changed || ext_changed || date_changed) {
            continue;
        }

        if opts.dry_run {
            changed += 1;
            println!("[dry-run] {}", bm.url);
            if url_changed {
                println!("          url   -> {new_url}");
            }
            if tags_changed {
                println!("          tags  -> [{}]", tags.join(" "));
            }
            if desc_changed {
                println!("          title -> {description}");
            }
            if ext_changed {
                println!("          notes -> (removed duplicated self-link)");
            }
            if date_changed {
                println!("          date  -> {dt}");
            }
            continue;
        }

        // Log and skip a single failed update so the rest of the pass still runs.
        match apply_update(
            pinboard,
            &mut wrote,
            BookmarkUpdate {
                url: &new_url,
                description: &description,
                extended: &extended,
                tags: &tags,
                shared: bm.is_shared(),
                toread: bm.is_toread(),
                dt: &dt,
            },
            url_changed.then_some(bm.url.as_str()),
        )
        .await
        {
            Ok(()) => {
                changed += 1;
                debug!("updated {} -> {new_url} [{}]", bm.url, tags.join(" "));
            }
            Err(e) => {
                failed += 1;
                error!("updating bookmark {}: {e:#}", bm.url);
            }
        }
    }

    if opts.dry_run {
        println!("{changed} bookmark(s) would change.");
    } else {
        info!("done: updated {changed} bookmark(s)");
    }
    if failed > 0 {
        bail!("{failed} bookmark(s) failed to update");
    }
    Ok(())
}

/// Batch-fetch `over_18` + `title` + `created_utc` for every post referenced by the
/// bookmarks, keyed by fullname. Empty when none of NSFW tagging, title fixing, or
/// `use_post_date` is requested (all that the `/api/info` call feeds).
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
            map.insert(
                name,
                PostMeta {
                    over_18: entry.fields.over_18,
                    title: entry.fields.title.filter(|s| !s.is_empty()),
                    created_utc: entry.fields.created_utc,
                },
            );
        }
    }
    Ok(map)
}

// --- pure transforms ---------------------------------------------------------

/// Whether `url`'s host is reddit.com or a `*.reddit.com` subdomain.
pub fn is_reddit_url(url: &str) -> bool {
    let Some((_, after)) = url.split_once("://") else {
        return false;
    };
    let (host, _) = split_host_path(after);
    host_matches(&host, "reddit.com")
}

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
    let (_scheme, after) = url.split_once("://")?;
    let (host, rest) = split_host_path(after);
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
    fn is_reddit_url_checks_host_only() {
        assert!(is_reddit_url("https://old.reddit.com/r/rust/"));
        assert!(is_reddit_url("http://reddit.com/r/x/"));
        assert!(!is_reddit_url("https://example.com/reddit.com/x"));
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
        // permalink. The URL is already normalized, so only the notes should change.
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
