use super::*;
use crate::bookmark::{AccountView, CleanupStore};
use crate::pinboard::PinboardBookmark;
use crate::test_support::{listing_entry, FakePinboard, FakeReddit};
use serde_json::json;

fn opts() -> RedditCleanupOpts {
    RedditCleanupOpts {
        mark_nsfw: true,
        fix_titles: true,
        config: RedditConfig::default(),
        dates: DateOpts {
            use_post_date: false,
            max_age_days: 30,
            stale_to_now: false,
        },
    }
}

/// Dating on, with a cap so large the (old) test post is always "within" it.
const DATED: DateOpts = DateOpts {
    use_post_date: true,
    max_age_days: 1_000_000,
    stale_to_now: false,
};

fn bookmark(url: &str, description: &str, tags: &str) -> Bookmark {
    PinboardBookmark {
        url: url.into(),
        description: description.into(),
        extended: String::new(),
        tags: tags.into(),
        time: String::new(),
        shared: "no".into(),
        toread: "no".into(),
    }
    .try_into()
    .unwrap()
}

#[tokio::test]
async fn normalizes_url_tags_nsfw_and_title() {
    let pinboard = FakePinboard {
        // Carries notes + metadata that cleanup must preserve across the re-add.
        all: vec![PinboardBookmark {
            url: "https://www.reddit.com/r/NEWS/comments/abc/x/".into(),
            description: "Reddit - Dive into anything".into(),
            extended: "original notes".into(),
            tags: String::new(),
            time: "2023-11-14T22:13:20Z".into(),
            shared: "yes".into(),
            toread: "yes".into(),
        }
        .try_into()
        .unwrap()],
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

    run(
        &CleanupStore::new(&pinboard, AccountView::new(pinboard.all.clone()), false),
        Some(&reddit),
        &opts(),
    )
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
    assert_eq!(updated[0].dt, "2023-11-14T22:13:20Z");
    // URL changed, so the old one is deleted.
    assert_eq!(
        *pinboard.deleted.borrow(),
        vec!["https://www.reddit.com/r/NEWS/comments/abc/x/".to_string()]
    );
}

#[tokio::test]
async fn marks_nsfw_with_configured_tag_not_literal_nsfw() {
    // With reddit.tag_nsfw = "adult", cleanup must apply the configured marker (as
    // sync does), never a second literal "nsfw". The www host forces a URL rewrite so
    // the write is emitted; the post has no marker yet, so cleanup adds one.
    let pinboard = FakePinboard {
        all: vec![bookmark(
            "https://www.reddit.com/r/NEWS/comments/abc/x/",
            "A real title",
            "reddit subreddit:news",
        )],
        ..Default::default()
    };
    let reddit = FakeReddit {
        info: vec![listing_entry(
            "t3",
            json!({ "name": "t3_abc", "subreddit": "NEWS",
                    "permalink": "/r/NEWS/comments/abc/x/", "title": "A real title",
                    "over_18": true }),
        )],
        ..Default::default()
    };
    let opts = RedditCleanupOpts {
        fix_titles: false,
        config: RedditConfig {
            nsfw: "adult".into(),
            ..RedditConfig::default()
        },
        ..opts()
    };

    run(
        &CleanupStore::new(&pinboard, AccountView::new(pinboard.all.clone()), false),
        Some(&reddit),
        &opts,
    )
    .await
    .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert!(updated[0].tags.contains(&"adult".to_string()));
    assert!(!updated[0].tags.contains(&"nsfw".to_string()));
}

#[tokio::test]
async fn configured_nsfw_tag_is_idempotent() {
    // A post already carrying the configured "adult" marker (what sync wrote) and
    // otherwise normalized must not be rewritten to add a duplicate/literal marker.
    let pinboard = FakePinboard {
        all: vec![bookmark(
            "https://old.reddit.com/r/NEWS/comments/abc/x/",
            "A real title",
            "reddit subreddit:news adult",
        )],
        ..Default::default()
    };
    let reddit = FakeReddit {
        info: vec![listing_entry(
            "t3",
            json!({ "name": "t3_abc", "subreddit": "NEWS",
                    "permalink": "/r/NEWS/comments/abc/x/", "title": "A real title",
                    "over_18": true }),
        )],
        ..Default::default()
    };
    let opts = RedditCleanupOpts {
        fix_titles: false,
        config: RedditConfig {
            nsfw: "adult".into(),
            ..RedditConfig::default()
        },
        ..opts()
    };

    run(
        &CleanupStore::new(&pinboard, AccountView::new(pinboard.all.clone()), false),
        Some(&reddit),
        &opts,
    )
    .await
    .unwrap();

    assert!(pinboard.updated.borrow().is_empty());
}

#[tokio::test]
async fn preserves_markup_in_stored_title() {
    // A non-placeholder stored title carrying angle brackets / tag-like text is kept
    // verbatim through cleanup, not stripped as HTML — the deliberate tradeoff of
    // treating every title as plain text (html_to_plain escapes `<`/`>` before
    // parsing). The www host still forces a rewrite so an update is emitted.
    let pinboard = FakePinboard {
        all: vec![bookmark(
            "https://www.reddit.com/r/rust/comments/a/x/",
            "Vec<String> vs <b>Box</b> in Rust",
            "reddit subreddit:rust",
        )],
        ..Default::default()
    };

    run(
        &CleanupStore::new(&pinboard, AccountView::new(pinboard.all.clone()), false),
        None::<&FakeReddit>,
        &opts(),
    )
    .await
    .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].description, "Vec<String> vs <b>Box</b> in Rust");
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
    let opts = RedditCleanupOpts {
        dates: DATED,
        mark_nsfw: false,
        fix_titles: false,
        ..opts()
    };

    run(
        &CleanupStore::new(&pinboard, AccountView::new(pinboard.all.clone()), false),
        Some(&reddit),
        &opts,
    )
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
        all: vec![PinboardBookmark {
            url: "https://old.reddit.com/r/rust/comments/a/x/".into(),
            description: "A real title".into(),
            extended: "https://www.reddit.com/r/rust/comments/a/x/".into(),
            tags: "reddit subreddit:rust".into(),
            time: "2023-11-14T22:13:20Z".into(),
            shared: "no".into(),
            toread: "no".into(),
        }
        .try_into()
        .unwrap()],
        ..Default::default()
    };
    let opts = RedditCleanupOpts {
        mark_nsfw: false,
        fix_titles: false,
        ..opts()
    };

    run(
        &CleanupStore::new(&pinboard, AccountView::new(pinboard.all.clone()), false),
        Some(&FakeReddit::default()),
        &opts,
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
        all: vec![PinboardBookmark {
            url: "https://www.reddit.com/r/rust/comments/b/y/".into(),
            description: "A real title".into(),
            extended: "https://example.com/article".into(),
            tags: "reddit subreddit:rust".into(),
            time: "2023-11-14T22:13:20Z".into(),
            shared: "no".into(),
            toread: "no".into(),
        }
        .try_into()
        .unwrap()],
        ..Default::default()
    };
    let opts = RedditCleanupOpts {
        mark_nsfw: false,
        fix_titles: false,
        ..opts()
    };

    run(
        &CleanupStore::new(&pinboard, AccountView::new(pinboard.all.clone()), false),
        Some(&FakeReddit::default()),
        &opts,
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
        all: vec![PinboardBookmark {
            url: "https://old.reddit.com/r/rust/comments/a/x/".into(),
            description: "A real title".into(),
            extended: "the body text".into(), // pre-blockquote notes
            tags: "reddit subreddit:rust".into(),
            time: String::new(),
            shared: "no".into(),
            toread: "no".into(),
        }
        .try_into()
        .unwrap()],
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

    run(
        &CleanupStore::new(&pinboard, AccountView::new(pinboard.all.clone()), false),
        Some(&reddit),
        &opts(),
    )
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
async fn preserves_hand_written_note_on_a_self_post() {
    // A manually-saved self-post carries a hand-written annotation that is neither the
    // authoritative rebuild nor the raw selftext. Even with /api/info present, cleanup
    // must not clobber it with the <blockquote>-wrapped selftext. The www host forces a
    // URL rewrite so an update is still emitted and we can observe the preserved note.
    let pinboard = FakePinboard {
        all: vec![PinboardBookmark {
            url: "https://www.reddit.com/r/rust/comments/a/x/".into(),
            description: "A real title".into(),
            extended: "my own thoughts -- revisit this later".into(),
            tags: "reddit subreddit:rust".into(),
            time: String::new(),
            shared: "no".into(),
            toread: "no".into(),
        }
        .try_into()
        .unwrap()],
        ..Default::default()
    };
    let reddit = FakeReddit {
        info: vec![listing_entry(
            "t3",
            json!({ "name": "t3_a", "subreddit": "rust",
                    "permalink": "/r/rust/comments/a/x/", "title": "A real title",
                    "selftext": "the authoritative body text" }),
        )],
        ..Default::default()
    };

    run(
        &CleanupStore::new(&pinboard, AccountView::new(pinboard.all.clone()), false),
        Some(&reddit),
        &opts(),
    )
    .await
    .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].extended, "my own thoughts -- revisit this later");
}

#[tokio::test]
async fn retrofits_empty_note_from_authoritative_selftext() {
    // A self-post saved with no notes gets the authoritative selftext, wrapped in a
    // <blockquote> -- an empty stored note is recognizably tool-shaped, so the rebuild
    // fills it in. The www host forces the URL rewrite that carries the write.
    let pinboard = FakePinboard {
        all: vec![PinboardBookmark {
            url: "https://www.reddit.com/r/rust/comments/a/x/".into(),
            description: "A real title".into(),
            extended: String::new(),
            tags: "reddit subreddit:rust".into(),
            time: String::new(),
            shared: "no".into(),
            toread: "no".into(),
        }
        .try_into()
        .unwrap()],
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

    run(
        &CleanupStore::new(&pinboard, AccountView::new(pinboard.all.clone()), false),
        Some(&reddit),
        &opts(),
    )
    .await
    .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(
        updated[0].extended,
        "<blockquote>the body text</blockquote>"
    );
}

#[tokio::test]
async fn leaves_comment_notes_untouched() {
    // A comment bookmark: /api/info only returns the parent *post* (post_fullname keys
    // by t3), never the comment body, so the comment's notes must be left as-is — the
    // post's selftext must not leak into the comment's notes.
    let stored = "Thread: https://old.reddit.com/r/rust/comments/a/x/\n\nmy comment";
    let pinboard = FakePinboard {
        all: vec![PinboardBookmark {
            url: "https://old.reddit.com/r/rust/comments/a/x/c/".into(),
            description: "Parent title".into(),
            extended: stored.into(),
            tags: "reddit subreddit:rust reddit-comment".into(),
            time: String::new(),
            shared: "no".into(),
            toread: "no".into(),
        }
        .try_into()
        .unwrap()],
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

    run(
        &CleanupStore::new(&pinboard, AccountView::new(pinboard.all.clone()), false),
        Some(&reddit),
        &opts(),
    )
    .await
    .unwrap();

    // Nothing changed (notes preserved, URL/title/tags already current), so no write.
    assert!(pinboard.updated.borrow().is_empty());
}

#[tokio::test]
async fn use_post_date_leaves_comment_timestamp_untouched() {
    // A comment bookmark under `use_post_date`: /api/info is keyed by the parent post
    // (post_fullname → t3), so its `created_utc` is the *thread* creation time, not the
    // comment's. That must not be applied to the comment — its stored date stays.
    let pinboard = FakePinboard {
        all: vec![PinboardBookmark {
            url: "https://old.reddit.com/r/rust/comments/a/x/c/".into(),
            description: "Parent title".into(),
            extended: "my comment note".into(),
            tags: "reddit subreddit:rust reddit-comment".into(),
            time: "2019-01-01T00:00:00Z".into(),
            shared: "no".into(),
            toread: "no".into(),
        }
        .try_into()
        .unwrap()],
        ..Default::default()
    };
    let reddit = FakeReddit {
        info: vec![listing_entry(
            "t3",
            json!({ "name": "t3_a", "subreddit": "rust",
                    "permalink": "/r/rust/comments/a/x/", "title": "Parent title",
                    "over_18": false, "created_utc": 1_700_000_000 }),
        )],
        ..Default::default()
    };
    let opts = RedditCleanupOpts {
        dates: DATED,
        mark_nsfw: false,
        fix_titles: false,
        ..opts()
    };

    run(
        &CleanupStore::new(&pinboard, AccountView::new(pinboard.all.clone()), false),
        Some(&reddit),
        &opts,
    )
    .await
    .unwrap();

    // Only the date could have changed, and the fix keeps the comment's own date, so
    // nothing is written. Without the guard the thread's created_utc would leak in.
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

    run(
        &CleanupStore::new(&pinboard, AccountView::new(pinboard.all.clone()), false),
        Some(&reddit),
        &opts(),
    )
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
    let opts = RedditCleanupOpts {
        mark_nsfw: false,
        fix_titles: false,
        ..opts()
    };
    run(
        &CleanupStore::new(&pinboard, AccountView::new(pinboard.all.clone()), true),
        Some(&reddit),
        &opts,
    )
    .await
    .unwrap();
    assert!(pinboard.updated.borrow().is_empty());
    assert!(pinboard.deleted.borrow().is_empty());
}

#[tokio::test]
async fn an_expired_cookie_surfaces_as_reauth_not_a_flattened_error() {
    // `cleanup reddit` reads `/api/info` for NSFW/title/date data, and an expired
    // reddit_session fails it. The variant has to survive to the caller, or the
    // auth-failure hook can never fire for cleanup the way it does for sync.
    let pinboard = FakePinboard {
        all: vec![bookmark(
            "https://www.reddit.com/r/rust/comments/abc/x/",
            "T",
            "reddit",
        )],
        ..Default::default()
    };
    let reddit = FakeReddit {
        info_error: Some(SourceError::ReauthRequired("cookie expired".into())),
        ..Default::default()
    };

    let err = run(
        &CleanupStore::new(&pinboard, AccountView::new(pinboard.all.clone()), false),
        Some(&reddit),
        &opts(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, SourceError::ReauthRequired(_)),
        "expected reauth, got {err:?}"
    );
}

#[tokio::test]
async fn an_api_info_block_stays_a_block() {
    // The variant must survive so `main` withholds the hook, which no cookie change
    // would make useful for a block.
    let pinboard = FakePinboard {
        all: vec![bookmark(
            "https://www.reddit.com/r/rust/comments/abc/x/",
            "T",
            "reddit",
        )],
        ..Default::default()
    };
    let reddit = FakeReddit {
        info_error: Some(SourceError::Blocked("edge block".into())),
        ..Default::default()
    };

    let err = run(
        &CleanupStore::new(&pinboard, AccountView::new(pinboard.all.clone()), false),
        Some(&reddit),
        &opts(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, SourceError::Blocked(m) if m == "edge block"),
        "expected the block unchanged, got {err:?}"
    );
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
    let opts = RedditCleanupOpts {
        mark_nsfw: false,
        fix_titles: false,
        ..opts()
    };

    // The run reports failure (non-zero exit)...
    let err = run(
        &CleanupStore::new(&pinboard, AccountView::new(pinboard.all.clone()), false),
        Some(&reddit),
        &opts,
    )
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
