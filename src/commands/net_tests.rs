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
