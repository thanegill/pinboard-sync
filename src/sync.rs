//! The sync loop: fetch saved items, skip those already on Pinboard, write the
//! rest. Generic over the [`SavedSource`]/[`BookmarkStore`] ports so it can be
//! unit-tested with in-memory fakes.

use std::time::Duration;

use anyhow::{Context, Result};

use crate::model::{reddit_key, ListingEntry, SavedItem};
use crate::pinboard::{BookmarkStore, RATE_LIMIT_SECS};
use crate::reddit::{RedditError, SavedSource};

pub struct SyncConfig {
    /// Optional cap on bookmarks written per run; 0 = all.
    pub limit: usize,
    pub base_tag: String,
    pub subreddit_tag_prefix: String,
    pub dry_run: bool,
    pub verbose: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SyncSummary {
    pub fetched: usize,
    pub already_present: usize,
    pub new: usize,
    pub written: usize,
}

/// Run the sync. Errors as `RedditError` so the caller can fire the
/// auth-failure hook on `ReauthRequired`; Pinboard errors map to `Other`.
pub async fn run<R: SavedSource, P: BookmarkStore>(
    reddit: &R,
    pinboard: &P,
    cfg: &SyncConfig,
) -> Result<SyncSummary, RedditError> {
    let items: Vec<SavedItem> = reddit
        .fetch_saved()
        .await?
        .into_iter()
        .filter_map(ListingEntry::into_saved_item)
        .collect();
    let fetched = items.len();

    // Pinboard is the sync state: skip saved items already bookmarked there.
    let existing = pinboard.existing_reddit_keys().await?;
    let mut new_items: Vec<SavedItem> = items
        .into_iter()
        .filter(|it| reddit_key(&it.permalink).is_none_or(|k| !existing.contains(&k)))
        .collect();
    let new = new_items.len();

    let capped = cfg.limit > 0 && new > cfg.limit;
    if capped {
        new_items.truncate(cfg.limit);
    }

    println!(
        "Fetched {fetched} saved item(s); {} already on Pinboard; {new} new{}{}.",
        fetched - new,
        if capped {
            format!(", writing {}", cfg.limit)
        } else {
            String::new()
        },
        if cfg.dry_run { " (dry run)" } else { "" }
    );

    let mut written = 0usize;
    let mut posted = false;
    for item in &new_items {
        let url = item.bookmark_url();
        let tags = item.tags(&cfg.base_tag, &cfg.subreddit_tag_prefix);

        if cfg.dry_run {
            println!("[dry-run] {url}");
            println!("          title: {}", item.description);
            if !item.extended.is_empty() {
                println!("          notes: {}", crate::preview(&item.extended));
            }
            println!("          tags:  [{}]", tags.join(" "));
            continue;
        }

        // Pinboard asks for ~3s between posts/add calls.
        if posted {
            tokio::time::sleep(Duration::from_secs(RATE_LIMIT_SECS)).await;
        }
        pinboard
            .add(&url, &item.description, &item.extended, &tags)
            .await
            .with_context(|| format!("adding bookmark {url}"))?;
        posted = true;
        written += 1;
        if cfg.verbose {
            eprintln!("added {url}  [{}]", tags.join(" "));
        }
    }

    if !cfg.dry_run {
        println!("Done. Wrote {written} bookmark(s) to Pinboard.");
    }
    Ok(SyncSummary {
        fetched,
        already_present: fetched - new,
        new,
        written,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{listing_entry, FakePinboard, FakeReddit};
    use serde_json::json;
    use std::collections::HashSet;

    fn config() -> SyncConfig {
        SyncConfig {
            limit: 0,
            base_tag: "reddit".into(),
            subreddit_tag_prefix: "subreddit:".into(),
            dry_run: false,
            verbose: false,
        }
    }

    fn post(name: &str, permalink: &str) -> crate::model::ListingEntry {
        listing_entry(
            "t3",
            json!({ "name": name, "subreddit": "rust", "permalink": permalink, "title": "T" }),
        )
    }

    #[tokio::test]
    async fn writes_only_items_not_already_present() {
        let reddit = FakeReddit {
            saved: vec![
                post("t3_a", "/r/rust/comments/a/x/"),
                post("t3_b", "/r/rust/comments/b/y/"),
            ],
            ..Default::default()
        };
        let pinboard = FakePinboard {
            existing: HashSet::from(["/r/rust/comments/a/x".to_string()]),
            ..Default::default()
        };

        let summary = run(&reddit, &pinboard, &config()).await.unwrap();

        assert_eq!(summary.fetched, 2);
        assert_eq!(summary.already_present, 1);
        assert_eq!(summary.new, 1);
        assert_eq!(summary.written, 1);

        let added = pinboard.added.borrow();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].url, "https://old.reddit.com/r/rust/comments/b/y/");
        assert_eq!(added[0].tags, vec!["reddit", "subreddit:rust"]);
    }

    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let reddit = FakeReddit {
            saved: vec![post("t3_a", "/r/rust/comments/a/x/")],
            ..Default::default()
        };
        let pinboard = FakePinboard::default();
        let cfg = SyncConfig {
            dry_run: true,
            ..config()
        };

        let summary = run(&reddit, &pinboard, &cfg).await.unwrap();
        assert_eq!(summary.new, 1);
        assert_eq!(summary.written, 0);
        assert!(pinboard.added.borrow().is_empty());
    }

    #[tokio::test]
    async fn limit_caps_the_number_written() {
        let reddit = FakeReddit {
            saved: vec![
                post("t3_a", "/r/rust/comments/a/"),
                post("t3_b", "/r/rust/comments/b/"),
                post("t3_c", "/r/rust/comments/c/"),
            ],
            ..Default::default()
        };
        let pinboard = FakePinboard::default();
        let cfg = SyncConfig {
            limit: 2,
            ..config()
        };

        let summary = run(&reddit, &pinboard, &cfg).await.unwrap();
        assert_eq!(summary.new, 3);
        assert_eq!(summary.written, 2);
        assert_eq!(pinboard.added.borrow().len(), 2);
    }
}
