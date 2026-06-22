//! The sync loop: fetch a source's drafts, skip those already on Pinboard, write
//! the rest. Generic over the [`Source`]/[`BookmarkStore`] ports so it can be
//! unit-tested with in-memory fakes.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::pinboard::{Bookmark, BookmarkStore, RATE_LIMIT_SECS};
use crate::source::{Source, SourceError};

pub struct SyncConfig {
    /// Optional cap on bookmarks written per run; 0 = all.
    pub limit: usize,
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

/// Run the sync. `existing_bookmarks` is the already-fetched `posts/all` set (the
/// caller fetches it once and shares it across accounts in an `--all` run). Errors
/// as `SourceError` so the caller can fire the auth-failure hook on `ReauthRequired`;
/// Pinboard errors map to `Other`.
pub async fn run<S: Source, P: BookmarkStore>(
    source: &S,
    pinboard: &P,
    cfg: &SyncConfig,
    existing_bookmarks: &[Bookmark],
) -> Result<SyncSummary, SourceError> {
    let drafts = source.fetch().await?;
    let fetched = drafts.len();

    // Pinboard is the sync state: skip drafts whose dedup key is already present,
    // mapping each existing bookmark URL through this source's `existing_key`.
    let existing: HashSet<String> = existing_bookmarks
        .iter()
        .filter_map(|b| source.existing_key(&b.url))
        .collect();
    let mut new_items: Vec<_> = drafts
        .into_iter()
        .filter(|d| !existing.contains(&d.dedup_key))
        .collect();
    let new = new_items.len();

    let capped = cfg.limit > 0 && new > cfg.limit;
    if capped {
        new_items.truncate(cfg.limit);
    }

    println!(
        "Fetched {fetched} item(s); {} already on Pinboard; {new} new{}{}.",
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
    for draft in &new_items {
        if cfg.dry_run {
            println!("[dry-run] {}", draft.url);
            println!("          title: {}", draft.description);
            if !draft.extended.is_empty() {
                println!("          notes: {}", crate::preview(&draft.extended));
            }
            println!("          tags:  [{}]", draft.tags.join(" "));
            continue;
        }

        // Pinboard asks for ~3s between posts/add calls.
        if posted {
            tokio::time::sleep(Duration::from_secs(RATE_LIMIT_SECS)).await;
        }
        pinboard
            .add(&draft.url, &draft.description, &draft.extended, &draft.tags)
            .await
            .with_context(|| format!("adding bookmark {}", draft.url))?;
        posted = true;
        written += 1;
        if cfg.verbose {
            eprintln!("added {}  [{}]", draft.url, draft.tags.join(" "));
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
    use crate::pinboard::Bookmark;
    use crate::test_support::{listing_entry, FakePinboard, FakeReddit};
    use serde_json::json;

    fn config() -> SyncConfig {
        SyncConfig {
            limit: 0,
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

    /// An existing Pinboard bookmark at `url` (other fields irrelevant to dedup).
    fn bookmark(url: &str) -> Bookmark {
        Bookmark {
            url: url.into(),
            description: String::new(),
            extended: String::new(),
            tags: String::new(),
            time: String::new(),
            shared: "no".into(),
            toread: "no".into(),
        }
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
        let pinboard = FakePinboard::default();
        // Post a is already on Pinboard (any reddit host/case matches via reddit_key).
        let existing = vec![bookmark("https://www.reddit.com/r/Rust/comments/a/x/")];

        let summary = run(&reddit, &pinboard, &config(), &existing).await.unwrap();

        assert_eq!(summary.fetched, 2);
        assert_eq!(summary.already_present, 1);
        assert_eq!(summary.new, 1);
        assert_eq!(summary.written, 1);

        // run() must not fetch posts/all itself — the caller supplies it.
        assert_eq!(*pinboard.all_calls.borrow(), 0);

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

        let summary = run(&reddit, &pinboard, &cfg, &[]).await.unwrap();
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

        let summary = run(&reddit, &pinboard, &cfg, &[]).await.unwrap();
        assert_eq!(summary.new, 3);
        assert_eq!(summary.written, 2);
        assert_eq!(pinboard.added.borrow().len(), 2);
    }
}
