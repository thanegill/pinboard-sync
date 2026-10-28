use super::algolia::{ALGOLIA_CHUNK_PAUSE, ITEM_BATCH};
use super::favorites::MAX_FAVORITES_PAGES;
use super::*;
use crate::backup::RawKind;
use serde_json::json;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn fetch_scrapes_favorites_then_reads_items() {
    let hn = MockServer::start().await;
    // Stories favorites page: one story id, no More link. (The `comments`-absent
    // matcher keeps this from also serving the comments-page request below.)
    Mock::given(method("GET"))
        .and(path("/favorites"))
        .and(query_param("id", "psophis"))
        .and(query_param_is_missing("comments"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<table><tr class="athing" id="42"><td>x</td></tr></table>"#),
        )
        .mount(&hn)
        .await;
    // Comments favorites page (comments=t): one comment id.
    Mock::given(method("GET"))
        .and(path("/favorites"))
        .and(query_param("comments", "t"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                r#"<table><tr class="athing comtr" id="9"><td>c</td></tr></table>"#,
            ),
        )
        .mount(&hn)
        .await;

    // Algolia returns both items (story + comment) in one batched search.
    let algolia = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "hits": [
                { "objectID": "42", "title": "Cool", "url": "https://example.com/x",
                  "author": "alice", "_tags": ["story", "author_alice", "story_42"] },
                { "objectID": "9", "comment_text": "hi", "author": "carol",
                  "_tags": ["comment", "author_carol", "story_8"] }
            ]
        })))
        .expect(1) // a single batched query, not one per item
        .mount(&algolia)
        .await;

    let client = hackernews_client(hn.uri(), algolia.uri());
    let drafts = crate::http::recording_waits(client.fetch())
        .await
        .0
        .unwrap();
    assert_eq!(drafts.len(), 2);
    assert_eq!(drafts[0].bookmark.url.as_str(), "https://example.com/x");
    assert_eq!(
        drafts[1].bookmark.url.as_str(),
        "https://news.ycombinator.com/item?id=9"
    );
    assert!(drafts[1]
        .bookmark
        .tags
        .contains(&"hackernews-comment".to_string()));
}

#[tokio::test]
async fn fetch_errors_when_favorites_markup_breaks() {
    // A recognizable page whose favorites structure is gone (no rows, no marker)
    // must fail loudly rather than silently importing nothing.
    let hn = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/favorites"))
        .and(query_param_is_missing("comments"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<html op="favorites"><body><table id="hnmain"></table></body></html>"#,
        ))
        .mount(&hn)
        .await;
    Mock::given(method("GET"))
        .and(path("/favorites"))
        .and(query_param("comments", "t"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<html op="news"><body><div>Site moved.</div></body></html>"#),
        )
        .mount(&hn)
        .await;

    let client = hackernews_client(hn.uri(), UNUSED.into());
    let err = crate::http::recording_waits(client.fetch())
        .await
        .0
        .unwrap_err();
    let SourceError::Other(err) = err else {
        panic!("expected Other, got {err:?}");
    };
    assert!(
        err.to_string()
            .contains("hn favorites-comments for 'psophis'"),
        "the error names the page whose markup broke: {err:#}"
    );
}

#[tokio::test]
async fn fetch_succeeds_on_genuinely_empty_favorites() {
    // Both favorites pages are empty but carry the marker: a recognized empty
    // list, so the fetch succeeds with no drafts (Algolia is never queried).
    let hn = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/favorites"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<html op="favorites"><body><table id="hnmain"></table></body></html>"#,
        ))
        .mount(&hn)
        .await;

    let client = hackernews_client(hn.uri(), UNUSED.into());
    let drafts = crate::http::recording_waits(client.fetch())
        .await
        .0
        .unwrap();
    assert!(drafts.is_empty());
}

/// A client reading favorites from `hn` and items from `algolia`, as `psophis`.
fn hackernews_client(hn: String, algolia: String) -> HackerNewsClient {
    HackerNewsClient::with_base_urls("psophis".into(), HackerNewsConfig::default(), hn, algolia)
}

/// A base URL for the half of a client a test never reaches.
const UNUSED: &str = "http://unused.invalid";

fn favorites_page(ids: &[u64], more: Option<&str>) -> ResponseTemplate {
    let rows: String = ids
        .iter()
        .map(|id| format!(r#"<tr class="athing" id="{id}"><td>x</td></tr>"#))
        .collect();
    let more = more
        .map(|href| format!(r#"<tr><td><a class="morelink" href="{href}">More</a></td></tr>"#))
        .unwrap_or_default();
    ResponseTemplate::new(200).set_body_string(format!(
        r#"<html op="favorites"><body><table>{rows}{more}</table></body></html>"#
    ))
}

/// Mounts two favorites streams of two pages each, expecting each page `hits` times:
/// stories 1, 2 then 3, comments 11 then 12, 13. Every item exists on Algolia; story 2
/// links to the article `https://example.com/two`, the rest are text posts.
async fn two_page_favorites(hn: &MockServer, algolia: &MockServer, hits: [u64; 4]) {
    let streams = [
        (
            false,
            [
                (&[1, 2][..], Some("favorites?id=psophis&amp;p=2")),
                (&[3][..], None),
            ],
        ),
        (
            true,
            [
                (
                    &[11][..],
                    Some("favorites?id=psophis&amp;comments=t&amp;p=2"),
                ),
                (&[12, 13][..], None),
            ],
        ),
    ];
    let mut hits = hits.into_iter();
    for (comments, pages) in streams {
        for (page, (ids, more)) in pages.into_iter().enumerate() {
            let stream = if comments {
                Mock::given(query_param("comments", "t"))
            } else {
                Mock::given(query_param_is_missing("comments"))
            };
            let numbered = match page {
                0 => stream.and(query_param_is_missing("p")),
                _ => stream.and(query_param("p", "2")),
            };
            numbered
                .and(method("GET"))
                .and(path("/favorites"))
                .respond_with(favorites_page(ids, more))
                .expect(hits.next().unwrap())
                .mount(hn)
                .await;
        }
    }
    let hits: Vec<_> = [1, 2, 3, 11, 12, 13]
        .iter()
        .map(|id| {
            let article = (*id == 2).then_some("https://example.com/two");
            json!({ "objectID": id.to_string(), "title": format!("Item {id}"),
                    "url": article, "author": "alice", "_tags": ["story"] })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "hits": hits })))
        .mount(algolia)
        .await;
}

#[tokio::test]
async fn incremental_fetch_stops_each_stream_at_a_synced_page() {
    let (hn, algolia) = (MockServer::start().await, MockServer::start().await);
    two_page_favorites(&hn, &algolia, [1, 0, 1, 1]).await;
    let client = hackernews_client(hn.uri(), algolia.uri());
    let existing: Vec<Bookmark> = [
        ("https://news.ycombinator.com/item?id=1", ""),
        ("https://example.com/two", ""),
        ("https://news.ycombinator.com/item?id=12", ""),
        ("https://news.ycombinator.com/item?id=13", ""),
    ]
    .into_iter()
    .map(|(url, note)| Bookmark {
        note: note.into(),
        ..crate::test_support::stored_at(url)
    })
    .collect();

    let drafts = crate::http::recording_waits(client.fetch_incremental(&existing))
        .await
        .0
        .unwrap();

    let fetched: Vec<String> = drafts.iter().map(|d| d.bookmark.url.to_string()).collect();
    let permalink = |id| format!("https://news.ycombinator.com/item?id={id}");
    assert_eq!(
        fetched,
        [
            permalink(1),
            "https://example.com/two".into(),
            permalink(11),
            permalink(12),
            permalink(13),
        ],
        "stories stop after page 1, whose article story was saved by hand; comments \
         page 1 has a new favorite, so page 2 is read"
    );
}

#[tokio::test]
async fn incremental_fetch_reads_every_page_when_nothing_is_synced() {
    let (hn, algolia) = (MockServer::start().await, MockServer::start().await);
    two_page_favorites(&hn, &algolia, [1, 1, 1, 1]).await;
    let client = hackernews_client(hn.uri(), algolia.uri());

    let (drafts, waits) = crate::http::recording_waits(client.fetch_incremental(&[])).await;

    assert_eq!(drafts.unwrap().len(), 6);
    assert_eq!(
        waits, [FAVORITES_PAGE_PAUSE; 3],
        "one pause before each of the three pages after the first, none after the last"
    );
}

#[tokio::test]
async fn backup_reads_every_favorites_page() {
    let (hn, algolia) = (MockServer::start().await, MockServer::start().await);
    two_page_favorites(&hn, &algolia, [1, 1, 1, 1]).await;
    let client = hackernews_client(hn.uri(), algolia.uri());

    let dump = crate::http::recording_waits(client.dump()).await.0.unwrap();

    let pages = |stream: &str| dump.raw.iter().filter(|p| p.stream == stream).count();
    assert_eq!(pages("favorites"), 2);
    assert_eq!(pages("favorites-comments"), 2);
    assert_eq!(pages(""), 1, "one batched Algolia lookup, not one per page");
}

#[tokio::test]
async fn a_favorites_429_without_retry_after_is_not_retried() {
    let hn = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/favorites"))
        .respond_with(ResponseTemplate::new(429))
        .expect(1)
        .mount(&hn)
        .await;

    let err = hackernews_client(hn.uri(), UNUSED.into())
        .fetch()
        .await
        .unwrap_err();

    let SourceError::RateLimited(message) = err else {
        panic!("expected RateLimited, got {err:?}");
    };
    assert!(message.contains("hn favorites"), "{message}");
    assert!(message.contains("429"), "{message}");
}

#[tokio::test]
async fn a_favorites_429_with_a_long_retry_after_is_not_retried() {
    let hn = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/favorites"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "3600"))
        .expect(1)
        .mount(&hn)
        .await;

    let err = hackernews_client(hn.uri(), UNUSED.into())
        .fetch()
        .await
        .unwrap_err();

    let SourceError::RateLimited(message) = err else {
        panic!("expected RateLimited, got {err:?}");
    };
    assert!(message.contains("Retry-After: 3600"), "{message}");
}

#[tokio::test]
async fn a_favorites_429_with_a_short_retry_after_is_retried_once() {
    let hn = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/favorites"))
        .and(query_param_is_missing("comments"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&hn)
        .await;
    Mock::given(method("GET"))
        .and(path("/favorites"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<html op="favorites"><body><table id="hnmain"></table></body></html>"#,
        ))
        .expect(2)
        .mount(&hn)
        .await;

    let (drafts, waits) =
        crate::http::recording_waits(hackernews_client(hn.uri(), UNUSED.into()).fetch()).await;

    assert!(drafts.unwrap().is_empty());
    assert_eq!(
        waits,
        [Duration::from_secs(30), FAVORITES_PAGE_PAUSE],
        "the retry waits out the Retry-After, not the linear backoff, then pauses once \
         before the comments page, and not after it"
    );
}

#[tokio::test]
async fn a_second_favorites_429_stops_the_fetch() {
    let hn = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/favorites"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .expect(2)
        .mount(&hn)
        .await;

    let (result, waits) =
        crate::http::recording_waits(hackernews_client(hn.uri(), UNUSED.into()).fetch()).await;

    let err = result.unwrap_err();
    assert!(matches!(err, SourceError::RateLimited(_)), "got {err:?}");
    assert_eq!(waits, [Duration::from_secs(1)]);
}

#[tokio::test]
async fn collect_ids_stops_at_the_page_cap() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::{Request, Respond};

    // Every page advertises a fresh "More" link, so only the page cap can stop
    // the walk. `recording_waits` skips the inter-page pauses.
    struct PagingFavorites {
        hits: Arc<AtomicUsize>,
    }
    impl Respond for PagingFavorites {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            let n = self.hits.fetch_add(1, Ordering::SeqCst);
            let body = format!(
                r#"<html op="favorites"><table>
                         <tr class="athing" id="{n}"><td>x</td></tr>
                         <tr><td><a class="morelink" href="favorites?id=u&amp;p={next}">More</a></td></tr>
                       </table></html>"#,
                next = n + 2,
            );
            ResponseTemplate::new(200).set_body_string(body)
        }
    }

    let hits = Arc::new(AtomicUsize::new(0));
    let hn = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/favorites"))
        .respond_with(PagingFavorites { hits: hits.clone() })
        .mount(&hn)
        .await;

    let client = HackerNewsClient::with_base_urls(
        "u".into(),
        HackerNewsConfig::default(),
        hn.uri(),
        UNUSED.into(),
    );
    let mut ids = Vec::new();
    crate::http::recording_waits(client.collect_ids(
        false,
        None,
        &mut ids,
        &mut HashMap::new(),
        &mut RawSink::disabled(),
    ))
    .await
    .0
    .unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), MAX_FAVORITES_PAGES);
    assert_eq!(ids.len(), MAX_FAVORITES_PAGES);
}

#[tokio::test]
async fn fetch_items_pauses_only_between_algolia_chunks() {
    let algolia = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "hits": [] })))
        .expect(2)
        .mount(&algolia)
        .await;
    let client = HackerNewsClient::with_base_urls(
        "u".into(),
        HackerNewsConfig::default(),
        UNUSED.into(),
        algolia.uri(),
    );
    let ids: Vec<HackerNewsItemId> = (0..=ITEM_BATCH as u64)
        .map(HackerNewsItemId::from)
        .collect();

    let (items, waits) =
        crate::http::recording_waits(client.fetch_items(&ids, &mut RawSink::disabled())).await;

    assert!(items.unwrap().is_empty());
    assert_eq!(waits, [ALGOLIA_CHUNK_PAUSE]);
}

#[tokio::test]
async fn dump_captures_both_scraped_pages_and_the_algolia_response() {
    let hn = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/favorites"))
        .and(query_param_is_missing("comments"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                r#"<table><tr class="athing" id="42"><td>story</td></tr></table>"#,
            ),
        )
        .mount(&hn)
        .await;
    Mock::given(method("GET"))
        .and(path("/favorites"))
        .and(query_param("comments", "t"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<table><tr class="athing comtr" id="9"><td>comment</td></tr></table>"#,
        ))
        .mount(&hn)
        .await;

    let algolia = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "hits": [
                { "objectID": "42", "title": "Cool", "url": "https://example.com/x",
                  "author": "alice", "_tags": ["story", "author_alice", "story_42"],
                  // `HackerNewsItem` keeps none of these.
                  "points": 271, "num_comments": 88 },
                { "objectID": "9", "comment_text": "hi", "author": "carol",
                  "_tags": ["comment", "author_carol", "story_8"] }
            ]
        })))
        .mount(&algolia)
        .await;

    let client = hackernews_client(hn.uri(), algolia.uri());
    let dump = crate::http::recording_waits(client.dump()).await.0.unwrap();

    // Three streams' worth of pages: two scraped HTML pages and one Algolia response.
    let by_stream = |s: &str| -> Vec<&crate::backup::RawPage> {
        dump.raw.iter().filter(|p| p.stream == s).collect()
    };
    assert_eq!(by_stream("favorites").len(), 1);
    assert_eq!(by_stream("favorites-comments").len(), 1);
    assert_eq!(by_stream("").len(), 1);
    assert!(by_stream("favorites")[0].body.contains(r#"id="42""#));
    assert_eq!(by_stream("favorites")[0].kind, RawKind::Html);

    // The Algolia fields the typed item drops are still in the captured body.
    let raw: serde_json::Value = serde_json::from_str(&by_stream("")[0].body).unwrap();
    assert_eq!(raw["hits"][0]["points"], 271);
    assert_eq!(raw["hits"][0]["num_comments"], 88);

    // Each stream becomes its own file; only the HTML ones are non-JSON.
    let files = crate::backup::layout("hackernews-psophis", &dump).unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "raw/hackernews-psophis.json",
            "raw/hackernews-psophis-favorites.html",
            "raw/hackernews-psophis-favorites-comments.html",
            "normalized/hackernews-psophis.json",
        ]
    );

    // End to end: fetch through the driver and land the snapshot on disk.
    let scratch = crate::test_support::scratch_dir("hn-backup-e2e");
    let dir = scratch.path();
    crate::http::recording_waits(crate::backup::run_job(
        &client,
        "hackernews-psophis",
        dir,
        false,
    ))
    .await
    .0
    .unwrap();

    let raw_json = std::fs::read_to_string(dir.join("raw/hackernews-psophis.json")).unwrap();
    let raw: serde_json::Value = serde_json::from_str(&raw_json).unwrap();
    assert_eq!(
        raw[0]["hits"][0]["points"], 271,
        "a dropped Algolia field survives all the way to disk"
    );

    let normalized =
        std::fs::read_to_string(dir.join("normalized/hackernews-psophis.json")).unwrap();
    let records: serde_json::Value = serde_json::from_str(&normalized).unwrap();
    assert_eq!(records[0]["title"], "Cool");
    assert!(records[0]["dedup_key"].is_string());

    let html = std::fs::read_to_string(dir.join("raw/hackernews-psophis-favorites.html")).unwrap();
    assert!(html.contains(r#"id="42""#));
}

async fn algolia_answering_with_a_long_error_page() -> (MockServer, HackerNewsClient) {
    let algolia = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .respond_with(
            ResponseTemplate::new(400).set_body_string(crate::test_support::long_error_page()),
        )
        .mount(&algolia)
        .await;
    let client = hackernews_client(UNUSED.into(), algolia.uri());
    (algolia, client)
}

#[tokio::test]
async fn algolia_item_error_quotes_a_capped_one_line_excerpt() {
    let (_algolia, client) = algolia_answering_with_a_long_error_page().await;
    let err = client
        .fetch_items(&[HackerNewsItemId::from(1)], &mut RawSink::disabled())
        .await
        .unwrap_err();
    crate::test_support::assert_quotes_capped_excerpt(
        &err.to_string(),
        "hn algolia returned 400 Bad Request",
    );
}

#[tokio::test]
async fn algolia_url_search_error_quotes_a_capped_one_line_excerpt() {
    let (_algolia, client) = algolia_answering_with_a_long_error_page().await;
    let err = client
        .search_by_url(&Url::parse("https://example.com/a").unwrap())
        .await
        .unwrap_err();
    crate::test_support::assert_quotes_capped_excerpt(
        &err.to_string(),
        "hn algolia url search returned 400 Bad Request",
    );
}

#[tokio::test]
async fn a_story_linking_github_is_not_re_added_after_cleanup_github_canonicalizes_it() {
    use crate::github::{GitHubClient, GitHubConfig};
    use crate::test_support::{replay, FakePinboard, NO_DATING};

    let github = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/Owner/Repo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "full_name": "Owner/Repo",
            "html_url": "https://github.com/Owner/Repo"
        })))
        .mount(&github)
        .await;

    let client = HackerNewsClient::new("u".into(), HackerNewsConfig::default()).unwrap();
    let drafts = [client.story_draft("https://www.github.com/Owner/Repo.git")];
    let synced = client.sync_once(&drafts, &[]).await;
    assert_eq!(synced.len(), 1);

    let pinboard = FakePinboard {
        all: synced.clone(),
        ..Default::default()
    };
    let github_client =
        GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), github.uri());
    crate::github::cleanup::run(
        &pinboard.cleanup_store(false),
        &github_client,
        NO_DATING,
        &[],
    )
    .await
    .unwrap();
    let cleaned = replay(&synced, &pinboard.ops.borrow());
    assert_eq!(cleaned[0].url.as_str(), "https://github.com/Owner/Repo");

    assert_eq!(client.sync_once(&drafts, &cleaned).await, cleaned);
}

#[tokio::test]
async fn a_favorites_page_cut_off_mid_body_is_a_read_error() {
    let hn = crate::test_support::truncated_body_server(
        "200 OK",
        "text/html",
        r#"<table><tr class="athing" id="42"><td>x</td></tr>"#,
    );
    let client = hackernews_client(hn, UNUSED.into());
    let err = crate::http::recording_waits(client.fetch())
        .await
        .0
        .unwrap_err();
    let SourceError::Other(error) = &err else {
        panic!("a cut-off body is a transient failure: {err:?}");
    };
    let message = format!("{error:#}");
    assert!(
        message.starts_with("reading hn favorites page 1: "),
        "{message}"
    );
}
