use super::*;
use crate::github::gists::GistPostDate;
use crate::pinboard::TRUNCATION_MARKER;
use crate::source::{Halt, Source};
use crate::test_support::{
    assert_cleanup_changes_nothing, stored_at, strings, synced_account, tag_list, FakePinboard,
    NO_DATING, ROUND_TRIP_DATINGS,
};
use serde_json::json;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// An expired token during the pass has to reach the caller as `Halt::Reauth`, or the
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
            tags: tag_list(&["github-star"]),
            timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
            ..stored_at("https://github.com/o/r")
        }],
        ..Default::default()
    };
    let client = github_client(&server);

    let err = run(&pinboard.cleanup_store(), &client, NO_DATING, &[])
        .await
        .unwrap_err();

    assert!(
        matches!(err, SourceError::Halted(Halt::Reauth(_))),
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
    // would not prove it -- wiremock answers those with a 404, which `repo()` reads as
    // a deleted repo and skips silently -- so assert zero calls explicitly. The
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
        tags: tag_list(&["github-star"]),
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

    let err = run(&pinboard.cleanup_store(), &client, NO_DATING, &[])
        .await
        .unwrap_err();

    assert!(err.to_string().contains("rate limit"), "{err}");
    // The repo read before the limit is still rewritten -- the work isn't thrown away.
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
        tags: tag_list(&["github-star"]),
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

    run(&pinboard.cleanup_store(), &client, NO_DATING, &[])
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
            tags: tag_list(&["github-star", "lang:python", "mine"]),
            timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
            ..stored_at("https://github.com/old/name")
        }],
        ..Default::default()
    };
    let client = github_client(&server);
    run(&pinboard.cleanup_store(), &client, NO_DATING, &[])
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://github.com/new/name");
    assert_eq!(updated[0].description, "new/name");
    // Hand-written notes are kept: only a note in the tool's shape is rebuilt.
    assert_eq!(updated[0].extended, "notes");
    // Language tag refreshed (python -> rust), base + user tags kept.
    assert!(
        updated[0].tags.contains(&"lang:rust".to_string()),
        "{:?}",
        updated[0].tags
    );
    assert!(
        !updated[0].tags.contains(&"lang:python".to_string()),
        "{:?}",
        updated[0].tags
    );
    assert!(
        updated[0].tags.contains(&"github-star".to_string()),
        "{:?}",
        updated[0].tags
    );
    assert!(
        updated[0].tags.contains(&"mine".to_string()),
        "{:?}",
        updated[0].tags
    );
    // Old URL deleted after the rewrite.
    assert_eq!(
        pinboard.deleted.borrow().as_slice(),
        &strings(&["https://github.com/old/name"])
    );
}

#[tokio::test]
async fn cleanup_leaves_deep_links_untouched() {
    fn stored(url: &str) -> Bookmark {
        Bookmark {
            title: "title".into(),
            note: "notes".into(),
            tags: tag_list(&["github-star"]),
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
            stored("https://gist.github.com/user"),
            stored("https://gist.github.com/user/aa5a315d61ae9438b18d/0123abc"),
        ],
        ..Default::default()
    };
    let client = github_client(&server);
    run(&pinboard.cleanup_store(), &client, NO_DATING, &[])
        .await
        .unwrap();

    assert!(
        pinboard.updated.borrow().is_empty(),
        "{:?}",
        pinboard.updated.borrow()
    );
    assert!(
        pinboard.deleted.borrow().is_empty(),
        "{:?}",
        pinboard.deleted.borrow()
    );
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
            tags: tag_list(&["github-star"]),
            timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
            ..stored_at("https://github.com/Owner/Repo.git/")
        }],
        ..Default::default()
    };
    let client = github_client(&server);
    run(&pinboard.cleanup_store(), &client, NO_DATING, &[])
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
            tags: tag_list(&["github-star"]),
            timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
            ..stored_at("https://www.github.com/Owner/Repo")
        }],
        ..Default::default()
    };
    let client = github_client(&server);
    run(&pinboard.cleanup_store(), &client, NO_DATING, &[])
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
            tags: tag_list(&["github-star"]),
            timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
            ..stored_at("https://github.com/o/r")
        }],
        ..Default::default()
    };
    let client = github_client(&server);
    run(&pinboard.cleanup_store(), &client, NO_DATING, &[])
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].extended, "<blockquote>A thing</blockquote>");
    // Nothing else changed, so no delete (URL is unchanged).
    assert!(
        pinboard.deleted.borrow().is_empty(),
        "{:?}",
        pinboard.deleted.borrow()
    );
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
            tags: tag_list(&[
                "hackernews",
                "author:hackernews:alice",
                "hackernews:show-hn",
            ]),
            timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
            ..stored_at("https://github.com/o/r")
        }],
        ..Default::default()
    };
    let client = github_client(&server);

    run(&pinboard.cleanup_store(), &client, NO_DATING, &[])
        .await
        .unwrap();

    assert!(
        pinboard.updated.borrow().is_empty(),
        "{:?}",
        pinboard.updated.borrow()
    );
    assert!(
        pinboard.deleted.borrow().is_empty(),
        "{:?}",
        pinboard.deleted.borrow()
    );
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
                tags: tag_list(&["work-star", "language:go"]),
                ..stored_at("https://github.com/o/work")
            },
            Bookmark {
                tags: tag_list(&["github-star"]),
                ..stored_at("https://github.com/o/home")
            },
        ],
        ..Default::default()
    };
    let work = GitHubConfig {
        tags: vec!["work-star".into()],
        lang_prefix: "language:".into(),
        author_prefix: "owner:".into(),
        ..GitHubConfig::default()
    };
    let client = github_client(&server);

    run(
        &pinboard.cleanup_store(),
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
            tags: tag_list(&["tools"]),
            ..stored_at("https://github.com/o/r")
        }],
        ..Default::default()
    };
    let client = github_client(&server);

    run(&pinboard.cleanup_store(), &client, NO_DATING, &[])
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
            tags: tag_list(&["github-star", "lang:python"]),
            ..stored_at("https://github.com/old/name")
        },
        Bookmark {
            title: "Show HN: Linked".into(),
            note: "HN Link: https://news.ycombinator.com/item?id=7".into(),
            tags: tag_list(&["hackernews"]),
            ..stored_at("https://www.github.com/o/linked.git")
        },
        Bookmark {
            title: "o/plain".into(),
            tags: tag_list(&["github-star"]),
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
                tags: tag_list(&["github-star"]),
                timestamp: crate::timefmt::parse_rfc3339("2023-01-02T03:04:05Z"),
                ..stored_at("https://github.com/old/name")
            },
            Bookmark {
                title: "Show HN: New".into(),
                note: "HN Link: https://news.ycombinator.com/item?id=9".into(),
                tags: tag_list(&["hackernews"]),
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
        tags: tag_list(&[tag]),
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

    run(&pinboard.cleanup_store(), &client, dates, &[])
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
    run(&pinboard.cleanup_store(), client, dates, &[])
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
                tags: tag_list(&["github-star"]),
                ..stored_at("https://github.com/o/star")
            },
        ],
        ..Default::default()
    };
    let client = github_client(&server);
    run(&pinboard.cleanup_store(), &client, NO_DATING, &[])
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

const GIST_ID: &str = "aa5a315d61ae9438b18d";

fn gist_url(owner: Option<&str>) -> String {
    match owner {
        Some(owner) => format!("https://gist.github.com/{owner}/{GIST_ID}"),
        None => format!("https://gist.github.com/{GIST_ID}"),
    }
}

/// The gist `{GIST_ID}` by `someone` described "Fresh", with one Rust file and the dates
/// `created_at`/`updated_at`.
fn fresh_gist(created_at: &str) -> serde_json::Value {
    json!({
        "id": GIST_ID,
        "html_url": gist_url(None),
        "description": "Fresh",
        "files": { "main.rs": { "filename": "main.rs", "language": "Rust" } },
        "owner": { "login": "someone" },
        "created_at": created_at,
        "updated_at": "2024-05-06T07:08:09Z",
    })
}

/// Mounts `/gists/starred` answering `response`.
async fn mount_starred(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path("/gists/starred"))
        .respond_with(response)
        .mount(server)
        .await;
}

/// Mounts [`fresh_gist`] as a gist the account hasn't starred: absent from
/// `/gists/starred`, so the pass looks it up at `/gists/{GIST_ID}`.
async fn mount_gist(server: &MockServer, created_at: &str) {
    mount_starred(server, ResponseTemplate::new(200).set_body_json(json!([]))).await;
    Mock::given(method("GET"))
        .and(path(format!("/gists/{GIST_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(fresh_gist(created_at)))
        .mount(server)
        .await;
}

/// A starred gist is planned from the one `/gists/starred` read, with no lookup of its own.
#[tokio::test]
async fn cleanup_plans_a_starred_gist_from_the_starred_list() {
    let server = MockServer::start().await;
    let starred = json!([fresh_gist("2020-01-02T03:04:05Z")]);
    mount_starred(&server, ResponseTemplate::new(200).set_body_json(starred)).await;
    Mock::given(method("GET"))
        .and(path(format!("/gists/{GIST_ID}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(0)
        .mount(&server)
        .await;
    let seed = vec![Bookmark {
        title: "main.rs".into(),
        tags: tag_list(&["github-star", "github-gist"]),
        ..stored_at(&gist_url(None))
    }];

    let (writes, stored) = run_and_store(seed, &github_client(&server), NO_DATING).await;
    assert_eq!(writes, 1);
    assert_eq!(stored[0].url.as_str(), gist_url(Some("someone")));
    assert_eq!(stored[0].title, "Fresh");
}

#[tokio::test]
async fn cleanup_with_gists_off_leaves_gists_alone() {
    let server = MockServer::start().await;
    for gist_path in ["/gists/starred".to_string(), format!("/gists/{GIST_ID}")] {
        Mock::given(method("GET"))
            .and(path(gist_path))
            .respond_with(ResponseTemplate::new(404))
            .expect(0)
            .mount(&server)
            .await;
    }
    let seed = vec![Bookmark {
        title: "Stale".into(),
        tags: tag_list(&["github-star", "github-gist"]),
        ..stored_at(&gist_url(None))
    }];
    let client =
        GitHubClient::with_base_url("tok".into(), GitHubConfig::stars_only(), server.uri());

    let (writes, stored) = run_and_store(seed.clone(), &client, NO_DATING).await;
    assert_eq!(writes, 0);
    assert_eq!(stored, seed);
}

/// The starred gists are read only when there is a gist to plan.
#[tokio::test]
async fn cleanup_without_gist_bookmarks_reads_no_gists() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/gists/starred"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(0)
        .mount(&server)
        .await;
    let pinboard = FakePinboard {
        all: vec![stored_at(&format!(
            "https://gist.github.com/someone/{GIST_ID}/0123abc"
        ))],
        ..Default::default()
    };

    run(
        &pinboard.cleanup_store(),
        &github_client(&server),
        NO_DATING,
        &[],
    )
    .await
    .unwrap();
}

/// A failed read of the starred gists fails the pass before anything is planned, so it
/// can't let the gists look reached; a rate limit there stays a rate limit.
#[tokio::test]
async fn a_failed_starred_gist_read_fails_the_gist_pass() {
    let refusals = [
        (ResponseTemplate::new(500), "other"),
        (
            ResponseTemplate::new(403).insert_header("x-ratelimit-remaining", "0"),
            "rate limited",
        ),
    ];
    for (response, expected) in refusals {
        let server = MockServer::start().await;
        mount_starred(&server, response).await;
        Mock::given(method("GET"))
            .and(path(format!("/gists/{GIST_ID}")))
            .respond_with(ResponseTemplate::new(404))
            .expect(0)
            .mount(&server)
            .await;
        let pinboard = FakePinboard {
            all: vec![Bookmark {
                tags: tag_list(&["github-star", "github-gist"]),
                ..stored_at(&gist_url(None))
            }],
            ..Default::default()
        };

        let (result, _) = crate::http::recording_waits(run(
            &pinboard.cleanup_store(),
            &github_client(&server),
            NO_DATING,
            &[],
        ))
        .await;
        let err = result.unwrap_err();
        let variant = match err {
            SourceError::Halted(Halt::RateLimited(_)) => "rate limited",
            SourceError::Other(_) => "other",
            _ => "unexpected",
        };
        assert_eq!(variant, expected, "{err:?}");
        assert!(
            pinboard.updated.borrow().is_empty(),
            "{:?}",
            pinboard.updated.borrow()
        );
    }
}

#[tokio::test]
async fn cleanup_canonicalizes_a_synced_gist_and_refreshes_what_sync_wrote() {
    let server = MockServer::start().await;
    mount_gist(&server, "2020-01-02T03:04:05Z").await;
    let seed = vec![Bookmark {
        title: "Stale".into(),
        note: "<blockquote>Stale</blockquote>\n\nFiles: old.go".into(),
        tags: tag_list(&["github-star", "github-gist", "lang:go", "mine"]),
        ..stored_at(&gist_url(None))
    }];

    let (writes, stored) = run_and_store(seed, &github_client(&server), NO_DATING).await;
    assert_eq!(writes, 1);
    let [gist] = &stored[..] else {
        panic!("one bookmark, moved: {stored:?}");
    };
    assert_eq!(gist.url.as_str(), gist_url(Some("someone")));
    assert_eq!(gist.title, "Fresh");
    assert_eq!(
        gist.note,
        "<blockquote>Fresh</blockquote>\n\nFiles: main.rs"
    );
    assert_eq!(
        gist.tags,
        strings(&[
            "github-star",
            "github-gist",
            "mine",
            "lang:rust",
            "author:github:someone"
        ])
    );

    let (second_writes, _) = run_and_store(stored, &github_client(&server), NO_DATING).await;
    assert_eq!(second_writes, 0, "settles after one run");
}

/// A gist saved by hand and tagged like a star, as one might have before sync wrote
/// gists, is not a synced gist: the inherited `github-star` is not the gist's marker, so
/// only its URL is canonicalized.
#[tokio::test]
async fn cleanup_leaves_a_hand_saved_gist_tagged_like_a_star_as_it_was() {
    let server = MockServer::start().await;
    mount_gist(&server, "2020-01-02T03:04:05Z").await;
    let seed = vec![
        Bookmark {
            title: "my gist".into(),
            note: "my notes".into(),
            tags: tag_list(&["github-star", "lang:go"]),
            ..stored_at(&gist_url(None))
        },
        Bookmark {
            title: "Show HN: Fresh".into(),
            note: "HN Link: https://news.ycombinator.com/item?id=7".into(),
            tags: tag_list(&["hackernews"]),
            ..stored_at(&format!("http://gist.github.com/other/{GIST_ID}.git"))
        },
    ];

    let (_, stored) = run_and_store(seed, &github_client(&server), NO_DATING).await;
    let [gist] = &stored[..] else {
        panic!("both forms of the gist fold into one: {stored:?}");
    };
    assert_eq!(gist.url.as_str(), gist_url(Some("someone")));
    assert_eq!(gist.title, "my gist");
    assert!(gist.note.starts_with("my notes"), "{:?}", gist.note);
    assert!(gist.note.contains("HN Link:"), "{:?}", gist.note);
    let mut tags = gist.tags.clone();
    tags.sort();
    assert_eq!(tags, strings(&["github-star", "hackernews", "lang:go"]));
}

/// Two accounts with default gist tags both tag their gists `github-gist`; running as one
/// must not add its star tag to the other's gist.
#[tokio::test]
async fn cleanup_refreshes_another_accounts_gist_by_its_own_tags() {
    let server = MockServer::start().await;
    mount_gist(&server, "2020-01-02T03:04:05Z").await;
    let config = crate::config::Config::parse(
        "[[github]]\nname = \"home\"\n[[github]]\nname = \"work\"\ntags = [\"work-star\"]\n",
    )
    .unwrap();
    let configured: Vec<GitHubConfig> = config
        .github
        .iter()
        .map(|account| GitHubConfig::resolve(Some(account), &config.defaults.github))
        .collect();
    let pinboard = FakePinboard {
        all: vec![Bookmark {
            title: "Fresh".into(),
            note: "<blockquote>Fresh</blockquote>\n\nFiles: main.rs".into(),
            tags: tag_list(&["work-star", "github-gist", "lang:go"]),
            ..stored_at(&gist_url(Some("someone")))
        }],
        ..Default::default()
    };
    let client = GitHubClient::with_base_url("tok".into(), configured[0].clone(), server.uri());

    run(&pinboard.cleanup_store(), &client, NO_DATING, &configured)
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1, "{updated:?}");
    assert_eq!(
        updated[0].tags,
        [
            "work-star",
            "github-gist",
            "lang:rust",
            "author:github:someone"
        ]
    );
}

/// A deleted gist keeps its URL, and one 404 doesn't fail the run.
#[tokio::test]
async fn cleanup_keeps_a_deleted_gist_where_it_is() {
    let server = MockServer::start().await;
    mount_starred(&server, ResponseTemplate::new(200).set_body_json(json!([]))).await;
    let seed = vec![Bookmark {
        tags: tag_list(&["github-star", "github-gist"]),
        ..stored_at(&gist_url(None))
    }];

    let (writes, stored) = run_and_store(seed.clone(), &github_client(&server), NO_DATING).await;
    assert_eq!(writes, 0);
    assert_eq!(stored, seed);
}

#[tokio::test]
async fn cleanup_redates_a_synced_gist_by_its_gist_post_date() {
    let server = MockServer::start().await;
    mount_gist(&server, "2020-01-02T03:04:05Z").await;
    Mock::given(method("GET"))
        .and(path("/user/starred"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    let stored = Bookmark {
        title: "Fresh".into(),
        note: "<blockquote>Fresh</blockquote>\n\nFiles: main.rs".into(),
        tags: tag_list(&[
            "github-star",
            "github-gist",
            "lang:rust",
            "author:github:someone",
        ]),
        timestamp: crate::timefmt::parse_rfc3339("2025-01-01T00:00:00Z"),
        ..stored_at(&gist_url(Some("someone")))
    };
    let dates = crate::timefmt::DateOpts {
        use_post_date: true,
        max_age_days: 365 * 100,
        stale_to_now: false,
    };
    for (post_date, expected) in [
        (GistPostDate::Now, None),
        (GistPostDate::Created, Some("2020-01-02T03:04:05Z")),
        (GistPostDate::Updated, Some("2024-05-06T07:08:09Z")),
    ] {
        let mut config = GitHubConfig::default();
        config.gists.post_date = post_date;
        let client = GitHubClient::with_base_url("tok".into(), config, server.uri());
        let pinboard = FakePinboard {
            all: vec![stored.clone()],
            ..Default::default()
        };
        run(&pinboard.cleanup_store(), &client, dates, &[])
            .await
            .unwrap();
        let updated = pinboard.updated.borrow();
        let dt: Option<&str> = updated.first().map(|call| call.dt.as_str());
        assert_eq!(dt, expected, "{post_date}");
    }
}

/// A dead token stops the repo pass, and the gist pass then makes no doomed lookup.
#[tokio::test]
async fn a_dead_token_on_the_repos_skips_the_gists() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/gists/starred"))
        .respond_with(ResponseTemplate::new(401))
        .expect(0)
        .mount(&server)
        .await;
    let pinboard = FakePinboard {
        all: vec![
            stored_at("https://github.com/o/r"),
            stored_at(&gist_url(None)),
        ],
        ..Default::default()
    };

    let err = run(
        &pinboard.cleanup_store(),
        &github_client(&server),
        NO_DATING,
        &[],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, SourceError::Halted(Halt::Reauth(_))),
        "{err:?}"
    );
}

/// A dead token on the lookup of a gist that isn't starred halts the gist pass as a
/// re-auth, as one on a repo lookup does.
#[tokio::test]
async fn a_dead_token_on_a_gist_lookup_is_a_reauth() {
    let server = MockServer::start().await;
    mount_starred(&server, ResponseTemplate::new(200).set_body_json(json!([]))).await;
    Mock::given(method("GET"))
        .and(path(format!("/gists/{GIST_ID}")))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let pinboard = FakePinboard {
        all: vec![stored_at(&gist_url(None))],
        ..Default::default()
    };

    let err = run(
        &pinboard.cleanup_store(),
        &github_client(&server),
        NO_DATING,
        &[],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, SourceError::Halted(Halt::Reauth(_))),
        "{err:?}"
    );
}

/// The gist pass's own dead token fires the hook too, and a failed repo pass ahead of it
/// is still reported.
#[tokio::test]
async fn a_dead_token_on_the_gists_is_a_reauth_even_after_failed_repos() {
    let server = MockServer::start().await;
    for repo in ["/repos/o/r", "/repos/o/s"] {
        Mock::given(method("GET"))
            .and(path(repo))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
    }
    mount_starred(&server, ResponseTemplate::new(401)).await;
    let pinboard = FakePinboard {
        // Two failed lookups and none answered: a majority, which fails the repo pass.
        all: vec![
            stored_at("https://github.com/o/r"),
            stored_at("https://github.com/o/s"),
            stored_at(&gist_url(None)),
        ],
        ..Default::default()
    };

    let (result, _) = crate::http::recording_waits(run(
        &pinboard.cleanup_store(),
        &github_client(&server),
        NO_DATING,
        &[],
    ))
    .await;
    let err = result.unwrap_err();
    let SourceError::Halted(Halt::Reauth(message)) = &err else {
        panic!("expected reauth, got {err:?}");
    };
    assert!(message.contains("and for repos:"), "{message}");
}

/// A server whose one starred gist, described `description`, has so many files that the
/// notes `sync` gives it are too long for a `posts/add` URL. No repo is starred.
async fn many_file_gist_server(description: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/starred"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    let files: serde_json::Map<String, serde_json::Value> = (0..200)
        .map(|n| {
            let name = format!("file_number_{n:03}.txt");
            (
                name.clone(),
                json!({ "filename": name, "language": "Text" }),
            )
        })
        .collect();
    let gist = json!({
        "id": GIST_ID,
        "html_url": gist_url(None),
        "description": description,
        "files": files,
        "owner": { "login": "octocat" },
    });
    mount_starred(
        &server,
        ResponseTemplate::new(200).set_body_json(json!([gist])),
    )
    .await;
    server
}

/// The account after `sync github` writes what `server` lists to a Pinboard that trims
/// long notes, each bookmark dated as Pinboard dates an undated write.
async fn synced_trimmed(server: &MockServer) -> Vec<Bookmark> {
    let drafts = crate::http::recording_waits(github_client(server).fetch())
        .await
        .0
        .unwrap();
    let pinboard = FakePinboard {
        trims_notes: true,
        ..Default::default()
    };
    let new = crate::sync::prepare_new_drafts(
        &github_client(server),
        drafts,
        &[],
        &crate::sync::JobSettings {
            limit: crate::config::SyncLimit::All,
            read_later: false,
            public: false,
            dates: NO_DATING,
        },
        time::OffsetDateTime::now_utc(),
    );
    let tally = crate::sync::write_drafts(&pinboard, &new, crate::bookmark::WriteMode::Live).await;
    assert_eq!(tally.failed, 0);
    let mut synced = crate::test_support::replay(&[], &pinboard.ops.take());
    for bookmark in &mut synced {
        bookmark.timestamp = crate::timefmt::parse_rfc3339("2024-05-06T07:08:09Z");
    }
    synced
}

/// One cleanup run over `account` on a Pinboard that trims long notes, returning how many
/// bookmarks it wrote and the account after it.
async fn clean_trimmed(account: Vec<Bookmark>, server: &MockServer) -> (usize, Vec<Bookmark>) {
    let pinboard = FakePinboard {
        all: account.clone(),
        trims_notes: true,
        ..Default::default()
    };
    run(
        &pinboard.cleanup_store(),
        &github_client(server),
        NO_DATING,
        &[GitHubConfig::default()],
    )
    .await
    .unwrap();
    let writes = pinboard.updated.borrow().len();
    let cleaned = crate::test_support::replay(&account, &pinboard.ops.take());
    (writes, cleaned)
}

#[tokio::test]
async fn cleanup_leaves_a_gist_note_pinboard_trimmed_as_it_is() {
    let server = many_file_gist_server("Many files").await;
    let synced = synced_trimmed(&server).await;
    assert!(
        synced[0].note.ends_with(TRUNCATION_MARKER),
        "the fixture's note must be trimmed: {:?}",
        synced[0].note
    );

    let (writes, _) = clean_trimmed(synced, &server).await;
    assert_eq!(
        writes, 0,
        "a trimmed note that is otherwise current is no change"
    );
}

#[tokio::test]
async fn cleanup_refreshes_an_edited_gist_with_a_trimmed_note_once() {
    let synced = synced_trimmed(&many_file_gist_server("Many files").await).await;

    let edited = many_file_gist_server("Many more files").await;
    let (first_writes, cleaned) = clean_trimmed(synced, &edited).await;
    assert_eq!(first_writes, 1);
    assert!(
        cleaned[0]
            .note
            .starts_with("<blockquote>Many more files</blockquote>")
            && cleaned[0].note.ends_with(TRUNCATION_MARKER),
        "{:?}",
        cleaned[0].note
    );
    let (second_writes, _) = clean_trimmed(cleaned, &edited).await;
    assert_eq!(second_writes, 0, "the refreshed note settles");
}

/// `days` days before now, at whole seconds, as GitHub dates things.
fn days_ago(days: i64) -> String {
    let instant = time::OffsetDateTime::now_utc() - time::Duration::days(days);
    crate::timefmt::to_pinboard_dt(instant).unwrap()
}

/// A date long outside the default 30-day dating cap.
const LONG_AGO: &str = "2020-09-13T12:26:40Z";

/// Stars as `/user/starred` lists them, newest first: a repo with a language that slugs,
/// one with no description, homepage or language, and one starred long ago.
fn round_trip_stars() -> Vec<serde_json::Value> {
    vec![
        json!({ "starred_at": days_ago(2), "repo": {
            "full_name": "jupyter/notebook", "html_url": "https://github.com/jupyter/notebook",
            "description": "Jupyter   Interactive Notebook",
            "homepage": "https://jupyter-notebook.readthedocs.io/",
            "language": "Jupyter Notebook" } }),
        json!({ "starred_at": days_ago(3), "repo": {
            "full_name": "someone/dotfiles", "html_url": "https://github.com/someone/dotfiles",
            "description": null, "homepage": "", "language": null } }),
        json!({ "starred_at": LONG_AGO, "repo": {
            "full_name": "tokio-rs/tokio", "html_url": "https://github.com/tokio-rs/tokio",
            "description": "A runtime for writing reliable asynchronous applications & more",
            "homepage": "https://tokio.rs", "language": "Rust" } }),
    ]
}

/// The defaults with gists off, and a config that adds a base tag, renames the language
/// tag and turns off the owner tag.
fn round_trip_star_configs() -> [GitHubConfig; 2] {
    let custom = GitHubConfig {
        tags: strings(&["github-star", "starred"]),
        lang_prefix: "language:".into(),
        author_prefix: String::new(),
        ..GitHubConfig::stars_only()
    };
    [GitHubConfig::stars_only(), custom]
}

#[tokio::test]
async fn cleanup_leaves_the_stars_sync_wrote_as_they_are() {
    let stars = round_trip_stars();
    for config in round_trip_star_configs() {
        for dates in ROUND_TRIP_DATINGS {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/user/starred"))
                .respond_with(ResponseTemplate::new(200).set_body_json(&stars))
                .expect(if dates.use_post_date { 2 } else { 1 })
                .mount(&server)
                .await;
            for star in &stars {
                let repo = &star["repo"];
                Mock::given(method("GET"))
                    .and(path(format!(
                        "/repos/{}",
                        repo["full_name"].as_str().unwrap()
                    )))
                    .respond_with(ResponseTemplate::new(200).set_body_json(repo))
                    .expect(1)
                    .mount(&server)
                    .await;
            }
            let client = GitHubClient::with_base_url("tok".into(), config.clone(), server.uri());

            let account = synced_account(&client, dates).await;
            assert_eq!(account.len(), stars.len(), "{config:?} {dates:?}");
            let configured = [config.clone()];
            assert_cleanup_changes_nothing(account, async |store| {
                run(store, &client, dates, &configured).await
            })
            .await;
        }
    }
}

/// Starred gists as `/gists/starred` lists them: one by `octocat` in three languages, made
/// recently, and an undescribed one-file gist by `someone`, made long ago.
fn round_trip_gists() -> Vec<serde_json::Value> {
    vec![
        json!({
            "id": "aa5a315d61ae9438b18d",
            "html_url": "https://gist.github.com/aa5a315d61ae9438b18d",
            "description": "Hello   world helpers ",
            "files": {
                "hello.rb": { "filename": "hello.rb", "language": "Ruby" },
                "hello.py": { "filename": "hello.py", "language": "Python" },
                "README.md": { "filename": "README.md", "language": "Markdown" },
            },
            "owner": { "login": "octocat" },
            "created_at": days_ago(5),
            "updated_at": days_ago(1),
        }),
        json!({
            "id": "0123456789abcdef0123",
            "html_url": "https://gist.github.com/0123456789abcdef0123",
            "description": "",
            "files": { "notes.txt": { "filename": "notes.txt", "language": null } },
            "owner": { "login": "someone" },
            "created_at": LONG_AGO,
            "updated_at": LONG_AGO,
        }),
    ]
}

/// The defaults dating gists by `post_date`, and the same with gist tags of their own, a
/// renamed language tag and no owner tag.
fn round_trip_gist_configs(post_date: GistPostDate) -> [GitHubConfig; 2] {
    let mut default = GitHubConfig::default();
    default.gists.post_date = post_date;
    let mut custom = default.clone();
    custom.gists.tags = strings(&["gist", "saved"]);
    custom.gists.lang_prefix = "language:".into();
    custom.gists.author_prefix = String::new();
    [default, custom]
}

#[tokio::test]
async fn cleanup_leaves_the_gists_sync_wrote_as_they_are() {
    let gists = round_trip_gists();
    for post_date in [
        GistPostDate::Now,
        GistPostDate::Created,
        GistPostDate::Updated,
    ] {
        for config in round_trip_gist_configs(post_date) {
            for dates in ROUND_TRIP_DATINGS {
                let server = MockServer::start().await;
                Mock::given(method("GET"))
                    .and(path("/user/starred"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
                    .mount(&server)
                    .await;
                Mock::given(method("GET"))
                    .and(path("/gists/starred"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(&gists))
                    .expect(2)
                    .mount(&server)
                    .await;
                // Every gist is planned from the starred list, never looked up on its own.
                Mock::given(method("GET"))
                    .and(path_regex("^/gists/[0-9a-f]+$"))
                    .respond_with(ResponseTemplate::new(404))
                    .expect(0)
                    .mount(&server)
                    .await;
                let client =
                    GitHubClient::with_base_url("tok".into(), config.clone(), server.uri());

                let account = synced_account(&client, dates).await;
                assert_eq!(account.len(), gists.len(), "{config:?} {dates:?}");
                let configured = [config.clone()];
                assert_cleanup_changes_nothing(account, async |store| {
                    run(store, &client, dates, &configured).await
                })
                .await;
            }
        }
    }
}
