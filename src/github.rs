//! The GitHub source: reads the authenticated user's starred repositories
//! (`/user/starred`, token-authenticated) and shapes each into a Pinboard draft.

use std::time::Duration;

use anyhow::Context;
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use serde::Deserialize;

use crate::http::send_retrying;
use crate::source::{push_prefixed, push_tag, url_key, BookmarkDraft, Source, SourceError};

const UA: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const API_BASE: &str = "https://api.github.com";
const MAX_RETRIES: u32 = 4;
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Tag vocabulary for GitHub stars; each field defaults to its built-in value, and
/// an empty string disables that tag.
#[derive(Debug, Clone)]
pub struct GithubConfig {
    pub base: String,
    pub lang_prefix: String,
    /// Extra tags appended to every bookmark from this account.
    pub extra: Vec<String>,
}

impl Default for GithubConfig {
    fn default() -> Self {
        Self {
            base: "github-star".into(),
            lang_prefix: "lang:".into(),
            extra: Vec::new(),
        }
    }
}

/// A starred repository, as returned by `/user/starred`.
#[derive(Debug, Clone, Deserialize)]
struct Repo {
    full_name: String,
    html_url: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    language: Option<String>,
}

impl Repo {
    /// Shape the repo into a Pinboard draft.
    fn into_draft(self, cfg: &GithubConfig) -> BookmarkDraft {
        let dedup_key = url_key(&self.html_url).unwrap_or_else(|| self.html_url.clone());

        let mut extended = self
            .description
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.html_url.clone());
        if let Some(home) = self.homepage.filter(|s| !s.is_empty()) {
            extended = format!("{extended}\n\nProject homepage: {home}");
        }

        let mut tags = Vec::new();
        push_tag(&mut tags, &cfg.base);
        if let Some(lang) = self.language.filter(|s| !s.is_empty()) {
            push_prefixed(&mut tags, &cfg.lang_prefix, &lang.to_lowercase());
        }
        for tag in &cfg.extra {
            push_tag(&mut tags, tag);
        }

        BookmarkDraft {
            url: self.html_url,
            description: self.full_name,
            extended,
            tags,
            dedup_key,
        }
    }
}

/// Reads a user's starred repos via a personal access token.
pub struct GitHubClient {
    http: reqwest::Client,
    token: String,
    config: GithubConfig,
    /// API base (overridden in tests).
    base: String,
}

impl GitHubClient {
    pub fn new(token: String, config: GithubConfig) -> anyhow::Result<Self> {
        Self::build(token, config, API_BASE.to_string())
    }

    fn build(token: String, config: GithubConfig, base: String) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            token,
            config,
            base,
        })
    }
}

impl Source for GitHubClient {
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError> {
        let endpoint = format!("{}/user/starred", self.base);
        let mut out: Vec<BookmarkDraft> = Vec::new();
        let mut page: u32 = 1;

        loop {
            let page_str = page.to_string();
            let resp = send_retrying("github starred", MAX_RETRIES, RETRY_DELAY, || {
                self.http
                    .get(&endpoint)
                    .query(&[("sort", "created"), ("page", page_str.as_str())])
                    .header(AUTHORIZATION, format!("Bearer {}", self.token))
                    .header(ACCEPT, "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28")
                    .header(USER_AGENT, UA)
            })
            .await?;

            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                return Err(SourceError::ReauthRequired(
                    "GitHub returned 401 — the token (GITHUB_TOKEN) is invalid or expired."
                        .to_string(),
                ));
            }
            let next = resp
                .headers()
                .get(reqwest::header::LINK)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_link_next);
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(
                    anyhow::anyhow!("github starred returned {status}: {}", body.trim()).into(),
                );
            }

            let repos: Vec<Repo> = resp
                .json()
                .await
                .context("parsing github starred response")?;
            out.extend(repos.into_iter().map(|r| r.into_draft(&self.config)));

            match next {
                Some(p) => page = p,
                None => break,
            }
        }
        Ok(out)
    }

    fn existing_key(&self, url: &str) -> Option<String> {
        let key = url_key(url)?;
        (key == "github.com" || key.starts_with("github.com/")).then_some(key)
    }
}

/// Extract the `page` number of the `rel="next"` link from a GitHub `Link` header,
/// or `None` when there is no next page.
fn parse_link_next(link: &str) -> Option<u32> {
    for part in link.split(',') {
        if part.contains("rel=\"next\"") {
            let url = part.split('<').nth(1)?.split('>').next()?;
            let page = url
                .split(['?', '&'])
                .find_map(|kv| kv.strip_prefix("page="))?;
            return page.parse().ok();
        }
    }
    None
}

#[cfg(test)]
impl GitHubClient {
    fn with_base_url(token: String, config: GithubConfig, base: String) -> Self {
        Self::build(token, config, base).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn repo(value: serde_json::Value) -> Repo {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn draft_maps_fields_and_language_tag() {
        let d = repo(json!({
            "full_name": "owner/Repo",
            "html_url": "https://github.com/owner/Repo",
            "description": "A thing",
            "homepage": "https://example.com",
            "language": "Rust",
        }))
        .into_draft(&GithubConfig::default());

        assert_eq!(d.url, "https://github.com/owner/Repo");
        assert_eq!(d.description, "owner/Repo");
        assert_eq!(
            d.extended,
            "A thing\n\nProject homepage: https://example.com"
        );
        assert_eq!(d.tags, vec!["github-star", "lang:rust"]);
        assert_eq!(d.dedup_key, "github.com/owner/repo");
    }

    #[test]
    fn draft_without_description_or_language() {
        let d = repo(json!({
            "full_name": "o/r",
            "html_url": "https://github.com/o/r",
        }))
        .into_draft(&GithubConfig::default());
        // Missing description falls back to the URL; no language → no lang tag.
        assert_eq!(d.extended, "https://github.com/o/r");
        assert_eq!(d.tags, vec!["github-star"]);
    }

    #[test]
    fn extra_tags_appended() {
        let cfg = GithubConfig {
            extra: vec!["account:work".into()],
            ..GithubConfig::default()
        };
        let d = repo(json!({ "full_name": "o/r", "html_url": "https://github.com/o/r" }))
            .into_draft(&cfg);
        assert!(d.tags.contains(&"account:work".to_string()));
    }

    #[test]
    fn parse_link_next_extracts_next_page() {
        let link = "<https://api.github.com/user/starred?sort=created&page=2>; rel=\"next\", \
                    <https://api.github.com/user/starred?sort=created&page=5>; rel=\"last\"";
        assert_eq!(parse_link_next(link), Some(2));
        // No next link (last page).
        assert_eq!(
            parse_link_next("<https://api.github.com/user/starred?page=1>; rel=\"prev\""),
            None
        );
    }

    #[test]
    fn existing_key_only_matches_github() {
        let c = GitHubClient::new("t".into(), GithubConfig::default()).unwrap();
        assert_eq!(
            c.existing_key("https://github.com/o/r").as_deref(),
            Some("github.com/o/r")
        );
        assert!(c.existing_key("https://example.com/o/r").is_none());
    }
}

/// Integration tests against a `wiremock` server. These bind a TCP socket, so they
/// can't run in the Nix build sandbox — the flake skips `net_tests` there.
#[cfg(test)]
mod net_tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn fetch_paginates_via_link_header_and_sends_token() {
        let server = MockServer::start().await;
        // Page 1 advertises a next page via the Link header.
        Mock::given(method("GET"))
            .and(path("/user/starred"))
            .and(query_param("page", "1"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        "link",
                        format!("<{}/user/starred?page=2>; rel=\"next\"", server.uri()).as_str(),
                    )
                    .set_body_json(json!([
                        { "full_name": "a/one", "html_url": "https://github.com/a/one",
                          "language": "Rust" }
                    ])),
            )
            .mount(&server)
            .await;
        // Page 2 is the last (no Link header).
        Mock::given(method("GET"))
            .and(path("/user/starred"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "full_name": "b/two", "html_url": "https://github.com/b/two" }
            ])))
            .mount(&server)
            .await;

        let client =
            GitHubClient::with_base_url("tok".into(), GithubConfig::default(), server.uri());
        let drafts = client.fetch().await.unwrap();
        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[0].description, "a/one");
        assert_eq!(drafts[0].tags, vec!["github-star", "lang:rust"]);
        assert_eq!(drafts[1].description, "b/two");
    }

    #[tokio::test]
    async fn maps_401_to_reauth() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad creds"))
            .mount(&server)
            .await;
        let client =
            GitHubClient::with_base_url("tok".into(), GithubConfig::default(), server.uri());
        assert!(matches!(
            client.fetch().await,
            Err(SourceError::ReauthRequired(_))
        ));
    }
}
