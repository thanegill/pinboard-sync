//! The sync write path: pick the drafts not already on Pinboard, and write them
//! sequentially (Pinboard rate-limits `posts/add`). Source reads happen in the
//! caller (so an `--all` run can fetch services concurrently); only the writes are
//! serialized here. Generic over the [`Source`]/[`BookmarkStore`] ports so it can
//! be unit-tested with in-memory fakes.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::pinboard::{Bookmark, BookmarkStore, RATE_LIMIT_SECS};
use crate::source::{BookmarkDraft, Source};

/// The drafts not already present on Pinboard, matching each existing bookmark URL
/// through this source's `existing_key`.
pub fn filter_new<S: Source>(
    source: &S,
    drafts: Vec<BookmarkDraft>,
    existing: &[Bookmark],
) -> Vec<BookmarkDraft> {
    let keys: HashSet<String> = existing
        .iter()
        .filter_map(|b| source.existing_key(&b.url))
        .collect();
    drafts
        .into_iter()
        .filter(|d| !keys.contains(&d.dedup_key))
        .collect()
}

/// Write `drafts` to Pinboard sequentially, pausing [`RATE_LIMIT_SECS`] between
/// `posts/add` calls. Returns the number written (0 in `dry_run`).
pub async fn write_drafts<P: BookmarkStore>(
    pinboard: &P,
    drafts: &[BookmarkDraft],
    dry_run: bool,
    verbose: bool,
) -> Result<usize> {
    let mut written = 0usize;
    let mut posted = false;
    for draft in drafts {
        if dry_run {
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
        if verbose {
            eprintln!("added {}  [{}]", draft.url, draft.tags.join(" "));
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Source;
    use crate::test_support::{listing_entry, FakePinboard, FakeReddit};
    use serde_json::json;

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
    async fn filter_new_skips_present_then_write_adds_the_rest() {
        let reddit = FakeReddit {
            saved: vec![
                post("t3_a", "/r/rust/comments/a/x/"),
                post("t3_b", "/r/rust/comments/b/y/"),
            ],
            ..Default::default()
        };
        // Post a is already on Pinboard (any reddit host/case matches via reddit_key).
        let existing = vec![bookmark("https://www.reddit.com/r/Rust/comments/a/x/")];

        let drafts = reddit.fetch().await.unwrap();
        let new = filter_new(&reddit, drafts, &existing);
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].url, "https://old.reddit.com/r/rust/comments/b/y/");
        assert_eq!(new[0].tags, vec!["reddit", "subreddit:rust"]);

        let pinboard = FakePinboard::default();
        let written = write_drafts(&pinboard, &new, false, false).await.unwrap();
        assert_eq!(written, 1);
        // The write path never lists posts/all itself.
        assert_eq!(*pinboard.all_calls.borrow(), 0);
        assert_eq!(pinboard.added.borrow().len(), 1);
    }

    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let reddit = FakeReddit {
            saved: vec![post("t3_a", "/r/rust/comments/a/x/")],
            ..Default::default()
        };
        let new = filter_new(&reddit, reddit.fetch().await.unwrap(), &[]);
        let pinboard = FakePinboard::default();
        let written = write_drafts(&pinboard, &new, true, false).await.unwrap();
        assert_eq!(written, 0);
        assert!(pinboard.added.borrow().is_empty());
    }

    #[tokio::test]
    async fn caller_can_cap_the_number_written() {
        let reddit = FakeReddit {
            saved: vec![
                post("t3_a", "/r/rust/comments/a/"),
                post("t3_b", "/r/rust/comments/b/"),
                post("t3_c", "/r/rust/comments/c/"),
            ],
            ..Default::default()
        };
        let mut new = filter_new(&reddit, reddit.fetch().await.unwrap(), &[]);
        assert_eq!(new.len(), 3);
        new.truncate(2);
        let pinboard = FakePinboard::default();
        let written = write_drafts(&pinboard, &new, false, false).await.unwrap();
        assert_eq!(written, 2);
        assert_eq!(pinboard.added.borrow().len(), 2);
    }
}
