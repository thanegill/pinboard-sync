use super::*;
use crate::test_support::{stored_at, FakePinboard, NO_DATING};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

/// What one Reddit endpoint answers in
/// [`a_reddit_cleanup_fires_the_hook_only_for_a_refused_cookie`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RedditAnswer {
    Ok,
    Block,
    RefusedCookie,
}

impl RedditAnswer {
    fn response(self, ok_body: &serde_json::Value) -> ResponseTemplate {
        match self {
            RedditAnswer::Ok => ResponseTemplate::new(200).set_body_json(ok_body),
            RedditAnswer::Block => ResponseTemplate::new(403).set_body_raw(
                "<html><body><h1>You've been blocked by network security.</h1></body></html>",
                "text/html; charset=utf-8",
            ),
            RedditAnswer::RefusedCookie => ResponseTemplate::new(403).set_body_string("denied"),
        }
    }
}

/// `cleanup reddit` fires the auth-failure hook when `/api/info`, or the saved listing
/// standing in for it, refuses the cookie. A saved listing standing in for a blocked
/// `/api/info` is a run that succeeds.
#[tokio::test]
async fn a_reddit_cleanup_fires_the_hook_only_for_a_refused_cookie() {
    use crate::test_support::scratch_dir;
    use RedditAnswer::{Block, RefusedCookie};

    let listing = serde_json::json!({
        "kind": "Listing",
        "data": { "after": null, "children": [
            { "kind": "t3", "data": { "name": "t3_abc", "subreddit": "rust",
              "permalink": "/r/rust/comments/abc/x/", "title": "Real Title" } }
        ] }
    });
    let cases = [
        (RefusedCookie, None, true),
        (Block, Some(RedditAnswer::Ok), false),
        (Block, Some(RefusedCookie), true),
        (Block, Some(Block), false),
    ];
    for (info, saved, fires) in cases {
        let scratch = scratch_dir("reddit-cleanup-hook");
        let marker = scratch.path().join("fired");
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/info.json"))
            .respond_with(info.response(&listing))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/user/psophis/saved.json"))
            .respond_with(saved.unwrap_or(RedditAnswer::Ok).response(&listing))
            .expect(u64::from(saved.is_some()))
            .mount(&server)
            .await;
        let pinboard = FakePinboard {
            all: vec![stored_at("https://www.reddit.com/r/rust/comments/abc/x/")],
            ..Default::default()
        };
        let cleanup = RedditCleanup {
            opts: reddit::cleanup::RedditCleanupOpts {
                mark_nsfw: true,
                fix_titles: true,
                config: RedditConfig::default(),
                dates: NO_DATING,
                configured: Vec::new(),
            },
            client: Some(RedditInfoClient::with_base_url(
                Some("reddit_session=synthetic".into()),
                Some("psophis"),
                server.uri(),
            )),
            hook: Some(format!("touch {}", marker.display())),
        };

        let (result, _) =
            crate::http::recording_waits(cleanup.run(&pinboard.cleanup_store())).await;

        let case = format!("{info:?} then {saved:?}");
        assert_eq!(marker.exists(), fires, "{case}");
        assert_eq!(
            result.is_ok(),
            saved == Some(RedditAnswer::Ok),
            "{case}: {result:?}"
        );
    }
}
