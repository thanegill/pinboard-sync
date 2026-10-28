use super::*;
use crate::test_support::{stored_at, FakePinboard, NO_DATING};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A HackerNews cleanup that can't read its items fails the run.
#[tokio::test]
async fn a_hackernews_cleanup_fails_when_algolia_refuses_its_items() {
    let algolia = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .respond_with(ResponseTemplate::new(400))
        .expect(1)
        .mount(&algolia)
        .await;
    let pinboard = FakePinboard {
        all: vec![stored_at("https://news.ycombinator.com/item?id=42")],
        ..Default::default()
    };
    let cleanup = HackerNewsCleanup {
        opts: HackerNewsCleanupOpts {
            link_discussions: false,
            dates: NO_DATING,
            config: HackerNewsConfig::default(),
        },
        client: AlgoliaClient::with_base_url(algolia.uri()),
    };

    let error = cleanup.run(&pinboard.cleanup_store()).await.unwrap_err();
    assert!(
        error.to_string().starts_with("hn algolia returned 400"),
        "{error}"
    );
}

/// An `--all` run where one account's fetch fails and the other's bookmark fails to write
/// reports both, as the run's error.
#[tokio::test]
async fn a_sync_run_reports_failed_writes_beside_a_failed_account() {
    let hn = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/favorites"))
        .and(wiremock::matchers::query_param_is_missing("comments"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<html op="favorites"><body><table>
                 <tr class="athing" id="1"><td>x</td></tr>
               </table></body></html>"#,
        ))
        .mount(&hn)
        .await;
    Mock::given(method("GET"))
        .and(path("/favorites"))
        .and(wiremock::matchers::query_param("comments", "t"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<html op="favorites"><body><table id="hnmain"></table></body></html>"#,
        ))
        .mount(&hn)
        .await;
    let algolia = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "hits": [{ "objectID": "1", "title": "Ask HN: one", "author": "a",
                       "_tags": ["story"] }]
        })))
        .mount(&algolia)
        .await;
    let no_favorites = MockServer::start().await;
    let pinboard_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/posts/add"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "result_code": "something went wrong" })),
        )
        .expect(1)
        .mount(&pinboard_server)
        .await;

    let job = |label: &str, hn: String| SyncJob {
        client: crate::jobs::SourceClient::HackerNews(
            crate::hackernews::HackerNewsClient::with_base_urls(
                "u".into(),
                HackerNewsConfig::default(),
                hn,
                algolia.uri(),
            ),
        ),
        label: label.into(),
        hook: None,
        settings: sync::JobSettings {
            limit: config::SyncLimit::All,
            read_later: false,
            public: false,
            dates: NO_DATING,
        },
    };
    let jobs = vec![
        job("hackernews[ok]", hn.uri()),
        job("hackernews[down]", no_favorites.uri()),
    ];
    let pinboard = PinboardClient::with_base_url("user:tok".into(), pinboard_server.uri()).unwrap();

    let (result, _) = crate::http::recording_waits(run_sync_jobs(
        jobs,
        AllRun::default(),
        &pinboard,
        &[],
        WriteMode::Live,
    ))
    .await;
    assert_eq!(
        result.unwrap_err().to_string(),
        "1 account(s) failed; 1 bookmark(s) failed to write"
    );
}
