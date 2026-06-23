//! Minimal Pinboard API client: `posts/add`, `posts/all`, and `posts/delete`,
//! with rate limiting and transient-failure retries (see [`crate::http`]).

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::http::send_retrying;

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
    /// Seconds to pause between successive `posts/add` writes.
    rate_limit_secs: u64,
    /// API base, e.g. `https://api.pinboard.in/v1`.
    base: String,
}

/// Fields for [`BookmarkStore::update`]: an existing bookmark re-added with
/// normalized content (`url`/`description`/`extended`/`tags`) plus the metadata to
/// preserve (`shared`/`toread`, and `dt` — the original time, empty for none).
pub struct BookmarkUpdate<'a> {
    pub url: &'a str,
    pub description: &'a str,
    pub extended: &'a str,
    pub tags: &'a [String],
    pub shared: bool,
    pub toread: bool,
    pub dt: &'a str,
}

/// The Pinboard operations the sync/cleanup loops depend on. Abstracted from the
/// concrete client so those loops can be exercised with an in-memory fake.
/// (Crate-internal, never spawned across threads, so the missing `Send` bound
/// from `async fn` in a trait is irrelevant here.)
#[allow(async_fn_in_trait)]
pub trait BookmarkStore {
    /// Every bookmark in the account (`posts/all`).
    async fn all(&self) -> Result<Vec<Bookmark>>;
    /// Add a new bookmark.
    async fn add(
        &self,
        url: &str,
        description: &str,
        extended: &str,
        tags: &[String],
        toread: bool,
    ) -> Result<()>;
    /// Re-add an existing bookmark with normalized fields, preserving metadata.
    async fn update(&self, b: BookmarkUpdate<'_>) -> Result<()>;
    /// Delete a bookmark by URL.
    async fn delete(&self, url: &str) -> Result<()>;
    /// Seconds to pause between successive writes (Pinboard asks for ~3s).
    fn rate_limit_secs(&self) -> u64 {
        RATE_LIMIT_SECS
    }
}

/// The shared write step of the cleanup loops: rate-limit after the first write (so
/// successive `posts/add`s are spaced), `update` the bookmark, then `delete` the old
/// URL when it changed (`old_url`). `wrote` gates the inter-write delay and is set
/// once any write has happened.
pub async fn apply_update<P: BookmarkStore>(
    pinboard: &P,
    wrote: &mut bool,
    update: BookmarkUpdate<'_>,
    old_url: Option<&str>,
) -> Result<()> {
    if *wrote {
        tokio::time::sleep(Duration::from_secs(pinboard.rate_limit_secs())).await;
    }
    let target = update.url; // `&str` is Copy, so this outlives the move below
    pinboard
        .update(update)
        .await
        .with_context(|| format!("updating bookmark {target}"))?;
    if let Some(old) = old_url {
        pinboard
            .delete(old)
            .await
            .with_context(|| format!("deleting old URL {old}"))?;
    }
    *wrote = true;
    Ok(())
}

impl PinboardClient {
    pub fn new(auth_token: String, shared: bool, rate_limit_secs: u64) -> Result<Self> {
        Self::build(
            auth_token,
            shared,
            rate_limit_secs,
            DEFAULT_BASE.to_string(),
        )
    }

    fn build(auth_token: String, shared: bool, rate_limit_secs: u64, base: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            auth_token,
            shared,
            rate_limit_secs,
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

    fn rate_limit_secs(&self) -> u64 {
        self.rate_limit_secs
    }

    async fn add(
        &self,
        url: &str,
        description: &str,
        extended: &str,
        tags: &[String],
        toread: bool,
    ) -> Result<()> {
        self.post_add(BookmarkUpdate {
            url,
            description,
            extended,
            tags,
            shared: self.shared,
            toread,
            dt: "",
        })
        .await
    }

    async fn update(&self, b: BookmarkUpdate<'_>) -> Result<()> {
        self.post_add(b).await
    }
}

impl PinboardClient {
    /// `posts/add` with `replace=yes`. A non-empty `dt` sets the bookmark time.
    async fn post_add(&self, b: BookmarkUpdate<'_>) -> Result<()> {
        let tags = b.tags.join(" ");
        let mut params = vec![
            ("url", b.url),
            ("description", b.description),
            ("extended", b.extended),
            ("tags", tags.as_str()),
            ("replace", "yes"),
            ("shared", if b.shared { "yes" } else { "no" }),
            ("toread", if b.toread { "yes" } else { "no" }),
            ("auth_token", self.auth_token.as_str()),
            ("format", "json"),
        ];
        if !b.dt.is_empty() {
            params.push(("dt", b.dt));
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

    /// Construct a client pointed at an arbitrary API base, for tests. No inter-write
    /// pacing so `net_tests` don't sleep.
    #[cfg(test)]
    pub fn with_base_url(auth_token: String, shared: bool, base: String) -> Result<Self> {
        Self::build(auth_token, shared, 0, base)
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
                false,
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
                false,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing url"));
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
        assert_eq!(all[0].url, "https://old.reddit.com/r/rust/comments/a/x/");
        assert_eq!(all[0].tag_list(), vec!["reddit", "subreddit:rust"]);
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
