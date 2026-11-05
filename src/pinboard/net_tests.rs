use super::*;
use crate::test_support::{
    assert_quotes_capped_excerpt, capturing_logs, long_error_page, stored_at, tag_list,
    ERROR_PAGE_TAIL, SHORT_READ_TIMEOUT,
};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The reddit post the write tests add.
fn post() -> Bookmark {
    Bookmark {
        title: "Title".into(),
        tags: tag_list(&["reddit"]),
        ..stored_at("https://old.reddit.com/r/x/")
    }
}

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

    let err = leak_client(&server).add(&post()).await.unwrap_err();
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
    result.unwrap_err();
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

    client(&server).add(&post()).await.unwrap();
}

/// `posts/add` names the fields after delicious: the title is `description` and the
/// notes are `extended`. Every write replaces the record at its URL, and the two flags
/// go out as `yes`/`no`.
#[tokio::test]
async fn add_sends_each_field_under_its_pinboard_name() {
    let server = MockServer::start().await;
    let done = || ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"}));
    let common = |url: &str| {
        Mock::given(method("GET"))
            .and(path("/posts/add"))
            .and(query_param("url", url))
            .and(query_param("description", "Title"))
            .and(query_param("extended", "Some notes"))
            .and(query_param("tags", "reddit subreddit:rust"))
            .and(query_param("replace", "yes"))
            .and(query_param("auth_token", "user:tok"))
            .and(query_param("format", "json"))
    };
    common("https://example.com/public")
        .and(query_param("shared", "yes"))
        .and(query_param("toread", "no"))
        .respond_with(done())
        .expect(1)
        .mount(&server)
        .await;
    common("https://example.com/read-later")
        .and(query_param("shared", "no"))
        .and(query_param("toread", "yes"))
        .respond_with(done())
        .expect(1)
        .mount(&server)
        .await;

    let bookmark = |url: &str, public: bool, read_later: bool| Bookmark {
        title: "Title".into(),
        note: "Some notes".into(),
        tags: tag_list(&["reddit", "subreddit:rust"]),
        public,
        read_later,
        ..stored_at(url)
    };
    let client = client(&server);
    client
        .add(&bookmark("https://example.com/public", true, false))
        .await
        .unwrap();
    let stored = client
        .update(&bookmark("https://example.com/read-later", false, true))
        .await
        .unwrap();
    assert_eq!(stored, Update::Written);
}

/// The tag set, as cleanup's `diff` compares it and `fit` measures it.
#[tokio::test]
async fn add_sends_each_tag_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/add"))
        .and(query_param("tags", "reddit rust"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"})))
        .expect(1)
        .mount(&server)
        .await;

    client(&server)
        .add(&Bookmark {
            tags: tag_list(&["reddit", "rust", "reddit"]),
            ..post()
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn add_sends_dt_in_utc_at_whole_seconds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/add"))
        .and(query_param("dt", "2010-12-11T19:48:02Z"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"})))
        .expect(1)
        .mount(&server)
        .await;

    let instant = crate::timefmt::from_unix(1_292_096_882).unwrap()
        + time::Duration::nanoseconds(661_420_000);
    let offset = time::UtcOffset::from_hms(1, 0, 0).unwrap();
    client(&server)
        .add(&Bookmark {
            timestamp: Some(instant.to_offset(offset)),
            ..post()
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn add_retries_after_414_then_succeeds() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::{Request, Respond};

    // 414 on the first hit (URL too long), then `done` -- the client should shrink
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

    let long = Bookmark {
        note: "long notes ".repeat(2000),
        ..post()
    };
    let (result, logs) = capturing_logs(client(&server).add(&long)).await;
    result.unwrap();

    let sent = sent_notes(&server).await;
    assert_eq!(sent.len(), 2);
    assert!(
        sent[1].len() < sent[0].len(),
        "the retry must send a shorter note: {} then {} bytes",
        sent[0].len(),
        sent[1].len()
    );
    assert!(sent[1].ends_with(TRUNCATION_MARKER), "{}", sent[1]);
    assert_eq!(
        crate::test_support::warnings(&logs),
        [format!(
            "WARN note trimmed to fit the request URL for https://old.reddit.com/r/x/: {} \
             bytes -> {}",
            long.note.len(),
            sent[1].len()
        )]
    );
}

#[tokio::test]
async fn add_sends_every_tag_and_cuts_the_note_to_make_room() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/add"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"})))
        .mount(&server)
        .await;
    let tagged = Bookmark {
        note: "long notes ".repeat(2000),
        tags: (0..100).map(|n| format!("tag-number-{n:03}")).collect(),
        timestamp: crate::timefmt::from_unix(1_700_000_000),
        ..post()
    };

    client(&server).add(&tagged).await.unwrap();

    let requests = server
        .received_requests()
        .await
        .expect("recording requests");
    let [request] = requests.as_slice() else {
        panic!("one request, got {}", requests.len());
    };
    let param = |key: &str| {
        request
            .url
            .query_pairs()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.into_owned())
    };
    assert_eq!(param("tags"), Some(tagged.tags.joined()));
    assert!(param("dt").is_some(), "the write is dated");
    let sent_note = param("extended").expect("posts/add sends extended");
    assert!(sent_note.ends_with(TRUNCATION_MARKER), "{sent_note}");
    assert!(request.url.as_str().len() <= MAX_URL_BYTES);
}

#[tokio::test]
async fn update_refuses_a_414_without_retrying_and_add_still_halves() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/add"))
        .respond_with(|request: &wiremock::Request| {
            if request.url.as_str().len() > MAX_URL_BYTES * 3 / 4 {
                ResponseTemplate::new(414)
            } else {
                ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"}))
            }
        })
        .mount(&server)
        .await;
    let client = client(&server);
    let long = client.fit(Bookmark {
        note: "long notes ".repeat(2000),
        ..stored_at("https://example.com/long")
    });

    assert_eq!(client.update(&long).await.unwrap(), Update::TooLong);
    assert_eq!(sent_notes(&server).await.len(), 1, "no retry");

    client.add(&long).await.unwrap();
    let sent = sent_notes(&server).await;
    assert_eq!(sent.len(), 3, "a 414, then the halved write");
    assert!(sent[2].len() < long.note.len());
}

/// The `extended` value of every request `server` received, in order.
async fn sent_notes(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .expect("recording requests")
        .iter()
        .map(|request| {
            request
                .url
                .query_pairs()
                .find(|(key, _)| key == "extended")
                .map(|(_, value)| value.into_owned())
                .expect("posts/add sends extended")
        })
        .collect()
}

#[tokio::test]
async fn add_stops_halving_the_budget_at_its_floor() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/add"))
        .respond_with(ResponseTemplate::new(414))
        .mount(&server)
        .await;
    let pinboard = PinboardClient::build(
        "user:tok".into(),
        RATE_LIMIT,
        server.uri(),
        client_builder(),
    )
    .unwrap();

    let (result, waits) = crate::http::recording_waits(pinboard.add(&Bookmark {
        note: "long notes ".repeat(2000),
        ..post()
    }))
    .await;

    let err = result.unwrap_err();
    assert!(err.to_string().contains("414"), "{err:#}");
    let sent = sent_lengths(&server).await;
    let budgets = [8100, 4050, 2025, 1012, 506];
    assert_eq!(sent.len(), budgets.len(), "{sent:?}");
    for (sent, budget) in sent.iter().zip(budgets) {
        assert!(*sent <= budget, "{sent} bytes over {budget}");
    }
    assert_eq!(
        waits, [RATE_LIMIT; 4],
        "each retry waits out the rate limit, as any write after the first does"
    );
}

/// The date the dated bookmarks below carry, so the URL sent is the one `fit` measures.
fn dated() -> Option<time::OffsetDateTime> {
    crate::timefmt::from_unix(1_700_000_000)
}

/// A bookmark dated `timestamp` whose `posts/add` URL with no note is exactly `len` bytes,
/// with a note far too long to send whole.
fn sent_at(
    client: &PinboardClient,
    len: usize,
    timestamp: Option<time::OffsetDateTime>,
) -> Bookmark {
    let mut b = Bookmark {
        note: "long notes ".repeat(2000),
        timestamp,
        ..post()
    };
    b.title = "t".repeat(len - client.sent_without_note(&b) + b.title.len());
    assert_eq!(client.sent_without_note(&b), len);
    b
}

/// Every `posts/add` URL `server` received, in order.
async fn sent_urls(server: &MockServer) -> Vec<Url> {
    server
        .received_requests()
        .await
        .expect("recording requests")
        .into_iter()
        .map(|request| request.url)
        .collect()
}

/// The length of every URL `server` received, as the client sent it: wiremock rebuilds
/// each one on `http://localhost`, without the port the client sent to.
async fn sent_lengths(server: &MockServer) -> Vec<usize> {
    sent_urls(server)
        .await
        .iter()
        .map(|url| server.uri().len() + url[url::Position::BeforePath..].len())
        .collect()
}

#[tokio::test]
async fn add_sends_nothing_when_even_no_note_is_over_pinboards_limit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/add"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"})))
        .mount(&server)
        .await;
    let pinboard = PinboardClient::build(
        format!("user:{LEAK_SECRET}"),
        RATE_LIMIT,
        server.uri(),
        client_builder(),
    )
    .unwrap();
    let over = sent_at(&pinboard, PINBOARD_URL_LIMIT + 1, None);

    let ((refused, written), waits) = crate::http::recording_waits(async {
        (pinboard.add(&over).await, pinboard.add(&post()).await)
    })
    .await;

    written.unwrap();
    assert_eq!(
        waits,
        Vec::<Duration>::new(),
        "the next write is the first sent, so it isn't paced"
    );
    let sent = sent_urls(&server).await;
    let [request] = sent.as_slice() else {
        panic!("only the write that fits is sent, got {sent:?}");
    };
    assert!(
        request
            .query_pairs()
            .any(|(key, value)| key == "url" && value == post().url.as_str()),
        "{request}"
    );

    let err = refused.unwrap_err();
    assert_eq!(
        err.to_string(),
        "the posts/add URL without the note is 8202 bytes, over Pinboard's 8201-byte limit, \
         so cutting the note can't fit it"
    );
    for rendering in [format!("{err:#}"), format!("{err:?}")] {
        for leak in [LEAK_SECRET, "auth_token", &server.uri()] {
            assert!(!rendering.contains(leak), "{leak} in {rendering}");
        }
    }
}

#[tokio::test]
async fn update_refuses_without_sending_when_even_no_note_is_over_pinboards_limit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/add"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"})))
        .expect(0)
        .mount(&server)
        .await;
    let client = client(&server);
    let over = sent_at(&client, PINBOARD_URL_LIMIT + 1, dated());

    assert_eq!(client.update(&over).await.unwrap(), Update::TooLong);
}

/// A write whose URL with no note is exactly Pinboard's limit goes out with no note, dated
/// or not: an undated one measures 28 bytes over the limit with the `dt` `fit` counts, yet
/// is sent, since what goes on the wire fits.
#[tokio::test]
async fn a_write_with_room_for_no_note_at_all_is_sent_without_one() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/add"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"})))
        .mount(&server)
        .await;
    let client = client(&server);
    let dated_at_limit = sent_at(&client, PINBOARD_URL_LIMIT, dated());
    let undated_at_limit = sent_at(&client, PINBOARD_URL_LIMIT, None);

    client.add(&undated_at_limit).await.unwrap();
    client.add(&dated_at_limit).await.unwrap();
    assert_eq!(
        client.update(&dated_at_limit).await.unwrap(),
        Update::Written
    );

    assert_eq!(sent_notes(&server).await, ["", "", ""]);
    assert_eq!(sent_lengths(&server).await, [PINBOARD_URL_LIMIT; 3]);
    assert_eq!(client.fit(dated_at_limit).note, "");
}

#[tokio::test]
async fn add_stops_halving_once_the_fields_besides_the_note_are_over_the_next_budget() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/add"))
        .respond_with(ResponseTemplate::new(414))
        .mount(&server)
        .await;
    let client = client(&server);

    let err = client
        .add(&sent_at(&client, 4050, dated()))
        .await
        .unwrap_err();

    assert_eq!(
        format!("{err:#}"),
        "Pinboard answered 414 at a 4050-byte budget: the posts/add fields other than the \
         note measure 4050 bytes, over the next 2025-byte budget, so a smaller budget can't \
         shorten the URL"
    );
    assert_eq!(
        sent_notes(&server).await.len(),
        2,
        "8100, then 4050, which the fields fill exactly"
    );
}

/// A bookmark whose fields leave no room under the budget but some under Pinboard's limit:
/// written with what fits there, stored as `fit` predicts, and left alone by the next
/// cleanup run. One over the limit is refused before anything is sent.
mod between_the_budget_and_the_limit {
    use super::*;
    use crate::bookmark::{AccountView, CleanupStore, WriteMode};
    use crate::cleanup_pass::{run_pass, CleanupPass, PassOutcome, Plan};
    use crate::source::SourceError;
    use crate::test_support::NO_DATING;

    /// Plans the bookmark at `0`'s URL as `0`, and leaves every other one unchanged.
    struct Planned(Bookmark);

    impl CleanupPass for Planned {
        async fn plan(&self, b: &Bookmark) -> Result<Plan, SourceError> {
            Ok(if b.url == self.0.url {
                Plan::Bookmark(self.0.clone())
            } else {
                Plan::Unchanged
            })
        }
    }

    async fn cleanup(
        client: &PinboardClient,
        stored: &Bookmark,
        planned: &Bookmark,
        mode: WriteMode,
    ) -> PassOutcome {
        let store = CleanupStore::new(client, AccountView::new(vec![stored.clone()]), mode);
        let pass = Planned(planned.clone());
        let slice = std::slice::from_ref(stored);
        crate::http::recording_waits(run_pass(&store, slice, "test", NO_DATING, &pass))
            .await
            .0
    }

    #[tokio::test]
    async fn a_note_is_cut_to_the_limit_as_fit_predicts_and_then_left_alone() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/posts/add"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"})))
            .mount(&server)
            .await;
        let client = client(&server);
        let synced = sent_at(&client, MAX_URL_BYTES + 40, dated());

        client.add(&synced).await.unwrap();
        let stored = Bookmark {
            note: sent_notes(&server).await.remove(0),
            ..synced.clone()
        };
        assert_eq!(stored, client.fit(synced.clone()));
        assert!(stored.note.ends_with(TRUNCATION_MARKER), "{}", stored.note);
        let sent_len = sent_lengths(&server).await[0];
        assert!(
            sent_len > MAX_URL_BYTES && sent_len <= PINBOARD_URL_LIMIT,
            "{sent_len}"
        );

        let retagged = Bookmark {
            tags: tag_list(&["reddit", "retagged"]),
            ..synced
        };
        let first = cleanup(&client, &stored, &retagged, WriteMode::Live).await;
        assert_eq!((first.changed, first.refused), (1, 0));
        let rewritten = Bookmark {
            note: sent_notes(&server).await.remove(1),
            ..retagged.clone()
        };
        assert_eq!(rewritten, client.fit(retagged.clone()));

        let second = cleanup(&client, &rewritten, &retagged, WriteMode::Live).await;
        assert_eq!((second.changed, second.refused), (0, 0));
        assert_eq!(
            sent_notes(&server).await.len(),
            2,
            "the second run writes nothing"
        );
    }

    #[tokio::test]
    async fn a_rewrite_over_the_limit_is_refused_up_front_dry_run_included() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"})))
            .expect(0)
            .mount(&server)
            .await;
        let client = leak_client(&server);
        let stored = sent_at(&client, PINBOARD_URL_LIMIT + 1, dated());
        let retagged = Bookmark {
            tags: tag_list(&["reddit", "retagged"]),
            ..stored.clone()
        };

        let (dry, logs) = capturing_logs(Box::pin(cleanup(
            &client,
            &stored,
            &retagged,
            WriteMode::DryRun,
        )))
        .await;
        assert_eq!(
            crate::test_support::warnings(&logs)[0],
            format!(
                "WARN skipping cleanup write to {url} from {url}: too long for Pinboard even \
                 with no note ({} bytes without the note, over the 8201-byte limit)",
                client.sent_without_note(&retagged),
                url = stored.url
            )
        );
        for line in &logs {
            for leak in [LEAK_SECRET, "auth_token", &server.uri()] {
                assert!(!line.contains(leak), "{leak} in {line}");
            }
        }
        assert_eq!((dry.changed, dry.refused), (0, 1));
        assert_eq!(
            dry.preview,
            [
                format!("[dry-run] {}", stored.url),
                "          (refused: too long for Pinboard even with no note)".to_string(),
            ]
        );
        let live = cleanup(&client, &stored, &retagged, WriteMode::Live).await;
        assert_eq!((live.changed, live.refused, live.write_failed), (0, 1, 0));
    }
}

#[tokio::test]
async fn add_does_not_warn_for_a_note_that_fits() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/add"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"})))
        .mount(&server)
        .await;

    let (result, logs) = capturing_logs(client(&server).add(&Bookmark {
        note: "a short note".into(),
        ..post()
    }))
    .await;
    result.unwrap();
    assert_eq!(crate::test_support::warnings(&logs), Vec::<&str>::new());
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

    let err = leak_client(&server).add(&post()).await.unwrap_err();
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
    assert_eq!(all[1].timestamp, None); // empty time -> no timestamp
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
    // Raw body carrying `meta`/`hash` and an unparseable `href` -- both of which the
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
        // `posts/all` is rate-limited to one call per five minutes -- far longer than
        // the retry budget -- so a second call in one run would 429 and fail the target.
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

fn item_not_found() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"result_code": "item not found"}))
}

/// The first delete is carried out but its response never arrives within the read
/// timeout, so the retry finds the bookmark already gone: that is the delete succeeding.
#[tokio::test]
async fn a_retried_delete_that_finds_the_item_gone_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/delete"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"result_code": "done"}))
                .set_delay(SHORT_READ_TIMEOUT * 10),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/posts/delete"))
        .respond_with(item_not_found())
        .expect(1)
        .mount(&server)
        .await;
    let pinboard = PinboardClient::with_read_timeout(server.uri(), SHORT_READ_TIMEOUT).unwrap();

    let (deleted, _) = crate::http::recording_waits(
        pinboard.delete(&Url::parse("https://old.reddit.com/r/x/").unwrap()),
    )
    .await;

    deleted.unwrap();
}

#[tokio::test]
async fn a_first_delete_that_finds_no_item_fails() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/delete"))
        .respond_with(item_not_found())
        .expect(1)
        .mount(&server)
        .await;

    let err = client(&server)
        .delete(&Url::parse("https://old.reddit.com/r/x/").unwrap())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("item not found"), "{err}");
}

#[tokio::test]
async fn successive_adds_wait_out_the_rate_limit_between_them() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/add"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"})))
        .expect(2)
        .mount(&server)
        .await;
    let rate_limit = Duration::from_secs(7);
    let pinboard = PinboardClient::build(
        "user:tok".into(),
        rate_limit,
        server.uri(),
        client_builder(),
    )
    .unwrap();

    let (written, waits) = crate::http::recording_waits(async {
        pinboard.add(&post()).await?;
        pinboard.add(&post()).await
    })
    .await;

    written.unwrap();
    assert_eq!(waits, [rate_limit], "a pause before the second add only");
}

/// Cleanup runs over the real client against a Pinboard whose limit is below the budget:
/// a rewrite it refuses is not retried shorter, and the writes after it start from the
/// full budget again.
mod under_a_lower_limit {
    use super::*;
    use crate::bookmark::{AccountView, CleanupStore, WriteMode};
    use crate::cleanup_pass::{run_pass, CleanupPass, Plan};
    use crate::source::SourceError;
    use crate::test_support::NO_DATING;

    /// The URL length above which the server answers 414: under the budget, so a note
    /// `fit` keeps whole can still be refused.
    const LIMIT: usize = MAX_URL_BYTES * 4 / 5;

    /// Moves each `from` to its `to`, and retags a bookmark that stays where it is, so its
    /// rewrite has something to write.
    struct MapPass(Vec<(String, String)>);

    impl MapPass {
        fn new(moves: &[(&str, &str)]) -> Self {
            Self(
                moves
                    .iter()
                    .map(|(from, to)| ((*from).to_string(), (*to).to_string()))
                    .collect(),
            )
        }
    }

    impl CleanupPass for MapPass {
        async fn plan(&self, b: &Bookmark) -> Result<Plan, SourceError> {
            let Some((_, to)) = self.0.iter().find(|(from, _)| from == b.url.as_str()) else {
                return Ok(Plan::Unchanged);
            };
            let tags = if to == b.url.as_str() {
                tag_list(&["x", "retagged"])
            } else {
                b.tags.clone()
            };
            Ok(Plan::Bookmark(Bookmark {
                url: Url::parse(to).unwrap(),
                tags,
                ..b.clone()
            }))
        }
    }

    async fn limited_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/posts/add"))
            .respond_with(|request: &wiremock::Request| {
                if request.url.as_str().len() > LIMIT {
                    ResponseTemplate::new(414)
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"}))
                }
            })
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/posts/delete"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"})))
            .mount(&server)
            .await;
        server
    }

    fn with_note(url: &str, note: String) -> Bookmark {
        Bookmark {
            title: "T".into(),
            note,
            tags: tag_list(&["x"]),
            ..stored_at(url)
        }
    }

    /// A bookmark whose move to `https://b1.example/` the server refuses.
    fn refused_first() -> Bookmark {
        with_note("https://a1.example/", "first ".repeat(1200))
    }

    /// Every request `server` received for `url` at `endpoint`, as the `extended` it sent,
    /// or the `url` for a delete.
    async fn sent(server: &MockServer, endpoint: &str, url: &str) -> Vec<String> {
        server
            .received_requests()
            .await
            .expect("recording requests")
            .iter()
            .filter(|request| request.url.path() == endpoint)
            .filter(|request| {
                request
                    .url
                    .query_pairs()
                    .any(|(name, value)| name == "url" && value == url)
            })
            .map(|request| {
                request
                    .url
                    .query_pairs()
                    .find(|(name, _)| name == "extended")
                    .map_or_else(|| url.to_string(), |(_, value)| value.into_owned())
            })
            .collect()
    }

    /// Asserts the server refused `url`'s one write, with no shorter retry, and no delete
    /// of `kept` followed it.
    async fn assert_refused_once(server: &MockServer, url: &str, kept: &str) {
        assert_eq!(sent(server, "/posts/add", url).await.len(), 1, "{url}");
        assert!(
            sent(server, "/posts/delete", kept).await.is_empty(),
            "{kept}"
        );
    }

    /// A cleanup pass over `slice` of the account `all`, returning how many bookmarks it
    /// changed and how many writes it refused.
    async fn run_over(
        server: &MockServer,
        all: &[Bookmark],
        slice: &[Bookmark],
        pass: &MapPass,
    ) -> (usize, usize) {
        let client = client(server);
        let store = CleanupStore::new(&client, AccountView::new(all.to_vec()), WriteMode::Live);
        let (outcome, _) =
            crate::http::recording_waits(run_pass(&store, slice, "test", NO_DATING, pass)).await;
        (outcome.changed, outcome.refused)
    }

    #[tokio::test]
    async fn a_later_move_keeps_its_whole_note() {
        let server = limited_server().await;
        let second = with_note("https://a2.example/", "second ".repeat(700));
        let all = vec![refused_first(), second.clone()];
        let pass = MapPass::new(&[
            ("https://a1.example/", "https://b1.example/"),
            ("https://a2.example/", "https://b2.example/"),
        ]);

        assert_eq!(run_over(&server, &all, &all, &pass).await, (1, 1));

        assert_refused_once(&server, "https://b1.example/", "https://a1.example/").await;
        assert_eq!(
            sent(&server, "/posts/add", "https://b2.example/").await,
            [second.note]
        );
        assert_eq!(
            sent(&server, "/posts/delete", "https://a2.example/").await,
            ["https://a2.example/"]
        );
    }

    #[tokio::test]
    async fn a_later_merge_keeps_the_movers_text() {
        let server = limited_server().await;
        let first = refused_first();
        let resident = with_note("https://t.example/", "resident ".repeat(550));
        let mover = with_note("https://m.example/", "the mover's own words".into());
        let all = vec![first.clone(), resident, mover.clone()];
        let pass = MapPass::new(&[
            ("https://a1.example/", "https://b1.example/"),
            ("https://m.example/", "https://t.example/"),
        ]);

        assert_eq!(
            run_over(&server, &all, &[first, mover.clone()], &pass).await,
            (1, 1)
        );

        assert_refused_once(&server, "https://b1.example/", "https://a1.example/").await;
        let at_target = sent(&server, "/posts/add", "https://t.example/").await;
        assert!(
            at_target.iter().any(|note| note.contains(&mover.note)),
            "{at_target:?}"
        );
    }

    #[tokio::test]
    async fn a_later_rewrite_in_place_keeps_its_whole_note() {
        let server = limited_server().await;
        let second = with_note("https://c.example/", "second ".repeat(700));
        let all = vec![refused_first(), second.clone()];
        let pass = MapPass::new(&[
            ("https://a1.example/", "https://b1.example/"),
            ("https://c.example/", "https://c.example/"),
        ]);

        assert_eq!(run_over(&server, &all, &all, &pass).await, (1, 1));

        assert_refused_once(&server, "https://b1.example/", "https://a1.example/").await;
        assert_eq!(
            sent(&server, "/posts/add", "https://c.example/").await,
            [second.note]
        );
    }

    #[tokio::test]
    async fn an_overlong_url_does_not_cut_the_writes_after_it() {
        let server = limited_server().await;
        let overlong = format!("https://p.example/?q={}", "a".repeat(LIMIT));
        let second = with_note("https://a2.example/", "second ".repeat(150));
        let all = vec![with_note(&overlong, "n".into()), second.clone()];
        let pass = MapPass::new(&[
            (&overlong, &overlong),
            ("https://a2.example/", "https://b2.example/"),
        ]);

        assert_eq!(run_over(&server, &all, &all, &pass).await, (1, 1));

        assert_refused_once(&server, &overlong, &overlong).await;
        assert_eq!(
            sent(&server, "/posts/add", "https://b2.example/").await,
            [second.note]
        );
    }

    #[tokio::test]
    async fn a_residents_whole_note_is_never_shortened() {
        let server = limited_server().await;
        let resident = with_note("https://t.example/", "resident ".repeat(680));
        let mover = with_note("https://m.example/", "mover ".repeat(80));
        let all = vec![resident, mover.clone()];
        let pass = MapPass::new(&[("https://m.example/", "https://t.example/")]);

        assert_eq!(run_over(&server, &all, &[mover], &pass).await, (0, 1));

        assert_refused_once(&server, "https://t.example/", "https://m.example/").await;
    }

    #[tokio::test]
    async fn a_tags_only_rewrite_refused_writes_nothing() {
        let server = limited_server().await;
        let whole = with_note("https://c.example/", "second ".repeat(950));
        let pass = MapPass::new(&[("https://c.example/", "https://c.example/")]);

        let account = std::slice::from_ref(&whole);
        assert_eq!(run_over(&server, account, account, &pass).await, (0, 1));

        assert_refused_once(&server, "https://c.example/", "https://c.example/").await;
    }
}
