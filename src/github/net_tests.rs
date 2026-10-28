use super::*;
use crate::test_support::titles;
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
    let client = github_client(&server);

    let err = client.repo("o", "r").await.unwrap_err();
    assert!(
        matches!(err, SourceError::Halted(Halt::RateLimited(_))),
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
    let client = github_client(&server);

    let err = client.repo("o", "r").await.unwrap_err();
    assert!(
        matches!(err, SourceError::Halted(Halt::RateLimited(_))),
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
    let client = github_client(&server);

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
    let client = github_client(&server);

    let err = client.repo("o", "r").await.unwrap_err();
    assert!(
        matches!(err, SourceError::Halted(Halt::RateLimited(_))),
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
    let client = github_client(&server);

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
    let client = github_client(&server);

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

    let client = github_client(&server);
    let drafts = client.fetch().await.unwrap();
    assert_eq!(drafts.len(), 2);
    assert_eq!(drafts[0].bookmark.title, "a/one");
    assert_eq!(
        drafts[0].bookmark.tags,
        vec!["github-star", "lang:rust", "author:github:a"]
    );
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

    let client = github_client(&server);
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

    let client = github_client(&server);
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

    let client = github_client(&server);
    let result = client.fetch().await;
    assert!(matches!(result, Err(SourceError::Other(_))), "{result:?}");
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

    let client = github_client(&server);
    let drafts = client.fetch().await.unwrap();
    assert!(drafts.is_empty(), "{drafts:?}");
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
        let SourceError::Halted(Halt::Reauth(message)) = &err else {
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
    let client = GitHubClient::with_base_url("SECRET422".into(), Default::default(), server.uri());
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
    let client = github_client(&server);

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
    let client = github_client(&server);

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

    let client = github_client(&server);
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

    let client = github_client(&server);
    let dump = client.dump().await.unwrap();
    let star_pages: Vec<&crate::backup::RawPage> = dump
        .raw
        .iter()
        .filter(|page| page.stream.is_empty())
        .collect();
    assert_eq!(star_pages.len(), 2, "one captured body per page");

    // GitHub's pages are arrays, so they flatten into one array of repos.
    let bodies: Vec<&str> = star_pages.iter().map(|p| p.body.as_str()).collect();
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

fn github_client(server: &MockServer) -> GitHubClient {
    GitHubClient::with_base_url("tok".into(), GitHubConfig::stars_only(), server.uri())
}

#[tokio::test]
async fn incremental_fetch_stops_at_the_first_page_with_nothing_new() {
    let server = MockServer::start().await;
    three_star_pages(&server, [1, 1, 0]).await;

    let existing = [crate::test_support::stored_at("https://github.com/s/o")];
    let drafts = github_client(&server)
        .fetch_incremental(&existing)
        .await
        .unwrap();
    assert_eq!(titles(&drafts), ["s/n", "s/o"]);
}

#[tokio::test]
async fn incremental_fetch_reads_on_while_every_page_has_something_new() {
    let server = MockServer::start().await;
    three_star_pages(&server, [1, 1, 1]).await;

    let drafts = github_client(&server).fetch_incremental(&[]).await.unwrap();
    assert_eq!(titles(&drafts), ["s/n", "s/o", "s/p"]);
}

#[tokio::test]
async fn dump_reads_every_star_page() {
    let server = MockServer::start().await;
    three_star_pages(&server, [1, 1, 1]).await;

    let dump = github_client(&server).dump().await.unwrap();
    let star_pages = dump.raw.iter().filter(|page| page.stream.is_empty());
    assert_eq!(star_pages.count(), 3);
}

/// With the gists off, a backup still replaces last run's gists file, with `[]`, so it
/// can't outlive the gists in the normalized half.
#[tokio::test]
async fn a_backup_with_gists_off_empties_their_file() {
    let server = MockServer::start().await;
    three_star_pages(&server, [1, 1, 1]).await;
    two_gist_pages(&server, [0, 0]).await;

    let dump = github_client(&server).dump().await.unwrap();
    let files = crate::backup::layout("github-me", &dump).unwrap();
    let gists = files
        .iter()
        .find(|file| file.path == "raw/github-me-gists.json")
        .expect("a gists file");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&gists.body).unwrap(),
        json!([])
    );
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
        &pinboard.cleanup_store(),
        &github_client(&server),
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

/// The id of the gist [`gist_json`] numbers `n`.
fn gist_id(n: usize) -> String {
    format!("{n:020x}")
}

/// A `/gists/starred` element, a one-file Rust gist by `someone`.
fn gist_json(n: usize) -> serde_json::Value {
    let id = gist_id(n);
    json!({
        "id": id,
        "html_url": format!("https://gist.github.com/{id}"),
        "description": format!("gist {n}"),
        "files": { "main.rs": { "filename": "main.rs", "language": "Rust",
                                "raw_url": "https://gist.githubusercontent.com/x" } },
        "owner": { "login": "someone" },
        "public": true,
        "comments": 3,
    })
}

/// Mounts two `/gists/starred` pages of one gist each (gists 1 and 2), expecting each
/// page `hits` times. Only a request asking for 100 a page matches.
async fn two_gist_pages(server: &MockServer, hits: [u64; 2]) {
    for (index, hits) in hits.into_iter().enumerate() {
        let page = index + 1;
        let mut response = ResponseTemplate::new(200).set_body_json(json!([gist_json(page)]));
        if page == 1 {
            let next = format!("<{}/gists/starred?page=2>; rel=\"next\"", server.uri());
            response = response.insert_header("link", next.as_str());
        }
        Mock::given(method("GET"))
            .and(path("/gists/starred"))
            .and(query_param("per_page", "100"))
            .and(query_param("page", page.to_string().as_str()))
            .and(header("accept", "application/vnd.github+json"))
            .respond_with(response)
            .expect(hits)
            .mount(server)
            .await;
    }
}

fn client_with_gists(server: &MockServer) -> GitHubClient {
    GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri())
}

#[tokio::test]
async fn fetch_reads_the_starred_gists_after_the_stars() {
    let server = MockServer::start().await;
    three_star_pages(&server, [1, 1, 1]).await;
    two_gist_pages(&server, [1, 1]).await;

    let drafts = client_with_gists(&server).fetch().await.unwrap();
    assert_eq!(titles(&drafts), ["s/n", "s/o", "s/p", "gist 1", "gist 2"]);
    let gist = &drafts[3].bookmark;
    assert_eq!(
        gist.url.as_str(),
        format!("https://gist.github.com/someone/{}", gist_id(1))
    );
    assert_eq!(
        gist.tags,
        [
            "github-star",
            "github-gist",
            "lang:rust",
            "author:github:someone"
        ]
    );
    assert_eq!(
        drafts[3].dedup_key,
        format!("gist.github.com/{}", gist_id(1))
    );
}

#[tokio::test]
async fn turning_gists_off_never_asks_for_them() {
    let server = MockServer::start().await;
    three_star_pages(&server, [1, 1, 1]).await;
    two_gist_pages(&server, [0, 0]).await;

    let drafts = github_client(&server).fetch().await.unwrap();
    assert_eq!(titles(&drafts), ["s/n", "s/o", "s/p"]);
}

/// Each stream stops at its own caught-up page: the stars, whose second page is already
/// on Pinboard, stop there, while the gists, whose first page is new, read on.
#[tokio::test]
async fn incremental_fetch_stops_each_stream_at_its_own_caught_up_page() {
    let server = MockServer::start().await;
    three_star_pages(&server, [1, 1, 0]).await;
    two_gist_pages(&server, [1, 1]).await;

    let existing = [
        crate::test_support::stored_at("https://github.com/s/o"),
        // Stored in the owner-less form a hand-saved gist often has: still the same gist.
        crate::test_support::stored_at(&format!("https://gist.github.com/{}", gist_id(2))),
    ];
    let client = client_with_gists(&server);
    let drafts = client.fetch_incremental(&existing).await.unwrap();
    assert_eq!(titles(&drafts), ["s/n", "s/o", "gist 1", "gist 2"]);

    let new = crate::sync::filter_new(&client, drafts, &existing);
    assert_eq!(
        titles(&new),
        ["s/n", "gist 1"],
        "the stored gist dedups by id"
    );
}

#[tokio::test]
async fn the_gist_stream_stops_once_caught_up_even_when_the_stars_are_not() {
    let server = MockServer::start().await;
    three_star_pages(&server, [1, 1, 1]).await;
    two_gist_pages(&server, [1, 0]).await;

    let existing = [crate::test_support::stored_at(&format!(
        "https://gist.github.com/someone/{}",
        gist_id(1)
    ))];
    let drafts = client_with_gists(&server)
        .fetch_incremental(&existing)
        .await
        .unwrap();
    assert_eq!(titles(&drafts), ["s/n", "s/o", "s/p", "gist 1"]);
}

/// A refused gist stream fails the account's fetch the way a refused star list does: a
/// 401 asks for a new token, a rate limit stays a rate limit, anything else is an error.
#[tokio::test]
async fn the_gist_stream_is_refused_like_the_stars() {
    let variant = |err: &SourceError| match err {
        SourceError::Halted(Halt::Reauth(_)) => "reauth",
        SourceError::Halted(Halt::RateLimited(_)) => "rate limited",
        SourceError::Halted(Halt::Blocked(_)) => "blocked",
        SourceError::Other(_) => "other",
    };
    let refusals = [
        (ResponseTemplate::new(401), "reauth"),
        (
            ResponseTemplate::new(403).insert_header("x-ratelimit-remaining", "0"),
            "rate limited",
        ),
        (ResponseTemplate::new(403), "other"),
    ];
    for (response, expected) in refusals {
        let server = MockServer::start().await;
        three_star_pages(&server, [1, 1, 1]).await;
        Mock::given(method("GET"))
            .and(path("/gists/starred"))
            .respond_with(response)
            .mount(&server)
            .await;

        let err = client_with_gists(&server).fetch().await.unwrap_err();
        assert_eq!(variant(&err), expected, "{err:?}");
    }
}

#[tokio::test]
async fn dump_captures_the_gists_as_their_own_stream() {
    let server = MockServer::start().await;
    three_star_pages(&server, [1, 1, 1]).await;
    two_gist_pages(&server, [1, 1]).await;

    let dump = client_with_gists(&server).dump().await.unwrap();
    let streams: Vec<&str> = dump.raw.iter().map(|page| page.stream).collect();
    assert_eq!(streams, ["", "", "", "gists", "gists"]);
    let raw: serde_json::Value = serde_json::from_str(&dump.raw[3].body).unwrap();
    assert_eq!(
        raw[0]["comments"], 3,
        "fields the typed gist drops are kept"
    );

    let files = crate::backup::layout("github-me", &dump).unwrap();
    let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
    assert!(paths.contains(&"raw/github-me-gists.json"), "{paths:?}");
    let records = dump.records.expect("normalization succeeded");
    assert_eq!(records.len(), 5, "stars and gists alike");
}

/// A numeric `limit` caps the account's writes across both streams, not each stream.
#[tokio::test]
async fn a_capped_sync_counts_stars_and_gists_together() {
    let server = MockServer::start().await;
    three_star_pages(&server, [1, 1, 1]).await;
    two_gist_pages(&server, [1, 1]).await;
    let client = client_with_gists(&server);
    let drafts = client.fetch().await.unwrap();

    let settings = crate::sync::JobSettings {
        limit: crate::config::SyncLimit::Max(std::num::NonZeroUsize::new(4).unwrap()),
        read_later: false,
        public: false,
        dates: crate::test_support::NO_DATING,
    };
    let new = crate::sync::prepare_new_drafts(
        &client,
        drafts,
        &[],
        &settings,
        time::OffsetDateTime::UNIX_EPOCH,
    );
    assert_eq!(titles(&new), ["gist 1", "s/p", "s/o", "s/n"]);
}

fn truncated_client(status: &'static str, body: &'static str) -> GitHubClient {
    GitHubClient::with_base_url(
        "tok".into(),
        GitHubConfig::default(),
        crate::test_support::truncated_body_server(status, "application/json", body),
    )
}

/// A 403's or 429's body decides whether it is a block, a secondary rate limit or an
/// ordinary refusal, so one cut off mid-read is a read error rather than an empty body
/// that reads as the last of those.
#[tokio::test]
async fn a_403_or_429_cut_off_mid_body_is_a_read_error() {
    for (status, body) in [
        (
            "403 Forbidden",
            r#"{"message": "Repository access blocked", "bl"#,
        ),
        (
            "429 Too Many Requests",
            r#"{"message": "You have exceeded a se"#,
        ),
    ] {
        let client = truncated_client(status, body);
        let results = crate::http::recording_waits(async {
            [
                client.repo("o", "r").await.map(|_| ()),
                client.gist("abc").await.map(|_| ()),
                client.fetch().await.map(|_| ()),
            ]
        })
        .await
        .0;
        for (result, what) in
            results
                .into_iter()
                .zip(["github repo", "github gist", "github starred"])
        {
            let Err(SourceError::Other(error)) = &result else {
                panic!("{what} {status}: a cut-off body is a transient failure: {result:?}");
            };
            let message = format!("{error:#}");
            assert!(
                message.starts_with(&format!("reading {what} response ({status}): ")),
                "{message}"
            );
        }
    }
}

/// Rate-limit headers decide without the body, so a cut-off one still halts as a limit.
#[tokio::test]
async fn a_rate_limit_known_by_its_headers_survives_a_cut_off_body() {
    for status in ["403 Forbidden", "429 Too Many Requests"] {
        let client = GitHubClient::with_base_url(
            "tok".into(),
            GitHubConfig::default(),
            crate::test_support::truncated_body_server_with(
                status,
                vec![
                    ("Content-Type", "application/json"),
                    ("x-ratelimit-remaining", "0"),
                    ("x-ratelimit-reset", "1700000000"),
                ],
                r#"{"message": "API rate limit exc"#,
            ),
        );
        let results = crate::http::recording_waits(async {
            [
                client.repo("o", "r").await.map(|_| ()),
                client.fetch().await.map(|_| ()),
            ]
        })
        .await
        .0;
        for result in results {
            let Err(SourceError::Halted(Halt::RateLimited(message))) = &result else {
                panic!("{status}: the headers mark a rate limit: {result:?}");
            };
            assert!(
                message.starts_with("GitHub rate limit exhausted"),
                "{message}"
            );
        }
    }
}

/// A 5xx body only feeds the error's excerpt, so a cut-off one still reports the status.
#[tokio::test]
async fn a_5xx_cut_off_mid_body_still_reports_its_status() {
    let client = truncated_client("502 Bad Gateway", "<html>");
    let result = crate::http::recording_waits(client.repo("o", "r")).await.0;
    let Err(SourceError::Other(error)) = &result else {
        panic!("a 502 is a transient failure: {result:?}");
    };
    let message = format!("{error:#}");
    assert!(
        message.starts_with("github repo returned 502 Bad Gateway"),
        "{message}"
    );
}
