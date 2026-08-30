//! The Reddit client: reads the user's private **saved listing**
//! (`/user/<name>/saved.json`, for `sync`) and looks up post metadata via
//! **`/api/info.json`** (for `cleanup`). Both run over the same transport — a
//! `reddit_session` cookie + native-tls, no OAuth app — because Reddit's anti-bot
//! edge 403s cookieless requests *and* rustls's TLS fingerprint (see CLAUDE.md).
//! The cookie authenticates the private saved listing; the username (non-secret)
//! selects whose saves to read.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context;
use log::warn;
use serde::de::DeserializeOwned;
use url::Url;

use crate::http::send_retrying;
use crate::model::{reddit_key, RedditConfig, RedditListing, RedditListingEntry};
use crate::source::{BookmarkDraft, Source, SourceError, UrlKey};

/// Descriptive User-Agent built from the crate name + version. Reddit rejects
/// generic or missing User-Agents.
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// Host serving both the saved listing and `/api/info.json`; old.reddit.com
/// matches the bookmark URLs.
const REDDIT_BASE: &str = "https://old.reddit.com";

/// Retry budget for transient failures on Reddit requests.
const MAX_RETRIES: u32 = 4;
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// Backstop against a malformed `after` cursor that never nulls out. Reddit caps any
/// listing near 1000 items at 100 per page, so this covers a full saved listing many
/// times over -- it only bounds a response that keeps returning a non-null cursor; the
/// cursor-repeat case is caught by the `visited` guard.
const MAX_SAVED_PAGES: u32 = 100;

/// Reddit `/api/info` lookups by fullname (used by `cleanup` for over_18/title).
#[allow(async_fn_in_trait)]
pub trait PostInfo {
    async fn info(&self, fullnames: &[String]) -> Result<Vec<RedditListingEntry>, SourceError>;
}

/// Reads Reddit's saved listing and `/api/info.json`, authenticated by a session
/// cookie over native-tls.
pub struct RedditClient {
    http: reqwest::Client,
    /// `Cookie` header value, e.g. `reddit_session=…`.
    cookie: Option<String>,
    /// Whose saved listing to read; required by `fetch_saved`.
    username: Option<String>,
    /// Host base for both endpoints (overridden in tests).
    base: String,
    /// Per-account config: bookmark domain + tag vocabulary for shaping drafts.
    config: RedditConfig,
}

impl RedditClient {
    /// Client for `sync`: reads `<base>/user/<username>/saved.json` and shapes
    /// drafts using `config` (bookmark domain + tag vocabulary).
    pub fn for_user(
        username: String,
        cookie: Option<String>,
        config: RedditConfig,
    ) -> anyhow::Result<Self> {
        Self::build(cookie, Some(username), REDDIT_BASE.to_string(), config)
    }

    /// Client for `cleanup`: `/api/info.json` lookups only (no saved listing, so
    /// the bookmark domain/tag config are irrelevant).
    pub fn for_info(cookie: Option<String>) -> anyhow::Result<Self> {
        Self::build(
            cookie,
            None,
            REDDIT_BASE.to_string(),
            RedditConfig::default(),
        )
    }

    fn build(
        cookie: Option<String>,
        username: Option<String>,
        base: String,
        config: RedditConfig,
    ) -> anyhow::Result<Self> {
        // native-tls is the only TLS backend compiled in (see Cargo.toml), so it's
        // the default connector — Reddit's edge rejects rustls's TLS fingerprint.
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            cookie,
            username,
            base,
            config,
        })
    }

    /// Attach the session cookie to a request, if one is configured.
    fn with_cookie(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.cookie {
            Some(cookie) => req.header(reqwest::header::COOKIE, cookie),
            None => req,
        }
    }
}

impl RedditClient {
    /// Fetch all saved items, newest first, following `after` pagination (Reddit
    /// caps any listing at ~1000). Inherent (no longer a port): only `sync`'s
    /// `Source::fetch` and the net tests call it, both within this crate.
    async fn fetch_saved(&self) -> Result<Vec<RedditListingEntry>, SourceError> {
        let username = self
            .username
            .as_deref()
            .context("no username configured (set REDDIT_USERNAME)")?;
        let endpoint = format!("{}/user/{}/saved.json", self.base, username);
        let mut out: Vec<RedditListingEntry> = Vec::new();
        let mut after: Option<String> = None;
        let mut visited: HashSet<String> = HashSet::new();
        let mut page: u32 = 0;

        loop {
            page += 1;
            if page > MAX_SAVED_PAGES {
                warn!(
                    "reddit saved for '{username}': hit the {MAX_SAVED_PAGES}-page cap; \
                     stopping (some saved items may be missing)"
                );
                break;
            }
            let after_ref = after.as_deref();
            let resp = send_retrying("saved listing", MAX_RETRIES, RETRY_DELAY, || {
                let mut req = self
                    .http
                    .get(&endpoint)
                    .query(&[("limit", "100"), ("raw_json", "1")]);
                if let Some(a) = after_ref {
                    req = req.query(&[("after", a)]);
                }
                self.with_cookie(req)
            })
            .await?;
            let listing: RedditListing = decode_reddit_json(resp, "saved listing").await?;
            let got = listing.data.children.len();
            out.extend(listing.data.children);

            match listing.data.after {
                Some(a) if got > 0 => {
                    if !visited.insert(a.clone()) {
                        warn!(
                            "reddit saved for '{username}': 'after' cursor looped back to \
                             {a}; stopping"
                        );
                        break;
                    }
                    after = Some(a);
                }
                _ => break,
            }
            // Be gentle with Reddit's rate limit between pages.
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }

        Ok(out)
    }
}

impl PostInfo for RedditClient {
    /// Batch-fetch Thing data by fullname via `/api/info.json` (100 per request).
    /// Returns the entries that exist.
    async fn info(&self, fullnames: &[String]) -> Result<Vec<RedditListingEntry>, SourceError> {
        let mut out: Vec<RedditListingEntry> = Vec::new();
        let chunks: Vec<&[String]> = fullnames.chunks(100).collect();
        let endpoint = format!("{}/api/info.json", self.base);
        for (i, chunk) in chunks.iter().enumerate() {
            let ids = chunk.join(",");
            let resp = send_retrying("api/info", MAX_RETRIES, RETRY_DELAY, || {
                self.with_cookie(
                    self.http
                        .get(&endpoint)
                        .query(&[("id", ids.as_str()), ("raw_json", "1")]),
                )
            })
            .await?;
            let listing: RedditListing = decode_reddit_json(resp, "api/info").await?;
            out.extend(listing.data.children);
            // Be gentle with Reddit's rate limit between requests.
            if i + 1 < chunks.len() {
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        }
        Ok(out)
    }
}

impl Source for RedditClient {
    /// Fetch the saved listing and shape each post/comment into a draft. A non-empty
    /// listing that yields zero drafts is an error, not an empty success: it means every
    /// entry was dropped (e.g. a renamed required subfield that still deserializes as
    /// `None`), which would otherwise make `sync` exit 0 having imported nothing.
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError> {
        let entries = self.fetch_saved().await?;
        let entry_count = entries.len();
        let drafts: Vec<BookmarkDraft> = entries
            .into_iter()
            .filter_map(|e| e.into_saved_item(&self.config.domain))
            .filter_map(|it| it.into_draft(&self.config))
            .collect();
        if entry_count > 0 && drafts.is_empty() {
            return Err(anyhow::anyhow!(
                "all {entry_count} saved reddit entries were dropped (none yielded a \
                 bookmark) — the saved-listing response shape may have changed"
            )
            .into());
        }
        Ok(drafts)
    }
}

impl UrlKey for RedditClient {
    /// Reddit's dedup key is the host-agnostic permalink path, so the same post matches
    /// across `old.`/`www.`/`m.` subdomains.
    fn dedup_key(&self, url: &Url) -> Option<String> {
        reddit_key(url)
    }
}

/// The body text of a Reddit response, centralizing status handling: 401/403 become
/// [`SourceError::ReauthRequired`] (with a fixed, actionable message — the body is
/// usually a large anti-bot HTML page, so it isn't echoed), any other non-success
/// status becomes [`SourceError::Other`]. `what` names the request.
async fn reddit_body(resp: reqwest::Response, what: &str) -> Result<String, SourceError> {
    use reqwest::StatusCode;

    let status = resp.status();
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(SourceError::ReauthRequired(format!(
            "{what} returned {status} — Reddit blocked the request. REDDIT_COOKIE (your \
             reddit_session) likely expired or is missing; re-copy it from a logged-in browser."
        )));
    }
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow::anyhow!("{what} returned {status}: {}", body.trim()).into());
    }
    Ok(body)
}

/// Decode a Reddit JSON response into `T`, over [`reddit_body`]'s status handling.
async fn decode_reddit_json<T: DeserializeOwned>(
    resp: reqwest::Response,
    what: &str,
) -> Result<T, SourceError> {
    let body = reddit_body(resp, what).await?;
    serde_json::from_str(&body)
        .with_context(|| format!("parsing {what} response"))
        .map_err(Into::into)
}

#[cfg(test)]
impl RedditClient {
    /// Test constructor pointing both endpoints at a mock server.
    fn for_test(cookie: Option<String>, username: Option<String>, base: String) -> Self {
        Self::build(cookie, username, base, RedditConfig::default()).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[tokio::test]
    async fn maps_401_and_403_to_reauth() {
        for code in [401u16, 403] {
            let err = decode_reddit_json::<Sample>(response(code, "denied"), "req")
                .await
                .unwrap_err();
            assert!(
                matches!(err, SourceError::ReauthRequired(_)),
                "status {code} should require re-auth"
            );
        }
    }

    #[tokio::test]
    async fn maps_500_to_other() {
        let err = decode_reddit_json::<Sample>(response(500, "boom"), "req")
            .await
            .unwrap_err();
        assert!(matches!(err, SourceError::Other(_)));
    }

    #[tokio::test]
    async fn parses_success_and_reports_bad_json_as_other() {
        let ok = decode_reddit_json::<Sample>(response(200, r#"{"ok":true}"#), "req")
            .await
            .unwrap();
        assert_eq!(ok, Sample { ok: true });

        let err = decode_reddit_json::<Sample>(response(200, "not json"), "req")
            .await
            .unwrap_err();
        assert!(matches!(err, SourceError::Other(_)));
    }
}

/// Integration tests against a `wiremock` server. These bind a TCP socket, so
/// they can't run in the Nix build sandbox — the flake skips `net_tests` there.
#[cfg(test)]
mod net_tests {
    use super::*;
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

        let client = RedditClient::for_test(
            Some("reddit_session=secret".into()),
            Some("psophis".into()),
            server.uri(),
        );
        let items: Vec<_> = client
            .fetch_saved()
            .await
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
        assert!(items[1].is_comment);
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

        let client = RedditClient::for_test(None, Some("psophis".into()), server.uri());
        // Page 1 (no cursor) records the cursor; page 2 (after=t3_loop) re-sees it and
        // breaks, so exactly two pages are fetched.
        let entries = client.fetch_saved().await.unwrap();
        assert_eq!(entries.len(), 2);
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

        let client = RedditClient::for_test(None, Some("psophis".into()), server.uri());
        assert!(matches!(client.fetch().await, Err(SourceError::Other(_))));
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

        let client = RedditClient::for_test(None, Some("psophis".into()), server.uri());
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

        let client = RedditClient::for_test(None, Some("psophis".into()), server.uri());
        assert!(client.fetch().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fetch_saved_maps_403_to_reauth() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).set_body_string("blocked"))
            .mount(&server)
            .await;
        let client = RedditClient::for_test(None, Some("psophis".into()), server.uri());
        assert!(matches!(
            client.fetch_saved().await,
            Err(SourceError::ReauthRequired(_))
        ));
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

        let client =
            RedditClient::for_test(Some("reddit_session=secret".into()), None, server.uri());
        let entries = client.info(&["t3_a".to_string()]).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].fields.over_18);
    }
}
