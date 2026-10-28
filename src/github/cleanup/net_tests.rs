use super::*;
use crate::test_support::{stored_at, strings, FakePinboard, NO_DATING};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// An expired token during the pass has to reach the caller as `ReauthRequired`, or the
/// `cleanup` command cannot tell it apart from any other failure and the auth-failure
/// hook never fires.
#[tokio::test]
async fn cleanup_surfaces_an_expired_token_as_reauth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"message":"Bad creds"}"#))
        .mount(&server)
        .await;

    let pinboard = FakePinboard {
        all: vec![Bookmark {
            title: "o/r".into(),
            note: "notes".into(),
            tags: vec!["github-star".into()],
            timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
            ..stored_at("https://github.com/o/r")
        }],
        ..Default::default()
    };
    let client = github_client(&server);

    let err = run(&pinboard.cleanup_store(false), &client, NO_DATING, &[])
        .await
        .unwrap_err();

    assert!(
        matches!(err, SourceError::ReauthRequired(_)),
        "expected reauth, got {err:?}"
    );
}

/// End to end: a rate limit part-way through stops the pass rather than spending a
/// doomed request on every remaining bookmark, while keeping the work already done.
#[tokio::test]
async fn cleanup_stops_at_a_rate_limit_and_keeps_what_it_did() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/old/name"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "full_name": "new/name",
            "html_url": "https://github.com/new/name",
            "description": "Still here",
            "language": "Rust"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/limited/repo"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", "1788102000")
                .set_body_string(r#"{"message":"API rate limit exceeded"}"#),
        )
        .mount(&server)
        .await;
    // The point of the test: this one must never be requested. An unmounted path
    // would not prove it — wiremock answers those with a 404, which `repo()` reads as
    // a deleted repo and skips silently — so assert zero calls explicitly. The
    // expectation is checked when the server drops.
    Mock::given(method("GET"))
        .and(path("/repos/never/looked"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let stored = |url: &str, title: &str| Bookmark {
        title: title.into(),
        note: "notes".into(),
        tags: vec!["github-star".into()],
        timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
        ..stored_at(url)
    };
    let pinboard = FakePinboard {
        all: vec![
            stored("https://github.com/old/name", "old/name"),
            stored("https://github.com/limited/repo", "limited/repo"),
            stored("https://github.com/never/looked", "never/looked"),
        ],
        ..Default::default()
    };
    let client = github_client(&server);

    let err = run(&pinboard.cleanup_store(false), &client, NO_DATING, &[])
        .await
        .unwrap_err();

    assert!(err.to_string().contains("rate limit"), "{err}");
    // The repo read before the limit is still rewritten — the work isn't thrown away.
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://github.com/new/name");
}

#[tokio::test]
async fn cleanup_survives_a_permanently_blocked_repo() {
    // A DMCA-blocked repo answers 451 forever, so the run must still succeed on the
    // strength of the repos it *could* read.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/blocked/repo"))
        .respond_with(ResponseTemplate::new(451).set_body_string(
            r#"{"message":"Repository access blocked","block":{"reason":"dmca"}}"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/old/name"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "full_name": "new/name",
            "html_url": "https://github.com/new/name",
            "description": "Still here",
            "language": "Rust"
        })))
        .mount(&server)
        .await;

    let stored = |url: &str, title: &str| Bookmark {
        title: title.into(),
        note: "notes".into(),
        tags: vec!["github-star".into()],
        timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
        ..stored_at(url)
    };
    let pinboard = FakePinboard {
        all: vec![
            stored("https://github.com/blocked/repo", "blocked/repo"),
            stored("https://github.com/old/name", "old/name"),
        ],
        ..Default::default()
    };
    let client = github_client(&server);

    run(&pinboard.cleanup_store(false), &client, NO_DATING, &[])
        .await
        .expect("one dead repo must not fail the whole run");

    // The readable repo was still cleaned up.
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://github.com/new/name");
}

#[tokio::test]
async fn cleanup_refresh_rewrites_renamed_repo_and_language() {
    // The repo was renamed old/name -> new/name, and its language changed.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/old/name"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "full_name": "new/name",
            "html_url": "https://github.com/new/name",
            "description": "A renamed thing",
            "language": "Rust"
        })))
        .mount(&server)
        .await;

    let pinboard = FakePinboard {
        all: vec![Bookmark {
            title: "old/name".into(),
            note: "notes".into(),
            tags: vec!["github-star".into(), "lang:python".into(), "mine".into()],
            timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
            ..stored_at("https://github.com/old/name")
        }],
        ..Default::default()
    };
    let client = github_client(&server);
    run(&pinboard.cleanup_store(false), &client, NO_DATING, &[])
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://github.com/new/name");
    assert_eq!(updated[0].description, "new/name");
    // Hand-written notes are kept: only a note in the tool's shape is rebuilt.
    assert_eq!(updated[0].extended, "notes");
    // Language tag refreshed (python → rust), base + user tags kept.
    assert!(updated[0].tags.contains(&"lang:rust".to_string()));
    assert!(!updated[0].tags.contains(&"lang:python".to_string()));
    assert!(updated[0].tags.contains(&"github-star".to_string()));
    assert!(updated[0].tags.contains(&"mine".to_string()));
    // Old URL deleted after the rewrite.
    assert_eq!(
        pinboard.deleted.borrow().as_slice(),
        &strings(&["https://github.com/old/name"])
    );
}

#[tokio::test]
async fn cleanup_leaves_deep_links_and_gists_untouched() {
    fn stored(url: &str) -> Bookmark {
        Bookmark {
            title: "title".into(),
            note: "notes".into(),
            tags: vec!["github-star".into()],
            timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
            ..stored_at(url)
        }
    }

    // No mocks: a lookup of any of these would 404 (and any rewrite would show up
    // as an update), so an empty updated/deleted set proves they were skipped.
    let server = MockServer::start().await;
    let pinboard = FakePinboard {
        all: vec![
            stored("https://github.com/rust-lang/rust/issues/12345"),
            stored("https://github.com/rust-lang/rust/tree/master/library"),
            stored("https://gist.github.com/user/abcd1234"),
        ],
        ..Default::default()
    };
    let client = github_client(&server);
    run(&pinboard.cleanup_store(false), &client, NO_DATING, &[])
        .await
        .unwrap();

    assert!(pinboard.updated.borrow().is_empty());
    assert!(pinboard.deleted.borrow().is_empty());
}

#[tokio::test]
async fn cleanup_canonicalizes_non_canonical_repo_root() {
    // A repo-root URL with a .git suffix and trailing slash canonicalizes to the
    // bare owner/repo root.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/Owner/Repo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "full_name": "Owner/Repo",
            "html_url": "https://github.com/Owner/Repo"
        })))
        .mount(&server)
        .await;

    let pinboard = FakePinboard {
        all: vec![Bookmark {
            title: "Owner/Repo".into(),
            note: "https://github.com/Owner/Repo".into(),
            tags: vec!["github-star".into()],
            timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
            ..stored_at("https://github.com/Owner/Repo.git/")
        }],
        ..Default::default()
    };
    let client = github_client(&server);
    run(&pinboard.cleanup_store(false), &client, NO_DATING, &[])
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://github.com/Owner/Repo");
    // The non-canonical original was deleted after the rewrite.
    assert_eq!(
        pinboard.deleted.borrow().as_slice(),
        &strings(&["https://github.com/Owner/Repo.git/"])
    );
}

#[tokio::test]
async fn cleanup_folds_www_host_onto_github_com() {
    // A www.github.com repo root is the same site as github.com: it is looked up
    // (owner/repo) and rewritten to the bare host, the old www URL deleted.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/Owner/Repo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "full_name": "Owner/Repo",
            "html_url": "https://github.com/Owner/Repo"
        })))
        .mount(&server)
        .await;

    let pinboard = FakePinboard {
        all: vec![Bookmark {
            title: "Owner/Repo".into(),
            note: "https://github.com/Owner/Repo".into(),
            tags: vec!["github-star".into()],
            timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
            ..stored_at("https://www.github.com/Owner/Repo")
        }],
        ..Default::default()
    };
    let client = github_client(&server);
    run(&pinboard.cleanup_store(false), &client, NO_DATING, &[])
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://github.com/Owner/Repo");
    assert_eq!(
        pinboard.deleted.borrow().as_slice(),
        &strings(&["https://www.github.com/Owner/Repo"])
    );
}

#[tokio::test]
async fn cleanup_rewrites_notes_only_to_retrofit_the_blockquote() {
    // URL, title, and tags are already current; only the notes are stale (an old
    // unwrapped description). The extended_changed guard must still trigger a write.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "full_name": "o/r",
            "html_url": "https://github.com/o/r",
            "description": "A thing"
        })))
        .mount(&server)
        .await;

    let pinboard = FakePinboard {
        all: vec![Bookmark {
            title: "o/r".into(),
            note: "A thing".into(), // pre-blockquote notes
            tags: vec!["github-star".into()],
            timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
            ..stored_at("https://github.com/o/r")
        }],
        ..Default::default()
    };
    let client = github_client(&server);
    run(&pinboard.cleanup_store(false), &client, NO_DATING, &[])
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].extended, "<blockquote>A thing</blockquote>");
    // Nothing else changed, so no delete (URL is unchanged).
    assert!(pinboard.deleted.borrow().is_empty());
}

/// Mount `/repos/{owner}/{name}` answering with `repo`.
async fn mount_repo(server: &MockServer, owner_and_name: &str, repo: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/{owner_and_name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(repo))
        .mount(server)
        .await;
}

#[tokio::test]
async fn cleanup_leaves_an_hn_story_of_a_repo_as_hn_wrote_it() {
    // `sync hackernews` bookmarks a Show HN at the repo it links. That bookmark is HN's:
    // its title, `HN Link:` notes and tags must survive `cleanup github`.
    let server = MockServer::start().await;
    mount_repo(
        &server,
        "o/r",
        json!({ "full_name": "o/r", "html_url": "https://github.com/o/r",
                "description": "A tool", "language": "Rust" }),
    )
    .await;
    let pinboard = FakePinboard {
        all: vec![Bookmark {
            title: "Show HN: My tool".into(),
            note: "HN Link: https://news.ycombinator.com/item?id=42\n\nmy own notes".into(),
            tags: vec![
                "hackernews".into(),
                "author:hackernews:alice".into(),
                "hackernews:show-hn".into(),
            ],
            timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
            ..stored_at("https://github.com/o/r")
        }],
        ..Default::default()
    };
    let client = github_client(&server);

    run(&pinboard.cleanup_store(false), &client, NO_DATING, &[])
        .await
        .unwrap();

    assert!(pinboard.updated.borrow().is_empty());
    assert!(pinboard.deleted.borrow().is_empty());
}

#[tokio::test]
async fn cleanup_refreshes_the_stars_of_every_configured_account_by_their_own_tags() {
    let server = MockServer::start().await;
    for name in ["o/work", "o/home"] {
        mount_repo(
            &server,
            name,
            json!({ "full_name": name, "html_url": format!("https://github.com/{name}"),
                    "language": "Rust" }),
        )
        .await;
    }
    let pinboard = FakePinboard {
        all: vec![
            Bookmark {
                tags: vec!["work-star".into(), "language:go".into()],
                ..stored_at("https://github.com/o/work")
            },
            Bookmark {
                tags: vec!["github-star".into()],
                ..stored_at("https://github.com/o/home")
            },
        ],
        ..Default::default()
    };
    let work = GitHubConfig {
        tags: vec!["work-star".into()],
        lang_prefix: "language:".into(),
        author_prefix: "owner:".into(),
    };
    let client = github_client(&server);

    run(
        &pinboard.cleanup_store(false),
        &client,
        NO_DATING,
        &[GitHubConfig::default(), work],
    )
    .await
    .unwrap();

    let updated = pinboard.updated.borrow();
    let written = |url: &str| {
        let call = updated.iter().find(|call| call.url == url).expect(url);
        (call.description.as_str(), call.tags.clone())
    };
    assert_eq!(
        written("https://github.com/o/work"),
        (
            "o/work",
            strings(&["work-star", "language:rust", "owner:o"])
        )
    );
    assert_eq!(
        written("https://github.com/o/home"),
        (
            "o/home",
            strings(&["github-star", "lang:rust", "author:github:o"])
        )
    );
}

#[tokio::test]
async fn cleanup_gives_an_unsynced_bookmark_a_note_but_not_a_title_or_tags() {
    // Saved by hand with no note: the empty note is the tool's to fill, but without the
    // base tag the bookmark is not a synced star, so its title and tags stay.
    let server = MockServer::start().await;
    mount_repo(
        &server,
        "o/r",
        json!({ "full_name": "o/r", "html_url": "https://github.com/o/r",
                "description": "A tool", "language": "Rust" }),
    )
    .await;
    let pinboard = FakePinboard {
        all: vec![Bookmark {
            title: "My bookmark".into(),
            tags: vec!["tools".into()],
            ..stored_at("https://github.com/o/r")
        }],
        ..Default::default()
    };
    let client = github_client(&server);

    run(&pinboard.cleanup_store(false), &client, NO_DATING, &[])
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].description, "My bookmark");
    assert_eq!(updated[0].extended, "<blockquote>A tool</blockquote>");
    assert_eq!(updated[0].tags, strings(&["tools"]));
}

#[tokio::test]
async fn cleanup_settles_after_one_run() {
    let server = MockServer::start().await;
    let renamed = json!({ "full_name": "new/name", "html_url": "https://github.com/new/name",
                          "description": "Renamed", "homepage": "https://home.example",
                          "language": "Rust" });
    mount_repo(&server, "old/name", renamed.clone()).await;
    mount_repo(&server, "new/name", renamed).await;
    mount_repo(
        &server,
        "o/linked",
        json!({ "full_name": "o/linked", "html_url": "https://github.com/o/linked",
                "description": "Linked", "language": "Go" }),
    )
    .await;
    mount_repo(
        &server,
        "o/plain",
        json!({ "full_name": "o/plain", "html_url": "https://github.com/o/plain" }),
    )
    .await;

    let seed = vec![
        Bookmark {
            title: "old/name".into(),
            note: "<blockquote>Old description</blockquote>".into(),
            tags: vec!["github-star".into(), "lang:python".into()],
            ..stored_at("https://github.com/old/name")
        },
        Bookmark {
            title: "Show HN: Linked".into(),
            note: "HN Link: https://news.ycombinator.com/item?id=7".into(),
            tags: vec!["hackernews".into()],
            ..stored_at("https://www.github.com/o/linked.git")
        },
        Bookmark {
            title: "o/plain".into(),
            tags: vec!["github-star".into()],
            ..stored_at("https://github.com/o/plain")
        },
    ];
    let client = github_client(&server);

    let (first_writes, stored) = run_and_store(seed, &client, NO_DATING).await;
    assert_eq!(first_writes, 3);
    let linked = stored
        .iter()
        .find(|bookmark| bookmark.url.as_str() == "https://github.com/o/linked")
        .expect("the HN story is canonicalized");
    assert_eq!(linked.title, "Show HN: Linked");
    assert_eq!(
        linked.note,
        "HN Link: https://news.ycombinator.com/item?id=7"
    );
    assert_eq!(linked.tags, ["hackernews"]);
    let (second_writes, _) = run_and_store(stored, &client, NO_DATING).await;
    assert_eq!(second_writes, 0);
}

#[tokio::test]
async fn cleanup_settles_after_merging_a_renamed_star_into_an_hn_story() {
    // The star is stored under the repo's old name and the HN story under its new one, so
    // the rename makes them collide and the driver field-merges them. The merged record
    // then carries the base tag, which must not make the next run take over the HN title
    // or, under `use_post_date`, its date.
    let server = MockServer::start().await;
    let renamed = json!({ "full_name": "new/name", "html_url": "https://github.com/new/name",
                          "description": "Renamed", "language": "Rust" });
    mount_repo(&server, "old/name", renamed.clone()).await;
    mount_repo(&server, "new/name", renamed).await;
    Mock::given(method("GET"))
        .and(path("/user/starred"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "starred_at": "2023-01-02T03:04:05Z",
              "repo": { "full_name": "new/name", "html_url": "https://github.com/new/name" } }
        ])))
        .mount(&server)
        .await;
    let story_date = crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z");
    let seed = |story_url: &str| {
        vec![
            Bookmark {
                title: "old/name".into(),
                note: "<blockquote>Old</blockquote>".into(),
                tags: vec!["github-star".into()],
                timestamp: crate::timefmt::parse_rfc3339("2023-01-02T03:04:05Z"),
                ..stored_at("https://github.com/old/name")
            },
            Bookmark {
                title: "Show HN: New".into(),
                note: "HN Link: https://news.ycombinator.com/item?id=9".into(),
                tags: vec!["hackernews".into()],
                timestamp: story_date,
                ..stored_at(story_url)
            },
        ]
    };
    let client = github_client(&server);
    let post_dating = crate::timefmt::DateOpts {
        use_post_date: true,
        max_age_days: 365 * 100,
        stale_to_now: false,
    };

    // The story either already sits at the new name, so the merge keeps its record, or
    // moves too (from a non-canonical old URL), so neither record is kept as is.
    for story_url in [
        "https://github.com/new/name",
        "https://www.github.com/old/name.git",
    ] {
        let story_is_kept = story_url == "https://github.com/new/name";
        for dates in [NO_DATING, post_dating] {
            let case = format!("{story_url} {dates:?}");
            let (first_writes, stored) = run_and_store(seed(story_url), &client, dates).await;
            assert_eq!(first_writes, 1, "{case}");
            assert_eq!(stored.len(), 1, "{case}");
            if story_is_kept {
                assert_eq!(stored[0].title, "Show HN: New", "{case}");
                assert_eq!(stored[0].timestamp, story_date, "{case}");
            }
            let (second_writes, settled) = run_and_store(stored.clone(), &client, dates).await;
            assert_eq!(second_writes, 0, "{case}");
            assert_eq!(settled, stored, "{case}");
        }
    }
}

#[tokio::test]
async fn cleanup_redates_only_synced_stars_to_their_star_time() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/starred"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "starred_at": "2023-01-02T03:04:05Z",
              "repo": { "full_name": "o/star", "html_url": "https://github.com/o/star" } },
            { "starred_at": "2023-01-02T03:04:05Z",
              "repo": { "full_name": "o/story", "html_url": "https://github.com/o/story" } }
        ])))
        .mount(&server)
        .await;
    for name in ["o/star", "o/story"] {
        mount_repo(
            &server,
            name,
            json!({ "full_name": name, "html_url": format!("https://github.com/{name}") }),
        )
        .await;
    }
    let stored = |url: &str, title: &str, note: &str, tag: &str| Bookmark {
        title: title.into(),
        note: note.into(),
        tags: vec![tag.into()],
        timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
        ..stored_at(url)
    };
    let pinboard = FakePinboard {
        all: vec![
            stored(
                "https://github.com/o/star",
                "o/star",
                "https://github.com/o/star",
                "github-star",
            ),
            stored(
                "https://github.com/o/story",
                "Show HN: Story",
                "HN Link: https://news.ycombinator.com/item?id=5",
                "hackernews",
            ),
        ],
        ..Default::default()
    };
    let client = github_client(&server);
    let dates = crate::timefmt::DateOpts {
        use_post_date: true,
        max_age_days: 365 * 100,
        stale_to_now: false,
    };

    run(&pinboard.cleanup_store(false), &client, dates, &[])
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1, "{updated:?}");
    assert_eq!(updated[0].url, "https://github.com/o/star");
    assert_eq!(updated[0].dt, "2023-01-02T03:04:05Z");
}

/// One cleanup run over `seed`, returning how many bookmarks it wrote and the account as
/// Pinboard then stores it (tags whitespace-split on read, as Pinboard returns them).
async fn run_and_store(
    seed: Vec<Bookmark>,
    client: &GitHubClient,
    dates: crate::timefmt::DateOpts,
) -> (usize, Vec<Bookmark>) {
    let pinboard = FakePinboard {
        all: seed.clone(),
        ..Default::default()
    };
    run(&pinboard.cleanup_store(false), client, dates, &[])
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
    let writes = pinboard.updated.borrow().len();
    (writes, stored)
}

#[tokio::test]
async fn cleanup_keeps_entity_text_in_a_stored_title() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/gone"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    mount_repo(
        &server,
        "o/star",
        json!({ "full_name": "o/star", "html_url": "https://github.com/o/star",
                "language": "Rust" }),
    )
    .await;
    let pinboard = FakePinboard {
        all: vec![
            Bookmark {
                title: "Use &amp; to escape".into(),
                ..stored_at("https://github.com/o/gone")
            },
            Bookmark {
                title: "Use &amp;amp; to escape".into(),
                tags: vec!["github-star".into()],
                ..stored_at("https://github.com/o/star")
            },
        ],
        ..Default::default()
    };
    let client = github_client(&server);
    run(&pinboard.cleanup_store(false), &client, NO_DATING, &[])
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(
        updated.len(),
        1,
        "only the synced star is written: {updated:?}"
    );
    assert_eq!(updated[0].url, "https://github.com/o/star");
    assert_eq!(updated[0].description, "Use &amp;amp; to escape");
}

fn github_client(server: &MockServer) -> GitHubClient {
    GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri())
}
