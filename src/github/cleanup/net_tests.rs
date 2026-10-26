use super::*;
use crate::test_support::{FakePinboard, NO_DATING};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// An expired token during the pass has to reach the caller as `ReauthRequired`, or
/// `main` cannot tell it apart from any other failure and the auth-failure hook
/// never fires for `cleanup`.
#[tokio::test]
async fn cleanup_surfaces_an_expired_token_as_reauth() {
    use crate::pinboard::PinboardBookmark;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"message":"Bad creds"}"#))
        .mount(&server)
        .await;

    let pinboard = FakePinboard {
        all: vec![PinboardBookmark {
            url: "https://github.com/o/r".into(),
            description: "o/r".into(),
            extended: "notes".into(),
            tags: "github-star".into(),
            time: "2020-01-01T00:00:00Z".into(),
            shared: "no".into(),
            toread: "no".into(),
        }
        .try_into()
        .unwrap()],
        ..Default::default()
    };
    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

    let err = run(&pinboard.cleanup_store(false), &client, NO_DATING)
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
    use crate::pinboard::PinboardBookmark;

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

    let stored = |url: &str, title: &str| {
        PinboardBookmark {
            url: url.into(),
            description: title.into(),
            extended: "notes".into(),
            tags: "github-star".into(),
            time: "2020-01-01T00:00:00Z".into(),
            shared: "no".into(),
            toread: "no".into(),
        }
        .try_into()
        .unwrap()
    };
    let pinboard = FakePinboard {
        all: vec![
            stored("https://github.com/old/name", "old/name"),
            stored("https://github.com/limited/repo", "limited/repo"),
            stored("https://github.com/never/looked", "never/looked"),
        ],
        ..Default::default()
    };
    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

    let err = run(&pinboard.cleanup_store(false), &client, NO_DATING)
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
    use crate::pinboard::PinboardBookmark;

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

    let stored = |url: &str, title: &str| {
        PinboardBookmark {
            url: url.into(),
            description: title.into(),
            extended: "notes".into(),
            tags: "github-star".into(),
            time: "2020-01-01T00:00:00Z".into(),
            shared: "no".into(),
            toread: "no".into(),
        }
        .try_into()
        .unwrap()
    };
    let pinboard = FakePinboard {
        all: vec![
            stored("https://github.com/blocked/repo", "blocked/repo"),
            stored("https://github.com/old/name", "old/name"),
        ],
        ..Default::default()
    };
    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

    run(&pinboard.cleanup_store(false), &client, NO_DATING)
        .await
        .expect("one dead repo must not fail the whole run");

    // The readable repo was still cleaned up.
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://github.com/new/name");
}

#[tokio::test]
async fn cleanup_refresh_rewrites_renamed_repo_and_language() {
    use crate::pinboard::PinboardBookmark;

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
        all: vec![PinboardBookmark {
            url: "https://github.com/old/name".into(),
            description: "old/name".into(),
            extended: "notes".into(),
            tags: "github-star lang:python mine".into(),
            time: "2020-01-01T00:00:00Z".into(),
            shared: "no".into(),
            toread: "no".into(),
        }
        .try_into()
        .unwrap()],
        ..Default::default()
    };
    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
    run(&pinboard.cleanup_store(false), &client, NO_DATING)
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://github.com/new/name");
    assert_eq!(updated[0].description, "new/name");
    // Notes rebuilt from fresh data: description wrapped in a <blockquote>.
    assert_eq!(
        updated[0].extended,
        "<blockquote>A renamed thing</blockquote>"
    );
    // Language tag refreshed (python → rust), base + user tags kept.
    assert!(updated[0].tags.contains(&"lang:rust".to_string()));
    assert!(!updated[0].tags.contains(&"lang:python".to_string()));
    assert!(updated[0].tags.contains(&"github-star".to_string()));
    assert!(updated[0].tags.contains(&"mine".to_string()));
    // Old URL deleted after the rewrite.
    assert_eq!(
        pinboard.deleted.borrow().as_slice(),
        &["https://github.com/old/name".to_string()]
    );
}

#[tokio::test]
async fn cleanup_leaves_deep_links_and_gists_untouched() {
    use crate::pinboard::PinboardBookmark;

    fn stored(url: &str) -> Bookmark {
        PinboardBookmark {
            url: url.into(),
            description: "title".into(),
            extended: "notes".into(),
            tags: "github-star".into(),
            time: "2020-01-01T00:00:00Z".into(),
            shared: "no".into(),
            toread: "no".into(),
        }
        .try_into()
        .unwrap()
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
    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
    run(&pinboard.cleanup_store(false), &client, NO_DATING)
        .await
        .unwrap();

    assert!(pinboard.updated.borrow().is_empty());
    assert!(pinboard.deleted.borrow().is_empty());
}

#[tokio::test]
async fn cleanup_canonicalizes_non_canonical_repo_root() {
    use crate::pinboard::PinboardBookmark;

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
        all: vec![PinboardBookmark {
            url: "https://github.com/Owner/Repo.git/".into(),
            description: "Owner/Repo".into(),
            extended: "https://github.com/Owner/Repo".into(),
            tags: "github-star".into(),
            time: "2020-01-01T00:00:00Z".into(),
            shared: "no".into(),
            toread: "no".into(),
        }
        .try_into()
        .unwrap()],
        ..Default::default()
    };
    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
    run(&pinboard.cleanup_store(false), &client, NO_DATING)
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://github.com/Owner/Repo");
    // The non-canonical original was deleted after the rewrite.
    assert_eq!(
        pinboard.deleted.borrow().as_slice(),
        &["https://github.com/Owner/Repo.git/".to_string()]
    );
}

#[tokio::test]
async fn cleanup_folds_www_host_onto_github_com() {
    use crate::pinboard::PinboardBookmark;

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
        all: vec![PinboardBookmark {
            url: "https://www.github.com/Owner/Repo".into(),
            description: "Owner/Repo".into(),
            extended: "https://github.com/Owner/Repo".into(),
            tags: "github-star".into(),
            time: "2020-01-01T00:00:00Z".into(),
            shared: "no".into(),
            toread: "no".into(),
        }
        .try_into()
        .unwrap()],
        ..Default::default()
    };
    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
    run(&pinboard.cleanup_store(false), &client, NO_DATING)
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://github.com/Owner/Repo");
    assert_eq!(
        pinboard.deleted.borrow().as_slice(),
        &["https://www.github.com/Owner/Repo".to_string()]
    );
}

#[tokio::test]
async fn cleanup_rewrites_notes_only_to_retrofit_the_blockquote() {
    use crate::pinboard::PinboardBookmark;

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
        all: vec![PinboardBookmark {
            url: "https://github.com/o/r".into(),
            description: "o/r".into(),
            extended: "A thing".into(), // pre-blockquote notes
            tags: "github-star".into(),
            time: "2020-01-01T00:00:00Z".into(),
            shared: "no".into(),
            toread: "no".into(),
        }
        .try_into()
        .unwrap()],
        ..Default::default()
    };
    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
    run(&pinboard.cleanup_store(false), &client, NO_DATING)
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].extended, "<blockquote>A thing</blockquote>");
    // Nothing else changed, so no delete (URL is unchanged).
    assert!(pinboard.deleted.borrow().is_empty());
}
