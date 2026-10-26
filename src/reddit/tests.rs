use super::*;
use crate::reddit::cookie::unnamed_cookie_warning;
use serde::Deserialize;

#[derive(Debug, PartialEq, Deserialize)]
struct Sample {
    ok: bool,
}

fn response(status: u16, body: &str) -> reqwest::Response {
    reqwest::Response::from(
        http::Response::builder()
            .status(status)
            .body(body.to_string())
            .unwrap(),
    )
}

fn test_cookie() -> SessionCookie {
    SessionCookie {
        header: "a=1; reddit_session=test-session; token_v2=x".into(),
        source: TEST_COOKIE_SOURCE,
    }
}

#[tokio::test]
async fn redacts_the_cookie_from_a_logged_or_quoted_body() {
    let echo = "sent a=1; reddit_session=test-session; token_v2=x, session test-session";
    let redacted = "sent [redacted], session [redacted]";
    let (result, logs) = crate::test_support::capturing_logs(decode_reddit_json::<Sample>(
        response(500, echo),
        "req",
        Some(&test_cookie()),
    ))
    .await;
    let Err(SourceError::Other(err)) = result else {
        panic!("a 500 is a plain error: {result:?}");
    };
    assert_eq!(
        err.to_string(),
        format!("req returned 500 Internal Server Error: {redacted}")
    );
    assert_eq!(
        logs,
        [format!(
            "DEBUG req returned 500 Internal Server Error; full response body: {redacted:?}"
        )]
    );

    let (result, logs) = crate::test_support::capturing_logs(decode_reddit_json::<Sample>(
        html_response(403, echo),
        "req",
        Some(&test_cookie()),
    ))
    .await;
    assert!(matches!(result, Err(SourceError::ReauthRequired(_))));
    let [_, debug] = logs.as_slice() else {
        panic!("expected the size warning and the body, got {logs:?}");
    };
    assert!(debug.ends_with(&format!("{redacted:?}")), "{debug}");
}

#[tokio::test]
async fn maps_401_and_403_to_reauth() {
    for code in [401u16, 403] {
        let err =
            decode_reddit_json::<Sample>(response(code, "denied"), "req", Some(&test_cookie()))
                .await
                .unwrap_err();
        assert!(
            matches!(err, SourceError::ReauthRequired(_)),
            "status {code} should require re-auth"
        );
    }
}

/// A minimal stand-in for the page Reddit's edge has served JSON clients since
/// 2026-10-20 (redlib-org/redlib#591), built from its reported text, curly apostrophe
/// and all; the real one wraps that text in about 190KB of markup.
pub(super) const BLOCK_PAGE: &str = "<!DOCTYPE html><html><head><title>Blocked</title>\
    </head><body><h1>You\u{2019}ve been blocked by network security.</h1><p>If you \
    think you\u{2019}ve been blocked by mistake, file a ticket below and we\u{2019}ll \
    look into it.</p><a href=\"https://support.reddithelp.com/hc/en-us/requests/new?\
    ticket_form_id=21879292693140\">File a ticket</a></body></html>";

const HTML: &str = "text/html; charset=utf-8";

fn html_response(status: u16, body: &str) -> reqwest::Response {
    reqwest::Response::from(
        http::Response::builder()
            .status(status)
            .header("content-type", HTML)
            .body(body.to_string())
            .unwrap(),
    )
}

/// A minimal stand-in for the ~1.5KB page Reddit's edge serves for a junk cookie or
/// bearer token (and for blocked IP ranges).
const NETWORK_POLICY_PAGE: &str = "<!doctype html><html><head><title>Blocked</title>\
    </head><body><h1>whoa there, pardner!</h1><p>Your request has been blocked due \
    to a network policy.</p><p>Try logging in or creating an account here to get \
    back to browsing.</p></body></html>";

#[test]
fn recognises_reddit_edge_pages() {
    let forbidden = reqwest::StatusCode::FORBIDDEN;
    let security = Some(EdgePage::NetworkSecurity);
    assert_eq!(edge_page(forbidden, HTML, BLOCK_PAGE), security);
    assert_eq!(
        edge_page(
            forbidden,
            "TEXT/HTML",
            "YOU'VE BEEN BLOCKED BY NETWORK SECURITY"
        ),
        security
    );
    assert_eq!(
        edge_page(
            forbidden,
            HTML,
            "<h1>You've been blocked by\n      network  security.</h1>"
        ),
        security
    );
    assert_eq!(
        edge_page(forbidden, "text/html", NETWORK_POLICY_PAGE),
        Some(EdgePage::NetworkPolicy)
    );
}

#[test]
fn does_not_mistake_other_refusals_for_an_edge_page() {
    let forbidden = reqwest::StatusCode::FORBIDDEN;
    for body in [
        "",
        "denied",
        "blocked",
        r#"{"message": "Forbidden", "error": 403}"#,
        "<html><title>reddit: log in</title><body>Log in to continue</body></html>",
    ] {
        assert_eq!(
            edge_page(forbidden, HTML, body),
            None,
            "{body:?} is not an edge page"
        );
    }
    assert_eq!(
        edge_page(forbidden, "application/json", BLOCK_PAGE),
        None,
        "a JSON body is an API answer, not the edge's page"
    );
    assert_eq!(
        edge_page(reqwest::StatusCode::UNAUTHORIZED, HTML, BLOCK_PAGE),
        None,
        "only a 403 is an edge page"
    );
}

#[tokio::test]
async fn keeps_the_network_policy_page_a_credential_problem() {
    let err = decode_reddit_json::<Sample>(
        html_response(403, NETWORK_POLICY_PAGE),
        "req",
        Some(&test_cookie()),
    )
    .await
    .unwrap_err();
    let SourceError::ReauthRequired(message) = err else {
        panic!("a junk credential must still fire the hook: {err:?}");
    };
    assert!(message.contains(TEST_COOKIE_SOURCE), "{message}");
    assert!(message.contains("network policy"), "{message}");
}

#[tokio::test]
async fn keeps_other_403s_as_reauth() {
    let json_forbidden = reqwest::Response::from(
        http::Response::builder()
            .status(403)
            .header("content-type", "application/json; charset=UTF-8")
            .body(r#"{"message": "Forbidden", "error": 403}"#.to_string())
            .unwrap(),
    );
    for (label, resp) in [
        ("JSON API 403", json_forbidden),
        (
            "HTML without the phrase",
            html_response(403, "<html><title>reddit: log in</title></html>"),
        ),
        (
            "block text without a content type",
            response(403, BLOCK_PAGE),
        ),
    ] {
        let err = decode_reddit_json::<Sample>(resp, "req", Some(&test_cookie()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, SourceError::ReauthRequired(_)),
            "{label}: got {err:?}"
        );
    }
}

#[tokio::test]
async fn logs_an_unrecognised_refusal_page_at_debug() {
    let page = "<html>\n<title>reddit: log in</title>\n</html>";
    let (result, logs) = crate::test_support::capturing_logs(decode_reddit_json::<Sample>(
        html_response(403, page),
        "req",
        Some(&test_cookie()),
    ))
    .await;
    assert!(matches!(result, Err(SourceError::ReauthRequired(_))));
    let [warning, debug] = logs.as_slice() else {
        panic!("expected the size warning and the body, got {logs:?}");
    };
    assert!(
        warning.starts_with("WARN req returned 403 Forbidden ("),
        "{warning}"
    );
    assert!(
        warning.contains(&format!("{} bytes", page.len())),
        "{warning}"
    );
    assert_eq!(
        debug,
        &format!("DEBUG req returned 403 Forbidden; full response body: {page:?}")
    );
}

#[tokio::test]
async fn maps_a_403_block_page_to_blocked_not_reauth() {
    let err =
        decode_reddit_json::<Sample>(html_response(403, BLOCK_PAGE), "req", Some(&test_cookie()))
            .await
            .unwrap_err();
    let SourceError::Blocked(message) = err else {
        panic!("a block page must not ask for a new cookie: {err:?}");
    };
    assert!(message.contains("issues/78"), "{message}");
    assert!(!message.contains("likely expired"), "{message}");
}

#[tokio::test]
async fn maps_500_to_other() {
    let err = decode_reddit_json::<Sample>(response(500, "boom"), "req", Some(&test_cookie()))
        .await
        .unwrap_err();
    assert!(matches!(err, SourceError::Other(_)));
}

#[tokio::test]
async fn parses_success_and_reports_bad_json_as_other() {
    let ok =
        decode_reddit_json::<Sample>(response(200, r#"{"ok":true}"#), "req", Some(&test_cookie()))
            .await
            .unwrap();
    assert_eq!(ok, Sample { ok: true });

    let err = decode_reddit_json::<Sample>(response(200, "not json"), "req", Some(&test_cookie()))
        .await
        .unwrap_err();
    assert!(matches!(err, SourceError::Other(_)));
}

fn redirect(location: Option<&str>) -> reqwest::Response {
    let mut builder = http::Response::builder().status(302);
    if let Some(location) = location {
        builder = builder.header("location", location);
    }
    reqwest::Response::from(builder.body(String::new()).unwrap())
}

#[tokio::test]
async fn maps_a_redirect_to_login_to_reauth() {
    for location in [
        "https://old.reddit.com/login/?dest=https%3A%2F%2Fold.reddit.com%2Fuser%2Fx%2Fsaved.json",
        "/login",
    ] {
        let err =
            decode_reddit_json::<Sample>(redirect(Some(location)), "req", Some(&test_cookie()))
                .await
                .unwrap_err();
        let SourceError::ReauthRequired(message) = err else {
            panic!("a login redirect to {location} must ask for a new cookie: {err:?}");
        };
        assert!(message.contains("reddit_session"), "{message}");
    }
}

#[tokio::test]
async fn maps_any_other_redirect_to_other() {
    for location in [
        Some("https://www.reddit.com/user/x/saved.json"),
        Some("/user/login-help"),
        Some("/logins"),
        None,
    ] {
        let err = decode_reddit_json::<Sample>(redirect(location), "req", Some(&test_cookie()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, SourceError::Other(_)),
            "{location:?} is not a login redirect: {err:?}"
        );
    }
}

#[test]
fn every_entry_point_names_a_bare_cookie() {
    let configured = |value: &str| {
        Some(ConfiguredSecret {
            value: value.into(),
            source: TEST_COOKIE_SOURCE,
        })
    };
    let sent = |client: &RedditClient| client.cookie.as_ref().map(|c| c.header.clone());
    let named = Some("reddit_session=bare-value".to_string());
    let sync = RedditClient::for_user(
        "psophis".into(),
        configured(" bare-value "),
        RedditConfig::default(),
    )
    .unwrap();
    assert_eq!(sent(&sync), named);
    let cleanup = RedditClient::for_info(configured("bare-value\n")).unwrap();
    assert_eq!(sent(&cleanup), named);
}

#[tokio::test]
async fn names_the_cookie_setting_in_every_refusal() {
    let source = "the config's `cookie_file`";
    let cookie = SessionCookie {
        source,
        ..test_cookie()
    };
    let refusals = [
        decode_reddit_json::<Sample>(response(403, "denied"), "req", Some(&cookie)),
        decode_reddit_json::<Sample>(redirect(Some("/login")), "req", Some(&cookie)),
    ];
    for refusal in refusals {
        let SourceError::ReauthRequired(message) = refusal.await.unwrap_err() else {
            panic!("a refused cookie must ask for a new one");
        };
        assert!(message.contains(source), "{message}");
        assert!(!message.contains("REDDIT_COOKIE"), "{message}");
    }
    assert!(unnamed_cookie_warning(source).contains(source));
    assert!(!unnamed_cookie_warning(source).contains("REDDIT_COOKIE"));
}

#[tokio::test]
async fn says_no_cookie_is_configured_when_none_was() {
    let refusals = [
        decode_reddit_json::<Sample>(response(403, "denied"), "req", None),
        decode_reddit_json::<Sample>(redirect(Some("/login")), "req", None),
    ];
    for refusal in refusals {
        let SourceError::ReauthRequired(message) = refusal.await.unwrap_err() else {
            panic!("a refusal without a cookie must ask for one");
        };
        assert!(
            message.contains("No Reddit cookie is configured"),
            "{message}"
        );
    }
}

#[test]
fn a_saved_page_is_caught_up_when_its_saves_are_on_pinboard() {
    use crate::test_support::{listing_entry, stored_at};
    let post = |id: &str| {
        listing_entry(
            "t3",
            serde_json::json!({ "name": format!("t3_{id}"), "subreddit": "rust",
                                "permalink": format!("/r/rust/comments/{id}/x/"),
                                "title": "T" }),
        )
    };
    let unshapable = listing_entry("t3", serde_json::json!({ "name": "t3_gone" }));
    let client = RedditClient::for_test(None, None, "http://unused.invalid".into());
    let synced_keys = client.synced_keys(&[
        stored_at("https://www.reddit.com/r/rust/comments/a/x/"),
        stored_at("https://old.reddit.com/r/Rust/comments/b/x/"),
    ]);
    let caught_up = |page: Vec<RedditListingEntry>| {
        page_is_caught_up(page.len(), &client.shape(page), &synced_keys)
    };

    assert!(
        caught_up(vec![post("a"), post("b")]),
        "saves already on Pinboard, under any reddit host or case"
    );
    assert!(!caught_up(vec![post("a"), post("c")]), "a new save");
    assert!(
        caught_up(vec![post("a"), unshapable]),
        "an entry that makes no draft adds nothing"
    );
}
