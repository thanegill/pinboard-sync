//! The sync write path: pick the drafts not already on Pinboard, and write them
//! sequentially (Pinboard rate-limits `posts/add`). Source reads happen in the
//! caller (so an `--all` run can fetch services concurrently); only the writes are
//! serialized here. Generic over the [`Source`]/[`BookmarkStore`] ports so it can
//! be unit-tested with in-memory fakes.

use std::collections::HashSet;

use log::{debug, error};

use crate::bookmark::{Bookmark, BookmarkStore};
use crate::source::{BookmarkDraft, Source};

/// The drafts not already present on Pinboard, matching each existing bookmark URL
/// through this source's [`UrlKey::dedup_key`].
pub fn filter_new<S: Source>(
    source: &S,
    drafts: Vec<BookmarkDraft>,
    existing: &[Bookmark],
) -> Vec<BookmarkDraft> {
    let keys: HashSet<String> = existing
        .iter()
        .filter_map(|b| source.dedup_key(&b.url))
        .collect();
    drafts
        .into_iter()
        .filter(|d| !keys.contains(&d.dedup_key))
        .collect()
}

/// Tally of a write pass: how many drafts were written vs. failed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WriteOutcome {
    pub written: usize,
    pub failed: usize,
}

/// Write `drafts` to Pinboard sequentially (the store spaces its own `posts/add` calls).
/// A single bookmark that fails to write (e.g. a rejected URL) is logged and skipped so
/// the rest still go through; the failure is reflected in the returned [`WriteOutcome`]
/// for a non-zero exit. Writes nothing in `dry_run`.
pub async fn write_drafts<P: BookmarkStore>(
    pinboard: &P,
    drafts: &[BookmarkDraft],
    dry_run: bool,
) -> WriteOutcome {
    let mut outcome = WriteOutcome::default();
    for draft in drafts {
        let bm = &draft.bookmark;
        if dry_run {
            // The source post date as RFC3339, when set (the sync loop has already
            // applied the use_post_date flag + age cap); empty = let Pinboard default.
            let dt = bm
                .timestamp
                .and_then(crate::timefmt::to_rfc3339)
                .unwrap_or_default();
            println!("[dry-run] {}", bm.url);
            println!("          title: {}", bm.title);
            if !bm.note.is_empty() {
                println!("          notes: {}", crate::preview(&bm.note));
            }
            println!("          tags:  [{}]", bm.tags.join(" "));
            if !dt.is_empty() {
                println!("          date:  {dt}");
            }
            continue;
        }

        // The sync loop already resolved `public`/`read_later` and the post date on the
        // draft's bookmark, so write it straight through (the client paces itself).
        match pinboard.add(bm).await {
            Ok(()) => {
                outcome.written += 1;
                debug!("added {}  [{}]", bm.url, bm.tags.join(" "));
            }
            // Log and skip — one bad bookmark shouldn't abort the rest of the run.
            Err(e) => {
                outcome.failed += 1;
                error!("adding bookmark {}: {e:#}", bm.url);
            }
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Source;
    use crate::test_support::{listing_entry, FakePinboard, FakeReddit};
    use serde_json::json;
    use url::Url;

    fn post(name: &str, permalink: &str) -> crate::model::RedditListingEntry {
        listing_entry(
            "t3",
            json!({ "name": name, "subreddit": "rust", "permalink": permalink, "title": "T" }),
        )
    }

    /// An existing Pinboard bookmark at `url` (other fields irrelevant to dedup).
    fn bookmark(url: &str) -> Bookmark {
        Bookmark {
            url: Url::parse(url).unwrap(),
            title: String::new(),
            note: String::new(),
            tags: Vec::new(),
            timestamp: None,
            public: false,
            read_later: false,
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
        assert_eq!(
            new[0].bookmark.url.as_str(),
            "https://old.reddit.com/r/rust/comments/b/y/"
        );
        assert_eq!(new[0].bookmark.tags, vec!["reddit", "subreddit:rust"]);

        let pinboard = FakePinboard::default();
        let outcome = write_drafts(&pinboard, &new, false).await;
        assert_eq!(outcome.written, 1);
        assert_eq!(outcome.failed, 0);
        // The write path never lists posts/all itself.
        assert_eq!(*pinboard.all_calls.borrow(), 0);
        assert_eq!(pinboard.added.borrow().len(), 1);
    }

    #[tokio::test]
    async fn present_bookmark_with_unparseable_time_still_dedups() {
        // Regression: a stored bookmark whose Pinboard `time` won't parse must remain in
        // the dedup set. It reaches this path through `Bookmark::try_from` (timestamp
        // becomes None), and its draft must count as already-present, not new — otherwise
        // sync re-adds the URL and clobbers its date/title/tags/notes.
        let reddit = FakeReddit {
            saved: vec![post("t3_a", "/r/rust/comments/a/x/")],
            ..Default::default()
        };
        let stored = Bookmark::try_from(crate::pinboard::PinboardBookmark {
            url: "https://www.reddit.com/r/Rust/comments/a/x/".into(),
            description: "T".into(),
            extended: String::new(),
            tags: String::new(),
            time: "not a date".into(),
            shared: "no".into(),
            toread: "no".into(),
        })
        .unwrap();
        assert_eq!(stored.timestamp, None);

        let drafts = reddit.fetch().await.unwrap();
        let new = filter_new(&reddit, drafts, &[stored]);
        assert!(new.is_empty());
    }

    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let reddit = FakeReddit {
            saved: vec![post("t3_a", "/r/rust/comments/a/x/")],
            ..Default::default()
        };
        let new = filter_new(&reddit, reddit.fetch().await.unwrap(), &[]);
        let pinboard = FakePinboard::default();
        let outcome = write_drafts(&pinboard, &new, true).await;
        assert_eq!(outcome.written, 0);
        assert!(pinboard.added.borrow().is_empty());
    }

    #[tokio::test]
    async fn write_drafts_passes_each_drafts_toread_and_shared() {
        let draft = |url: &str, toread: bool, shared: bool| BookmarkDraft {
            bookmark: Bookmark {
                url: Url::parse(url).unwrap(),
                title: "T".into(),
                note: String::new(),
                tags: vec![],
                timestamp: None,
                public: shared,
                read_later: toread,
            },
            dedup_key: url.into(),
        };
        let drafts = vec![
            draft("https://a.test/", true, true),
            draft("https://b.test/", false, false),
        ];
        let pinboard = FakePinboard::default();
        write_drafts(&pinboard, &drafts, false).await;
        let added = pinboard.added.borrow();
        assert_eq!(added.len(), 2);
        assert!(added[0].toread && added[0].shared);
        assert!(!added[1].toread && !added[1].shared);
    }

    #[tokio::test]
    async fn write_drafts_sends_post_date_as_dt_when_set() {
        let draft = |url: &str, post_date: Option<i64>| BookmarkDraft {
            bookmark: Bookmark {
                url: Url::parse(url).unwrap(),
                title: "T".into(),
                note: String::new(),
                tags: vec![],
                timestamp: post_date.and_then(crate::timefmt::from_unix),
                public: false,
                read_later: false,
            },
            dedup_key: url.into(),
        };
        let drafts = vec![
            draft("https://a.test/", Some(1_292_096_882)),
            draft("https://b.test/", None),
        ];
        let pinboard = FakePinboard::default();
        write_drafts(&pinboard, &drafts, false).await;
        let added = pinboard.added.borrow();
        assert_eq!(added[0].dt, "2010-12-11T19:48:02Z");
        assert_eq!(added[1].dt, "");
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
        let outcome = write_drafts(&pinboard, &new, false).await;
        assert_eq!(outcome.written, 2);
        assert_eq!(pinboard.added.borrow().len(), 2);
    }

    #[tokio::test]
    async fn write_drafts_logs_and_skips_a_failing_bookmark() {
        let draft = |url: &str| BookmarkDraft {
            bookmark: Bookmark {
                url: Url::parse(url).unwrap(),
                title: "T".into(),
                note: String::new(),
                tags: vec![],
                timestamp: None,
                public: false,
                read_later: false,
            },
            dedup_key: url.into(),
        };
        let drafts = vec![
            draft("https://a.test/"),
            draft("https://bad.test/"),
            draft("https://c.test/"),
        ];
        let mut pinboard = FakePinboard::default();
        pinboard.fail_add_urls.insert("https://bad.test/".into());

        let outcome = write_drafts(&pinboard, &drafts, false).await;
        // The bad one is skipped; the ones on either side still go through.
        assert_eq!(outcome.written, 2);
        assert_eq!(outcome.failed, 1);
        let added = pinboard.added.borrow();
        let urls: Vec<&str> = added.iter().map(|a| a.url.as_str()).collect();
        assert_eq!(urls, vec!["https://a.test/", "https://c.test/"]);
    }
}
