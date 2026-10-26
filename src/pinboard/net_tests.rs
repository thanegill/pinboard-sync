use super::*;
use crate::test_support::{
    assert_quotes_capped_excerpt, capturing_logs, long_error_page, ERROR_PAGE_TAIL,
};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> PinboardClient {
    PinboardClient::with_base_url("user:tok".into(), server.uri()).unwrap()
}

const LEAK_SECRET: &str = "0A1B2C3D4E5F6A7B8C9D";

/// An error page that quotes the request back, token and all, the way a proxy's 400
/// or 414 page might.
fn echoing_error(status: u16, endpoint: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_string(format!(
        "<html>\n<h1>Bad Request</h1>\n<p>GET /{endpoint}?auth_token=user%3A{LEAK_SECRET}\
         &format=json</p>\n<p>token: user:{LEAK_SECRET}</p>\n</html>"
    ))
}

fn leak_client(server: &MockServer) -> PinboardClient {
    PinboardClient::with_base_url(format!("user:{LEAK_SECRET}"), server.uri()).unwrap()
}

/// Like `http::tests::assert_error_hides`, for an error that isn't a `reqwest::Error`:
/// no rendering carries the secret, and the message still names where it failed.
fn assert_error_hides_secret(err: &anyhow::Error, endpoint: &str, status: &str) {
    let renderings = [format!("{err}"), format!("{err:#}"), format!("{err:?}")]
        .into_iter()
        .chain(err.chain().map(|cause| format!("{cause:?}")));
    for rendering in renderings {
        assert!(
            !rendering.contains(LEAK_SECRET),
            "token leaked: {rendering}"
        );
        assert!(!rendering.contains('\n'), "not one line: {rendering}");
    }
    let message = err.to_string();
    assert!(message.contains(endpoint), "no endpoint: {message}");
    assert!(message.contains(status), "no status: {message}");
    assert!(message.contains("[redacted]"), "body dropped: {message}");
}

#[tokio::test]
async fn add_error_body_does_not_leak_the_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/add"))
        .respond_with(echoing_error(400, "posts/add"))
        .expect(1)
        .mount(&server)
        .await;

    let err = leak_client(&server)
        .add(&Bookmark {
            url: Url::parse("https://old.reddit.com/r/x/").unwrap(),
            title: "Title".into(),
            note: String::new(),
            tags: vec!["reddit".into()],
            timestamp: None,
            public: false,
            read_later: false,
        })
        .await
        .unwrap_err();
    assert_error_hides_secret(&err, "posts/add", "400 Bad Request");
}

#[tokio::test]
async fn posts_all_error_body_does_not_leak_the_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/all"))
        .respond_with(echoing_error(403, "posts/all"))
        .expect(1)
        .mount(&server)
        .await;

    let err = leak_client(&server).all().await.unwrap_err();
    assert_error_hides_secret(&err, "posts/all", "403 Forbidden");
}

#[tokio::test]
async fn debug_logged_body_is_whole_escaped_and_has_no_token() {
    let server = MockServer::start().await;
    let body = format!(
        "<p>GET /posts/all?auth_token=user%3A{LEAK_SECRET}</p>\n{}",
        long_error_page()
    );
    Mock::given(method("GET"))
        .and(path("/posts/all"))
        .respond_with(ResponseTemplate::new(404).set_body_string(body))
        .expect(1)
        .mount(&server)
        .await;

    let (result, logs) = capturing_logs(leak_client(&server).all()).await;
    assert!(result.is_err());
    for line in &logs {
        assert!(!line.contains(LEAK_SECRET), "token logged: {line}");
    }
    let body_lines: Vec<_> = logs
        .iter()
        .filter(|line| line.contains("full response body"))
        .collect();
    let [line] = body_lines.as_slice() else {
        panic!("expected one body line, got {logs:?}");
    };
    assert!(line.starts_with("DEBUG Pinboard "), "{line}");
    assert!(line.contains("auth_token=[redacted]"), "{line}");
    assert!(line.contains(ERROR_PAGE_TAIL), "body cut: {line}");
    assert!(!line.contains('\n'), "not escaped: {line}");
}

#[tokio::test]
async fn status_error_quotes_a_capped_one_line_excerpt() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/all"))
        .respond_with(ResponseTemplate::new(404).set_body_string(long_error_page()))
        .expect(1)
        .mount(&server)
        .await;

    let err = client(&server).all().await.unwrap_err();
    assert_quotes_capped_excerpt(
        &err.to_string(),
        &format!("Pinboard {}/posts/all returned 404 Not Found", server.uri()),
    );
}

#[tokio::test]
async fn add_succeeds_on_done() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/add"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"})))
        .expect(1)
        .mount(&server)
        .await;

    client(&server)
        .add(&Bookmark {
            url: Url::parse("https://old.reddit.com/r/x/").unwrap(),
            title: "Title".into(),
            note: String::new(),
            tags: vec!["reddit".into()],
            timestamp: None,
            public: false,
            read_later: false,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn add_retries_after_414_then_succeeds() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::{Request, Respond};

    // 414 on the first hit (URL too long), then `done` — the client should shrink
    // the notes and retry rather than failing. A stateful responder makes the
    // sequence deterministic (two ambiguous mocks don't reliably order).
    struct FailFirst(AtomicUsize);
    impl Respond for FailFirst {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(414).set_body_string("Request URI is too long")
            } else {
                ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"}))
            }
        }
    }

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/add"))
        .respond_with(FailFirst(AtomicUsize::new(0)))
        .expect(2) // exactly one retry after the 414
        .mount(&server)
        .await;

    client(&server)
        .add(&Bookmark {
            url: Url::parse("https://old.reddit.com/r/x/").unwrap(),
            title: "Title".into(),
            note: "long notes ".repeat(2000),
            tags: vec!["reddit".into()],
            timestamp: None,
            public: false,
            read_later: false,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn add_errors_on_non_done_result() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/add"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"result_code": format!("missing url for user:{LEAK_SECRET}")}),
            ),
        )
        .mount(&server)
        .await;

    let err = leak_client(&server)
        .add(&Bookmark {
            url: Url::parse("https://old.reddit.com/r/x/").unwrap(),
            title: "Title".into(),
            note: String::new(),
            tags: vec!["reddit".into()],
            timestamp: None,
            public: false,
            read_later: false,
        })
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Pinboard posts/add failed: missing url for [redacted]"
    );
}

#[tokio::test]
async fn all_parses_bookmarks() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/all"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "href": "https://old.reddit.com/r/rust/comments/a/x/", "description": "A",
              "extended": "", "tags": "reddit subreddit:rust", "time": "2020-01-01T00:00:00Z",
              "shared": "no", "toread": "no" },
            { "href": "https://example.com/", "description": "E", "extended": "", "tags": "",
              "time": "", "shared": "no", "toread": "no" }
        ])))
        .mount(&server)
        .await;

    let all = client(&server).all().await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(
        all[0].url.as_str(),
        "https://old.reddit.com/r/rust/comments/a/x/"
    );
    assert_eq!(all[0].tags, vec!["reddit", "subreddit:rust"]);
    // The ISO-8601 `time` is parsed into a timestamp on read.
    assert_eq!(all[0].timestamp, crate::timefmt::from_unix(1_577_836_800)); // 2020-01-01T00:00:00Z
    assert_eq!(all[1].timestamp, None); // empty time → no timestamp
}

#[tokio::test]
async fn all_skips_bookmark_with_unparseable_href() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/all"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "href": "not a url", "description": "bad", "extended": "", "tags": "",
              "time": "", "shared": "no", "toread": "no" },
            { "href": "https://example.com/", "description": "E", "extended": "", "tags": "",
              "time": "", "shared": "no", "toread": "no" }
        ])))
        .mount(&server)
        .await;

    // The unparseable href is skipped (logged); the valid bookmark still comes through.
    let all = client(&server).all().await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].url.as_str(), "https://example.com/");
}

#[tokio::test]
async fn all_keeps_bookmark_with_unparseable_time() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/all"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "href": "https://example.com/bad-time/", "description": "bad", "extended": "",
              "tags": "", "time": "not a date", "shared": "no", "toread": "no" },
            { "href": "https://example.com/", "description": "E", "extended": "", "tags": "",
              "time": "2020-01-01T00:00:00Z", "shared": "no", "toread": "no" }
        ])))
        .mount(&server)
        .await;

    // A bookmark whose (non-empty) time won't parse is kept with no timestamp, not
    // dropped: dropping it would evict its URL from sync's dedup set and make the next
    // `sync` re-add (and clobber) it. The valid bookmark still comes through too.
    let all = client(&server).all().await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].url.as_str(), "https://example.com/bad-time/");
    assert_eq!(all[0].timestamp, None);
    assert_eq!(all[1].url.as_str(), "https://example.com/");
    assert_eq!(all[1].timestamp, crate::timefmt::from_unix(1_577_836_800));
}

#[tokio::test]
async fn export_all_returns_body_verbatim() {
    let server = MockServer::start().await;
    // Raw body carrying `meta`/`hash` and an unparseable `href` — both of which the
    // parsed `all()` path drops. `export_all` must return the bytes untouched, so
    // assert against a raw string (not re-serialized JSON).
    let body = r#"[{"href":"not a url","description":"bad","meta":"abc","hash":"def"},
{"href":"https://example.com/","description":"E","meta":"123","hash":"456"}]"#;
    Mock::given(method("GET"))
        .and(path("/posts/all"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(1)
        .mount(&server)
        .await;

    let dumped = client(&server).export_all().await.unwrap();
    assert_eq!(dumped, body);
}

#[tokio::test]
async fn dump_hits_posts_all_exactly_once() {
    let server = MockServer::start().await;
    // One entry has an unparseable `href`: it stays in the raw body but is skipped by
    // the parsed half, which is the discrepancy the two halves exist to record.
    let body = r#"[{"href":"not a url","description":"bad","meta":"abc"},
{"href":"https://example.com/","description":"E","meta":"123","hash":"456"}]"#;
    Mock::given(method("GET"))
        .and(path("/posts/all"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        // `posts/all` is rate-limited to one call per five minutes — far longer than
        // the retry budget — so a second call in one run would 429 and fail the target.
        .expect(1)
        .mount(&server)
        .await;

    let dump = client(&server).dump().await.unwrap();

    assert_eq!(dump.raw.len(), 1);
    assert_eq!(dump.raw[0].body, body, "the raw half is byte-for-byte");
    let records = dump.records.expect("normalization succeeded");
    assert_eq!(records.len(), 1, "the unparseable href is skipped");
    assert_eq!(records[0].url, "https://example.com/");
}

#[tokio::test]
async fn delete_succeeds_on_done() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/delete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"})))
        .expect(1)
        .mount(&server)
        .await;

    client(&server)
        .delete(&Url::parse("https://old.reddit.com/r/x/").unwrap())
        .await
        .unwrap();
}
