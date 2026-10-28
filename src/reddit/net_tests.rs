use super::*;
use crate::test_support::{
    assert_cleanup_changes_nothing, strings, synced_account, titles, ROUND_TRIP_DATINGS,
};
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn fetch_saved_sends_cookie_paginates_and_parses_metadata() {
    let server = MockServer::start().await;
    // Page 1 (no `after`) returns a cursor and a richly-tagged post.
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
    // Page 2 (after=t3_first) is the last.
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
fn assert_refused_at_login(err: SourceError) {
    let SourceError::Halted(Halt::Reauth(message)) = &err else {
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
    assert_refused_at_login(err);
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
    assert_refused_at_login(err);
}

#[tokio::test]
async fn fetch_saved_terminates_when_after_cursor_loops() {
    // A pathological listing that keeps returning the same non-null `after` with a
    // non-empty page would loop forever without the visited-cursor guard. It must
    // stop after re-seeing the cursor rather than hanging / growing `out` unbounded.
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
    // Page 1 (no cursor) records the cursor; page 2 (after=t3_loop) re-sees it and
    // breaks, so exactly two pages are fetched.
    let (entries, waits) = crate::http::recording_waits(client.fetch_saved()).await;
    assert_eq!(entries.unwrap().len(), 2);
    assert_eq!(waits, [REQUEST_PAUSE]);
}

#[tokio::test]
async fn fetch_errors_when_a_non_empty_listing_yields_no_drafts() {
    // Every entry deserializes (all fields are optional) but lacks the required
    // subreddit/permalink, so all are dropped. A non-empty listing that produces
    // zero drafts must error rather than silently import nothing.
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
}

#[tokio::test]
async fn fetch_keeps_a_good_entry_and_drops_a_schema_broken_sibling() {
    // One entry is well-formed, the other is missing subreddit/permalink. The good
    // one survives (and the bad one is warned + dropped), so fetch succeeds.
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
    // A genuinely empty saved listing (nothing saved) is a success, not an error.
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
    assert!(
        matches!(err, SourceError::Halted(Halt::Blocked(_))),
        "got {err:?}"
    );
}

#[tokio::test]
async fn info_maps_a_block_page_to_blocked() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/info.json"))
        .respond_with(block_page())
        .expect(1)
        .mount(&server)
        .await;
    let client = info_client(&server, "reddit_session=secret");
    let err = client.info(&strings(&["t3_a"])).await.unwrap_err();
    assert!(
        matches!(err, SourceError::Halted(Halt::Blocked(_))),
        "got {err:?}"
    );
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
    let entries = client.info(&strings(&["t3_a"])).await.unwrap();
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
    let entries = client.info(&strings(&["t3_a"])).await.unwrap();
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
    let entries = entries.unwrap();
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
    RedditInfoClient::with_base_url(Some(cookie.into()), server.uri())
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
    // Without the whole page there is no telling the block (whose cookie is fine) from a
    // refused cookie, so the hook must not send the operator after the cookie.
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
    use crate::reddit::cleanup::{run, RedditCleanupOpts};

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
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/info.json"))
                .respond_with(RedditInfoAnswers(known.clone()))
                .expect(1)
                .mount(&server)
                .await;
            let mut client = RedditClient::with_base_url(
                Some("reddit_session=secret".into()),
                "psophis",
                server.uri(),
            );
            client.config = config.clone();
            let info =
                RedditInfoClient::with_base_url(Some("reddit_session=secret".into()), server.uri());

            let account = synced_account(&client, dates).await;
            assert_eq!(account.len(), saves.len(), "{} {dates:?}", config.domain);
            let opts = RedditCleanupOpts {
                mark_nsfw: true,
                fix_titles: true,
                config: config.clone(),
                dates,
                configured: vec![config.clone()],
            };
            assert_cleanup_changes_nothing(account, async |store| {
                run(store, Some(&info), &opts).await
            })
            .await;
        }
    }
}
