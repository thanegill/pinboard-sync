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

/// The per-account write knobs resolved for one sync job: the flags stamped onto every
/// new bookmark and the caps applied to the batch. Mirrors the resolved fields of a
/// `SyncJob` so the per-job transform can run without the concrete client.
#[derive(Debug, Clone, Copy)]
pub struct JobSettings {
    pub toread: bool,
    pub shared: bool,
    pub use_post_date: bool,
    pub max_age_days: u64,
    pub limit: usize,
}

/// The new drafts for one account, ready to write: those not already on Pinboard
/// ([`filter_new`]) and collapsed so each `dedup_key` appears once (keep-first, matching
/// the cross-job [`merge_deduped`] so `limit` counts distinct items rather than letting
/// duplicate-key drafts consume slots), each stamped with the account's resolved
/// `toread`/`shared` flags and post date (kept only when `use_post_date` is on and the
/// post is within the age cap relative to `now`, else cleared so Pinboard defaults to
/// "now"), and truncated to the per-job `limit` (0 = unlimited). `now` is a parameter so
/// tests are deterministic.
pub fn prepare_new_drafts<S: Source>(
    source: &S,
    drafts: Vec<BookmarkDraft>,
    existing: &[Bookmark],
    settings: &JobSettings,
    now: i64,
) -> Vec<BookmarkDraft> {
    let mut new = filter_new(source, drafts, existing);
    let mut seen = HashSet::new();
    new.retain(|d| seen.insert(d.dedup_key.clone()));
    for d in &mut new {
        d.bookmark.read_later = settings.toread;
        d.bookmark.public = settings.shared;
        d.bookmark.timestamp = if settings.use_post_date {
            d.bookmark.timestamp.filter(|t| {
                crate::timefmt::within_age_cap(now, t.unix_timestamp(), settings.max_age_days)
            })
        } else {
            None
        };
    }
    if settings.limit > 0 && new.len() > settings.limit {
        new.truncate(settings.limit);
    }
    new
}

/// Flatten each job's prepared drafts into one write batch, keeping the first occurrence
/// of any `dedup_key` so two drafts the dedup logic treats as one item are written once
/// (matching [`filter_new`]'s existing-key check — otherwise two fresh drafts whose URLs
/// differ byte-for-byte but share a key, e.g. two HN favorites of the same article stored
/// as `http://x` vs `https://x`, would each be written on a first-time run). Order is
/// preserved: jobs in order, drafts in order within a job.
pub fn merge_deduped(per_job: Vec<Vec<BookmarkDraft>>) -> Vec<BookmarkDraft> {
    let mut merged: Vec<BookmarkDraft> = Vec::new();
    let mut seen = HashSet::new();
    for drafts in per_job {
        for draft in drafts {
            if seen.insert(draft.dedup_key.clone()) {
                merged.push(draft);
            }
        }
    }
    merged
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

    fn post_at(name: &str, permalink: &str, created_utc: i64) -> crate::model::RedditListingEntry {
        listing_entry(
            "t3",
            json!({ "name": name, "subreddit": "rust", "permalink": permalink, "title": "T", "created_utc": created_utc }),
        )
    }

    fn settings(use_post_date: bool, max_age_days: u64, limit: usize) -> JobSettings {
        JobSettings {
            toread: false,
            shared: false,
            use_post_date,
            max_age_days,
            limit,
        }
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
    async fn prepare_new_drafts_stamps_flags_and_clears_dates_outside_the_age_cap() {
        let now = 1_700_000_000;
        let reddit = FakeReddit {
            saved: vec![
                post_at("t3_recent", "/r/rust/comments/recent/", now - 5 * 86_400),
                post_at("t3_old", "/r/rust/comments/old/", now - 60 * 86_400),
            ],
            ..Default::default()
        };
        let job = JobSettings {
            toread: true,
            shared: true,
            use_post_date: true,
            max_age_days: 30,
            limit: 0,
        };
        let drafts = reddit.fetch().await.unwrap();
        let new = prepare_new_drafts(&reddit, drafts, &[], &job, now);
        assert_eq!(new.len(), 2);
        // The resolved flags are stamped onto every new draft.
        assert!(new
            .iter()
            .all(|d| d.bookmark.read_later && d.bookmark.public));
        let find = |needle: &str| {
            new.iter()
                .find(|d| d.bookmark.url.as_str().contains(needle))
                .unwrap()
        };
        // Within the cap: source date kept. Outside: cleared so Pinboard uses "now".
        assert_eq!(
            find("comments/recent")
                .bookmark
                .timestamp
                .unwrap()
                .unix_timestamp(),
            now - 5 * 86_400
        );
        assert_eq!(find("comments/old").bookmark.timestamp, None);
    }

    #[tokio::test]
    async fn prepare_new_drafts_clears_all_dates_when_use_post_date_off() {
        let now = 1_700_000_000;
        let reddit = FakeReddit {
            saved: vec![post_at("t3_a", "/r/rust/comments/a/", now - 5 * 86_400)],
            ..Default::default()
        };
        let new = prepare_new_drafts(
            &reddit,
            reddit.fetch().await.unwrap(),
            &[],
            &settings(false, 30, 0),
            now,
        );
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].bookmark.timestamp, None);
    }

    #[tokio::test]
    async fn prepare_new_drafts_truncates_to_the_per_job_limit() {
        let reddit = FakeReddit {
            saved: vec![
                post("t3_a", "/r/rust/comments/a/"),
                post("t3_b", "/r/rust/comments/b/"),
                post("t3_c", "/r/rust/comments/c/"),
            ],
            ..Default::default()
        };
        // limit 0 means unlimited.
        let all = prepare_new_drafts(
            &reddit,
            reddit.fetch().await.unwrap(),
            &[],
            &settings(false, 30, 0),
            0,
        );
        assert_eq!(all.len(), 3);
        // A positive limit caps the batch.
        let capped = prepare_new_drafts(
            &reddit,
            reddit.fetch().await.unwrap(),
            &[],
            &settings(false, 30, 2),
            0,
        );
        assert_eq!(capped.len(), 2);
    }

    #[tokio::test]
    async fn prepare_new_drafts_dedups_within_the_batch_before_applying_the_limit() {
        // Regression: two drafts sharing a dedup_key must collapse to one BEFORE the
        // limit truncation, so `limit` counts distinct items. Otherwise the duplicate
        // consumes a slot and crowds out a genuinely new item (which merge_deduped then
        // drops when it collapses the pair across jobs).
        let draft = |url: &str, key: &str| BookmarkDraft {
            bookmark: Bookmark {
                url: Url::parse(url).unwrap(),
                title: "T".into(),
                note: String::new(),
                tags: vec![],
                timestamp: None,
                public: false,
                read_later: false,
            },
            dedup_key: key.into(),
        };
        let drafts = vec![
            draft("http://example.com/x", "k"),
            draft("https://example.com/x", "k"),
            draft("https://distinct.test/", "kB"),
        ];
        let new = prepare_new_drafts(
            &FakeReddit::default(),
            drafts,
            &[],
            &settings(false, 30, 2),
            0,
        );
        let keys: Vec<&str> = new.iter().map(|d| d.dedup_key.as_str()).collect();
        assert_eq!(keys, vec!["k", "kB"]);
        // The kept duplicate is the first occurrence.
        assert_eq!(new[0].bookmark.url.as_str(), "http://example.com/x");
    }

    #[tokio::test]
    async fn merge_deduped_writes_a_url_shared_across_jobs_once() {
        // Two jobs each yield the same reddit post; merged it appears once, so the
        // writer adds the URL a single time.
        let reddit = FakeReddit {
            saved: vec![post("t3_a", "/r/rust/comments/a/x/")],
            ..Default::default()
        };
        let job = settings(false, 30, 0);
        let one = prepare_new_drafts(&reddit, reddit.fetch().await.unwrap(), &[], &job, 0);
        let two = prepare_new_drafts(&reddit, reddit.fetch().await.unwrap(), &[], &job, 0);
        assert_eq!(one.len(), 1);
        assert_eq!(two.len(), 1);

        let merged = merge_deduped(vec![one, two]);
        assert_eq!(merged.len(), 1);

        let pinboard = FakePinboard::default();
        let outcome = write_drafts(&pinboard, &merged, false).await;
        assert_eq!(outcome.written, 1);
        assert_eq!(pinboard.added.borrow().len(), 1);
    }

    #[tokio::test]
    async fn merge_deduped_collapses_drafts_sharing_a_dedup_key_with_differing_urls() {
        // Regression: two fresh drafts in one batch that share a dedup_key but whose URLs
        // differ byte-for-byte (e.g. two HN favorites of the same article stored as
        // http vs https) must collapse to one write — matching filter_new's existing-key
        // check, so a first-time run doesn't create two bookmarks for one item.
        let draft = |url: &str, key: &str| BookmarkDraft {
            bookmark: Bookmark {
                url: Url::parse(url).unwrap(),
                title: "T".into(),
                note: String::new(),
                tags: vec![],
                timestamp: None,
                public: false,
                read_later: false,
            },
            dedup_key: key.into(),
        };
        let merged = merge_deduped(vec![
            vec![draft("http://example.com/x", "hn:1")],
            vec![draft("https://example.com/x", "hn:1")],
        ]);
        assert_eq!(merged.len(), 1);
        // The first occurrence is kept.
        assert_eq!(merged[0].bookmark.url.as_str(), "http://example.com/x");
    }

    #[tokio::test]
    async fn merge_deduped_keeps_drafts_with_distinct_dedup_keys() {
        let draft = |url: &str, key: &str| BookmarkDraft {
            bookmark: Bookmark {
                url: Url::parse(url).unwrap(),
                title: "T".into(),
                note: String::new(),
                tags: vec![],
                timestamp: None,
                public: false,
                read_later: false,
            },
            dedup_key: key.into(),
        };
        let merged = merge_deduped(vec![
            vec![draft("https://a.test/", "hn:1")],
            vec![draft("https://b.test/", "hn:2")],
        ]);
        assert_eq!(merged.len(), 2);
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
