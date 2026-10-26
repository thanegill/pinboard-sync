use std::num::NonZeroUsize;

use super::*;
use crate::source::{page_is_caught_up, Source, SourceError, UrlKey};
use crate::test_support::{listing_entry, replay, FakePinboard, FakeReddit};
use serde_json::json;
use url::Url;

/// A source serving one or more streams of pages, each newest first and concatenated
/// in order (as HackerNews serves stories then comments). Its incremental read stops
/// each stream at its first caught-up page, as the real sources' do. Its dedup key is
/// the URL itself.
struct PagedSource {
    streams: Vec<Vec<Vec<BookmarkDraft>>>,
}

impl PagedSource {
    fn single(pages: Vec<Vec<BookmarkDraft>>) -> Self {
        Self {
            streams: vec![pages],
        }
    }
}

impl UrlKey for PagedSource {
    fn dedup_key(&self, url: &Url) -> Option<String> {
        Some(url.to_string())
    }
}

impl Source for PagedSource {
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError> {
        Ok(self.streams.concat().concat())
    }

    async fn fetch_incremental(
        &self,
        existing: &[Bookmark],
    ) -> Result<Vec<BookmarkDraft>, SourceError> {
        let synced_keys = self.synced_keys(existing);
        let mut drafts = Vec::new();
        for stream in &self.streams {
            for page in stream {
                drafts.extend(page.iter().cloned());
                if page_is_caught_up(page.len(), page, &synced_keys) {
                    break;
                }
            }
        }
        Ok(drafts)
    }
}

fn tagged_draft(url: &str, tag: &str) -> BookmarkDraft {
    BookmarkDraft {
        bookmark: Bookmark {
            tags: vec![tag.into()],
            ..bookmark(url)
        },
        dedup_key: url.into(),
    }
}

fn draft(url: &str) -> BookmarkDraft {
    BookmarkDraft {
        bookmark: bookmark(url),
        dedup_key: url.into(),
    }
}

fn added_urls(pinboard: &FakePinboard) -> Vec<String> {
    pinboard
        .added
        .borrow()
        .iter()
        .map(|add| add.url.clone())
        .collect()
}

fn post(name: &str, permalink: &str) -> crate::reddit::model::RedditListingEntry {
    listing_entry(
        "t3",
        json!({ "name": name, "subreddit": "rust", "permalink": permalink, "title": "T" }),
    )
}

fn post_at(
    name: &str,
    permalink: &str,
    created_utc: i64,
) -> crate::reddit::model::RedditListingEntry {
    listing_entry(
        "t3",
        json!({ "name": name, "subreddit": "rust", "permalink": permalink, "title": "T", "created_utc": created_utc }),
    )
}

fn settings(use_post_date: bool, max_age_days: u64, limit: Option<NonZeroUsize>) -> JobSettings {
    JobSettings {
        limit: limit.map_or(SyncLimit::All, SyncLimit::Max),
        toread: false,
        shared: false,
        dates: DateOpts {
            use_post_date,
            max_age_days,
            stale_to_now: false,
        },
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
    let _ = write_drafts(&pinboard, &drafts, false).await;
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
    let _ = write_drafts(&pinboard, &drafts, false).await;
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
        limit: SyncLimit::All,
        toread: true,
        shared: true,
        dates: DateOpts {
            use_post_date: true,
            max_age_days: 30,
            stale_to_now: false,
        },
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
        &settings(false, 30, None),
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
    let all = prepare_new_drafts(
        &reddit,
        reddit.fetch().await.unwrap(),
        &[],
        &settings(false, 30, None),
        0,
    );
    assert_eq!(all.len(), 3);
    // A positive limit caps the batch.
    let capped = prepare_new_drafts(
        &reddit,
        reddit.fetch().await.unwrap(),
        &[],
        &settings(false, 30, NonZeroUsize::new(2)),
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
        &settings(false, 30, NonZeroUsize::new(2)),
        0,
    );
    let keys: Vec<&str> = new.iter().map(|d| d.dedup_key.as_str()).collect();
    assert_eq!(keys, vec!["kB", "k"]);
    // The kept duplicate is the first occurrence.
    assert_eq!(new[1].bookmark.url.as_str(), "http://example.com/x");
}

#[tokio::test]
async fn merge_deduped_writes_a_url_shared_across_jobs_once() {
    // Two jobs each yield the same reddit post; merged it appears once, so the
    // writer adds the URL a single time.
    let reddit = FakeReddit {
        saved: vec![post("t3_a", "/r/rust/comments/a/x/")],
        ..Default::default()
    };
    let job = settings(false, 30, None);
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

#[tokio::test]
async fn new_drafts_are_written_oldest_first() {
    let source = PagedSource::single(vec![
        vec![draft("https://c.test/"), draft("https://b.test/")],
        vec![draft("https://a.test/")],
    ]);
    let new = prepare_new_drafts(
        &source,
        source.fetch().await.unwrap(),
        &[],
        &settings(false, 30, None),
        0,
    );
    let pinboard = FakePinboard::default();
    let _ = write_drafts(&pinboard, &new, false).await;
    assert_eq!(
        added_urls(&pinboard),
        ["https://a.test/", "https://b.test/", "https://c.test/"]
    );
}

#[tokio::test]
async fn a_capped_job_writes_its_newest_drafts_oldest_first() {
    let source = PagedSource::single(vec![vec![
        draft("https://d.test/"),
        draft("https://c.test/"),
        draft("https://b.test/"),
        draft("https://a.test/"),
    ]]);
    let new = prepare_new_drafts(
        &source,
        source.fetch().await.unwrap(),
        &[],
        &settings(false, 30, NonZeroUsize::new(2)),
        0,
    );
    let pinboard = FakePinboard::default();
    let _ = write_drafts(&pinboard, &new, false).await;
    assert_eq!(
        added_urls(&pinboard),
        ["https://c.test/", "https://d.test/"]
    );
}

/// For every point at which a first `auto` sync of `source` could be killed, a second,
/// uninterrupted one writes everything the first didn't.
async fn assert_any_interrupted_backfill_resumes(source: &PagedSource) {
    let mut every_url: Vec<String> = source
        .fetch()
        .await
        .unwrap()
        .iter()
        .map(|d| d.bookmark.url.to_string())
        .collect();
    every_url.sort();
    for killed_after in 0..every_url.len() {
        assert_eq!(
            write_after_interrupting(source, killed_after).await,
            every_url,
            "killed after {killed_after} writes"
        );
    }
}

/// Run an `auto` sync of `source` that is killed after `killed_after` writes, then a
/// second, uninterrupted one, and return every URL written across both, sorted.
async fn write_after_interrupting(source: &PagedSource, killed_after: usize) -> Vec<String> {
    let job = settings(false, 30, None);
    let pinboard = FakePinboard::default();

    let first_run = prepare_new_drafts(
        source,
        source.fetch_incremental(&[]).await.unwrap(),
        &[],
        &job,
        0,
    );
    let _ = write_drafts(&pinboard, &first_run[..killed_after], false).await;

    let account = replay(&[], &pinboard.ops.borrow());
    let second_run = prepare_new_drafts(
        source,
        source.fetch_incremental(&account).await.unwrap(),
        &account,
        &job,
        0,
    );
    let _ = write_drafts(&pinboard, &second_run, false).await;

    let mut written = added_urls(&pinboard);
    written.sort();
    written
}

#[tokio::test]
async fn the_run_after_an_interrupted_backfill_writes_the_rest() {
    let source = PagedSource::single(vec![
        vec![draft("https://e.test/"), draft("https://d.test/")],
        vec![draft("https://c.test/"), draft("https://b.test/")],
        vec![draft("https://a.test/")],
    ]);
    assert_any_interrupted_backfill_resumes(&source).await;
}

#[tokio::test]
async fn the_run_after_an_interrupted_backfill_writes_the_rest_of_both_hn_streams() {
    let source = PagedSource {
        streams: vec![
            vec![
                vec![draft("https://s4.test/"), draft("https://s3.test/")],
                vec![draft("https://s2.test/"), draft("https://s1.test/")],
            ],
            vec![
                vec![draft("https://c4.test/"), draft("https://c3.test/")],
                vec![draft("https://c2.test/"), draft("https://c1.test/")],
            ],
        ],
    };
    assert_any_interrupted_backfill_resumes(&source).await;
}

#[tokio::test]
async fn jobs_are_written_in_turn_each_oldest_first_and_the_earlier_keeps_a_shared_item() {
    let job = settings(false, 30, None);
    let first_job = PagedSource::single(vec![vec![
        draft("https://a2.test/"),
        tagged_draft("https://shared.test/", "first"),
        draft("https://a1.test/"),
    ]]);
    let second_job = PagedSource::single(vec![vec![
        tagged_draft("https://shared.test/", "second"),
        draft("https://b1.test/"),
    ]]);
    let mut per_job = Vec::new();
    for source in [&first_job, &second_job] {
        per_job.push(prepare_new_drafts(
            source,
            source.fetch().await.unwrap(),
            &[],
            &job,
            0,
        ));
    }
    let pinboard = FakePinboard::default();
    let _ = write_drafts(&pinboard, &merge_deduped(per_job), false).await;

    assert_eq!(
        added_urls(&pinboard),
        [
            "https://a1.test/",
            "https://shared.test/",
            "https://a2.test/",
            "https://b1.test/",
        ]
    );
    assert_eq!(pinboard.added.borrow()[1].tags, ["first"]);
}
