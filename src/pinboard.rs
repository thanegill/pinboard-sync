//! Minimal Pinboard API client: `posts/add`, `posts/all`, and `posts/delete`,
//! with rate limiting and transient-failure retries (see [`crate::http`]).

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::http::send_retrying;
use crate::model::reddit_key;

const DEFAULT_BASE: &str = "https://api.pinboard.in/v1";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// Seconds to wait between successful `posts/add` calls (Pinboard asks for ~3s).
pub const RATE_LIMIT_SECS: u64 = 3;
const MAX_RETRIES: u32 = 5;

#[derive(Deserialize)]
struct AddResponse {
    result_code: String,
}

/// A bookmark as returned by `posts/all`.
#[derive(Debug, Clone, Deserialize)]
pub struct Bookmark {
    #[serde(rename = "href")]
    pub url: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub extended: String,
    /// Space-separated tag string.
    #[serde(default)]
    pub tags: String,
    /// Creation timestamp (ISO 8601), preserved as `dt` on update.
    #[serde(default)]
    pub time: String,
    #[serde(default)]
    pub shared: String,
    #[serde(default)]
    pub toread: String,
}

impl Bookmark {
    pub fn tag_list(&self) -> Vec<String> {
        self.tags.split_whitespace().map(String::from).collect()
    }
    pub fn is_shared(&self) -> bool {
        self.shared == "yes"
    }
    pub fn is_toread(&self) -> bool {
        self.toread == "yes"
    }
}

pub struct PinboardClient {
    http: reqwest::Client,
    /// `username:TOKEN`.
    auth_token: String,
    shared: bool,
    /// API base, e.g. `https://api.pinboard.in/v1`.
    base: String,
}

/// The Pinboard operations the sync/cleanup loops depend on. Abstracted from the
/// concrete client so those loops can be exercised with an in-memory fake.
/// (Crate-internal, never spawned across threads, so the missing `Send` bound
/// from `async fn` in a trait is irrelevant here.)
#[allow(async_fn_in_trait)]
pub trait BookmarkStore {
    /// Every bookmark in the account (`posts/all`).
    async fn all(&self) -> Result<Vec<Bookmark>>;
    /// The set of Reddit permalink keys already bookmarked.
    async fn existing_reddit_keys(&self) -> Result<HashSet<String>>;
    /// Add a new bookmark.
    async fn add(
        &self,
        url: &str,
        description: &str,
        extended: &str,
        tags: &[String],
    ) -> Result<()>;
    /// Re-add an existing bookmark with normalized fields, preserving metadata.
    #[allow(clippy::too_many_arguments)]
    async fn update(
        &self,
        url: &str,
        description: &str,
        extended: &str,
        tags: &[String],
        shared: bool,
        toread: bool,
        dt: &str,
    ) -> Result<()>;
    /// Delete a bookmark by URL.
    async fn delete(&self, url: &str) -> Result<()>;
}

impl PinboardClient {
    pub fn new(auth_token: String, shared: bool) -> Result<Self> {
        Self::build(auth_token, shared, DEFAULT_BASE.to_string())
    }

    fn build(auth_token: String, shared: bool, base: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            auth_token,
            shared,
            base,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base)
    }
}

impl BookmarkStore for PinboardClient {
    async fn all(&self) -> Result<Vec<Bookmark>> {
        let params = [
            ("auth_token", self.auth_token.as_str()),
            ("format", "json"),
            ("meta", "no"),
        ];
        let resp = self
            .get_with_backoff(&self.url("posts/all"), &params)
            .await?;
        resp.json().await.context("parsing Pinboard posts/all")
    }

    /// The set of Reddit permalink keys already bookmarked — the destination *is*
    /// the sync state. Matched by host + permalink path, so it covers reddit
    /// bookmarks regardless of their tags or subdomain.
    async fn existing_reddit_keys(&self) -> Result<HashSet<String>> {
        Ok(self
            .all()
            .await?
            .into_iter()
            .filter_map(|b| reddit_key(&b.url))
            .collect())
    }

    async fn delete(&self, url: &str) -> Result<()> {
        let params = [
            ("url", url),
            ("auth_token", self.auth_token.as_str()),
            ("format", "json"),
        ];
        let resp = self
            .get_with_backoff(&self.url("posts/delete"), &params)
            .await?;
        let parsed: AddResponse = resp
            .json()
            .await
            .context("parsing Pinboard posts/delete response")?;
        if parsed.result_code != "done" {
            return Err(anyhow!(
                "Pinboard posts/delete failed: {}",
                parsed.result_code
            ));
        }
        Ok(())
    }

    async fn add(
        &self,
        url: &str,
        description: &str,
        extended: &str,
        tags: &[String],
    ) -> Result<()> {
        self.post_add(url, description, extended, tags, self.shared, false, None)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn update(
        &self,
        url: &str,
        description: &str,
        extended: &str,
        tags: &[String],
        shared: bool,
        toread: bool,
        dt: &str,
    ) -> Result<()> {
        let dt = (!dt.is_empty()).then_some(dt);
        self.post_add(url, description, extended, tags, shared, toread, dt)
            .await
    }
}

impl PinboardClient {
    /// `posts/add` with `replace=yes`. `dt` sets the bookmark time when given.
    #[allow(clippy::too_many_arguments)]
    async fn post_add(
        &self,
        url: &str,
        description: &str,
        extended: &str,
        tags: &[String],
        shared: bool,
        toread: bool,
        dt: Option<&str>,
    ) -> Result<()> {
        let tags = tags.join(" ");
        let mut params = vec![
            ("url", url),
            ("description", description),
            ("extended", extended),
            ("tags", tags.as_str()),
            ("replace", "yes"),
            ("shared", if shared { "yes" } else { "no" }),
            ("toread", if toread { "yes" } else { "no" }),
            ("auth_token", self.auth_token.as_str()),
            ("format", "json"),
        ];
        if let Some(dt) = dt {
            params.push(("dt", dt));
        }

        let resp = self
            .get_with_backoff(&self.url("posts/add"), &params)
            .await?;
        let parsed: AddResponse = resp
            .json()
            .await
            .context("parsing Pinboard posts/add response")?;
        if parsed.result_code != "done" {
            return Err(anyhow!("Pinboard posts/add failed: {}", parsed.result_code));
        }
        Ok(())
    }

    /// GET a Pinboard endpoint, retrying transient failures (network errors,
    /// HTTP 429, 5xx) with backoff. Errors on a non-success final status.
    async fn get_with_backoff(
        &self,
        url: &str,
        params: &[(&str, &str)],
    ) -> Result<reqwest::Response> {
        let resp = send_retrying(
            url,
            MAX_RETRIES,
            Duration::from_secs(RATE_LIMIT_SECS),
            || self.http.get(url).query(params),
        )
        .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("Pinboard {url} returned {status}: {}", body.trim());
        }
        Ok(resp)
    }

    /// Construct a client pointed at an arbitrary API base, for tests.
    #[cfg(test)]
    pub fn with_base_url(auth_token: String, shared: bool, base: String) -> Result<Self> {
        Self::build(auth_token, shared, base)
    }
}

/// Integration tests against a `wiremock` server. These bind a TCP socket, so
/// they can't run in the Nix build sandbox — the flake skips `net_tests` there.
#[cfg(test)]
mod net_tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> PinboardClient {
        PinboardClient::with_base_url("user:tok".into(), false, server.uri()).unwrap()
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
            .add(
                "https://old.reddit.com/r/x/",
                "Title",
                "",
                &["reddit".into()],
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn add_errors_on_non_done_result() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/posts/add"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"result_code": "missing url"})),
            )
            .mount(&server)
            .await;

        let err = client(&server)
            .add(
                "https://old.reddit.com/r/x/",
                "Title",
                "",
                &["reddit".into()],
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing url"));
    }

    #[tokio::test]
    async fn all_parses_and_existing_keys_filters_to_reddit() {
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

        let c = client(&server);
        assert_eq!(c.all().await.unwrap().len(), 2);

        let keys = c.existing_reddit_keys().await.unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys.contains("/r/rust/comments/a/x"));
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
            .delete("https://old.reddit.com/r/x/")
            .await
            .unwrap();
    }
}
