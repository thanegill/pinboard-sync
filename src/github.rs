//! The GitHub source: reads the authenticated user's starred repositories
//! (`/user/starred`, token-authenticated) and shapes each into a Pinboard draft.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use log::warn;
use serde::Deserialize;

use crate::bookmark::{Bookmark, BookmarkStore};
use crate::cleanup_pass::{run_pass, CleanupPass, DateOpts, Plan};
use crate::htmltext::{blockquote, html_to_plain};
use crate::http::send_retrying;
use crate::source::{
    deserialize_lenient, extend_unique, push_prefixed, push_tags, url_key, BookmarkDraft, Source,
    SourceError, UrlExt, UrlKey,
};
use url::Url;

const UA: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const API_BASE: &str = "https://api.github.com";
const MAX_RETRIES: u32 = 4;
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// Backstop against a malformed `Link: rel="next"` chain that never terminates.
/// At GitHub's default 30 results per page this covers hundreds of thousands of stars,
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
    http: reqwest::Client,
    config: GitHubConfig,
    /// API base (overridden in tests).
    base: String,
}

impl GitHubClient {
    pub fn new(token: String, config: GitHubConfig) -> anyhow::Result<Self> {
        Self::build(token, config, API_BASE.to_string())
    }

    fn build(token: String, config: GitHubConfig, base: String) -> anyhow::Result<Self> {
        use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION};

        // The auth/accept/version headers are constant for the client's lifetime, so
        // set them (and the User-Agent GitHub requires) once as defaults.
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))
            .context("invalid GitHub token (bad header bytes)")?;
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

        let http = reqwest::Client::builder()
            .user_agent(UA)
            .default_headers(headers)
            .build()
            .context("building HTTP client")?;
        Ok(Self { http, config, base })
    }

    /// Look up a repo via `/repos/{owner}/{name}` (the API follows renames and
    /// transfers, returning the current name/URL). `None` if it is permanently
    /// inaccessible — deleted or private (404), or blocked under the DMCA (451).
    async fn repo(&self, owner: &str, name: &str) -> Result<Option<GitHubRepo>, SourceError> {
        let endpoint = format!("{}/repos/{}/{}", self.base, owner, name);
        let resp = send_retrying("github repo", MAX_RETRIES, RETRY_DELAY, || {
            self.http.get(&endpoint)
        })
        .await?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(SourceError::ReauthRequired(
                "GitHub returned 401 — the token (GITHUB_TOKEN) is invalid or expired.".to_string(),
            ));
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
            let headers = resp.headers().clone();
            let body = resp.text().await.unwrap_or_default();
            if let Some(message) = rate_limit_message(status, &headers, &body) {
                return Err(SourceError::RateLimited(message));
            }
            return Err(anyhow::anyhow!("github repo returned {status}: {}", body.trim()).into());
        }
        let repo: GitHubRepo = resp.json().await.context("parsing github repo response")?;
        Ok(Some(repo))
    }
}

impl Source for GitHubClient {
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError> {
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
                break;
            }
            if !visited.insert(page) {
                warn!("github starred: 'next' link looped back to page {page}; stopping");
                break;
            }
            let resp = send_retrying("github starred", MAX_RETRIES, RETRY_DELAY, || {
                // The star+json media type makes each element `{ starred_at, repo }`, so
                // we can date each bookmark by when it was starred.
                self.http
                    .get(&endpoint)
                    .header(reqwest::header::ACCEPT, "application/vnd.github.star+json")
                    .query(&[("sort", "created")])
                    .query(&[("page", page)])
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
                let headers = resp.headers().clone();
                let body = resp.text().await.unwrap_or_default();
                if let Some(message) = rate_limit_message(status, &headers, &body) {
                    return Err(SourceError::RateLimited(message));
                }
                return Err(
                    anyhow::anyhow!("github starred returned {status}: {}", body.trim()).into(),
                );
            }

            // Deserialize element-by-element so one malformed repo is skipped with a
            // warning rather than discarding this page (and every earlier one). A body
            // that isn't a JSON array at all still fails the whole page; a non-empty page
            // where every element fails is a schema break (see `deserialize_lenient`).
            let body = resp
                .text()
                .await
                .context("reading github starred response")?;
            let elements: Vec<serde_json::Value> =
                serde_json::from_str(&body).context("parsing github starred response")?;
            let repos: Vec<GitHubStarredRepo> =
                deserialize_lenient(elements, "github starred element", |count| {
                    SourceError::Other(anyhow::anyhow!(
                        "github starred page {page}: all {count} element(s) failed to \
                         deserialize — the API response shape may have changed"
                    ))
                })?;
            for starred in repos {
                out.extend(starred.into_draft(&self.config));
            }

            match next {
                Some(p) => page = p,
                None => break,
            }
        }
        Ok(out)
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

/// Options for `cleanup github`.
pub struct GitHubCleanupOpts {
    pub dry_run: bool,
    /// Re-date bookmarks to when the repo was starred (within the age cap). Since the
    /// per-repo lookup doesn't carry `starred_at`, this fetches the star list to map it.
    pub use_post_date: bool,
    /// Backdate age cap, in days.
    pub max_age_days: u64,
    /// Re-date repos starred longer ago than the cap to "now" instead of leaving them.
    pub cleanup_stale_to_now: bool,
}

impl GitHubCleanupOpts {
    fn date_opts(&self) -> DateOpts {
        DateOpts {
            use_post_date: self.use_post_date,
            max_age_days: self.max_age_days,
            stale_to_now: self.cleanup_stale_to_now,
        }
    }
}

/// Normalize existing GitHub repo bookmarks. Only genuine repo-root URLs on the bare
/// `github.com` host (`owner/repo`) are touched; deep links (issues/PRs/blob/tree) and
/// gist or other subdomains are left alone. For each repo root, look it up via the API
/// (which follows renames/transfers), rewriting a moved repo's URL to the current one,
/// setting the title to the current `owner/repo`, and refreshing the `lang:` tag. A
/// URL change updates + deletes the old; notes are kept and creation time preserved.
/// A repo that no longer exists (404) keeps just the URL canonicalization.
pub async fn cleanup<P: BookmarkStore>(
    pinboard: &P,
    client: &GitHubClient,
    config: &GitHubConfig,
    opts: &GitHubCleanupOpts,
    bookmarks: &[Bookmark],
) -> Result<(), SourceError> {
    let gh_bms: Vec<_> = bookmarks
        .iter()
        .filter(|bookmark| bookmark.url.host_is("github.com"))
        .cloned()
        .collect();

    // `starred_at` isn't on the per-repo lookup, so for use_post_date fetch the star
    // list once and map url_key -> star epoch. A repo no longer starred won't be here
    // (it keeps its existing time).
    let star_dates: HashMap<String, i64> = if opts.use_post_date {
        client
            .fetch()
            .await?
            .into_iter()
            .filter_map(|d| {
                let key = url_key(&d.bookmark.url)?;
                Some((key, d.bookmark.timestamp?.unix_timestamp()))
            })
            .collect()
    } else {
        HashMap::new()
    };

    let pass = GitHubCleanupPass {
        client,
        config,
        star_dates,
    };
    run_pass(
        pinboard,
        &gh_bms,
        opts.dry_run,
        "github",
        opts.date_opts(),
        &pass,
    )
    .await
    .into_result()
}

/// Re-shapes one GitHub repo-root bookmark: skip anything that isn't a bare
/// `github.com/owner/repo` URL, else canonicalize it and refresh from the API (which
/// follows renames/transfers) — current URL/title, rebuilt `<blockquote>` notes, and
/// language tag. A 404 keeps just the canonicalization.
struct GitHubCleanupPass<'a> {
    client: &'a GitHubClient,
    config: &'a GitHubConfig,
    star_dates: HashMap<String, i64>,
}

impl CleanupPass for GitHubCleanupPass<'_> {
    async fn plan(&self, bookmark: &Bookmark) -> Result<Plan, SourceError> {
        // Only genuine repo-root URLs on the bare github.com host are normalized;
        // deep links (issues/PRs/blob/tree) and gist/other subdomains are left untouched
        // without an API call, so they say nothing about whether GitHub is reachable.
        let Some((owner, repo)) = repo_root(&bookmark.url) else {
            return Ok(Plan::Skipped);
        };
        // Default to the canonicalization, then refresh from the API when the repo
        // still exists (a 404 keeps just the canonical URL). We already hold the
        // parsed `(owner, repo)`, so build the canonical URL from them directly
        // rather than re-validating the host/path through `canonical_repo_url`.
        let mut url = Url::parse(&format!("https://github.com/{owner}/{repo}"))
            .context("building the canonical github URL")?;
        let mut title = html_to_plain(&bookmark.title);
        let mut note = bookmark.note.clone();
        let mut tags = bookmark.tags.clone();
        match self.client.repo(&owner, &repo).await {
            Ok(Some(info)) => {
                title = html_to_plain(&info.full_name);
                // Rebuild the notes from fresh data so old bookmarks retrofit to the
                // <blockquote> shape (sync skips already-present bookmarks).
                note = github_extended(
                    info.description.as_deref(),
                    info.homepage.as_deref(),
                    &info.html_url,
                );
                tags = refresh_tags(bookmark.tags.clone(), &info, self.config);
                url = Url::parse(&info.html_url).unwrap_or(url); // follows renames/transfers
            }
            Ok(None) => {}
            // A failed lookup is surfaced to the driver, which logs and counts it
            // (or, for a dead credential, aborts the pass).
            Err(e) => return Err(e),
        }

        let timestamp = url_key(&url)
            .and_then(|k| self.star_dates.get(&k).copied())
            .and_then(crate::timefmt::from_unix);
        Ok(Plan::Bookmark(Bookmark {
            url,
            title,
            note,
            tags,
            timestamp,
            public: bookmark.public,
            read_later: bookmark.read_later,
        }))
    }
}

/// Whether `host` is GitHub's repo web host: the bare `github.com` or its `www.`
/// alias (the same pages). Other subdomains -- `gist.github.com`, `api.github.com`,
/// `raw.githubusercontent.com` -- are distinct sites and are not repo hosts.
fn is_repo_host(host: &str) -> bool {
    matches!(host, "github.com" | "www.github.com")
}

/// The `(owner, repo)` of a genuine repo-root URL: the host must be the `github.com`
/// repo host (bare or the `www.` alias, not another subdomain) and the path exactly
/// two non-empty segments, ignoring a trailing slash and an optional `.git` suffix.
/// `None` for anything else -- a deep link (`/issues`, `/tree/...`), a gist or other
/// subdomain, or a non-GitHub host -- which cleanup leaves untouched.
fn repo_root(url: &Url) -> Option<(String, String)> {
    if !is_repo_host(url.host_str()?) {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    if segments.len() != 2 {
        return None;
    }
    let owner = segments[0].to_string();
    let repo = segments[1]
        .strip_suffix(".git")
        .unwrap_or(segments[1])
        .to_string();
    Some((owner, repo))
}

/// Refresh the language tag from the current repo, keeping existing tags and
/// ensuring the base tags: drop any old `lang_prefix` tag, then re-add the base
/// tags and the current `lang:` tag.
fn refresh_tags(existing: Vec<String>, repo: &GitHubRepo, cfg: &GitHubConfig) -> Vec<String> {
    let mut tags: Vec<String> = existing
        .into_iter()
        .filter(|t| cfg.lang_prefix.is_empty() || !t.starts_with(&cfg.lang_prefix))
        .collect();
    extend_unique(&mut tags, &cfg.tags);
    if let Some(lang) = repo.language.clone().filter(|s| !s.is_empty()) {
        push_prefixed(&mut tags, &cfg.lang_prefix, &lang.to_lowercase());
    }
    tags
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
    fn with_base_url(token: String, config: GitHubConfig, base: String) -> Self {
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
    fn repo_root_only_matches_bare_github_repo_roots() {
        // Bare owner/repo on github.com, with .git and trailing slash tolerated.
        assert_eq!(
            repo_root(&url("https://github.com/Owner/Repo")),
            Some(("Owner".into(), "Repo".into()))
        );
        assert_eq!(
            repo_root(&url("https://github.com/Owner/Repo.git")),
            Some(("Owner".into(), "Repo".into()))
        );
        assert_eq!(
            repo_root(&url("https://github.com/Owner/Repo/")),
            Some(("Owner".into(), "Repo".into()))
        );
        // The www. alias is the same repo host as the bare github.com.
        assert_eq!(
            repo_root(&url("https://www.github.com/Owner/Repo")),
            Some(("Owner".into(), "Repo".into()))
        );
        // Deep links, other subdomains, and non-GitHub hosts are not repo roots.
        assert_eq!(repo_root(&url("https://github.com/o/r/issues/5")), None);
        assert_eq!(repo_root(&url("https://github.com/o/r/tree/main")), None);
        assert_eq!(
            repo_root(&url("https://gist.github.com/user/abcd1234")),
            None
        );
        assert_eq!(repo_root(&url("https://api.github.com/o/r")), None);
        assert_eq!(repo_root(&url("https://github.com/o")), None);
        assert_eq!(repo_root(&url("https://example.com/o/r")), None);
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
    fn dedup_key_only_matches_github() {
        let c = GitHubClient::new("t".into(), GitHubConfig::default()).unwrap();
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
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// An expired token during the pass has to reach the caller as `ReauthRequired`, or
    /// `main` cannot tell it apart from any other failure and the auth-failure hook
    /// never fires for `cleanup`.
    #[tokio::test]
    async fn cleanup_surfaces_an_expired_token_as_reauth() {
        use crate::pinboard::PinboardBookmark;
        use crate::test_support::FakePinboard;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r"))
            .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"message":"Bad creds"}"#))
            .mount(&server)
            .await;

        let pinboard = FakePinboard {
            all: vec![PinboardBookmark {
                url: "https://github.com/o/r".into(),
                description: "o/r".into(),
                extended: "notes".into(),
                tags: "github-star".into(),
                time: "2020-01-01T00:00:00Z".into(),
                shared: "no".into(),
                toread: "no".into(),
            }
            .try_into()
            .unwrap()],
            ..Default::default()
        };
        let bookmarks = pinboard.all.clone();
        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

        let err = cleanup(
            &pinboard,
            &client,
            &GitHubConfig::default(),
            &GitHubCleanupOpts {
                dry_run: false,
                use_post_date: false,
                max_age_days: 30,
                cleanup_stale_to_now: false,
            },
            &bookmarks,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, SourceError::ReauthRequired(_)),
            "expected reauth, got {err:?}"
        );
    }

    /// End to end: a rate limit part-way through stops the pass rather than spending a
    /// doomed request on every remaining bookmark, while keeping the work already done.
    #[tokio::test]
    async fn cleanup_stops_at_a_rate_limit_and_keeps_what_it_did() {
        use crate::pinboard::PinboardBookmark;
        use crate::test_support::FakePinboard;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/old/name"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "full_name": "new/name",
                "html_url": "https://github.com/new/name",
                "description": "Still here",
                "language": "Rust"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/limited/repo"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("x-ratelimit-remaining", "0")
                    .insert_header("x-ratelimit-reset", "1788102000")
                    .set_body_string(r#"{"message":"API rate limit exceeded"}"#),
            )
            .mount(&server)
            .await;
        // The point of the test: this one must never be requested. An unmounted path
        // would not prove it — wiremock answers those with a 404, which `repo()` reads as
        // a deleted repo and skips silently — so assert zero calls explicitly. The
        // expectation is checked when the server drops.
        Mock::given(method("GET"))
            .and(path("/repos/never/looked"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let stored = |url: &str, title: &str| {
            PinboardBookmark {
                url: url.into(),
                description: title.into(),
                extended: "notes".into(),
                tags: "github-star".into(),
                time: "2020-01-01T00:00:00Z".into(),
                shared: "no".into(),
                toread: "no".into(),
            }
            .try_into()
            .unwrap()
        };
        let pinboard = FakePinboard {
            all: vec![
                stored("https://github.com/old/name", "old/name"),
                stored("https://github.com/limited/repo", "limited/repo"),
                stored("https://github.com/never/looked", "never/looked"),
            ],
            ..Default::default()
        };
        let bookmarks = pinboard.all.clone();
        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

        let err = cleanup(
            &pinboard,
            &client,
            &GitHubConfig::default(),
            &GitHubCleanupOpts {
                dry_run: false,
                use_post_date: false,
                max_age_days: 30,
                cleanup_stale_to_now: false,
            },
            &bookmarks,
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("rate limit"), "{err}");
        // The repo read before the limit is still rewritten — the work isn't thrown away.
        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://github.com/new/name");
    }

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
    async fn fetch_paginates_via_link_header_and_sends_token() {
        let server = MockServer::start().await;
        // Page 1 advertises a next page via the Link header.
        Mock::given(method("GET"))
            .and(path("/user/starred"))
            .and(query_param("page", "1"))
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
    async fn maps_401_to_reauth() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad creds"))
            .mount(&server)
            .await;
        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
        assert!(matches!(
            client.fetch().await,
            Err(SourceError::ReauthRequired(_))
        ));
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
    async fn cleanup_survives_a_permanently_blocked_repo() {
        use crate::pinboard::PinboardBookmark;
        use crate::test_support::FakePinboard;

        // A DMCA-blocked repo answers 451 forever, so the run must still succeed on the
        // strength of the repos it *could* read.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/blocked/repo"))
            .respond_with(ResponseTemplate::new(451).set_body_string(
                r#"{"message":"Repository access blocked","block":{"reason":"dmca"}}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/old/name"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "full_name": "new/name",
                "html_url": "https://github.com/new/name",
                "description": "Still here",
                "language": "Rust"
            })))
            .mount(&server)
            .await;

        let stored = |url: &str, title: &str| {
            PinboardBookmark {
                url: url.into(),
                description: title.into(),
                extended: "notes".into(),
                tags: "github-star".into(),
                time: "2020-01-01T00:00:00Z".into(),
                shared: "no".into(),
                toread: "no".into(),
            }
            .try_into()
            .unwrap()
        };
        let pinboard = FakePinboard {
            all: vec![
                stored("https://github.com/blocked/repo", "blocked/repo"),
                stored("https://github.com/old/name", "old/name"),
            ],
            ..Default::default()
        };
        let bookmarks = pinboard.all.clone();
        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());

        cleanup(
            &pinboard,
            &client,
            &GitHubConfig::default(),
            &GitHubCleanupOpts {
                dry_run: false,
                use_post_date: false,
                max_age_days: 30,
                cleanup_stale_to_now: false,
            },
            &bookmarks,
        )
        .await
        .expect("one dead repo must not fail the whole run");

        // The readable repo was still cleaned up.
        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://github.com/new/name");
    }

    #[tokio::test]
    async fn cleanup_refresh_rewrites_renamed_repo_and_language() {
        use crate::pinboard::PinboardBookmark;
        use crate::test_support::FakePinboard;

        // The repo was renamed old/name -> new/name, and its language changed.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/old/name"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "full_name": "new/name",
                "html_url": "https://github.com/new/name",
                "description": "A renamed thing",
                "language": "Rust"
            })))
            .mount(&server)
            .await;

        let pinboard = FakePinboard {
            all: vec![PinboardBookmark {
                url: "https://github.com/old/name".into(),
                description: "old/name".into(),
                extended: "notes".into(),
                tags: "github-star lang:python mine".into(),
                time: "2020-01-01T00:00:00Z".into(),
                shared: "no".into(),
                toread: "no".into(),
            }
            .try_into()
            .unwrap()],
            ..Default::default()
        };
        let bookmarks = pinboard.all.clone();
        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
        cleanup(
            &pinboard,
            &client,
            &GitHubConfig::default(),
            &GitHubCleanupOpts {
                dry_run: false,
                use_post_date: false,
                max_age_days: 30,
                cleanup_stale_to_now: false,
            },
            &bookmarks,
        )
        .await
        .unwrap();

        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://github.com/new/name");
        assert_eq!(updated[0].description, "new/name");
        // Notes rebuilt from fresh data: description wrapped in a <blockquote>.
        assert_eq!(
            updated[0].extended,
            "<blockquote>A renamed thing</blockquote>"
        );
        // Language tag refreshed (python → rust), base + user tags kept.
        assert!(updated[0].tags.contains(&"lang:rust".to_string()));
        assert!(!updated[0].tags.contains(&"lang:python".to_string()));
        assert!(updated[0].tags.contains(&"github-star".to_string()));
        assert!(updated[0].tags.contains(&"mine".to_string()));
        // Old URL deleted after the rewrite.
        assert_eq!(
            pinboard.deleted.borrow().as_slice(),
            &["https://github.com/old/name".to_string()]
        );
    }

    #[tokio::test]
    async fn cleanup_leaves_deep_links_and_gists_untouched() {
        use crate::pinboard::PinboardBookmark;
        use crate::test_support::FakePinboard;

        fn stored(url: &str) -> Bookmark {
            PinboardBookmark {
                url: url.into(),
                description: "title".into(),
                extended: "notes".into(),
                tags: "github-star".into(),
                time: "2020-01-01T00:00:00Z".into(),
                shared: "no".into(),
                toread: "no".into(),
            }
            .try_into()
            .unwrap()
        }

        // No mocks: a lookup of any of these would 404 (and any rewrite would show up
        // as an update), so an empty updated/deleted set proves they were skipped.
        let server = MockServer::start().await;
        let pinboard = FakePinboard {
            all: vec![
                stored("https://github.com/rust-lang/rust/issues/12345"),
                stored("https://github.com/rust-lang/rust/tree/master/library"),
                stored("https://gist.github.com/user/abcd1234"),
            ],
            ..Default::default()
        };
        let bookmarks = pinboard.all.clone();
        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
        cleanup(
            &pinboard,
            &client,
            &GitHubConfig::default(),
            &GitHubCleanupOpts {
                dry_run: false,
                use_post_date: false,
                max_age_days: 30,
                cleanup_stale_to_now: false,
            },
            &bookmarks,
        )
        .await
        .unwrap();

        assert!(pinboard.updated.borrow().is_empty());
        assert!(pinboard.deleted.borrow().is_empty());
    }

    #[tokio::test]
    async fn cleanup_canonicalizes_non_canonical_repo_root() {
        use crate::pinboard::PinboardBookmark;
        use crate::test_support::FakePinboard;

        // A repo-root URL with a .git suffix and trailing slash canonicalizes to the
        // bare owner/repo root.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/Owner/Repo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "full_name": "Owner/Repo",
                "html_url": "https://github.com/Owner/Repo"
            })))
            .mount(&server)
            .await;

        let pinboard = FakePinboard {
            all: vec![PinboardBookmark {
                url: "https://github.com/Owner/Repo.git/".into(),
                description: "Owner/Repo".into(),
                extended: "https://github.com/Owner/Repo".into(),
                tags: "github-star".into(),
                time: "2020-01-01T00:00:00Z".into(),
                shared: "no".into(),
                toread: "no".into(),
            }
            .try_into()
            .unwrap()],
            ..Default::default()
        };
        let bookmarks = pinboard.all.clone();
        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
        cleanup(
            &pinboard,
            &client,
            &GitHubConfig::default(),
            &GitHubCleanupOpts {
                dry_run: false,
                use_post_date: false,
                max_age_days: 30,
                cleanup_stale_to_now: false,
            },
            &bookmarks,
        )
        .await
        .unwrap();

        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://github.com/Owner/Repo");
        // The non-canonical original was deleted after the rewrite.
        assert_eq!(
            pinboard.deleted.borrow().as_slice(),
            &["https://github.com/Owner/Repo.git/".to_string()]
        );
    }

    #[tokio::test]
    async fn cleanup_folds_www_host_onto_github_com() {
        use crate::pinboard::PinboardBookmark;
        use crate::test_support::FakePinboard;

        // A www.github.com repo root is the same site as github.com: it is looked up
        // (owner/repo) and rewritten to the bare host, the old www URL deleted.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/Owner/Repo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "full_name": "Owner/Repo",
                "html_url": "https://github.com/Owner/Repo"
            })))
            .mount(&server)
            .await;

        let pinboard = FakePinboard {
            all: vec![PinboardBookmark {
                url: "https://www.github.com/Owner/Repo".into(),
                description: "Owner/Repo".into(),
                extended: "https://github.com/Owner/Repo".into(),
                tags: "github-star".into(),
                time: "2020-01-01T00:00:00Z".into(),
                shared: "no".into(),
                toread: "no".into(),
            }
            .try_into()
            .unwrap()],
            ..Default::default()
        };
        let bookmarks = pinboard.all.clone();
        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
        cleanup(
            &pinboard,
            &client,
            &GitHubConfig::default(),
            &GitHubCleanupOpts {
                dry_run: false,
                use_post_date: false,
                max_age_days: 30,
                cleanup_stale_to_now: false,
            },
            &bookmarks,
        )
        .await
        .unwrap();

        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://github.com/Owner/Repo");
        assert_eq!(
            pinboard.deleted.borrow().as_slice(),
            &["https://www.github.com/Owner/Repo".to_string()]
        );
    }

    #[tokio::test]
    async fn cleanup_rewrites_notes_only_to_retrofit_the_blockquote() {
        use crate::pinboard::PinboardBookmark;
        use crate::test_support::FakePinboard;

        // URL, title, and tags are already current; only the notes are stale (an old
        // unwrapped description). The extended_changed guard must still trigger a write.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "full_name": "o/r",
                "html_url": "https://github.com/o/r",
                "description": "A thing"
            })))
            .mount(&server)
            .await;

        let pinboard = FakePinboard {
            all: vec![PinboardBookmark {
                url: "https://github.com/o/r".into(),
                description: "o/r".into(),
                extended: "A thing".into(), // pre-blockquote notes
                tags: "github-star".into(),
                time: "2020-01-01T00:00:00Z".into(),
                shared: "no".into(),
                toread: "no".into(),
            }
            .try_into()
            .unwrap()],
            ..Default::default()
        };
        let bookmarks = pinboard.all.clone();
        let client =
            GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), server.uri());
        cleanup(
            &pinboard,
            &client,
            &GitHubConfig::default(),
            &GitHubCleanupOpts {
                dry_run: false,
                use_post_date: false,
                max_age_days: 30,
                cleanup_stale_to_now: false,
            },
            &bookmarks,
        )
        .await
        .unwrap();

        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].extended, "<blockquote>A thing</blockquote>");
        // Nothing else changed, so no delete (URL is unchanged).
        assert!(pinboard.deleted.borrow().is_empty());
    }
}
