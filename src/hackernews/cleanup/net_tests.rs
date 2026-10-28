use super::*;
use crate::test_support::{stored_at, strings, tag_list, FakePinboard, NO_DATING};
use serde_json::json;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
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

    let client = algolia_client(&algolia);
    run(
        &pinboard.cleanup_store(),
        &client,
        &HackerNewsCleanupOpts {
            link_discussions: false,
            dates: NO_DATING,
            config: HackerNewsConfig::default(),
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
        strings(&["https://news.ycombinator.com/item?id=42"])
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
            tags: tag_list(&["hackernews", "mine"]),
            timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
            ..stored_at("https://news.ycombinator.com/item?id=42")
        }],
        ..Default::default()
    };

    let client = algolia_client(&algolia);
    run(
        &pinboard.cleanup_store(),
        &client,
        &HackerNewsCleanupOpts {
            link_discussions: false,
            dates: NO_DATING,
            config: HackerNewsConfig::default(),
        },
    )
    .await
    .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://example.com/x");
    assert_eq!(updated[0].description, "Cool");
    // Existing tags are preserved and augmented.
    assert!(
        updated[0].tags.contains(&"mine".to_string()),
        "{:?}",
        updated[0].tags
    );
    // The old HN item URL is deleted after the rewrite.
    assert_eq!(
        pinboard.deleted.borrow().as_slice(),
        &strings(&["https://news.ycombinator.com/item?id=42"])
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
            tags: tag_list(&["hackernews"]),
            timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
            ..stored_at("https://news.ycombinator.com/item?id=7")
        }],
        ..Default::default()
    };

    let client = algolia_client(&algolia);
    run(
        &pinboard.cleanup_store(),
        &client,
        &HackerNewsCleanupOpts {
            link_discussions: false,
            dates: NO_DATING,
            config: HackerNewsConfig::default(),
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
    assert!(
        updated[0].tags.contains(&"hackernews:ask-hn".to_string()),
        "{:?}",
        updated[0].tags
    );
    assert!(
        pinboard.deleted.borrow().is_empty(),
        "{:?}",
        pinboard.deleted.borrow()
    );
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
            tags: tag_list(&["find-hn", "reading"]),
            timestamp: crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z"),
            ..stored_at("https://example.com/x")
        }],
        ..Default::default()
    };

    let client = algolia_client(&algolia);
    run(
        &pinboard.cleanup_store(),
        &client,
        &HackerNewsCleanupOpts {
            link_discussions: true,
            dates: NO_DATING,
            config: HackerNewsConfig::default(),
        },
    )
    .await
    .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    // URL unchanged (in-place), notes gain the HN link, marker tag swapped for
    // the base HN tag, existing tags kept.
    assert_eq!(updated[0].url, "https://example.com/x");
    assert!(
        updated[0].tags.contains(&"hackernews".to_string()),
        "{:?}",
        updated[0].tags
    );
    assert!(
        !updated[0].tags.contains(&"find-hn".to_string()),
        "{:?}",
        updated[0].tags
    );
    assert!(
        updated[0].tags.contains(&"reading".to_string()),
        "{:?}",
        updated[0].tags
    );
    assert!(
        pinboard.deleted.borrow().is_empty(),
        "{:?}",
        pinboard.deleted.borrow()
    );
}

/// The link pass looks up only articles carrying the marker tag: neither an untagged
/// article nor a tagged HN item costs a URL search.
#[tokio::test]
async fn link_discussions_searches_only_tagged_articles() {
    let algolia = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param_is_missing("restrictSearchableAttributes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "hits": [] })))
        .mount(&algolia)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param("restrictSearchableAttributes", "url"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "hits": [] })))
        .expect(1)
        .mount(&algolia)
        .await;
    let tagged = |url: &str| Bookmark {
        tags: tag_list(&["find-hn"]),
        ..stored_at(url)
    };
    let pinboard = FakePinboard {
        all: vec![
            tagged("https://example.com/tagged"),
            stored_at("https://example.com/untagged"),
            tagged("https://news.ycombinator.com/item?id=7"),
        ],
        ..Default::default()
    };
    let client = algolia_client(&algolia);
    let opts = HackerNewsCleanupOpts {
        link_discussions: true,
        dates: NO_DATING,
        config: HackerNewsConfig::default(),
    };
    run(&pinboard.cleanup_store(), &client, &opts)
        .await
        .unwrap();

    assert!(
        pinboard.updated.borrow().is_empty(),
        "{:?}",
        pinboard.updated.borrow()
    );
}

/// Two `find-hn` articles and, unless `with_item` is false, one HN story bookmark whose
/// rewrite to its article Pinboard refuses. Algolia answers the item batch but fails
/// every URL search, so the link pass fails its lookups in the majority. Returns what
/// `run` reports, with `--link-discussions` on.
async fn run_with_failing_lookups(with_item: bool) -> String {
    let algolia = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param_is_missing("restrictSearchableAttributes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "hits": [
                { "objectID": "42", "title": "Cool", "url": "https://example.com/story",
                  "author": "alice", "_tags": ["story", "author_alice", "story_42"] }
            ]
        })))
        .mount(&algolia)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param("restrictSearchableAttributes", "url"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&algolia)
        .await;
    let article = |url: &str| Bookmark {
        tags: tag_list(&["find-hn"]),
        ..stored_at(url)
    };
    let mut all = vec![
        article("https://example.com/a"),
        article("https://example.com/b"),
    ];
    if with_item {
        all.push(stored_at("https://news.ycombinator.com/item?id=42"));
    }
    let pinboard = FakePinboard {
        all,
        fail_update_urls: ["https://example.com/story".to_string()].into(),
        ..Default::default()
    };
    let client = algolia_client(&algolia);
    let opts = HackerNewsCleanupOpts {
        link_discussions: true,
        dates: NO_DATING,
        config: HackerNewsConfig::default(),
    };
    let (result, _) =
        crate::http::recording_waits(run(&pinboard.cleanup_store(), &client, &opts)).await;
    format!("{:#}", result.expect_err("the run fails"))
}

/// The item pass, which plans from a prefetched batch and so cannot fail a lookup, must
/// not vouch for the discussion-link pass: its lookups failing in the majority fail the
/// run on their own.
#[tokio::test]
async fn cleanup_fails_when_only_the_link_pass_fails() {
    let error = run_with_failing_lookups(false).await;
    assert!(error.contains("2 of 2 lookup(s) failed"), "{error}");
}

/// When both passes fail, the run reports both, so the first can't hide the second.
#[tokio::test]
async fn cleanup_reports_both_passes_when_both_fail() {
    let error = run_with_failing_lookups(true).await;
    assert!(error.contains("1 bookmark(s) failed to write"), "{error}");
    assert!(
        error.contains("; and for discussion links: 2 of 2 lookup(s) failed"),
        "{error}"
    );
}

/// A search hit whose `objectID` isn't a number is no discussion: it is never written into
/// the note, and the article keeps its marker tag for the next run.
#[tokio::test]
async fn link_discussions_ignores_a_non_numeric_object_id() {
    let algolia = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param("restrictSearchableAttributes", "url"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "hits": [{ "objectID": "99<script>", "url": "https://example.com/x",
                       "_tags": ["story"] }]
        })))
        .expect(1)
        .mount(&algolia)
        .await;
    let pinboard = FakePinboard {
        all: vec![Bookmark {
            note: "my notes".into(),
            tags: tag_list(&["find-hn"]),
            ..stored_at("https://example.com/x")
        }],
        ..Default::default()
    };
    let client = algolia_client(&algolia);
    let opts = HackerNewsCleanupOpts {
        link_discussions: true,
        dates: NO_DATING,
        config: HackerNewsConfig::default(),
    };
    let (result, logs) =
        crate::test_support::capturing_logs(run(&pinboard.cleanup_store(), &client, &opts)).await;
    result.unwrap();

    assert!(
        pinboard.updated.borrow().is_empty(),
        "{:?}",
        pinboard.updated.borrow()
    );
    let warned = crate::test_support::warnings(&logs);
    assert!(
        warned
            .iter()
            .any(|line| line.contains(r#"non-numeric objectID "99<script>""#)),
        "{warned:?}"
    );
}

/// A non-numeric hit is skipped, not taken as the answer: a later matching hit with a real
/// id is still linked.
#[tokio::test]
async fn link_discussions_looks_past_a_non_numeric_object_id() {
    let algolia = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param("restrictSearchableAttributes", "url"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "hits": [
                { "objectID": "99<script>", "url": "https://example.com/x", "_tags": ["story"] },
                { "objectID": "99", "url": "https://example.com/x", "_tags": ["story"] }
            ]
        })))
        .mount(&algolia)
        .await;
    let client = algolia_client(&algolia);
    let pass = HackerNewsLinkPass {
        client: &client,
        config: &HackerNewsConfig::default(),
    };
    let article = Bookmark {
        tags: tag_list(&["find-hn"]),
        ..stored_at("https://example.com/x")
    };

    let planned = pass.plan(&article).await.unwrap().bookmark().unwrap();
    assert!(
        planned
            .note
            .contains("https://news.ycombinator.com/item?id=99"),
        "{}",
        planned.note
    );
    assert!(!planned.note.contains("script"), "{}", planned.note);
}

/// An article with no HN discussion is still an answer from Algolia, so the link pass
/// reports it as `Unchanged` (counted in `PassOutcome::reached`), not `Skipped`.
#[tokio::test]
async fn link_pass_counts_an_article_without_a_discussion_as_an_answer() {
    let algolia = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param("restrictSearchableAttributes", "url"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "hits": [] })))
        .expect(1)
        .mount(&algolia)
        .await;
    let client = algolia_client(&algolia);
    let pass = HackerNewsLinkPass {
        client: &client,
        config: &HackerNewsConfig::default(),
    };

    let article = Bookmark {
        tags: tag_list(&["find-hn"]),
        ..stored_at("https://example.com/x")
    };
    assert_eq!(pass.plan(&article).await.unwrap(), Plan::Unchanged);
}

#[tokio::test]
async fn link_discussions_keeps_entity_text_in_the_stored_title() {
    let algolia = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param("restrictSearchableAttributes", "url"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "hits": [{ "objectID": "99", "url": "https://example.com/x",
                       "_tags": ["story", "story_99"] }]
        })))
        .mount(&algolia)
        .await;
    let pinboard = FakePinboard {
        all: vec![Bookmark {
            title: "Use  &amp;amp; to escape".into(),
            tags: tag_list(&["find-hn"]),
            ..stored_at("https://example.com/x")
        }],
        ..Default::default()
    };
    let client = algolia_client(&algolia);
    let opts = HackerNewsCleanupOpts {
        link_discussions: true,
        dates: NO_DATING,
        config: HackerNewsConfig::default(),
    };
    run(&pinboard.cleanup_store(), &client, &opts)
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].description, "Use &amp;amp; to escape");
}

/// A cleanup client that reads items from `algolia`.
fn algolia_client(algolia: &MockServer) -> AlgoliaClient {
    AlgoliaClient::build(algolia.uri()).unwrap()
}
