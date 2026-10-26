//! The GitHub source: reads the authenticated user's starred repositories
//! (`/user/starred`, token-authenticated) and shapes each into a Pinboard draft.

pub mod cleanup;

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result};
use log::{debug, warn};
use serde::Deserialize;

use crate::backup::{BackupDump, BackupSource, RawKind, RawSink};
use crate::bookmark::Bookmark;
use crate::htmltext::{blockquote, html_to_plain};
use crate::http::{client_builder, RetryingClient};
use crate::secrets::ConfiguredSecret;
use crate::source::{
    deserialize_lenient, page_is_caught_up, push_prefixed, push_tags, url_key, BookmarkDraft,
    Source, SourceError, UrlExt, UrlKey,
};
use url::Url;

const UA: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const API_BASE: &str = "https://api.github.com";
const MAX_RETRIES: u32 = 4;
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// The most stars `/user/starred` returns per page; GitHub's default is 30.
const STARRED_PER_PAGE: &str = "100";
/// Backstop against a malformed `Link: rel="next"` chain that never terminates.
/// At [`STARRED_PER_PAGE`] results per page this covers a million stars,
/// far beyond any real account -- it is only a backstop against a `Link` header that
/// increments forever; the per-page repeat case is caught by the `visited` guard.
const MAX_STARRED_PAGES: u32 = 10_000;

/// Tag vocabulary for GitHub stars. `tags` are applied to every bookmark
/// (defaulting to `["github-star"]`); `lang_prefix` defaults to its built-in value,
/// and an empty string disables that tag.
#[derive(Debug, Clone)]
pub struct GitHubConfig {
    pub tags: Vec<String>,
    pub lang_prefix: String,
}

impl Default for GitHubConfig {
    fn default() -> Self {
        Self {
            tags: vec!["github-star".into()],
            lang_prefix: "lang:".into(),
        }
    }
}

/// A starred repository, as returned by `/user/starred`.
#[derive(Debug, Clone, Deserialize)]
struct GitHubRepo {
    full_name: String,
    html_url: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    language: Option<String>,
}

/// A `/user/starred` element when requested with the star+json media type: the repo
/// plus the time the user starred it.
#[derive(Debug, Clone, Deserialize)]
struct GitHubStarredRepo {
    starred_at: String,
    repo: GitHubRepo,
}

impl GitHubStarredRepo {
    /// Shape into a draft, dating it by the (RFC3339) star time. `None` (with a warning)
    /// if the repo's `html_url` doesn't parse.
    fn into_draft(self, cfg: &GitHubConfig) -> Option<BookmarkDraft> {
        let post_date = crate::timefmt::rfc3339_to_unix(&self.starred_at);
        self.repo.into_draft_with_date(cfg, post_date)
    }
}

/// Build the Pinboard notes for a repo: the `description` wrapped in a `<blockquote>`
/// (Pinboard renders HTML in the notes), with the project homepage appended outside the
/// quote. Falls back to the repo URL when there's no description. Shared by sync and
/// cleanup so both produce the same shape.
fn github_extended(description: Option<&str>, homepage: Option<&str>, html_url: &str) -> String {
    let mut extended = match description.filter(|s| !s.is_empty()) {
        Some(desc) => blockquote(desc),
        None => html_url.to_string(),
    };
    if let Some(home) = homepage.filter(|s| !s.is_empty()) {
        extended = format!("{extended}\n\nProject homepage: {home}");
    }
    extended
}

impl GitHubRepo {
    /// Shape the repo into a Pinboard draft (no source date). Test-only convenience;
    /// the production path dates each draft via [`GitHubStarredRepo::into_draft`].
    #[cfg(test)]
    fn into_draft(self, cfg: &GitHubConfig) -> Option<BookmarkDraft> {
        self.into_draft_with_date(cfg, None)
    }

    /// Shape the repo into a Pinboard draft, carrying `post_date` (the star time). `None`
    /// (with a warning) if `html_url` doesn't parse.
    fn into_draft_with_date(
        self,
        cfg: &GitHubConfig,
        post_date: Option<i64>,
    ) -> Option<BookmarkDraft> {
        let url = Url::parse(&self.html_url)
            .map_err(|e| {
                warn!(
                    "skipping github repo with unparseable URL {}: {e}",
                    self.html_url
                )
            })
            .ok()?;
        let dedup_key = url_key(&url).unwrap_or_else(|| url.to_string());

        let note = github_extended(
            self.description.as_deref(),
            self.homepage.as_deref(),
            &self.html_url,
        );

        let mut tags = Vec::new();
        push_tags(&mut tags, &cfg.tags);
        if let Some(lang) = self.language.filter(|s| !s.is_empty()) {
            push_prefixed(&mut tags, &cfg.lang_prefix, &lang.to_lowercase());
        }

        Some(BookmarkDraft {
            bookmark: Bookmark {
                url,
                title: html_to_plain(&self.full_name),
                note,
                tags,
                timestamp: post_date.and_then(crate::timefmt::from_unix),
                public: false,
                read_later: false,
            },
            dedup_key,
        })
    }
}

/// The operator-facing explanation if `resp` is GitHub refusing us for rate limiting,
/// else `None`.
///
/// Needed because GitHub answers *both* its primary and secondary rate limits with a
/// `403` or a `429`, the same statuses a genuine permission denial uses — so the status
/// alone cannot tell them apart. The headers can: an exhausted primary quota zeroes
/// `x-ratelimit-remaining` and dates the reset in `x-ratelimit-reset`, while a secondary
/// limit carries `retry-after`. Getting this wrong in either direction is costly, so it
/// matches only on those signatures: a false positive would stop a pass over an ordinary
/// permission error, a false negative leaves the operator chasing a token problem.
fn rate_limit_message(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &str,
) -> Option<String> {
    if status != reqwest::StatusCode::FORBIDDEN && status != reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        return None;
    }
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };

    if let Some(retry_after) = header("retry-after") {
        // RFC 7231 allows an HTTP-date here as well as delta-seconds, so only call it a
        // duration once it parses as one.
        return Some(match retry_after.parse::<u64>() {
            Ok(seconds) => format!("GitHub rate limit hit; retry after {seconds}s"),
            Err(_) => format!("GitHub rate limit hit; retry after {retry_after}"),
        });
    }
    if header("x-ratelimit-remaining") == Some("0") {
        let reset = header("x-ratelimit-reset")
            .and_then(|value| value.parse::<i64>().ok())
            .and_then(crate::timefmt::from_unix)
            .and_then(crate::timefmt::to_rfc3339);
        return Some(match reset {
            Some(at) => format!("GitHub rate limit exhausted; it resets at {at}"),
            None => "GitHub rate limit exhausted".to_string(),
        });
    }
    // A secondary limit need carry neither header — the docs' guidance is then simply
    // "wait at least one minute" — so its error message is the only thing left to go on.
    if body.to_lowercase().contains("secondary rate limit") {
        return Some("GitHub secondary rate limit hit; wait a minute before retrying".to_string());
    }
    None
}

/// Reads a user's starred repos via a personal access token.
pub struct GitHubClient {
    http: RetryingClient,
    config: GitHubConfig,
    /// API base (overridden in tests).
    base: String,
    /// The setting the token came from, for messages; never the token itself.
    token_source: &'static str,
    /// Only to [redact](crate::http::redact) it from a response body that is logged or
    /// quoted.
    token: String,
}

impl GitHubClient {
    pub fn new(token: ConfiguredSecret, config: GitHubConfig) -> anyhow::Result<Self> {
        Self::build(token, config, API_BASE.to_string())
    }

    fn build(token: ConfiguredSecret, config: GitHubConfig, base: String) -> anyhow::Result<Self> {
        use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION};

        // The auth/accept/version headers are constant for the client's lifetime, so
        // set them (and the User-Agent GitHub requires) once as defaults.
        let mut auth =
            HeaderValue::from_str(&format!("Bearer {}", token.value)).with_context(|| {
                format!(
                    "invalid GitHub token from {} (bad header bytes)",
                    token.source
                )
            })?;
        auth.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, auth);
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static("2022-11-28"),
        );

        let http = client_builder()
            .user_agent(UA)
            .default_headers(headers)
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http: RetryingClient::new(http, MAX_RETRIES, RETRY_DELAY),
            config,
            base,
            token_source: token.source,
            token: token.value.trim().to_string(),
        })
    }

    fn token_refused(&self) -> SourceError {
        SourceError::ReauthRequired(format!(
            "GitHub returned 401 — the token from {} is invalid or expired.",
            self.token_source
        ))
    }

    /// The error for an unsuccessful (non-401) response to the `what` request:
    /// [`SourceError::RateLimited`] when [`rate_limit_message`] recognises one, else the
    /// status with an excerpt of the body, the token redacted.
    async fn refusal(&self, what: &str, resp: reqwest::Response) -> SourceError {
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.text().await.unwrap_or_default();
        if let Some(message) = rate_limit_message(status, &headers, &body) {
            return SourceError::RateLimited(message);
        }
        crate::http::error_with_body(&format!("{what} returned {status}"), &body, &[&self.token])
            .into()
    }

    /// Look up a repo via `/repos/{owner}/{name}` (the API follows renames and
    /// transfers, returning the current name/URL). `None` if it is permanently
    /// inaccessible — deleted or private (404), or blocked under the DMCA (451).
    async fn repo(&self, owner: &str, name: &str) -> Result<Option<GitHubRepo>, SourceError> {
        let endpoint = format!("{}/repos/{}/{}", self.base, owner, name);
        let resp = self
            .http
            .send("github repo", || self.http.get(&endpoint))
            .await?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(self.token_refused());
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        // Unlike a 404 the repo does still exist, so say so rather than skipping silently.
        if status == reqwest::StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS {
            warn!(
                "github repo {owner}/{name} is blocked (451); keeping its stored title, \
                 notes and tags"
            );
            return Ok(None);
        }
        if !status.is_success() {
            return Err(self.refusal("github repo", resp).await);
        }
        let repo: GitHubRepo = resp.json().await.context("parsing github repo response")?;
        Ok(Some(repo))
    }
}

impl Source for GitHubClient {
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError> {
        self.fetch_capturing(None, &mut RawSink::disabled()).await
    }

    /// Stars list newest first, so paging stops at the first page that would give `sync`
    /// nothing to write.
    async fn fetch_incremental(
        &self,
        existing: &[Bookmark],
    ) -> Result<Vec<BookmarkDraft>, SourceError> {
        self.fetch_capturing(Some(&self.synced_keys(existing)), &mut RawSink::disabled())
            .await
    }
}

impl GitHubClient {
    /// [`Source::fetch`], pushing each `/user/starred` page body to `sink` as it arrives
    /// so `backup` gets the verbatim response — including the ~75 repo fields
    /// [`GitHubStarredRepo`] doesn't keep — out of the same traversal. With
    /// `synced_keys`, it stops at the first page with nothing new ([`page_is_caught_up`]).
    async fn fetch_capturing(
        &self,
        synced_keys: Option<&HashSet<String>>,
        sink: &mut RawSink,
    ) -> Result<Vec<BookmarkDraft>, SourceError> {
        let endpoint = format!("{}/user/starred", self.base);
        let mut out = Vec::new();
        let mut page: u32 = 1;
        let mut visited = HashSet::new();

        loop {
            if page > MAX_STARRED_PAGES {
                warn!(
                    "github starred: hit the {MAX_STARRED_PAGES}-page cap; \
                     stopping (some stars may be missing)"
                );
                sink.mark_truncated(format!(
                    "github starred stopped at the {MAX_STARRED_PAGES}-page cap"
                ));
                break;
            }
            if !visited.insert(page) {
                warn!("github starred: 'next' link looped back to page {page}; stopping");
                break;
            }
            let resp = self
                .http
                .send("github starred", || {
                    // The star+json media type makes each element `{ starred_at, repo }`, so
                    // we can date each bookmark by when it was starred.
                    self.http
                        .get(&endpoint)
                        .header(reqwest::header::ACCEPT, "application/vnd.github.star+json")
                        .query(&[
                            ("sort", "created"),
                            ("direction", "desc"),
                            ("per_page", STARRED_PER_PAGE),
                        ])
                        .query(&[("page", page)])
                })
                .await?;

            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                return Err(self.token_refused());
            }
            let next = resp
                .headers()
                .get(reqwest::header::LINK)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_link_next);
            if !status.is_success() {
                return Err(self.refusal("github starred", resp).await);
            }

            // Deserialize element-by-element so one malformed repo is skipped with a
            // warning rather than discarding this page (and every earlier one). A body
            // that isn't a JSON array at all still fails the whole page; a non-empty page
            // where every element fails is a schema break (see `deserialize_lenient`).
            let body = resp
                .text()
                .await
                .context("reading github starred response")?;
            sink.push("", RawKind::Json, &body);
            let elements: Vec<serde_json::Value> =
                serde_json::from_str(&body).context("parsing github starred response")?;
            let repos: Vec<GitHubStarredRepo> =
                deserialize_lenient(elements, "github starred element", |count| {
                    SourceError::Other(anyhow::anyhow!(
                        "github starred page {page}: all {count} element(s) failed to \
                         deserialize — the API response shape may have changed"
                    ))
                })?;
            let page_len = repos.len();
            let page_drafts: Vec<BookmarkDraft> = repos
                .into_iter()
                .filter_map(|starred| starred.into_draft(&self.config))
                .collect();
            let caught_up =
                synced_keys.is_some_and(|keys| page_is_caught_up(page_len, &page_drafts, keys));
            out.extend(page_drafts);
            if caught_up {
                debug!("github starred: page {page} has nothing new; stopping");
                break;
            }

            match next {
                Some(p) => page = p,
                None => break,
            }
        }
        Ok(out)
    }
}

impl BackupSource for GitHubClient {
    async fn dump(&self) -> Result<BackupDump, SourceError> {
        let mut sink = RawSink::collecting();
        let drafts = self.fetch_capturing(None, &mut sink).await?;
        Ok(BackupDump::from_drafts(sink, &drafts))
    }
}

impl UrlKey for GitHubClient {
    /// The generic host+path key, gated to github.com hosts so a non-github bookmark
    /// never produces a key. The `www.` alias is folded onto the bare host so a
    /// `www.github.com/owner/repo` bookmark dedups against a `github.com/owner/repo`
    /// star -- the same notion of "github repo host" canonicalization uses.
    fn dedup_key(&self, url: &Url) -> Option<String> {
        let key = url_key(url).filter(|_| url.host_is("github.com"))?;
        Some(match key.strip_prefix("www.") {
            Some(stripped) => stripped.to_string(),
            None => key,
        })
    }
}

/// Extract the `page` number of the `rel="next"` link from a GitHub `Link` header,
/// or `None` when there is no next page.
fn parse_link_next(link: &str) -> Option<u32> {
    let next = link.split(',').find(|part| part.contains("rel=\"next\""))?;
    let url = next.split('<').nth(1)?.split('>').next()?;
    url.split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("page="))?
        .parse()
        .ok()
}

#[cfg(test)]
const TEST_TOKEN_SOURCE: &str = "$GITHUB_TOKEN_FILE";

#[cfg(test)]
impl GitHubClient {
    pub(crate) fn with_base_url(token: String, config: GitHubConfig, base: String) -> Self {
        let token = ConfiguredSecret {
            value: token,
            source: TEST_TOKEN_SOURCE,
        };
        Self::build(token, config, base).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Header-level cases for [`rate_limit_message`], kept off the network: a 429 through
    /// `send_retrying` would spend the full retry budget (~12s) before the body is ever
    /// read, which is too slow to pay on every `cargo test`.
    fn headers(pairs: &[(&str, &str)]) -> reqwest::header::HeaderMap {
        let mut map = reqwest::header::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                reqwest::header::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn retry_after_as_a_date_is_not_rendered_as_seconds() {
        // RFC 7231 allows an HTTP-date, which must not be suffixed into "…GMTs".
        let message = rate_limit_message(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            &headers(&[("retry-after", "Sun, 30 Aug 2026 15:00:00 GMT")]),
            "",
        )
        .expect("retry-after marks a rate limit");
        assert!(!message.contains("GMTs"), "{message}");
        assert!(message.contains("Sun, 30 Aug 2026"), "{message}");
    }

    #[test]
    fn retry_after_as_seconds_reads_as_a_duration() {
        let message = rate_limit_message(
            reqwest::StatusCode::FORBIDDEN,
            &headers(&[("retry-after", "60")]),
            "",
        )
        .expect("retry-after marks a rate limit");
        assert!(message.contains("60s"), "{message}");
    }

    #[test]
    fn a_success_is_never_a_rate_limit() {
        // The status gate matters: a 200 carrying an exhausted budget is the *last*
        // request that succeeded, not a refusal.
        assert_eq!(
            rate_limit_message(
                reqwest::StatusCode::OK,
                &headers(&[("x-ratelimit-remaining", "0")]),
                "",
            ),
            None
        );
    }

    #[test]
    fn an_unrelated_403_body_is_not_a_rate_limit() {
        assert_eq!(
            rate_limit_message(
                reqwest::StatusCode::FORBIDDEN,
                &headers(&[("x-ratelimit-remaining", "4999")]),
                r#"{"message":"Must have admin rights to Repository."}"#,
            ),
            None
        );
    }

    fn repo(value: serde_json::Value) -> GitHubRepo {
        serde_json::from_value(value).unwrap()
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
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
        .into_draft(&GitHubConfig::default())
        .unwrap();

        assert_eq!(d.bookmark.url.as_str(), "https://github.com/owner/Repo");
        assert_eq!(d.bookmark.title, "owner/Repo");
        assert_eq!(
            d.bookmark.note,
            "<blockquote>A thing</blockquote>\n\nProject homepage: https://example.com"
        );
        assert_eq!(d.bookmark.tags, vec!["github-star", "lang:rust"]);
        assert_eq!(d.dedup_key, "github.com/owner/repo");
    }

    #[test]
    fn into_draft_skips_repo_with_unparseable_url() {
        // A repo whose `html_url` doesn't parse is dropped rather than producing a draft.
        assert!(repo(json!({ "full_name": "o/r", "html_url": "not a url" }))
            .into_draft(&GitHubConfig::default())
            .is_none());
    }

    #[test]
    fn github_extended_wraps_description_and_keeps_homepage_outside() {
        assert_eq!(
            github_extended(Some("A thing"), Some("https://h"), "https://github.com/o/r"),
            "<blockquote>A thing</blockquote>\n\nProject homepage: https://h"
        );
        // No description: fall back to the URL, no blockquote.
        assert_eq!(
            github_extended(None, None, "https://github.com/o/r"),
            "https://github.com/o/r"
        );
        assert_eq!(
            github_extended(Some(""), None, "https://github.com/o/r"),
            "https://github.com/o/r"
        );
    }

    #[test]
    fn language_with_spaces_is_slugged() {
        // GitHub languages like "Jupyter Notebook" must not produce a space in the
        // tag (Pinboard would split it into two tags).
        let d = repo(json!({
            "full_name": "o/r", "html_url": "https://github.com/o/r",
            "language": "Jupyter Notebook"
        }))
        .into_draft(&GitHubConfig::default())
        .unwrap();
        assert_eq!(
            d.bookmark.tags,
            vec!["github-star", "lang:jupyter-notebook"]
        );
    }

    #[test]
    fn draft_without_description_or_language() {
        let d = repo(json!({
            "full_name": "o/r",
            "html_url": "https://github.com/o/r",
        }))
        .into_draft(&GitHubConfig::default())
        .unwrap();
        // Missing description falls back to the URL; no language → no lang tag.
        assert_eq!(d.bookmark.note, "https://github.com/o/r");
        assert_eq!(d.bookmark.tags, vec!["github-star"]);
    }

    #[test]
    fn tags_list_replaces_the_default_base() {
        let cfg = GitHubConfig {
            tags: vec!["github-star".into(), "account:work".into()],
            ..GitHubConfig::default()
        };
        let d = repo(json!({ "full_name": "o/r", "html_url": "https://github.com/o/r" }))
            .into_draft(&cfg)
            .unwrap();
        assert_eq!(d.bookmark.tags, vec!["github-star", "account:work"]);
    }

    #[test]
    fn parse_link_next_extracts_next_page() {
        let link = "<https://api.github.com/user/starred?sort=created&page=2>; rel=\"next\", \
                    <https://api.github.com/user/starred?sort=created&page=5>; rel=\"last\"";
        assert_eq!(parse_link_next(link), Some(2));
        let with_per_page = "<https://api.github.com/user/starred?sort=created&direction=desc\
                             &per_page=100&page=3>; rel=\"next\"";
        assert_eq!(parse_link_next(with_per_page), Some(3));
        // No next link (last page).
        assert_eq!(
            parse_link_next("<https://api.github.com/user/starred?page=1>; rel=\"prev\""),
            None
        );
    }

    #[test]
    fn a_starred_page_is_caught_up_when_its_stars_are_on_pinboard() {
        use crate::test_support::stored_at;
        let star = |name: &str, html_url: &str| -> GitHubStarredRepo {
            serde_json::from_value(json!({ "starred_at": "2023-01-02T03:04:05Z",
                "repo": { "full_name": name, "html_url": html_url } }))
            .unwrap()
        };
        let repo = |name: &str| star(name, &format!("https://github.com/{name}"));
        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), "unused".into());
        let synced_keys = client.synced_keys(&[
            stored_at("https://github.com/a/one"),
            stored_at("https://www.github.com/b/two"),
        ]);
        let caught_up = |page: Vec<GitHubStarredRepo>| {
            let page_len = page.len();
            let page_drafts: Vec<BookmarkDraft> = page
                .into_iter()
                .filter_map(|starred| starred.into_draft(&GitHubConfig::default()))
                .collect();
            page_is_caught_up(page_len, &page_drafts, &synced_keys)
        };

        assert!(
            caught_up(vec![repo("A/One"), repo("b/two")]),
            "stars already on Pinboard, in any case, with or without www."
        );
        assert!(!caught_up(vec![repo("a/one"), repo("c/new")]), "a new star");
        assert!(
            caught_up(vec![repo("a/one"), star("d/bad", "not a url")]),
            "a star that makes no draft adds nothing"
        );
    }

    #[test]
    fn an_unsendable_token_names_its_setting_but_never_the_token() {
        let token = ConfiguredSecret {
            value: "SECRET\nTOKEN".into(),
            source: "the config's `token`",
        };
        let Err(err) = GitHubClient::new(token, GitHubConfig::default()) else {
            panic!("a token with a newline cannot be a header");
        };
        let message = format!("{err:#}");
        assert!(message.contains("the config's `token`"), "{message}");
        assert!(!message.contains("SECRET"), "{message}");
    }

    #[test]
    fn dedup_key_only_matches_github() {
        let c = GitHubClient::with_base_url("t".into(), GitHubConfig::default(), API_BASE.into());
        assert_eq!(
            c.dedup_key(&url("https://github.com/o/r")).as_deref(),
            Some("github.com/o/r")
        );
        assert!(c.dedup_key(&url("https://example.com/o/r")).is_none());
        // The www. alias folds onto the bare host so it dedups against a github.com
        // star with the same owner/repo.
        assert_eq!(
            c.dedup_key(&url("https://www.github.com/o/r")).as_deref(),
            Some("github.com/o/r")
        );
        // Other subdomains keep their own host in the key (they are not repo stars).
        assert_eq!(
            c.dedup_key(&url("https://gist.github.com/user/abcd1234"))
                .as_deref(),
            Some("gist.github.com/user/abcd1234")
        );
    }
}

/// Integration tests against a `wiremock` server. These bind a TCP socket, so they
/// can't run in the Nix build sandbox — the flake skips `net_tests` there.
#[cfg(test)]
mod net_tests {
    use super::*;
    use crate::bookmark::{AccountView, CleanupStore};
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
        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

        let err = client.repo("o", "r").await.unwrap_err();
        assert!(
            matches!(err, SourceError::RateLimited(_)),
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
        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

        let err = client.repo("o", "r").await.unwrap_err();
        assert!(
            matches!(err, SourceError::RateLimited(_)),
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
        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

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
        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

        let err = client.repo("o", "r").await.unwrap_err();
        assert!(
            matches!(err, SourceError::RateLimited(_)),
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
        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

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
        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

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

        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
        let drafts = client.fetch().await.unwrap();
        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[0].bookmark.title, "a/one");
        assert_eq!(drafts[0].bookmark.tags, vec!["github-star", "lang:rust"]);
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

        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
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

        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
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

        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
        assert!(matches!(client.fetch().await, Err(SourceError::Other(_))));
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

        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
        assert!(client.fetch().await.unwrap().is_empty());
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
            let SourceError::ReauthRequired(message) = &err else {
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
            .respond_with(
                ResponseTemplate::new(422).set_body_string("bad request: Bearer SECRET422"),
            )
            .mount(&server)
            .await;
        let client =
            GitHubClient::with_base_url("SECRET422 ".into(), Default::default(), server.uri());
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
        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

        assert!(client.repo("deleted", "repo").await.unwrap().is_none());
        assert!(
            client.repo("blocked", "repo").await.unwrap().is_none(),
            "a DMCA block is permanent, so it must not surface as an error"
        );
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

        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
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

        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
        let dump = client.dump().await.unwrap();
        assert_eq!(dump.raw.len(), 2, "one captured body per page");

        // GitHub's pages are arrays, so they flatten into one array of repos.
        let bodies: Vec<&str> = dump.raw.iter().map(|p| p.body.as_str()).collect();
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

    fn star_client(server: &MockServer) -> GitHubClient {
        GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri())
    }

    fn titles(drafts: &[BookmarkDraft]) -> Vec<&str> {
        drafts.iter().map(|d| d.bookmark.title.as_str()).collect()
    }

    #[tokio::test]
    async fn incremental_fetch_stops_at_the_first_page_with_nothing_new() {
        let server = MockServer::start().await;
        three_star_pages(&server, [1, 1, 0]).await;

        let existing = [crate::test_support::stored_at("https://github.com/s/o")];
        let drafts = star_client(&server)
            .fetch_incremental(&existing)
            .await
            .unwrap();
        assert_eq!(titles(&drafts), ["s/n", "s/o"]);
    }

    #[tokio::test]
    async fn incremental_fetch_reads_on_while_every_page_has_something_new() {
        let server = MockServer::start().await;
        three_star_pages(&server, [1, 1, 1]).await;

        let drafts = star_client(&server).fetch_incremental(&[]).await.unwrap();
        assert_eq!(titles(&drafts), ["s/n", "s/o", "s/p"]);
    }

    #[tokio::test]
    async fn dump_reads_every_star_page() {
        let server = MockServer::start().await;
        three_star_pages(&server, [1, 1, 1]).await;

        let dump = star_client(&server).dump().await.unwrap();
        assert_eq!(dump.raw.len(), 3);
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
        let bookmarks = pinboard.all.clone();

        cleanup::run(
            &CleanupStore::new(&pinboard, AccountView::new(bookmarks), false),
            &star_client(&server),
            &GitHubConfig::default(),
            DateOpts {
                use_post_date: true,
                max_age_days: 30,
                stale_to_now: false,
            },
        )
        .await
        .unwrap();
    }
}
