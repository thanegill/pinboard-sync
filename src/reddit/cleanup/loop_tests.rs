use super::*;
use crate::test_support::{listing_entry, stored_at, strings, FakePinboard, FakeReddit, NO_DATING};
use serde_json::json;

fn opts() -> RedditCleanupOpts {
    RedditCleanupOpts {
        mark_nsfw: true,
        fix_titles: true,
        config: RedditConfig::default(),
        dates: NO_DATING,
        configured: Vec::new(),
    }
}

/// Dating on, with a cap so large the (old) test post is always "within" it.
const DATED: DateOpts = DateOpts {
    use_post_date: true,
    max_age_days: 1_000_000,
    stale_to_now: false,
};

fn bookmark(url: &str, title: &str, tags: &str) -> Bookmark {
    Bookmark {
        title: title.into(),
        tags: tags.split_whitespace().map(String::from).collect(),
        ..stored_at(url)
    }
}

#[tokio::test]
async fn normalizes_url_tags_nsfw_and_title() {
    let pinboard = FakePinboard {
        // Carries notes + metadata that cleanup must preserve across the re-add.
        all: vec![Bookmark {
            title: "Reddit - Dive into anything".into(),
            note: "original notes".into(),
            timestamp: crate::timefmt::parse_rfc3339("2023-11-14T22:13:20Z"),
            public: true,
            read_later: true,
            ..stored_at("https://www.reddit.com/r/NEWS/comments/abc/x/")
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

    run(&pinboard.cleanup_store(false), Some(&reddit), &opts())
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
        strings(&["https://www.reddit.com/r/NEWS/comments/abc/x/"])
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

    run(&pinboard.cleanup_store(false), Some(&reddit), &opts)
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert!(updated[0].tags.contains(&"adult".to_string()));
    assert!(!updated[0].tags.contains(&"nsfw".to_string()));
}

#[tokio::test]
async fn a_fixed_title_is_the_raw_api_title() {
    let pinboard = FakePinboard {
        all: vec![bookmark(
            "https://old.reddit.com/r/rust/comments/abc/x/",
            "Reddit - Dive into anything",
            "reddit subreddit:rust",
        )],
        ..Default::default()
    };
    let reddit = FakeReddit {
        info: vec![listing_entry(
            "t3",
            json!({ "name": "t3_abc", "subreddit": "rust",
                    "permalink": "/r/rust/comments/abc/x/",
                    "title": "What &amp; means  in HTML" }),
        )],
        ..Default::default()
    };

    run(&pinboard.cleanup_store(false), Some(&reddit), &opts())
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].description, "What &amp; means in HTML");
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

    run(&pinboard.cleanup_store(false), Some(&reddit), &opts)
        .await
        .unwrap();

    assert!(pinboard.updated.borrow().is_empty());
}

#[tokio::test]
async fn preserves_markup_in_stored_title() {
    // A non-placeholder stored title carrying angle brackets / tag-like text is kept
    // verbatim through cleanup, not stripped as HTML — the deliberate tradeoff of
    // treating every title as plain text. The www host still forces a rewrite so an
    // update is emitted.
    let pinboard = FakePinboard {
        all: vec![bookmark(
            "https://www.reddit.com/r/rust/comments/a/x/",
            "Vec<String> vs <b>Box</b> in Rust",
            "reddit subreddit:rust",
        )],
        ..Default::default()
    };

    run(&pinboard.cleanup_store(false), None::<&FakeReddit>, &opts())
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].description, "Vec<String> vs <b>Box</b> in Rust");
}

#[tokio::test]
async fn keeps_entity_text_in_a_stored_title() {
    // A stored title is already plain text: one that reads `&amp;` (literally, or left
    // by an older double-encoding) is kept as it is rather than decoded a step further
    // on every run.
    let pinboard = FakePinboard {
        all: vec![
            bookmark(
                "https://old.reddit.com/r/rust/comments/a/x/",
                "Use &amp; to escape",
                "reddit subreddit:rust",
            ),
            bookmark(
                "https://old.reddit.com/r/rust/comments/b/x/",
                "Tom &amp;amp; Jerry",
                "reddit subreddit:rust",
            ),
        ],
        ..Default::default()
    };

    run(&pinboard.cleanup_store(false), None::<&FakeReddit>, &opts())
        .await
        .unwrap();

    assert!(pinboard.updated.borrow().is_empty());
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

    run(&pinboard.cleanup_store(false), Some(&reddit), &opts)
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
        all: vec![Bookmark {
            title: "A real title".into(),
            note: "https://www.reddit.com/r/rust/comments/a/x/".into(),
            tags: vec!["reddit".into(), "subreddit:rust".into()],
            timestamp: crate::timefmt::parse_rfc3339("2023-11-14T22:13:20Z"),
            ..stored_at("https://old.reddit.com/r/rust/comments/a/x/")
        }],
        ..Default::default()
    };
    let opts = RedditCleanupOpts {
        mark_nsfw: false,
        fix_titles: false,
        ..opts()
    };

    run(
        &pinboard.cleanup_store(false),
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
        all: vec![Bookmark {
            title: "A real title".into(),
            note: "https://example.com/article".into(),
            tags: vec!["reddit".into(), "subreddit:rust".into()],
            timestamp: crate::timefmt::parse_rfc3339("2023-11-14T22:13:20Z"),
            ..stored_at("https://www.reddit.com/r/rust/comments/b/y/")
        }],
        ..Default::default()
    };
    let opts = RedditCleanupOpts {
        mark_nsfw: false,
        fix_titles: false,
        ..opts()
    };

    run(
        &pinboard.cleanup_store(false),
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
        all: vec![Bookmark {
            title: "A real title".into(),
            note: "the body text".into(), // pre-blockquote notes
            tags: vec!["reddit".into(), "subreddit:rust".into()],
            ..stored_at("https://old.reddit.com/r/rust/comments/a/x/")
        }],
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

    run(&pinboard.cleanup_store(false), Some(&reddit), &opts())
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
        all: vec![Bookmark {
            title: "A real title".into(),
            note: "my own thoughts -- revisit this later".into(),
            tags: vec!["reddit".into(), "subreddit:rust".into()],
            ..stored_at("https://www.reddit.com/r/rust/comments/a/x/")
        }],
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

    run(&pinboard.cleanup_store(false), Some(&reddit), &opts())
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
        all: vec![Bookmark {
            title: "A real title".into(),
            tags: vec!["reddit".into(), "subreddit:rust".into()],
            ..stored_at("https://www.reddit.com/r/rust/comments/a/x/")
        }],
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

    run(&pinboard.cleanup_store(false), Some(&reddit), &opts())
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
        all: vec![Bookmark {
            title: "Parent title".into(),
            note: stored.into(),
            tags: vec![
                "reddit".into(),
                "subreddit:rust".into(),
                "reddit-comment".into(),
            ],
            ..stored_at("https://old.reddit.com/r/rust/comments/a/x/c/")
        }],
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

    run(&pinboard.cleanup_store(false), Some(&reddit), &opts())
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
        all: vec![Bookmark {
            title: "Parent title".into(),
            note: "my comment note".into(),
            tags: vec![
                "reddit".into(),
                "subreddit:rust".into(),
                "reddit-comment".into(),
            ],
            timestamp: crate::timefmt::parse_rfc3339("2019-01-01T00:00:00Z"),
            ..stored_at("https://old.reddit.com/r/rust/comments/a/x/c/")
        }],
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

    run(&pinboard.cleanup_store(false), Some(&reddit), &opts)
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

    run(&pinboard.cleanup_store(false), Some(&reddit), &opts())
        .await
        .unwrap();
    assert!(pinboard.updated.borrow().is_empty());
    assert!(pinboard.deleted.borrow().is_empty());
}

#[tokio::test]
async fn bookmarks_moved_to_a_mirror_domain_still_dedup_for_sync() {
    let mirror = RedditConfig {
        domain: "redlib.example.org".into(),
        ..RedditConfig::default()
    };
    let seed = vec![bookmark(
        "https://www.reddit.com/r/rust/comments/a/x/",
        "T",
        "reddit subreddit:rust",
    )];
    let pinboard = FakePinboard {
        all: seed.clone(),
        ..Default::default()
    };
    let opts = RedditCleanupOpts {
        config: mirror.clone(),
        ..opts()
    };
    run(
        &pinboard.cleanup_store(false),
        Some(&FakeReddit::default()),
        &opts,
    )
    .await
    .unwrap();
    let account = crate::test_support::replay(&seed, &pinboard.ops.borrow());
    assert_eq!(
        account[0].url.as_str(),
        "https://redlib.example.org/r/rust/comments/a/x/"
    );

    let mut client = crate::reddit::RedditClient::with_base_url(None, None, "http://unused".into());
    client.config = mirror;
    let saved = listing_entry(
        "t3",
        json!({ "name": "t3_a", "subreddit": "rust",
                "permalink": "/r/rust/comments/a/x/", "title": "T" }),
    );
    let drafts = client.shape(vec![saved]);
    assert!(
        crate::sync::filter_new(&client, drafts, &account).is_empty(),
        "sync must not re-add the save cleanup moved onto the mirror"
    );
}

/// A `[[reddit]]` account on `domain` whose base tags are `reddit` and `tag`.
fn account_on(domain: &str, tag: &str) -> RedditConfig {
    RedditConfig {
        domain: domain.into(),
        tags: vec!["reddit".into(), tag.into()],
        ..RedditConfig::default()
    }
}

fn two_mirrors() -> Vec<RedditConfig> {
    vec![
        account_on("first.example", "first"),
        account_on("second.example", "second"),
    ]
}

/// Cleanup run as the one of `accounts` on `domain`, with all of them configured: the
/// account it leaves `seed` as, and how many writes that took.
async fn cleanup_as(
    accounts: &[RedditConfig],
    domain: &str,
    seed: &[Bookmark],
) -> (Vec<Bookmark>, usize) {
    let running = accounts
        .iter()
        .find(|account| account.domain == domain)
        .expect("an account on the domain");
    cleanup_running(accounts, running, &FakeReddit::default(), seed).await
}

/// [`cleanup_as`] for the `running` account, with `/api/info` answered by `reddit`.
async fn cleanup_running(
    accounts: &[RedditConfig],
    running: &RedditConfig,
    reddit: &FakeReddit,
    seed: &[Bookmark],
) -> (Vec<Bookmark>, usize) {
    let pinboard = FakePinboard {
        all: seed.to_vec(),
        ..Default::default()
    };
    let opts = RedditCleanupOpts {
        config: running.clone(),
        configured: accounts.to_vec(),
        ..opts()
    };
    run(&pinboard.cleanup_store(false), Some(reddit), &opts)
        .await
        .unwrap();
    let ops = pinboard.ops.borrow();
    (crate::test_support::replay(seed, &ops), ops.len())
}

#[tokio::test]
async fn a_post_stored_on_two_mirrors_collapses_onto_the_first_configured_one() {
    let accounts = two_mirrors();
    let seed = vec![
        bookmark(
            "https://second.example/r/rust/comments/a/x/",
            "T",
            "reddit second subreddit:rust",
        ),
        bookmark(
            "https://first.example/r/rust/comments/a/x/",
            "T",
            "reddit first subreddit:rust",
        ),
    ];
    for domain in ["first.example", "second.example"] {
        let (account, _) = cleanup_as(&accounts, domain, &seed).await;
        let urls: Vec<&str> = account
            .iter()
            .map(|bookmark| bookmark.url.as_str())
            .collect();
        assert_eq!(
            urls,
            ["https://first.example/r/rust/comments/a/x/"],
            "run as {domain}"
        );
        for again in ["first.example", "second.example"] {
            assert_eq!(
                cleanup_as(&accounts, again, &account).await.1,
                0,
                "{domain} then {again}"
            );
        }
    }
}

#[tokio::test]
async fn cleanup_shapes_a_bookmark_on_another_accounts_mirror_by_its_tags_and_leaves_it_there() {
    let accounts = two_mirrors();
    let seed = vec![
        bookmark("https://second.example/r/Rust/comments/a/x/", "T", ""),
        bookmark("https://www.reddit.com/r/rust/comments/b/y/", "U", ""),
    ];
    let (account, writes) = cleanup_as(&accounts, "first.example", &seed).await;
    assert!(writes > 0);
    let shaped: Vec<(&str, Vec<&str>)> = account
        .iter()
        .map(|bookmark| {
            let tags = bookmark.tags.iter().map(String::as_str).collect();
            (bookmark.url.as_str(), tags)
        })
        .collect();
    assert_eq!(
        shaped,
        [
            (
                "https://second.example/r/Rust/comments/a/x/",
                vec!["reddit", "second", "subreddit:rust"]
            ),
            (
                "https://first.example/r/rust/comments/b/y/",
                vec!["first", "reddit", "subreddit:rust"]
            ),
        ]
    );
    for again in ["first.example", "second.example"] {
        assert_eq!(
            cleanup_as(&accounts, again, &account).await.1,
            0,
            "then as {again}"
        );
    }
}

/// Moving a bookmark onto the running account's domain would carry it back and forth
/// between two accounts on reddit.com hosts, one move per run.
#[tokio::test]
async fn a_bookmark_on_one_accounts_reddit_host_stays_whichever_account_runs() {
    let accounts = vec![
        account_on("old.reddit.com", "old"),
        account_on("www.reddit.com", "www"),
    ];
    let seed = vec![bookmark(
        "https://www.reddit.com/r/rust/comments/a/x/",
        "T",
        "reddit subreddit:rust www",
    )];
    for domain in ["old.reddit.com", "www.reddit.com", "old.reddit.com"] {
        assert_eq!(
            cleanup_as(&accounts, domain, &seed).await.1,
            0,
            "as {domain}"
        );
    }
}

/// Cleanup run as each of `accounts` in turn, for several rounds: the account it leaves
/// `seed` as after the first round, asserting no later run writes anything.
async fn settled_by_turns(
    accounts: &[RedditConfig],
    reddit: &FakeReddit,
    seed: &[Bookmark],
) -> Vec<Bookmark> {
    let mut account = seed.to_vec();
    for running in accounts {
        account = cleanup_running(accounts, running, reddit, &account).await.0;
    }
    for round in 1..4 {
        for (index, running) in accounts.iter().enumerate() {
            let (after, writes) = cleanup_running(accounts, running, reddit, &account).await;
            assert_eq!(writes, 0, "round {round}, account {index}: {after:?}");
            account = after;
        }
    }
    account
}

#[tokio::test]
async fn a_base_tag_named_like_the_subreddit_settles_between_accounts_on_one_domain() {
    let accounts = vec![
        RedditConfig {
            tags: vec!["reddit".into(), "rust".into()],
            ..RedditConfig::default()
        },
        RedditConfig::default(),
    ];
    let seed = vec![bookmark(
        "https://old.reddit.com/r/rust/comments/a/x/",
        "T",
        "reddit subreddit:rust",
    )];
    let account = settled_by_turns(&accounts, &FakeReddit::default(), &seed).await;
    assert_eq!(account[0].tags, ["reddit", "rust", "subreddit:rust"]);
}

#[tokio::test]
async fn a_bare_subreddit_tag_no_account_uses_as_a_base_tag_is_still_stripped() {
    let accounts = vec![RedditConfig::default()];
    let seed = vec![bookmark(
        "https://old.reddit.com/r/rust/comments/a/x/",
        "T",
        "reddit rust subreddit:rust",
    )];
    let account = settled_by_turns(&accounts, &FakeReddit::default(), &seed).await;
    assert_eq!(account[0].tags, ["reddit", "subreddit:rust"]);
}

#[tokio::test]
async fn an_nsfw_marker_named_like_the_subreddit_settles_between_accounts_on_one_domain() {
    let accounts = vec![
        RedditConfig::default(),
        RedditConfig {
            nsfw: String::new(),
            ..RedditConfig::default()
        },
    ];
    let reddit = FakeReddit {
        info: vec![listing_entry(
            "t3",
            json!({ "name": "t3_a", "subreddit": "nsfw",
                    "permalink": "/r/nsfw/comments/a/x/", "title": "T",
                    "over_18": true }),
        )],
        ..Default::default()
    };
    let seed = vec![bookmark(
        "https://old.reddit.com/r/nsfw/comments/a/x/",
        "T",
        "reddit subreddit:nsfw",
    )];
    let account = settled_by_turns(&accounts, &reddit, &seed).await;
    assert_eq!(account[0].tags, ["nsfw", "reddit", "subreddit:nsfw"]);
}

#[tokio::test]
async fn a_base_tag_named_like_the_bare_prefix_settles_between_accounts_on_one_domain() {
    let accounts = vec![
        RedditConfig {
            tags: vec!["reddit".into(), "subreddit".into()],
            ..RedditConfig::default()
        },
        RedditConfig::default(),
    ];
    let seed = vec![bookmark(
        "https://old.reddit.com/r/rust/comments/a/x/",
        "T",
        "reddit subreddit:rust",
    )];
    let account = settled_by_turns(&accounts, &FakeReddit::default(), &seed).await;
    assert_eq!(account[0].tags, ["reddit", "subreddit", "subreddit:rust"]);
}

#[tokio::test]
async fn a_comment_tag_named_like_the_subreddit_settles_between_accounts_on_one_domain() {
    let accounts = vec![
        RedditConfig {
            comment: "rust".into(),
            ..RedditConfig::default()
        },
        RedditConfig::default(),
    ];
    let seed = vec![
        bookmark(
            "https://old.reddit.com/r/rust/comments/a/x/c/",
            "T",
            "reddit reddit-comment subreddit:rust",
        ),
        bookmark(
            "https://old.reddit.com/r/rust/comments/b/y/",
            "U",
            "reddit rust subreddit:rust",
        ),
    ];
    let account = settled_by_turns(&accounts, &FakeReddit::default(), &seed).await;
    let tags: Vec<&[String]> = account
        .iter()
        .map(|bookmark| bookmark.tags.as_slice())
        .collect();
    assert_eq!(
        tags,
        [
            &["reddit", "reddit-comment", "rust", "subreddit:rust"].map(String::from)[..],
            &["reddit", "subreddit:rust"].map(String::from)[..],
        ]
    );
}

#[tokio::test]
async fn a_base_tag_of_an_account_on_another_domain_is_still_stripped() {
    let accounts = vec![
        account_on("first.example", "rust"),
        RedditConfig {
            domain: "second.example".into(),
            ..RedditConfig::default()
        },
    ];
    let seed = vec![bookmark(
        "https://second.example/r/rust/comments/a/x/",
        "T",
        "reddit rust subreddit:rust",
    )];
    let account = settled_by_turns(&accounts, &FakeReddit::default(), &seed).await;
    assert_eq!(account[0].tags, ["reddit", "subreddit:rust"]);
}

#[tokio::test]
async fn a_self_link_to_the_post_on_another_mirror_is_dropped() {
    let seed = vec![Bookmark {
        note: "https://first.example/r/rust/comments/a/x/".into(),
        ..bookmark(
            "https://second.example/r/rust/comments/a/x/",
            "T",
            "reddit second subreddit:rust",
        )
    }];
    let (account, _) = cleanup_as(&two_mirrors(), "first.example", &seed).await;
    assert_eq!(account[0].note, "");
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
    run(&pinboard.cleanup_store(true), Some(&reddit), &opts)
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

    let err = run(&pinboard.cleanup_store(false), Some(&reddit), &opts())
        .await
        .unwrap_err();
    assert!(
        matches!(err, SourceError::ReauthRequired(_)),
        "expected reauth, got {err:?}"
    );
}

#[tokio::test]
async fn an_api_info_block_stays_a_block() {
    // The variant must survive so the `cleanup` command withholds the hook, which no
    // cookie change would make useful for a block.
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

    let err = run(&pinboard.cleanup_store(false), Some(&reddit), &opts())
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
    let err = run(&pinboard.cleanup_store(false), Some(&reddit), &opts)
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

#[tokio::test]
async fn plans_the_tags_sync_writes_for_each_tag_config() {
    let with_tags = |tags: &[&str]| RedditConfig {
        tags: tags.iter().map(|t| t.to_string()).collect(),
        ..RedditConfig::default()
    };
    let configs = [
        RedditConfig::default(),
        with_tags(&[]),
        with_tags(&[""]),
        with_tags(&["", "reddit"]),
        with_tags(&["reddit", "saved"]),
        with_tags(&["reddit", "rust"]),
        with_tags(&["subreddit"]),
        RedditConfig {
            subreddit_prefix: String::new(),
            ..RedditConfig::default()
        },
        RedditConfig {
            comment: String::new(),
            author_prefix: String::new(),
            flair_prefix: String::new(),
            media_types: vec![],
            ..RedditConfig::default()
        },
    ];
    let post = listing_entry(
        "t3",
        json!({ "name": "t3_a", "subreddit": "rust", "permalink": "/r/rust/comments/a/x/",
                "title": "T", "author": "alice", "link_flair_text": "Help Wanted",
                "post_hint": "image" }),
    );
    let comment = listing_entry(
        "t1",
        json!({ "name": "t1_c", "subreddit": "rust", "permalink": "/r/rust/comments/a/x/c/",
                "link_title": "T", "body": "hi", "author": "bob",
                "link_flair_text": "Help Wanted" }),
    );
    for config in configs {
        let opts = RedditCleanupOpts {
            config: config.clone(),
            ..opts()
        };
        // A comment's lookup returns its parent post too, as the real batch does.
        for saved in [post.clone(), comment.clone()] {
            let synced = saved
                .clone()
                .into_saved_item(&config.domain)
                .unwrap()
                .into_draft(&config)
                .unwrap()
                .bookmark;
            let reddit = FakeReddit {
                info: vec![post.clone(), saved],
                ..Default::default()
            };
            let pass = RedditCleanupPass {
                info: fetch_post_info(Some(&reddit), &opts, std::slice::from_ref(&synced))
                    .await
                    .unwrap(),
                opts: &opts,
                collapse_hosts: HashMap::new(),
            };
            let mut want = synced.tags.clone();
            want.sort();

            for stored_tags in [vec![], synced.tags.clone()] {
                let stored = Bookmark {
                    tags: stored_tags.clone(),
                    ..synced.clone()
                };
                let Plan::Bookmark(planned) = pass.plan(&stored).await.unwrap() else {
                    panic!("expected a planned bookmark for {config:?}");
                };
                assert_eq!(
                    planned.tags, want,
                    "{config:?}, {}, stored {stored_tags:?}",
                    synced.url
                );
            }
        }
    }
}

#[tokio::test]
async fn backfills_a_detail_tag_only_when_its_family_is_absent() {
    let pinboard = FakePinboard {
        all: vec![
            bookmark(
                "https://old.reddit.com/r/rust/comments/a/x/",
                "T",
                "reddit subreddit:rust reddit-flair:old",
            ),
            bookmark(
                "https://old.reddit.com/r/rust/comments/a/x/c/",
                "T",
                "reddit subreddit:rust",
            ),
        ],
        ..Default::default()
    };
    let reddit = FakeReddit {
        info: vec![
            listing_entry(
                "t3",
                json!({ "name": "t3_a", "subreddit": "rust",
                        "permalink": "/r/rust/comments/a/x/", "title": "T",
                        "author": "alice", "link_flair_text": "News" }),
            ),
            listing_entry(
                "t1",
                json!({ "name": "t1_c", "subreddit": "rust",
                        "permalink": "/r/rust/comments/a/x/c/", "author": "bob" }),
            ),
        ],
        ..Default::default()
    };

    run(&pinboard.cleanup_store(false), Some(&reddit), &opts())
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 2);
    // The stored flair stays; only the missing author family is filled in.
    assert_eq!(
        updated[0].tags,
        vec![
            "author:reddit:alice",
            "reddit",
            "reddit-flair:old",
            "subreddit:rust"
        ]
    );
    // The comment gets its own author, never the parent post's flair.
    assert_eq!(
        updated[1].tags,
        vec![
            "author:reddit:bob",
            "reddit",
            "reddit-comment",
            "subreddit:rust"
        ]
    );
}

#[tokio::test]
async fn backfills_only_the_comment_tag_without_a_lookup() {
    let seed = vec![
        bookmark(
            "https://old.reddit.com/r/rust/comments/a/x/",
            "T",
            "reddit subreddit:rust",
        ),
        bookmark(
            "https://old.reddit.com/r/rust/comments/a/x/c/",
            "T",
            "reddit subreddit:rust",
        ),
    ];
    let opts = RedditCleanupOpts {
        mark_nsfw: false,
        fix_titles: false,
        ..opts()
    };

    let (first_writes, stored) = run_and_store(seed, None, &opts).await;
    assert_eq!(first_writes, 1);
    assert_eq!(
        stored[1].tags,
        vec!["reddit", "reddit-comment", "subreddit:rust"]
    );
    let (second_writes, _) = run_and_store(stored, None, &opts).await;
    assert_eq!(second_writes, 0);
}

#[tokio::test]
async fn a_comment_missing_from_the_lookup_adds_nothing_and_does_not_fail() {
    let pinboard = FakePinboard {
        all: vec![bookmark(
            "https://old.reddit.com/r/rust/comments/a/x/c/",
            "T",
            "reddit subreddit:rust reddit-comment",
        )],
        ..Default::default()
    };
    let reddit = FakeReddit {
        info: vec![listing_entry(
            "t3",
            json!({ "name": "t3_a", "subreddit": "rust",
                    "permalink": "/r/rust/comments/a/x/", "title": "T",
                    "author": "alice", "link_flair_text": "News" }),
        )],
        ..Default::default()
    };

    run(&pinboard.cleanup_store(false), Some(&reddit), &opts())
        .await
        .unwrap();

    assert!(pinboard.updated.borrow().is_empty());
}

/// One cleanup run over `seed`, returning how many bookmarks it wrote and the account as
/// Pinboard then stores it. Tags round-trip as Pinboard keeps them (space-joined on
/// write, whitespace-split on read), which is what drops an empty tag.
async fn run_and_store(
    seed: Vec<Bookmark>,
    reddit: Option<&FakeReddit>,
    opts: &RedditCleanupOpts,
) -> (usize, Vec<Bookmark>) {
    let pinboard = FakePinboard {
        all: seed.clone(),
        ..Default::default()
    };
    run(&pinboard.cleanup_store(false), reddit, opts)
        .await
        .unwrap();
    let mut stored = crate::test_support::replay(&seed, &pinboard.ops.borrow());
    for bookmark in &mut stored {
        bookmark.tags = bookmark
            .tags
            .join(" ")
            .split_whitespace()
            .map(String::from)
            .collect();
    }
    (pinboard.updated.into_inner().len(), stored)
}

#[tokio::test]
async fn settles_after_one_run_when_base_tags_are_disabled() {
    for tags in [vec![], vec![String::new()]] {
        let opts = RedditCleanupOpts {
            config: RedditConfig {
                tags: tags.clone(),
                ..RedditConfig::default()
            },
            ..opts()
        };
        let seed = vec![
            bookmark("https://www.reddit.com/r/rust/comments/a/x/", "T", ""),
            bookmark(
                "https://old.reddit.com/r/rust/comments/b/y/",
                "T",
                "subreddit:rust",
            ),
        ];

        let (first_writes, stored) = run_and_store(seed, None, &opts).await;
        assert_eq!(first_writes, 1, "tags = {tags:?}");
        let (second_writes, _) = run_and_store(stored, None, &opts).await;
        assert_eq!(second_writes, 0, "tags = {tags:?}");
    }
}

#[tokio::test]
async fn settles_after_one_run_when_backfilling_from_a_lookup() {
    let seed = vec![
        bookmark("https://old.reddit.com/r/rust/comments/a/x/", "T", ""),
        bookmark("https://old.reddit.com/r/rust/comments/a/x/c/", "T", ""),
    ];
    let reddit = FakeReddit {
        info: vec![
            listing_entry(
                "t3",
                json!({ "name": "t3_a", "subreddit": "rust",
                        "permalink": "/r/rust/comments/a/x/", "title": "T",
                        "author": "alice", "link_flair_text": "News",
                        "post_hint": "image" }),
            ),
            listing_entry(
                "t1",
                json!({ "name": "t1_c", "subreddit": "rust",
                        "permalink": "/r/rust/comments/a/x/c/", "author": "bob" }),
            ),
        ],
        ..Default::default()
    };

    let (first_writes, stored) = run_and_store(seed, Some(&reddit), &opts()).await;
    assert_eq!(first_writes, 2);
    let (second_writes, _) = run_and_store(stored, Some(&reddit), &opts()).await;
    assert_eq!(second_writes, 0);
}
