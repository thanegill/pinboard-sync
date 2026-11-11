use super::*;
use crate::test_support::{
    cleanup_writes_nothing, strings, synced_account, titles, ROUND_TRIP_DATINGS,
};
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn fetch_saved_sends_cookie_paginates_and_parses_metadata() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/psophis/saved.json"))
        .and(header("cookie", "reddit_session=secret"))
        .and(query_param_is_missing("after"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "kind": "Listing",
            "data": { "after": "t3_first", "children": [
                { "kind": "t3", "data": { "name": "t3_first", "subreddit": "rust",
                  "permalink": "/r/rust/comments/1/x/", "title": "1", "author": "alice",
                  "post_hint": "image" } }
            ] }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/user/psophis/saved.json"))
        .and(query_param("after", "t3_first"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "kind": "Listing",
            "data": { "after": null, "children": [
                { "kind": "t1", "data": { "name": "t1_second", "subreddit": "news",
                  "permalink": "/r/news/comments/2/y/b/", "link_title": "T", "body": "hi" } }
            ] }
        })))
        .mount(&server)
        .await;

    let client = RedditClient::with_base_url(
        Some("reddit_session=secret".into()),
        "psophis",
        server.uri(),
    );
    let items: Vec<_> = crate::http::recording_waits(client.fetch_saved())
        .await
        .0
        .unwrap()
        .into_iter()
        .filter_map(|e| e.into_saved_item("old.reddit.com"))
        .collect();
    assert_eq!(items.len(), 2);
    assert_eq!(
        items[0].tags(&RedditConfig::default()),
        vec![
            "reddit",
            "subreddit:rust",
            "author:reddit:alice",
            "type:image"
        ]
    );
    assert_eq!(items[1].kind, crate::reddit::model::RedditKind::Comment);
}

#[tokio::test]
async fn sends_a_bare_cookie_value_as_reddit_session() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/psophis/saved.json"))
        .and(header("cookie", "reddit_session=bare-value"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "kind": "Listing",
            "data": { "after": null, "children": [] }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = RedditClient::with_base_url(Some("bare-value\n".into()), "psophis", server.uri());
    let saved = client.fetch_saved().await.unwrap();
    assert!(saved.is_empty(), "{saved:?}");
}

/// Reddit's answer to a request without a session it accepts, as `curl` saw it for a
/// request with no cookie.
fn login_redirect(server: &MockServer) -> ResponseTemplate {
    ResponseTemplate::new(302).insert_header(
        "location",
        format!(
            "{}/login/?dest=%2Fuser%2Fpsophis%2Fsaved.json",
            server.uri()
        )
        .as_str(),
    )
}

/// A login page that answers 403, as a followed redirect can reach; the client must
/// never request it.
async fn mount_login_page(server: &MockServer) {
    Mock::given(path("/login/"))
        .respond_with(ResponseTemplate::new(403))
        .expect(0)
        .mount(server)
        .await;
}

/// Following the redirect would also end in `Halt::Reauth` (the login page's 403), so
/// the message is what tells the two apart.
fn assert_refused_at_login(err: &SourceError) {
    let SourceError::Halted(Halt::Reauth(message)) = err else {
        panic!("a login redirect must ask for a new cookie: {err:?}");
    };
    assert!(message.contains("login page"), "{message}");
    assert!(message.contains(TEST_COOKIE_SOURCE), "{message}");
    assert!(!message.contains("stale"), "{message}");
}

#[tokio::test]
async fn fetch_saved_maps_a_login_redirect_to_reauth_without_following_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/psophis/saved.json"))
        .respond_with(login_redirect(&server))
        .expect(1)
        .mount(&server)
        .await;
    mount_login_page(&server).await;

    let client =
        RedditClient::with_base_url(Some("reddit_session=stale".into()), "psophis", server.uri());
    let err = client.fetch_saved().await.unwrap_err();
    assert_refused_at_login(&err);
}

#[tokio::test]
async fn info_maps_a_login_redirect_to_reauth_without_following_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/info.json"))
        .respond_with(login_redirect(&server))
        .expect(1)
        .mount(&server)
        .await;
    mount_login_page(&server).await;

    let client = info_client(&server, "reddit_session=stale");
    let err = client.info(&strings(&["t3_a"])).await.unwrap_err();
    assert_refused_at_login(&err);
}

#[tokio::test]
async fn fetch_saved_terminates_when_after_cursor_loops() {
    // A listing returning the same `after` with a non-empty page must not loop forever.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/psophis/saved.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "kind": "Listing",
            "data": { "after": "t3_loop", "children": [
                { "kind": "t3", "data": { "name": "t3_loop", "subreddit": "rust",
                  "permalink": "/r/rust/comments/1/x/", "title": "1" } }
            ] }
        })))
        .mount(&server)
        .await;

    let client = reddit_client(&server);
    let (entries, waits) = crate::http::recording_waits(client.fetch_saved()).await;
    assert_eq!(entries.unwrap().len(), 2);
    assert_eq!(waits, [REQUEST_PAUSE]);
}

#[tokio::test]
async fn fetch_errors_when_a_non_empty_listing_yields_no_drafts() {
    // Every entry lacks the required subreddit and permalink; a non-empty listing with no
    // drafts must error, not silently import nothing.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/psophis/saved.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "kind": "Listing",
            "data": { "after": null, "children": [
                { "kind": "t3", "data": { "name": "t3_a", "title": "A" } },
                { "kind": "t1", "data": { "name": "t1_b", "link_title": "B" } }
            ] }
        })))
        .mount(&server)
        .await;

    let client = reddit_client(&server);
    let result = client.fetch().await;
    assert!(matches!(result, Err(SourceError::Other(_))), "{result:?}");
    let result = client.fetch_incremental(&[]).await;
    assert!(matches!(result, Err(SourceError::Other(_))), "{result:?}");
}

#[tokio::test]
async fn fetch_keeps_a_good_entry_and_drops_a_schema_broken_sibling() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/psophis/saved.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "kind": "Listing",
            "data": { "after": null, "children": [
                { "kind": "t3", "data": { "name": "t3_a", "subreddit": "rust",
                  "permalink": "/r/rust/comments/a/x/", "title": "A" } },
                { "kind": "t3", "data": { "name": "t3_b", "title": "B" } }
            ] }
        })))
        .mount(&server)
        .await;

    let client = reddit_client(&server);
    let drafts = client.fetch().await.unwrap();
    assert_eq!(drafts.len(), 1);
    assert_eq!(
        drafts[0].bookmark.url.as_str(),
        "https://old.reddit.com/r/rust/comments/a/x/"
    );
}

#[tokio::test]
async fn fetch_empty_listing_is_ok() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/psophis/saved.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "kind": "Listing",
            "data": { "after": null, "children": [] }
        })))
        .mount(&server)
        .await;

    let client = reddit_client(&server);
    let drafts = client.fetch().await.unwrap();
    assert!(drafts.is_empty(), "{drafts:?}");
}

#[tokio::test]
async fn fetch_saved_maps_403_to_reauth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403).set_body_string("blocked"))
        .mount(&server)
        .await;
    let client = reddit_client(&server);
    let result = client.fetch_saved().await;
    assert!(
        matches!(result, Err(SourceError::Halted(Halt::Reauth(_)))),
        "{result:?}"
    );
}

#[tokio::test]
async fn info_names_the_cookie_setting_in_a_403_but_never_the_cookie() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403).set_body_string("denied"))
        .mount(&server)
        .await;
    let client = info_client(&server, "reddit_session=SECRET403");
    let err = client.info(&strings(&["t3_a"])).await.unwrap_err();
    let SourceError::Halted(Halt::Reauth(message)) = &err else {
        panic!("a 403 must ask for a new cookie: {err:?}");
    };
    assert!(message.contains(TEST_COOKIE_SOURCE), "{message}");
    assert!(!message.contains("SECRET403"), "{message}");
}

/// The block as Reddit serves it, headers included.
fn block_page() -> ResponseTemplate {
    ResponseTemplate::new(403)
        .insert_header("server", "snooserv")
        .insert_header("retry-after", "0")
        .set_body_raw(super::tests::BLOCK_PAGE, "text/html; charset=utf-8")
}

#[tokio::test]
async fn fetch_saved_maps_a_block_page_to_blocked() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/psophis/saved.json"))
        .respond_with(block_page())
        .expect(1)
        .mount(&server)
        .await;
    let client = RedditClient::with_base_url(
        Some("reddit_session=secret".into()),
        "psophis",
        server.uri(),
    );
    let err = client.fetch_saved().await.unwrap_err();
    let SourceError::Halted(Halt::Blocked(message)) = err else {
        panic!("got {err:?}");
    };
    assert!(message.contains("pinboard-sync/issues/78."), "{message}");
}

#[tokio::test]
async fn info_answers_a_block_page_as_the_block() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/info.json"))
        .respond_with(block_page())
        .expect(1)
        .mount(&server)
        .await;
    let client = info_client(&server, "reddit_session=secret");
    let answer = client.info(&strings(&["t3_a"])).await.unwrap();
    assert!(answer.entries.is_empty(), "{answer:?}");
    let message = answer.blocked.unwrap();
    assert!(
        message.starts_with("api/info returned 403 Forbidden"),
        "{message}"
    );
    assert!(message.contains("pinboard-sync/issues/89."), "{message}");
}

#[tokio::test]
async fn info_keeps_the_batches_answered_before_a_block() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/info.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "kind": "Listing",
            "data": { "after": null, "children": [
                { "kind": "t3", "data": { "name": "t3_0", "subreddit": "rust",
                  "permalink": "/r/rust/comments/0/x/", "title": "A" } }
            ] }
        })))
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/info.json"))
        .respond_with(block_page())
        .expect(1)
        .mount(&server)
        .await;

    let client = info_client(&server, "reddit_session=secret");
    let fullnames: Vec<String> = (0..=2 * INFO_BATCH).map(|n| format!("t3_{n}")).collect();
    let (answer, waits) = crate::http::recording_waits(client.info(&fullnames)).await;
    let answer = answer.unwrap();
    assert_eq!(answer.entries.len(), 1, "{answer:?}");
    assert!(answer.blocked.is_some(), "{answer:?}");
    assert_eq!(waits, [REQUEST_PAUSE], "no batch is sent after the block");
}

#[tokio::test]
async fn info_sends_a_bare_cookie_value_as_reddit_session() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/info.json"))
        .and(header("cookie", "reddit_session=bare-value"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "kind": "Listing",
            "data": { "after": null, "children": [] }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = info_client(&server, "bare-value");
    let entries = client.info(&strings(&["t3_a"])).await.unwrap().entries;
    assert!(entries.is_empty(), "{entries:?}");
}

#[tokio::test]
async fn info_sends_cookie_and_returns_entries() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/info.json"))
        .and(query_param("id", "t3_a"))
        .and(header("cookie", "reddit_session=secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "kind": "Listing",
            "data": { "after": null, "children": [
                { "kind": "t3", "data": { "name": "t3_a", "subreddit": "rust",
                  "permalink": "/r/rust/comments/a/x/", "title": "A", "over_18": true } }
            ] }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = info_client(&server, "reddit_session=secret");
    let entries = client.info(&strings(&["t3_a"])).await.unwrap().entries;
    assert_eq!(entries.len(), 1);
    assert!(entries[0].fields.over_18);
}

#[tokio::test]
async fn info_pauses_only_between_batches() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/info.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "kind": "Listing",
            "data": { "after": null, "children": [] }
        })))
        .expect(3)
        .mount(&server)
        .await;

    let client = info_client(&server, "reddit_session=secret");
    let fullnames: Vec<String> = (0..250).map(|n| format!("t3_{n}")).collect();
    let (entries, waits) = crate::http::recording_waits(client.info(&fullnames)).await;
    let entries = entries.unwrap().entries;
    assert!(entries.is_empty(), "{entries:?}");
    assert_eq!(
        waits, [REQUEST_PAUSE; 2],
        "one pause between each pair of batches"
    );
}

/// Mounts a three-page saved listing, newest first, one post per page (`n`, `o`,
/// then `p`), expecting each page `hits` times.
async fn three_saved_pages(server: &MockServer, hits: [u64; 3]) {
    let pages = [
        (None, "n", Some("t3_n")),
        (Some("t3_n"), "o", Some("t3_o")),
        (Some("t3_o"), "p", None),
    ];
    for ((after, id, next), hits) in pages.into_iter().zip(hits) {
        let saved = Mock::given(method("GET")).and(path("/user/psophis/saved.json"));
        let saved = match after {
            None => saved.and(query_param_is_missing("after")),
            Some(after) => saved.and(query_param("after", after)),
        };
        saved
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "kind": "Listing",
                "data": { "after": next, "children": [
                    { "kind": "t3", "data": { "name": format!("t3_{id}"), "subreddit": "rust",
                      "permalink": format!("/r/rust/comments/{id}/x/"), "title": id } }
                ] }
            })))
            .expect(hits)
            .mount(server)
            .await;
    }
}

/// A client reading `psophis`'s saved listing from `server`, with no cookie.
fn reddit_client(server: &MockServer) -> RedditClient {
    RedditClient::with_base_url(None, "psophis", server.uri())
}

/// A client for `/api/info` lookups on `server`, sending `cookie`.
fn info_client(server: &MockServer, cookie: &str) -> RedditInfoClient {
    RedditInfoClient::with_base_url(Some(cookie.into()), Some("psophis"), server.uri())
}

/// Pinboard already holds the second page's save, so that page has nothing new.
fn second_page_synced() -> [Bookmark; 1] {
    [crate::test_support::stored_at(
        "https://www.reddit.com/r/rust/comments/o/x/",
    )]
}

#[tokio::test]
async fn incremental_fetch_stops_at_the_first_page_with_nothing_new() {
    let server = MockServer::start().await;
    three_saved_pages(&server, [1, 1, 0]).await;

    let client = reddit_client(&server);
    let (drafts, waits) =
        crate::http::recording_waits(client.fetch_incremental(&second_page_synced())).await;
    assert_eq!(titles(&drafts.unwrap()), ["n", "o"]);
    assert_eq!(waits, [REQUEST_PAUSE], "one pause, between the two pages");
}

#[tokio::test]
async fn incremental_fetch_reads_on_while_every_page_has_something_new() {
    let server = MockServer::start().await;
    three_saved_pages(&server, [1, 1, 1]).await;

    let client = reddit_client(&server);
    let (drafts, waits) = crate::http::recording_waits(client.fetch_incremental(&[])).await;
    assert_eq!(titles(&drafts.unwrap()), ["n", "o", "p"]);
    assert_eq!(
        waits, [REQUEST_PAUSE; 2],
        "a pause between pages, none after"
    );
}

#[tokio::test]
async fn the_page_cap_stops_without_a_pause_after_the_last_page() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/psophis/saved.json"))
        .respond_with(|request: &wiremock::Request| {
            let page = request
                .url
                .query_pairs()
                .find(|(key, _)| key == "after")
                .map_or(0, |(_, after)| after[3..].parse::<u32>().unwrap() + 1);
            ResponseTemplate::new(200).set_body_json(json!({
                "kind": "Listing",
                "data": { "after": format!("t3_{page}"), "children": [
                    { "kind": "t3", "data": { "name": format!("t3_{page}"),
                      "subreddit": "rust", "title": "t",
                      "permalink": format!("/r/rust/comments/{page}/x/") } }
                ] }
            }))
        })
        .expect(u64::from(MAX_SAVED_PAGES))
        .mount(&server)
        .await;

    let (entries, waits) = crate::http::recording_waits(reddit_client(&server).fetch_saved()).await;
    assert_eq!(entries.unwrap().len(), MAX_SAVED_PAGES as usize);
    assert_eq!(waits, vec![REQUEST_PAUSE; MAX_SAVED_PAGES as usize - 1]);
}

#[tokio::test]
async fn dump_reads_every_saved_page() {
    let server = MockServer::start().await;
    three_saved_pages(&server, [1, 1, 1]).await;

    let client = reddit_client(&server);
    let dump = crate::http::recording_waits(client.dump()).await.0.unwrap();
    assert_eq!(dump.raw.len(), 3);
}

#[tokio::test]
async fn saved_listing_error_quotes_a_capped_one_line_excerpt() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/psophis/saved.json"))
        .respond_with(
            ResponseTemplate::new(404).set_body_string(crate::test_support::long_error_page()),
        )
        .mount(&server)
        .await;

    let err = reddit_client(&server).fetch_saved().await.unwrap_err();
    crate::test_support::assert_quotes_capped_excerpt(
        &err.to_string(),
        "saved listing returned 404 Not Found",
    );
}

#[tokio::test]
async fn cleanup_looks_up_comments_by_their_own_fullname_in_the_post_batch() {
    use crate::reddit::cleanup::{run, RedditCleanupOpts};
    use crate::test_support::{stored_at, FakePinboard, NO_DATING};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/info.json"))
        .and(query_param("id", "t1_c,t3_a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "kind": "Listing",
            "data": { "after": null, "children": [
                { "kind": "t3", "data": { "name": "t3_a", "subreddit": "rust",
                  "permalink": "/r/rust/comments/a/x/", "title": "T", "author": "alice" } },
                { "kind": "t1", "data": { "name": "t1_c", "subreddit": "rust",
                  "permalink": "/r/rust/comments/a/x/c/", "author": "bob" } }
            ] }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let pinboard = FakePinboard {
        all: vec![
            stored_at("https://old.reddit.com/r/rust/comments/a/x/"),
            stored_at("https://old.reddit.com/r/rust/comments/a/x/c/"),
        ],
        ..Default::default()
    };
    let client = info_client(&server, "reddit_session=secret");
    let opts = RedditCleanupOpts {
        mark_nsfw: true,
        fix_titles: false,
        config: RedditConfig::default(),
        dates: NO_DATING,
        configured: Vec::new(),
    };

    run(&pinboard.cleanup_store(), Some(&client), &opts)
        .await
        .unwrap();

    let updated = pinboard.updated.borrow();
    let tags_of = |url: &str| {
        updated
            .iter()
            .find(|u| u.url == url)
            .map(|u| u.tags.clone())
            .unwrap()
    };
    let tags = tags_of("https://old.reddit.com/r/rust/comments/a/x/");
    assert!(
        tags.contains(&"author:reddit:alice".to_string()),
        "{tags:?}"
    );
    let tags = tags_of("https://old.reddit.com/r/rust/comments/a/x/c/");
    assert!(tags.contains(&"author:reddit:bob".to_string()), "{tags:?}");
}

fn truncated_client(
    status: &'static str,
    content_type: &'static str,
    body: &'static str,
) -> RedditClient {
    RedditClient::with_base_url(
        Some("reddit_session=secret".into()),
        "psophis",
        crate::test_support::truncated_body_server(status, content_type, body),
    )
}

#[tokio::test]
async fn a_saved_page_cut_off_mid_body_is_a_read_error() {
    let client = truncated_client("200 OK", "application/json", r#"{"kind": "Listing", "#);
    let err = crate::http::recording_waits(client.fetch_saved())
        .await
        .0
        .unwrap_err();
    let SourceError::Other(error) = &err else {
        panic!("a cut-off body is a transient failure: {err:?}");
    };
    let message = format!("{error:#}");
    assert!(
        message.starts_with("reading saved listing response: "),
        "{message}"
    );
    assert!(!message.contains("127.0.0.1"), "{message}");
}

#[tokio::test]
async fn a_403_cut_off_mid_body_does_not_ask_for_a_new_cookie() {
    // Without the whole page a block, whose cookie is fine, looks like a refused cookie.
    let client = truncated_client(
        "403 Forbidden",
        "text/html; charset=utf-8",
        "<html><body>You've been blocked by network security.",
    );
    let err = crate::http::recording_waits(client.fetch_saved())
        .await
        .0
        .unwrap_err();
    let SourceError::Other(error) = &err else {
        panic!("a cut-off 403 must not be read as a dead cookie: {err:?}");
    };
    let message = format!("{error:#}");
    assert!(
        message.starts_with("reading saved listing response (403 Forbidden): "),
        "{message}"
    );
}

/// Answers `/api/info.json` with the entries of its pool whose fullnames the request asks
/// for, as Reddit does, and with a 400 when it asks for one the pool lacks, so a test
/// can't pass on lookups that found nothing.
struct RedditInfoAnswers(Vec<serde_json::Value>);

impl wiremock::Respond for RedditInfoAnswers {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let asked: Vec<String> = request
            .url
            .query_pairs()
            .find(|(key, _)| key == "id")
            .map(|(_, ids)| ids.split(',').map(String::from).collect())
            .unwrap_or_default();
        let children: Vec<&serde_json::Value> = self
            .0
            .iter()
            .filter(|entry| {
                entry["data"]["name"]
                    .as_str()
                    .is_some_and(|name| asked.iter().any(|fullname| fullname == name))
            })
            .collect();
        if children.len() < asked.len() {
            return ResponseTemplate::new(400);
        }
        ResponseTemplate::new(200).set_body_json(json!({
            "kind": "Listing",
            "data": { "after": null, "children": children }
        }))
    }
}

/// `psophis`'s saves, newest first: an NSFW image post with a flair, a comment in an NSFW
/// thread, a self post, an empty self post and a video. Those made at `recent` fall within
/// the default 30-day dating cap, those at `stale` long before it.
fn round_trip_saves(recent: f64, stale: f64) -> Vec<serde_json::Value> {
    vec![
        json!({ "kind": "t3", "data": {
            "name": "t3_pic1", "subreddit": "AskReddit",
            "permalink": "/r/AskReddit/comments/pic1/a_picture/", "title": "A   picture",
            "url": "https://i.redd.it/pic1.png", "is_self": false, "over_18": true,
            "author": "alice", "link_flair_text": "Help, Question", "post_hint": "image",
            "created_utc": recent } }),
        json!({ "kind": "t1", "data": {
            "name": "t1_cmt1", "subreddit": "NEWS",
            "permalink": "/r/NEWS/comments/thread1/big_story/cmt1/",
            "link_title": "Big story",
            "link_permalink": "https://www.reddit.com/r/NEWS/comments/thread1/big_story/",
            "body": "I **agree**", "over_18": true, "author": "bob",
            "link_flair_text": "Politics", "created_utc": recent } }),
        json!({ "kind": "t3", "data": {
            "name": "t3_self1", "subreddit": "rust",
            "permalink": "/r/rust/comments/self1/why_rust/", "title": "Why Rust?",
            "url": "https://www.reddit.com/r/rust/comments/self1/why_rust/", "is_self": true,
            "selftext": "Because *speed*.", "author": "dave", "created_utc": stale } }),
        json!({ "kind": "t3", "data": {
            "name": "t3_empty1", "subreddit": "ExplainLikeImFive",
            "permalink": "/r/ExplainLikeImFive/comments/empty1/eli5_tides/",
            "title": "ELI5: tides",
            "url": "https://www.reddit.com/r/ExplainLikeImFive/comments/empty1/eli5_tides/",
            "is_self": true, "selftext": "", "author": "erin", "created_utc": recent } }),
        json!({ "kind": "t3", "data": {
            "name": "t3_vid1", "subreddit": "videos",
            "permalink": "/r/videos/comments/vid1/a_video/", "title": "A video",
            "url": "https://v.redd.it/vid1", "is_self": false, "is_video": true,
            "post_hint": "hosted:video", "author": "frank", "created_utc": stale } }),
    ]
}

/// The comment's thread, which `/api/info` knows though `psophis` never saved it, made at
/// `created`, before the comment, so dating the comment by its thread would show.
fn round_trip_thread(created: f64) -> serde_json::Value {
    json!({ "kind": "t3", "data": {
        "name": "t3_thread1", "subreddit": "NEWS",
        "permalink": "/r/NEWS/comments/thread1/big_story/", "title": "Big story",
        "url": "https://news.example/big", "is_self": false, "over_18": true,
        "author": "carol", "link_flair_text": "Politics", "created_utc": created } })
}

/// The defaults, and a config with another domain, an extra base tag, renamed NSFW, author
/// and media tags, no subreddit, comment or flair tags, and media tags on images only.
fn round_trip_configs() -> [RedditConfig; 2] {
    [
        RedditConfig::default(),
        RedditConfig {
            domain: "www.reddit.com".into(),
            tags: strings(&["reddit", "saved"]),
            subreddit_prefix: String::new(),
            comment: String::new(),
            nsfw: "adult".into(),
            author_prefix: "by:".into(),
            flair_prefix: String::new(),
            media_prefix: "media:".into(),
            media_types: strings(&["image"]),
        },
    ]
}

#[tokio::test]
async fn cleanup_leaves_what_sync_wrote_as_it_is() {
    Box::pin(assert_cleanup_leaves_what_sync_wrote(false)).await;
}

/// The saved listing stands in for a blocked `/api/info` well enough that cleanup still
/// leaves sync's bookmarks alone, the comment's included, though its thread isn't saved.
#[tokio::test]
async fn cleanup_leaves_what_sync_wrote_as_it_is_while_api_info_is_blocked() {
    Box::pin(assert_cleanup_leaves_what_sync_wrote(true)).await;
}

async fn assert_cleanup_leaves_what_sync_wrote(api_info_blocked: bool) {
    use crate::reddit::cleanup::RedditCleanupOpts;

    #[expect(
        clippy::cast_precision_loss,
        reason = "a timestamp is far below where f64 loses whole seconds"
    )]
    let now = time::OffsetDateTime::now_utc().unix_timestamp() as f64;
    let (recent, stale) = (now - 2.0 * 86_400.0, 1_600_000_000.0);
    let saves = round_trip_saves(recent, stale);
    let mut known = saves.clone();
    known.push(round_trip_thread(recent - 86_400.0));

    for config in round_trip_configs() {
        for dates in ROUND_TRIP_DATINGS {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/user/psophis/saved.json"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "kind": "Listing",
                    "data": { "after": null, "children": saves }
                })))
                .expect(if api_info_blocked { 2 } else { 1 })
                .mount(&server)
                .await;
            let info_mock = Mock::given(method("GET")).and(path("/api/info.json"));
            let info_mock = if api_info_blocked {
                info_mock.respond_with(block_page())
            } else {
                info_mock.respond_with(RedditInfoAnswers(known.clone()))
            };
            info_mock.expect(1).mount(&server).await;
            let mut client = RedditClient::with_base_url(
                Some("reddit_session=secret".into()),
                "psophis",
                server.uri(),
            );
            client.config = config.clone();
            let info = info_client(&server, "reddit_session=secret");

            let account = synced_account(&client, dates).await;
            assert_eq!(account.len(), saves.len(), "{} {dates:?}", config.domain);
            let opts = RedditCleanupOpts {
                mark_nsfw: true,
                fix_titles: true,
                config: config.clone(),
                dates,
                configured: vec![config.clone()],
            };
            let complained = Box::pin(cleanup_writes_nothing(account, async |store| {
                run_cleanup(store, &info, &opts).await.0
            }))
            .await;
            if api_info_blocked {
                let [fallback] = complained.as_slice() else {
                    panic!("{complained:#?}");
                };
                assert!(
                    fallback.contains(&format!(
                        "{} bookmark(s) took their metadata from it and 0 got URL-only",
                        saves.len()
                    )),
                    "{fallback}"
                );
            } else {
                assert!(complained.is_empty(), "{complained:#?}");
            }
        }
    }
}

/// `cleanup reddit` over `store`, its pauses recorded, boxed for its size.
async fn run_cleanup<S: crate::bookmark::BookmarkStore + crate::bookmark::AccountState>(
    store: &S,
    client: &RedditInfoClient,
    opts: &crate::reddit::cleanup::RedditCleanupOpts,
) -> (Result<(), SourceError>, Vec<Duration>) {
    crate::http::recording_waits(Box::pin(crate::reddit::cleanup::run(
        store,
        Some(client),
        opts,
    )))
    .await
}

/// Answers `/api/info.json` with Reddit's network-security block page `hits` times.
async fn mount_info_block(server: &MockServer, hits: u64) {
    Mock::given(method("GET"))
        .and(path("/api/info.json"))
        .respond_with(block_page())
        .expect(hits)
        .mount(server)
        .await;
}

/// Mounts `psophis`'s saved listing as one page of `children`, expecting `hits` reads.
async fn mount_saved(server: &MockServer, children: &[serde_json::Value], hits: u64) {
    Mock::given(method("GET"))
        .and(path("/user/psophis/saved.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "kind": "Listing",
            "data": { "after": null, "children": children }
        })))
        .expect(hits)
        .mount(server)
        .await;
}

/// A saved NSFW self post with a placeholder-titled bookmark, and an unrelated save.
fn fallback_saves() -> Vec<serde_json::Value> {
    vec![
        json!({ "kind": "t3", "data": {
            "name": "t3_abc", "subreddit": "NEWS", "permalink": "/r/NEWS/comments/abc/x/",
            "title": "Real Title", "url": "https://www.reddit.com/r/NEWS/comments/abc/x/",
            "is_self": true, "selftext": "Body text", "over_18": true, "author": "alice" } }),
        json!({ "kind": "t3", "data": {
            "name": "t3_other", "subreddit": "rust", "permalink": "/r/rust/comments/other/z/",
            "title": "Other" } }),
    ]
}

/// The bookmark [`fallback_saves`] covers, and one it doesn't, carrying a hand-written
/// note, visibility, to-read and a date that a URL-only rewrite must keep.
fn fallback_account() -> Vec<Bookmark> {
    use crate::test_support::stored_at;
    vec![
        Bookmark {
            title: "Reddit - Dive into anything".into(),
            tags: crate::tags::Tags::parse("reddit"),
            ..stored_at("https://www.reddit.com/r/NEWS/comments/abc/x/")
        },
        Bookmark {
            title: "Reddit - Dive into anything".into(),
            note: "my own note".into(),
            tags: crate::tags::Tags::parse("reddit mine"),
            timestamp: crate::timefmt::parse_rfc3339("2023-11-14T22:13:20Z"),
            public: true,
            read_later: true,
            ..stored_at("https://www.reddit.com/r/rust/comments/zzz/y/")
        },
    ]
}

fn fallback_opts() -> crate::reddit::cleanup::RedditCleanupOpts {
    crate::reddit::cleanup::RedditCleanupOpts {
        mark_nsfw: true,
        fix_titles: true,
        config: RedditConfig::default(),
        dates: crate::test_support::NO_DATING,
        configured: Vec::new(),
    }
}

const FALLBACK_COOKIE: &str = "reddit_session=FALLBACK-SECRET";

#[tokio::test]
async fn a_blocked_api_info_falls_back_to_the_saved_listing() {
    use crate::test_support::{capturing_logs, listing_entry, warnings, FakePinboard};

    let server = MockServer::start().await;
    mount_info_block(&server, 1).await;
    mount_saved(&server, &fallback_saves(), 1).await;
    let pinboard = FakePinboard {
        all: fallback_account(),
        ..Default::default()
    };
    let client = info_client(&server, FALLBACK_COOKIE);

    let ((result, waits), logs) = capturing_logs(run_cleanup(
        &pinboard.cleanup_store(),
        &client,
        &fallback_opts(),
    ))
    .await;

    result.unwrap();
    assert_eq!(
        waits,
        [REQUEST_PAUSE],
        "one pause, before the saved listing"
    );
    let updated = pinboard.updated.borrow();
    let written = |url: &str| updated.iter().find(|u| u.url == url).unwrap();

    let covered = written("https://old.reddit.com/r/NEWS/comments/abc/x/");
    assert_eq!(covered.description, "Real Title");
    let saved_note = listing_entry("t3", fallback_saves()[0]["data"].clone())
        .fields
        .post_note();
    assert_eq!(covered.extended, saved_note);
    for tag in ["nsfw", "author:reddit:alice", "subreddit:news", "reddit"] {
        assert!(
            covered.tags.contains(&tag.to_string()),
            "{:?}",
            covered.tags
        );
    }

    let url_only = written("https://old.reddit.com/r/rust/comments/zzz/y/");
    assert_eq!(url_only.description, "Reddit - Dive into anything");
    assert_eq!(url_only.extended, "my own note");
    assert!(url_only.shared && url_only.toread, "{url_only:?}");
    assert_eq!(url_only.dt, "2023-11-14T22:13:20Z");
    assert_eq!(
        url_only.tags,
        strings(&["mine", "reddit", "subreddit:rust"])
    );
    assert_eq!(updated.len(), 2, "{updated:#?}");

    let [fallback] = warnings(&logs)[..] else {
        panic!("{logs:#?}");
    };
    assert!(
        fallback.starts_with("WARN api/info returned 403 Forbidden"),
        "{fallback}"
    );
    assert!(
        fallback.contains("https://github.com/thanegill/pinboard-sync/issues/89"),
        "{fallback}"
    );
    assert!(
        fallback.ends_with(
            "The saved listing stood in for it: 1 bookmark(s) took their metadata from it \
             and 1 got URL-only normalization."
        ),
        "{fallback}"
    );
    for line in &logs {
        assert!(!line.contains("FALLBACK-SECRET"), "{line}");
    }
}

#[tokio::test]
async fn a_dry_run_previews_the_fallback_and_writes_nothing() {
    use crate::test_support::{capturing_stdout, FakePinboard};

    let server = MockServer::start().await;
    mount_info_block(&server, 1).await;
    mount_saved(&server, &fallback_saves(), 1).await;
    let pinboard = FakePinboard {
        all: fallback_account(),
        ..Default::default()
    };
    let client = info_client(&server, FALLBACK_COOKIE);

    let ((result, _), out) = capturing_stdout(run_cleanup(
        &pinboard.dry_cleanup_store(),
        &client,
        &fallback_opts(),
    ))
    .await;

    result.unwrap();
    assert!(pinboard.updated.borrow().is_empty());
    assert!(pinboard.deleted.borrow().is_empty());
    assert!(out.contains("title -> Real Title"), "{out}");
    assert!(
        out.contains("https://old.reddit.com/r/rust/comments/zzz/y/"),
        "{out}"
    );
    assert!(out.contains("2 bookmark(s) would change."), "{out}");
}

#[tokio::test]
async fn a_blocked_saved_listing_still_fails_the_pass() {
    use crate::test_support::{capturing_logs, warnings, FakePinboard};

    let server = MockServer::start().await;
    mount_info_block(&server, 1).await;
    Mock::given(method("GET"))
        .and(path("/user/psophis/saved.json"))
        .respond_with(block_page())
        .expect(1)
        .mount(&server)
        .await;
    let pinboard = FakePinboard {
        all: fallback_account(),
        ..Default::default()
    };
    let client = info_client(&server, FALLBACK_COOKIE);

    let ((result, _), logs) = capturing_logs(run_cleanup(
        &pinboard.cleanup_store(),
        &client,
        &fallback_opts(),
    ))
    .await;

    let err = result.unwrap_err();
    let SourceError::Halted(Halt::Blocked(message)) = &err else {
        panic!("got {err:?}");
    };
    assert!(
        message.starts_with("saved listing returned 403"),
        "{message}"
    );
    assert!(pinboard.updated.borrow().is_empty());
    let [warning] = warnings(&logs)[..] else {
        panic!("{logs:#?}");
    };
    assert!(
        warning.ends_with("Reading the saved listing in its place failed too."),
        "{warning}"
    );
}

#[tokio::test]
async fn a_block_on_a_later_batch_keeps_the_answers_before_it() {
    use crate::test_support::{capturing_logs, stored_at, warnings, FakePinboard};

    let id = |n: usize| format!("p{n:03}");
    let post = |n: usize, title: &str| {
        json!({ "kind": "t3", "data": {
            "name": format!("t3_{}", id(n)), "subreddit": "rust",
            "permalink": format!("/r/rust/comments/{}/x/", id(n)), "title": title } })
    };
    let server = MockServer::start().await;
    let known: Vec<serde_json::Value> = (0..=INFO_BATCH).map(|n| post(n, "From info")).collect();
    Mock::given(method("GET"))
        .and(path("/api/info.json"))
        .respond_with(RedditInfoAnswers(known))
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    mount_info_block(&server, 1).await;
    mount_saved(
        &server,
        &[post(0, "From saved"), post(INFO_BATCH, "From saved")],
        1,
    )
    .await;
    let pinboard = FakePinboard {
        all: (0..=INFO_BATCH)
            .map(|n| Bookmark {
                title: "Reddit - Dive into anything".into(),
                ..stored_at(&format!(
                    "https://old.reddit.com/r/rust/comments/{}/x/",
                    id(n)
                ))
            })
            .collect(),
        ..Default::default()
    };
    let client = info_client(&server, FALLBACK_COOKIE);
    let opts = crate::reddit::cleanup::RedditCleanupOpts {
        mark_nsfw: false,
        ..fallback_opts()
    };

    let ((result, _), logs) =
        capturing_logs(run_cleanup(&pinboard.cleanup_store(), &client, &opts)).await;

    result.unwrap();
    let updated = pinboard.updated.borrow();
    let title_of = |n: usize| {
        let url = format!("https://old.reddit.com/r/rust/comments/{}/x/", id(n));
        updated
            .iter()
            .find(|u| u.url == url)
            .map(|u| u.description.clone())
    };
    assert_eq!(title_of(0).as_deref(), Some("From info"));
    assert_eq!(title_of(INFO_BATCH).as_deref(), Some("From saved"));
    let [fallback] = warnings(&logs)[..] else {
        panic!("{logs:#?}");
    };
    assert!(
        fallback.ends_with(&format!(
            "1 bookmark(s) took their metadata from it and 0 got URL-only normalization, \
             after /api/info answered for {INFO_BATCH}."
        )),
        "{fallback}"
    );
}

#[tokio::test]
async fn a_refused_cookie_at_api_info_does_not_fall_back() {
    use crate::test_support::FakePinboard;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/info.json"))
        .respond_with(ResponseTemplate::new(403).set_body_string("denied"))
        .expect(1)
        .mount(&server)
        .await;
    mount_saved(&server, &fallback_saves(), 0).await;
    let pinboard = FakePinboard {
        all: fallback_account(),
        ..Default::default()
    };
    let client = info_client(&server, FALLBACK_COOKIE);

    let err = run_cleanup(&pinboard.cleanup_store(), &client, &fallback_opts())
        .await
        .0
        .unwrap_err();

    assert!(
        matches!(err, SourceError::Halted(Halt::Reauth(_))),
        "got {err:?}"
    );
    assert!(pinboard.updated.borrow().is_empty());
}
