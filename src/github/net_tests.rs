use super::*;
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// GitHub answers an exhausted primary quota with 403 (not 429) and
/// `x-ratelimit-remaining: 0`. That must not read as a permission problem.
#[tokio::test]
async fn repo_lookup_reports_an_exhausted_rate_limit_with_its_reset() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                // 2026-08-30T15:00:00Z
                .insert_header("x-ratelimit-reset", "1788102000")
                .set_body_string(r#"{"message":"API rate limit exceeded"}"#),
        )
        .mount(&server)
        .await;
    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

    let err = client.repo("o", "r").await.unwrap_err();
    assert!(
        matches!(err, SourceError::RateLimited(_)),
        "expected a rate-limit error, got {err:?}"
    );
    let message = err.to_string();
    assert!(message.contains("rate limit"), "{message}");
    assert!(message.contains("2026-08-30"), "{message}");
}

/// GitHub's docs make both headers optional on a secondary limit — "Otherwise, wait
/// for at least one minute before retrying" — leaving the body's error message as the
/// only signal. Missing it drops us back to the per-bookmark 403 this exists to fix.
#[tokio::test]
async fn a_secondary_rate_limit_is_recognised_by_its_message_alone() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(ResponseTemplate::new(403).set_body_string(
            r#"{"message":"You have exceeded a secondary rate limit. Please wait a few minutes before you try again."}"#,
        ))
        .mount(&server)
        .await;
    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

    let err = client.repo("o", "r").await.unwrap_err();
    assert!(
        matches!(err, SourceError::RateLimited(_)),
        "expected a rate-limit error, got {err:?}"
    );
}

/// A 403 without the rate-limit signature is a real permission denial and must stay
/// an ordinary error — mapping it to a rate limit would stop the pass wrongly.
#[tokio::test]
async fn a_plain_403_is_not_treated_as_a_rate_limit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "4999")
                .set_body_string(r#"{"message":"Must have admin rights"}"#),
        )
        .mount(&server)
        .await;
    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

    let err = client.repo("o", "r").await.unwrap_err();
    assert!(
        matches!(err, SourceError::Other(_)),
        "expected an ordinary error, got {err:?}"
    );
}

/// Secondary rate limits carry `retry-after` and need not zero the remaining count.
#[tokio::test]
async fn a_secondary_rate_limit_is_recognised_by_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("retry-after", "60")
                .set_body_string(r#"{"message":"exceeded a secondary rate limit"}"#),
        )
        .mount(&server)
        .await;
    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

    let err = client.repo("o", "r").await.unwrap_err();
    assert!(
        matches!(err, SourceError::RateLimited(_)),
        "expected a rate-limit error, got {err:?}"
    );
    assert!(err.to_string().contains("60s"), "{err}");
}

#[tokio::test]
async fn repo_error_quotes_a_capped_one_line_excerpt() {
    use crate::test_support::{assert_quotes_capped_excerpt, long_error_page};
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(ResponseTemplate::new(422).set_body_string(long_error_page()))
        .mount(&server)
        .await;
    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

    let err = client.repo("o", "r").await.unwrap_err();
    assert_quotes_capped_excerpt(
        &err.to_string(),
        "github repo returned 422 Unprocessable Entity",
    );
}

#[tokio::test]
async fn starred_error_quotes_a_capped_one_line_excerpt() {
    use crate::test_support::{assert_quotes_capped_excerpt, long_error_page};
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/starred"))
        .respond_with(ResponseTemplate::new(404).set_body_string(long_error_page()))
        .mount(&server)
        .await;
    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

    let err = client.fetch().await.unwrap_err();
    assert_quotes_capped_excerpt(&err.to_string(), "github starred returned 404 Not Found");
}

#[tokio::test]
async fn fetch_paginates_via_link_header_and_sends_token() {
    let server = MockServer::start().await;
    // Page 1 advertises a next page via the Link header.
    Mock::given(method("GET"))
        .and(path("/user/starred"))
        .and(query_param("page", "1"))
        .and(query_param("per_page", "100"))
        .and(header("authorization", "Bearer tok"))
        // The client requests the star+json media type to get `starred_at`.
        .and(header("accept", "application/vnd.github.star+json"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header(
                    "link",
                    format!("<{}/user/starred?page=2>; rel=\"next\"", server.uri()).as_str(),
                )
                .set_body_json(json!([
                    { "starred_at": "2023-01-02T03:04:05Z",
                      "repo": { "full_name": "a/one", "html_url": "https://github.com/a/one",
                                "language": "Rust" } }
                ])),
        )
        .mount(&server)
        .await;
    // Page 2 is the last (no Link header).
    Mock::given(method("GET"))
        .and(path("/user/starred"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "starred_at": "2020-06-07T08:09:10Z",
              "repo": { "full_name": "b/two", "html_url": "https://github.com/b/two" } }
        ])))
        .mount(&server)
        .await;

    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
    let drafts = client.fetch().await.unwrap();
    assert_eq!(drafts.len(), 2);
    assert_eq!(drafts[0].bookmark.title, "a/one");
    assert_eq!(drafts[0].bookmark.tags, vec!["github-star", "lang:rust"]);
    // The star time becomes the draft bookmark's timestamp.
    assert_eq!(
        drafts[0].bookmark.timestamp,
        crate::timefmt::parse_rfc3339("2023-01-02T03:04:05Z")
    );
    assert_eq!(drafts[1].bookmark.title, "b/two");
}

#[tokio::test]
async fn fetch_terminates_when_next_link_loops_back() {
    // A malformed `Link: rel="next"` that points back to the same page must not
    // loop forever: the visited-page guard breaks out after the repeat.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/starred"))
        .and(query_param("page", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header(
                    "link",
                    format!("<{}/user/starred?page=1>; rel=\"next\"", server.uri()).as_str(),
                )
                .set_body_json(json!([
                    { "starred_at": "2023-01-02T03:04:05Z",
                      "repo": { "full_name": "a/one", "html_url": "https://github.com/a/one" } }
                ])),
        )
        .mount(&server)
        .await;

    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
    let drafts = client.fetch().await.unwrap();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].bookmark.title, "a/one");
}

#[tokio::test]
async fn fetch_skips_malformed_element_and_keeps_the_good_repos() {
    let server = MockServer::start().await;
    // The page mixes a valid starred entry with one missing the required `repo`
    // fields: the bad element is dropped, the good one survives.
    Mock::given(method("GET"))
        .and(path("/user/starred"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "starred_at": "2023-01-02T03:04:05Z",
              "repo": { "full_name": "a/one", "html_url": "https://github.com/a/one" } },
            { "starred_at": "2023-01-02T03:04:05Z", "repo": { "full_name": "b/two" } }
        ])))
        .mount(&server)
        .await;

    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
    let drafts = client.fetch().await.unwrap();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].bookmark.title, "a/one");
}

#[tokio::test]
async fn fetch_errors_when_a_whole_page_fails_to_parse() {
    // A schema break (here: every element missing the `repo` object) drops every
    // element. A non-empty page that parses to zero repos must error rather than
    // return an empty success that makes sync exit 0 having imported nothing.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/starred"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "starred_at": "2023-01-02T03:04:05Z" },
            { "starred_at": "2023-01-03T03:04:05Z" }
        ])))
        .mount(&server)
        .await;

    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
    assert!(matches!(client.fetch().await, Err(SourceError::Other(_))));
}

#[tokio::test]
async fn fetch_empty_page_is_ok() {
    // A genuinely empty starred list is legitimate: no error, no drafts.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/starred"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
    assert!(client.fetch().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_401_names_the_token_setting_but_never_the_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_string("bad creds"))
        .mount(&server)
        .await;
    let source = "the config's `token_file`";
    let token = ConfiguredSecret {
        value: "SECRET401".into(),
        source,
    };
    let client = GitHubClient::build(token, GitHubConfig::default(), server.uri()).unwrap();
    let refusals = [
        client.fetch().await.unwrap_err(),
        client.repo("o", "r").await.unwrap_err(),
    ];
    for err in refusals {
        let SourceError::ReauthRequired(message) = &err else {
            panic!("a 401 must ask for a new token: {err:?}");
        };
        assert!(message.contains(source), "{message}");
        assert!(!message.contains("GITHUB_TOKEN"), "{message}");
        assert!(!message.contains("SECRET401"), "{message}");
    }
}

#[tokio::test]
async fn an_echoed_token_is_redacted_from_the_error_and_the_logged_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(422).set_body_string("bad request: Bearer SECRET422"))
        .mount(&server)
        .await;
    let client = GitHubClient::with_base_url("SECRET422 ".into(), Default::default(), server.uri());
    let (errors, logs) = crate::test_support::capturing_logs(async {
        [
            client.fetch().await.unwrap_err(),
            client.repo("o", "r").await.unwrap_err(),
        ]
    })
    .await;
    for err in errors {
        let message = err.to_string();
        assert!(
            message.ends_with(": bad request: Bearer [redacted]"),
            "{message}"
        );
    }
    let bodies: Vec<_> = logs
        .iter()
        .filter(|line| line.contains("full response body"))
        .collect();
    assert_eq!(bodies.len(), 2, "{logs:?}");
    for line in logs {
        assert!(!line.contains("SECRET422"), "{line}");
    }
}

/// A repo lookup that can never succeed again reads as "gone", the same as a 404 —
/// no credential and no retry will clear either one.
#[tokio::test]
async fn repo_lookup_is_none_for_a_gone_repo() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/deleted/repo"))
        .respond_with(ResponseTemplate::new(404).set_body_string(r#"{"message":"Not Found"}"#))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/blocked/repo"))
        .respond_with(ResponseTemplate::new(451).set_body_string(
            r#"{"message":"Repository access blocked","block":{"reason":"dmca"}}"#,
        ))
        .mount(&server)
        .await;
    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

    assert!(client.repo("deleted", "repo").await.unwrap().is_none());
    assert!(
        client.repo("blocked", "repo").await.unwrap().is_none(),
        "a DMCA block is permanent, so it must not surface as an error"
    );
}

#[tokio::test]
async fn repo_lookup_is_none_for_a_403_that_will_repeat_but_errs_for_a_plain_denial() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/tos/blocked"))
        .respond_with(
            ResponseTemplate::new(403).set_body_string(
                r#"{"message":"Repository access blocked","block":{"reason":"tos"}}"#,
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/sso/gated"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header(
                    "x-github-sso",
                    "required; url=https://github.com/orgs/sso/sso",
                )
                .set_body_string(
                    r#"{"message":"Resource protected by organization SAML enforcement."}"#,
                ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/plain/denied"))
        .respond_with(
            ResponseTemplate::new(403).set_body_string(r#"{"message":"Must have admin rights"}"#),
        )
        .mount(&server)
        .await;
    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

    assert!(client.repo("tos", "blocked").await.unwrap().is_none());
    assert!(client.repo("sso", "gated").await.unwrap().is_none());
    let err = client.repo("plain", "denied").await.unwrap_err();
    assert!(matches!(err, SourceError::Other(_)), "{err:?}");
    assert!(err.to_string().contains("403"), "{err}");
}

#[tokio::test]
async fn dump_keeps_the_fields_the_typed_shape_drops() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/starred"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "starred_at": "2023-01-02T03:04:05Z",
              "repo": {
                  "full_name": "a/one",
                  "html_url": "https://github.com/a/one",
                  "language": "Rust",
                  // None of these survive into `GitHubStarredRepo`; a backup of the
                  // typed struct would lose them, so the raw body is what we keep.
                  "stargazers_count": 1234,
                  "license": { "spdx_id": "MIT" },
                  "topics": ["cli", "rust"],
              } }
        ])))
        .mount(&server)
        .await;

    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
    let dump = client.dump().await.unwrap();

    let raw: serde_json::Value =
        serde_json::from_str(&dump.raw[0].body).expect("the captured body is the API's JSON");
    assert_eq!(raw[0]["repo"]["stargazers_count"], 1234);
    assert_eq!(raw[0]["repo"]["license"]["spdx_id"], "MIT");
    assert_eq!(raw[0]["repo"]["topics"][1], "rust");

    // The normalized half comes from the same traversal, so both describe one instant.
    let records = dump.records.expect("normalization succeeded");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].url, "https://github.com/a/one");
}

#[tokio::test]
async fn dump_captures_every_page_in_order() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/starred"))
        .and(query_param("page", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header(
                    "link",
                    format!("<{}/user/starred?page=2>; rel=\"next\"", server.uri()).as_str(),
                )
                .set_body_json(json!([
                    { "starred_at": "2023-01-02T03:04:05Z",
                      "repo": { "full_name": "a/one", "html_url": "https://github.com/a/one" } }
                ])),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/user/starred"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "starred_at": "2023-01-03T03:04:05Z",
              "repo": { "full_name": "b/two", "html_url": "https://github.com/b/two" } }
        ])))
        .mount(&server)
        .await;

    let client = GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
    let dump = client.dump().await.unwrap();
    assert_eq!(dump.raw.len(), 2, "one captured body per page");

    // GitHub's pages are arrays, so they flatten into one array of repos.
    let bodies: Vec<&str> = dump.raw.iter().map(|p| p.body.as_str()).collect();
    let (merged, _) = crate::backup::merge_json_pages(&bodies).unwrap();
    let items = merged.as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["repo"]["full_name"], "a/one");
    assert_eq!(items[1]["repo"]["full_name"], "b/two");
}

/// Mounts three `/user/starred` pages, most recently starred first, one star per page
/// (`s/n`, `s/o`, then `s/p`), expecting each page `hits` times. Only a request that
/// pins the newest-first order matches.
async fn three_star_pages(server: &MockServer, hits: [u64; 3]) {
    let pages = ["s/n", "s/o", "s/p"];
    for (index, (name, hits)) in pages.into_iter().zip(hits).enumerate() {
        let page = index + 1;
        let mut response = ResponseTemplate::new(200).set_body_json(json!([
            { "starred_at": "2023-01-02T03:04:05Z",
              "repo": { "full_name": name, "html_url": format!("https://github.com/{name}") } }
        ]));
        if page < pages.len() {
            let next = format!(
                "<{}/user/starred?page={}>; rel=\"next\"",
                server.uri(),
                page + 1
            );
            response = response.insert_header("link", next.as_str());
        }
        Mock::given(method("GET"))
            .and(path("/user/starred"))
            .and(query_param("sort", "created"))
            .and(query_param("direction", "desc"))
            .and(query_param("page", page.to_string().as_str()))
            .respond_with(response)
            .expect(hits)
            .mount(server)
            .await;
    }
}

fn star_client(server: &MockServer) -> GitHubClient {
    GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri())
}

fn titles(drafts: &[BookmarkDraft]) -> Vec<&str> {
    drafts.iter().map(|d| d.bookmark.title.as_str()).collect()
}

#[tokio::test]
async fn incremental_fetch_stops_at_the_first_page_with_nothing_new() {
    let server = MockServer::start().await;
    three_star_pages(&server, [1, 1, 0]).await;

    let existing = [crate::test_support::stored_at("https://github.com/s/o")];
    let drafts = star_client(&server)
        .fetch_incremental(&existing)
        .await
        .unwrap();
    assert_eq!(titles(&drafts), ["s/n", "s/o"]);
}

#[tokio::test]
async fn incremental_fetch_reads_on_while_every_page_has_something_new() {
    let server = MockServer::start().await;
    three_star_pages(&server, [1, 1, 1]).await;

    let drafts = star_client(&server).fetch_incremental(&[]).await.unwrap();
    assert_eq!(titles(&drafts), ["s/n", "s/o", "s/p"]);
}

#[tokio::test]
async fn dump_reads_every_star_page() {
    let server = MockServer::start().await;
    three_star_pages(&server, [1, 1, 1]).await;

    let dump = star_client(&server).dump().await.unwrap();
    assert_eq!(dump.raw.len(), 3);
}

/// `cleanup github` dates bookmarks from the whole star list, so it must not stop at
/// the first page that is already on Pinboard the way `sync` does.
#[tokio::test]
async fn cleanup_with_post_dates_reads_every_star_page() {
    use crate::test_support::{stored_at, FakePinboard};
    use crate::timefmt::DateOpts;

    let server = MockServer::start().await;
    three_star_pages(&server, [1, 1, 1]).await;
    Mock::given(method("GET"))
        .and(path("/repos/s/n"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(
            { "full_name": "s/n", "html_url": "https://github.com/s/n" }
        )))
        .mount(&server)
        .await;
    let pinboard = FakePinboard {
        all: vec![stored_at("https://github.com/s/n")],
        ..Default::default()
    };

    cleanup::run(
        &pinboard.cleanup_store(false),
        &star_client(&server),
        DateOpts {
            use_post_date: true,
            max_age_days: 30,
            stale_to_now: false,
        },
        &[],
    )
    .await
    .unwrap();
}
