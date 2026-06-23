//! `cleanup` subcommand: normalize existing Reddit bookmarks in a Pinboard
//! account. Rewrites URLs to `old.reddit.com` (unwrapping `over18` redirects),
//! normalizes tags, and — using Reddit's `/api/info` for authoritative data —
//! marks NSFW posts and replaces generic placeholder titles.

use std::collections::{BTreeSet, HashMap};

use anyhow::{anyhow, Result};

use crate::model::cased_subreddit;
use crate::pinboard::{apply_update, Bookmark, BookmarkStore, BookmarkUpdate};
use crate::reddit::PostInfo;
use crate::source::{host_matches, split_host_path, SourceError};

pub struct CleanupOpts {
    pub dry_run: bool,
    pub verbose: bool,
    pub mark_nsfw: bool,
    pub fix_titles: bool,
    pub base_tag: String,
    pub subreddit_tag_prefix: String,
    /// Reddit host that URLs are rewritten to (default `old.reddit.com`).
    pub domain: String,
}

/// Authoritative per-post data from `/api/info`.
struct PostMeta {
    over_18: bool,
    title: Option<String>,
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
    println!(
        "Scanning {} reddit bookmark(s){}...",
        reddit_bms.len(),
        if opts.dry_run { " (dry run)" } else { "" }
    );

    let info = fetch_post_info(reddit, opts, &reddit_bms).await?;

    let mut changed = 0usize;
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

        let old_tags: BTreeSet<String> = bm.tag_list().into_iter().collect();
        let new_tags: BTreeSet<String> = tags.iter().cloned().collect();
        let tags_changed = old_tags != new_tags;
        let desc_changed = description != bm.description;

        if !(url_changed || tags_changed || desc_changed) {
            continue;
        }
        changed += 1;

        if opts.dry_run {
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
            continue;
        }

        apply_update(
            pinboard,
            &mut wrote,
            BookmarkUpdate {
                url: &new_url,
                description: &description,
                extended: &bm.extended,
                tags: &tags,
                shared: bm.is_shared(),
                toread: bm.is_toread(),
                dt: &bm.time,
            },
            url_changed.then_some(bm.url.as_str()),
        )
        .await?;
        if opts.verbose {
            eprintln!("updated {} -> {new_url} [{}]", bm.url, tags.join(" "));
        }
    }

    if opts.dry_run {
        println!("{changed} bookmark(s) would change.");
    } else {
        println!("Done. Updated {changed} bookmark(s).");
    }
    Ok(())
}

/// Batch-fetch `over_18` + `title` for every post referenced by the bookmarks,
/// keyed by fullname. Empty when neither NSFW nor title fixing is requested.
async fn fetch_post_info<R: PostInfo>(
    reddit: Option<&R>,
    opts: &CleanupOpts,
    bookmarks: &[Bookmark],
) -> Result<HashMap<String, PostMeta>> {
    let mut map = HashMap::new();
    if !(opts.mark_nsfw || opts.fix_titles) {
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
            verbose: false,
            mark_nsfw: true,
            fix_titles: true,
            base_tag: "reddit".into(),
            subreddit_tag_prefix: "subreddit:".into(),
            domain: "old.reddit.com".into(),
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
            all: vec![bookmark(
                "https://www.reddit.com/r/NEWS/comments/abc/x/",
                "Reddit - Dive into anything",
                "",
            )],
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
        // URL changed, so the old one is deleted.
        assert_eq!(
            *pinboard.deleted.borrow(),
            vec!["https://www.reddit.com/r/NEWS/comments/abc/x/".to_string()]
        );
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
}
