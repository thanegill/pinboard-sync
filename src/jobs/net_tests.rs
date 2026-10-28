use super::tests::fetch_only;
use super::*;
use serde_json::json;
use std::num::NonZeroUsize;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Fetches through a HackerNews `SyncJob` capped at `limit`, against favorites whose
/// first page (both streams) is already synced and links to a second page that is
/// expected `second_page_hits` times.
async fn fetch_hackernews_job(limit: SyncLimit, second_page_hits: u64) {
    let hn = MockServer::start().await;
    let page = |rows: &str| {
        ResponseTemplate::new(200).set_body_string(format!(
            r#"<html op="favorites"><body><table>{rows}</table></body></html>"#
        ))
    };
    Mock::given(method("GET"))
        .and(path("/favorites"))
        .and(query_param_is_missing("p"))
        .respond_with(page(
            r#"<tr class="athing" id="1"><td>x</td></tr>
                   <tr><td><a class="morelink" href="favorites?id=u&amp;p=2">More</a></td></tr>"#,
        ))
        .expect(2)
        .mount(&hn)
        .await;
    Mock::given(method("GET"))
        .and(path("/favorites"))
        .and(query_param("p", "2"))
        .respond_with(page(r#"<tr class="athing" id="2"><td>x</td></tr>"#))
        .expect(second_page_hits)
        .mount(&hn)
        .await;
    let algolia = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "hits": [
            { "objectID": "1", "title": "Ask HN: one", "author": "a", "_tags": ["story"] },
            { "objectID": "2", "title": "Ask HN: two", "author": "a", "_tags": ["story"] },
        ] })))
        .mount(&algolia)
        .await;
    let job = SyncJob {
        client: SourceClient::HackerNews(HackerNewsClient::with_base_urls(
            "u".into(),
            HackerNewsConfig::default(),
            hn.uri(),
            algolia.uri(),
        )),
        label: "hackernews[u]".into(),
        hook: None,
        settings: fetch_only(limit),
    };
    let existing = [crate::test_support::stored_at(
        "https://news.ycombinator.com/item?id=1",
    )];

    crate::http::recording_waits(job.fetch(&existing))
        .await
        .0
        .unwrap();
}

#[tokio::test]
async fn an_auto_sync_job_fetches_hackernews_incrementally() {
    fetch_hackernews_job(SyncLimit::Auto, 0).await;
}

#[tokio::test]
async fn an_all_sync_job_reads_every_favorites_page() {
    fetch_hackernews_job(SyncLimit::All, 2).await;
}

#[tokio::test]
async fn a_capped_sync_job_reads_every_favorites_page() {
    fetch_hackernews_job(SyncLimit::Max(NonZeroUsize::new(5).unwrap()), 2).await;
}

/// Under each limit, fetches through a job built by `client` against two mocked
/// pages whose first is already on Pinboard (`existing`), and asserts how often the
/// second page was read: never under `auto`, once under `all` and a count.
async fn assert_second_page_reads(
    mount: impl AsyncFn(&MockServer),
    client: impl Fn(&MockServer) -> SourceClient,
    is_second_page: impl Fn(&wiremock::Request) -> bool,
    existing: &[Bookmark],
) {
    let limits = [
        (SyncLimit::Auto, 0),
        (SyncLimit::All, 1),
        (SyncLimit::Max(NonZeroUsize::new(5).unwrap()), 1),
    ];
    for (limit, expected_reads) in limits {
        let server = MockServer::start().await;
        mount(&server).await;
        let job = SyncJob {
            client: client(&server),
            label: "test".into(),
            hook: None,
            settings: fetch_only(limit),
        };

        crate::http::recording_waits(job.fetch(existing))
            .await
            .0
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let second_page_reads = requests.iter().filter(|r| is_second_page(r)).count();
        assert_eq!(second_page_reads, expected_reads, "under {limit:?}");
    }
}

#[tokio::test]
async fn only_an_auto_sync_job_stops_reddit_at_a_synced_page() {
    let mount = async |server: &MockServer| {
        for (after, id, next) in [(None, "n", Some("t3_n")), (Some("t3_n"), "o", None)] {
            let saved = Mock::given(method("GET")).and(path("/user/u/saved.json"));
            let saved = match after {
                None => saved.and(query_param_is_missing("after")),
                Some(after) => saved.and(query_param("after", after)),
            };
            saved
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "kind": "Listing",
                    "data": { "after": next, "children": [
                        { "kind": "t3", "data": { "name": format!("t3_{id}"),
                          "subreddit": "rust", "title": id,
                          "permalink": format!("/r/rust/comments/{id}/x/") } }
                    ] }
                })))
                .mount(server)
                .await;
        }
    };
    assert_second_page_reads(
        mount,
        |server| {
            SourceClient::Reddit(RedditClient::with_base_url(
                None,
                Some("u".into()),
                server.uri(),
            ))
        },
        |request| request.url.query_pairs().any(|(key, _)| key == "after"),
        &[crate::test_support::stored_at(
            "https://old.reddit.com/r/rust/comments/n/x/",
        )],
    )
    .await;
}

#[tokio::test]
async fn only_an_auto_sync_job_stops_github_at_a_synced_page() {
    let mount = async |server: &MockServer| {
        for (page, name) in [(1, "s/n"), (2, "s/o")] {
            let mut response = ResponseTemplate::new(200).set_body_json(json!([
                { "starred_at": "2023-01-02T03:04:05Z",
                  "repo": { "full_name": name,
                            "html_url": format!("https://github.com/{name}") } }
            ]));
            if page == 1 {
                let next = format!("<{}/user/starred?page=2>; rel=\"next\"", server.uri());
                response = response.insert_header("link", next.as_str());
            }
            Mock::given(method("GET"))
                .and(path("/user/starred"))
                .and(query_param("page", page.to_string().as_str()))
                .respond_with(response)
                .mount(server)
                .await;
        }
    };
    assert_second_page_reads(
        mount,
        |server| {
            SourceClient::GitHub(GitHubClient::with_base_url(
                "tok".into(),
                GitHubConfig::stars_only(),
                server.uri(),
            ))
        },
        |request| {
            request
                .url
                .query_pairs()
                .any(|(key, value)| key == "page" && value == "2")
        },
        &[crate::test_support::stored_at("https://github.com/s/n")],
    )
    .await;
}
