//! The sync write path: pick the drafts not already on Pinboard, and write them
//! sequentially (Pinboard rate-limits `posts/add`). Source reads happen in the
//! caller (so an `--all` run can fetch services concurrently); only the writes are
//! serialized here. Generic over the [`Source`]/[`BookmarkStore`] ports so it can
//! be unit-tested with in-memory fakes.

use std::collections::HashSet;
use std::time::Duration;

use log::{debug, error};

use crate::pinboard::{Bookmark, BookmarkStore};
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

/// Tally of a write pass: how many drafts were written vs. failed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WriteOutcome {
    pub written: usize,
    pub failed: usize,
}

/// Write `drafts` to Pinboard sequentially, pausing `pinboard.rate_limit_secs()`
/// between `posts/add` calls. A single bookmark that fails to write (e.g. a
/// rejected URL) is logged and skipped so the rest still go through; the failure is
/// reflected in the returned [`WriteOutcome`] for a non-zero exit. Writes nothing in
/// `dry_run`.
pub async fn write_drafts<P: BookmarkStore>(
    pinboard: &P,
    drafts: &[BookmarkDraft],
    dry_run: bool,
) -> WriteOutcome {
    let mut outcome = WriteOutcome::default();
    let mut posted = false;
    for draft in drafts {
        // The source post date as RFC3339, when set (the sync loop has already applied
        // the use_post_date flag + age cap); empty = let Pinboard default to now.
        let dt = draft
            .post_date
            .and_then(crate::timefmt::unix_to_rfc3339)
            .unwrap_or_default();
        if dry_run {
            println!("[dry-run] {}", draft.url);
            println!("          title: {}", draft.description);
            if !draft.extended.is_empty() {
                println!("          notes: {}", crate::preview(&draft.extended));
            }
            println!("          tags:  [{}]", draft.tags.join(" "));
            if !dt.is_empty() {
                println!("          date:  {dt}");
            }
            continue;
        }

        // Pinboard asks for ~3s between posts/add calls (configurable). Pace after the
        // first attempt regardless of its outcome — a failed attempt still hit the API.
        if posted {
            tokio::time::sleep(Duration::from_secs(pinboard.rate_limit_secs())).await;
        }
        posted = true;
        match pinboard
            .add(
                &draft.url,
                &draft.description,
                &draft.extended,
                &draft.tags,
                draft.toread,
                draft.shared,
                &dt,
            )
            .await
        {
            Ok(()) => {
                outcome.written += 1;
                debug!("added {}  [{}]", draft.url, draft.tags.join(" "));
            }
            // Log and skip — one bad bookmark shouldn't abort the rest of the run.
            Err(e) => {
                outcome.failed += 1;
                error!("adding bookmark {}: {e:#}", draft.url);
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
        let outcome = write_drafts(&pinboard, &new, false).await;
        assert_eq!(outcome.written, 1);
        assert_eq!(outcome.failed, 0);
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
        let outcome = write_drafts(&pinboard, &new, true).await;
        assert_eq!(outcome.written, 0);
        assert!(pinboard.added.borrow().is_empty());
    }

    #[tokio::test]
    async fn write_drafts_passes_each_drafts_toread_and_shared() {
        let draft = |url: &str, toread: bool, shared: bool| BookmarkDraft {
            url: url.into(),
            description: "T".into(),
            extended: String::new(),
            tags: vec![],
            dedup_key: url.into(),
            toread,
            shared,
            post_date: None,
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
            url: url.into(),
            description: "T".into(),
            extended: String::new(),
            tags: vec![],
            dedup_key: url.into(),
            toread: false,
            shared: false,
            post_date,
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
            url: url.into(),
            description: "T".into(),
            extended: String::new(),
            tags: vec![],
            dedup_key: url.into(),
            toread: false,
            shared: false,
            post_date: None,
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
