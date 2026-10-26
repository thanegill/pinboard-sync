use super::*;
use crate::test_support::{stored_at, FakePinboard, NO_DATING};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn cleanup_merges_a_story_into_a_separately_saved_article() {
    // The story's article is *also* saved on its own, with the user's own notes and
    // tags. Pinboard holds one record per URL, so the end state has to be a single
    // bookmark: merge rather than either overwriting the user's record or leaving the
    // redundant item bookmark behind forever.
    let algolia = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "hits": [
                { "objectID": "42", "title": "Cool", "url": "https://example.com/x",
                  "author": "alice", "_tags": ["story", "author_alice", "story_42"] }
            ]
        })))
        .mount(&algolia)
        .await;

    let stored = |url: &str, title: &str, extended: &str, tags: &str| Bookmark {
        title: title.into(),
        note: extended.into(),
        tags: tags.split_whitespace().map(String::from).collect(),
        timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
        ..stored_at(url)
    };
    let pinboard = FakePinboard {
        all: vec![
            stored(
                "https://news.ycombinator.com/item?id=42",
                "old title",
                "",
                "hackernews",
            ),
            stored(
                "https://example.com/x",
                "My saved article",
                "hand written notes",
                "keepme",
            ),
        ],
        ..Default::default()
    };

    let client = HackerNewsClient::with_base_urls(
        String::new(),
        HackerNewsConfig::default(),
        "unused".into(),
        algolia.uri(),
    );
    run(
        &pinboard.cleanup_store(false),
        &client,
        &HackerNewsCleanupOpts {
            link_discussions: false,
            dates: NO_DATING,
        },
    )
    .await
    .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    let written = &updated[0];
    assert_eq!(written.url, "https://example.com/x");
    // The user's own title survives -- the resident is the first participant in the
    // merge, and the first non-empty title wins.
    assert_eq!(written.description, "My saved article");
    assert!(
        written.extended.contains("hand written notes"),
        "{written:?}"
    );
    assert!(written.extended.contains("HN Link:"), "{written:?}");
    assert!(written.tags.contains(&"keepme".to_string()), "{written:?}");
    assert!(
        written.tags.contains(&"hackernews".to_string()),
        "{written:?}"
    );
    // ...and the now-redundant HN item bookmark is absorbed, so the pass converges.
    assert_eq!(
        *pinboard.deleted.borrow(),
        vec!["https://news.ycombinator.com/item?id=42".to_string()]
    );
}

#[tokio::test]
async fn cleanup_rewrites_story_url_and_deletes_old() {
    let algolia = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "hits": [
                { "objectID": "42", "title": "Cool", "url": "https://example.com/x",
                  "author": "alice", "_tags": ["story", "author_alice", "story_42"] }
            ]
        })))
        .mount(&algolia)
        .await;

    let pinboard = FakePinboard {
        all: vec![Bookmark {
            title: "old title".into(),
            tags: vec!["hackernews".into(), "mine".into()],
            timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
            ..stored_at("https://news.ycombinator.com/item?id=42")
        }],
        ..Default::default()
    };

    let client = HackerNewsClient::with_base_urls(
        String::new(),
        HackerNewsConfig::default(),
        "unused".into(),
        algolia.uri(),
    );
    run(
        &pinboard.cleanup_store(false),
        &client,
        &HackerNewsCleanupOpts {
            link_discussions: false,
            dates: NO_DATING,
        },
    )
    .await
    .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://example.com/x");
    assert_eq!(updated[0].description, "Cool");
    // Existing tags are preserved and augmented.
    assert!(updated[0].tags.contains(&"mine".to_string()));
    // The old HN item URL is deleted after the rewrite.
    assert_eq!(
        pinboard.deleted.borrow().as_slice(),
        &["https://news.ycombinator.com/item?id=42".to_string()]
    );
}

#[tokio::test]
async fn cleanup_preserves_existing_note_on_text_post() {
    // A text post (no url): the bookmark URL already is the HN permalink.
    let algolia = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "hits": [
                { "objectID": "7", "title": "Ask HN: How?", "story_text": "<p>details</p>",
                  "author": "bob", "_tags": ["story", "author_bob", "story_7"] }
            ]
        })))
        .mount(&algolia)
        .await;

    let stored_note = "my own answer to this question";
    let pinboard = FakePinboard {
        all: vec![Bookmark {
            title: "Ask HN: How?".into(),
            note: stored_note.into(),
            tags: vec!["hackernews".into()],
            timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
            ..stored_at("https://news.ycombinator.com/item?id=7")
        }],
        ..Default::default()
    };

    let client = HackerNewsClient::with_base_urls(
        String::new(),
        HackerNewsConfig::default(),
        "unused".into(),
        algolia.uri(),
    );
    run(
        &pinboard.cleanup_store(false),
        &client,
        &HackerNewsCleanupOpts {
            link_discussions: false,
            dates: NO_DATING,
        },
    )
    .await
    .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    // URL unchanged (in-place); the author/special-type tags are added, but the note
    // the user wrote is kept verbatim rather than replaced by the reconstructed one.
    assert_eq!(updated[0].url, "https://news.ycombinator.com/item?id=7");
    assert_eq!(updated[0].extended, stored_note);
    assert!(updated[0].tags.contains(&"hackernews:ask-hn".to_string()));
    assert!(pinboard.deleted.borrow().is_empty());
}

#[tokio::test]
async fn link_discussions_adds_hn_link_to_tagged_article() {
    // The URL search returns a matching HN story id; the item batch (unused
    // here) would hit the same path, so the matcher keys on restrictSearch...
    let algolia = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param("restrictSearchableAttributes", "url"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "hits": [
                { "objectID": "99", "url": "https://example.com/x",
                  "_tags": ["story", "author_a", "story_99"] }
            ]
        })))
        .mount(&algolia)
        .await;

    let pinboard = FakePinboard {
        all: vec![Bookmark {
            title: "An article".into(),
            note: "my notes".into(),
            tags: vec!["find-hn".into(), "reading".into()],
            timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
            ..stored_at("https://example.com/x")
        }],
        ..Default::default()
    };

    let client = HackerNewsClient::with_base_urls(
        String::new(),
        HackerNewsConfig::default(),
        "unused".into(),
        algolia.uri(),
    );
    run(
        &pinboard.cleanup_store(false),
        &client,
        &HackerNewsCleanupOpts {
            link_discussions: true,
            dates: NO_DATING,
        },
    )
    .await
    .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    // URL unchanged (in-place), notes gain the HN link, marker tag swapped for
    // the base HN tag, existing tags kept.
    assert_eq!(updated[0].url, "https://example.com/x");
    assert!(updated[0].tags.contains(&"hackernews".to_string()));
    assert!(!updated[0].tags.contains(&"find-hn".to_string()));
    assert!(updated[0].tags.contains(&"reading".to_string()));
    assert!(pinboard.deleted.borrow().is_empty());
}
