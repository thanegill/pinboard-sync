//! The GitHub source: the token's starred repos (`/user/starred`) and, unless turned off,
//! starred gists (`/gists/starred`), each shaped into a draft.

pub mod cleanup;
pub mod gists;

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use anyhow::{Context, Result};
use log::{debug, warn};
use serde::Deserialize;
use time::OffsetDateTime;

use crate::backup::{BackupDump, BackupSource, RawKind, RawSink, PRIMARY_STREAM};
use crate::bookmark::Bookmark;
use crate::htmltext::{blockquote, html_to_plain};
use crate::http::{client_builder, read_body, RetryingClient, DEFAULT_BACKOFF, USER_AGENT};
use crate::keys::GitHubKeys;
use crate::secrets::ConfiguredSecret;
use crate::source::{
    deserialize_lenient, page_is_caught_up, BookmarkDraft, DedupKey, Halt, Source, SourceError,
    UrlKey,
};
use crate::tags::Tags;
use gists::{GistConfig, GitHubGist};
use url::Url;

const GITHUB_API_BASE: &str = "https://api.github.com";
/// The REST API version every request pins (`X-GitHub-Api-Version`).
const GITHUB_API_VERSION: &str = "2022-11-28";
/// The most items a GitHub listing returns per page; GitHub's default is 30.
const PER_PAGE: &str = "100";
/// Backstop against a `Link: rel="next"` chain that never ends: a million stars at
/// [`PER_PAGE`]. A repeating page is the `visited` guard's.
const MAX_PAGES: u32 = 10_000;

/// One `Link`-paged GitHub listing, walked by [`GitHubClient::walk`].
#[derive(Clone, Copy)]
struct PagedStream {
    /// Names the stream in messages and the request label.
    label: &'static str,
    /// The `backup` stream its pages are captured under (`""` is the primary).
    raw_stream: &'static str,
    path: &'static str,
    accept: &'static str,
    /// Sent with every page, besides `page` itself.
    query: &'static [(&'static str, &'static str)],
}

/// `/user/starred`. The star+json media type adds each star's `starred_at`, to date its
/// bookmark by. The sort is pinned, not left to GitHub's default: the incremental read
/// depends on newest-starred-first.
const STARS: PagedStream = PagedStream {
    label: "github starred",
    raw_stream: PRIMARY_STREAM,
    path: "/user/starred",
    accept: "application/vnd.github.star+json",
    query: &[
        ("sort", "created"),
        ("direction", "desc"),
        ("per_page", PER_PAGE),
    ],
};

/// `/gists/starred`, with no documented sort: the incremental read assumes
/// newest-starred-first, as every other stream lists.
const STARRED_GISTS: PagedStream = PagedStream {
    label: "github starred gists",
    raw_stream: "gists",
    path: "/gists/starred",
    accept: "application/vnd.github+json",
    query: &[("per_page", PER_PAGE)],
};

/// The tags for GitHub stars; an empty one is disabled.
#[derive(Debug, Clone)]
pub struct GitHubConfig {
    pub tags: Vec<String>,
    pub lang_prefix: String,
    /// Prefix for the tag naming the repo's owner.
    pub author_prefix: String,
    /// The starred gists' settings.
    pub gists: GistConfig,
}

impl Default for GitHubConfig {
    fn default() -> Self {
        let tags = vec!["github-star".to_string()];
        let lang_prefix = "lang:";
        let author_prefix = "author:github:";
        Self {
            gists: GistConfig::inheriting(&tags, lang_prefix, author_prefix),
            tags,
            lang_prefix: lang_prefix.into(),
            author_prefix: author_prefix.into(),
        }
    }
}

/// A starred repository, as `/user/starred` returns it.
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

/// A star+json `/user/starred` element: the repo and when it was starred.
#[derive(Debug, Clone, Deserialize)]
struct GitHubStarredRepo {
    starred_at: String,
    repo: GitHubRepo,
}

impl GitHubStarredRepo {
    /// The draft, dated by the star time at whole UTC seconds.
    fn into_draft(self, cfg: &GitHubConfig) -> Option<BookmarkDraft> {
        let starred_at = crate::timefmt::parse_rfc3339(&self.starred_at)
            .and_then(crate::timefmt::utc_whole_seconds);
        self.repo.into_draft_with_date(cfg, starred_at)
    }
}

/// The owner and [`repo_name`] of a repo-root path: exactly two non-empty segments
/// (`/owner//repo/` as `/owner/repo`), the last naming a repo.
pub(crate) fn repo_path<'a>(
    segments: impl IntoIterator<Item = &'a str>,
) -> Option<(&'a str, &'a str)> {
    let segments: Vec<&str> = segments
        .into_iter()
        .filter(|segment| !segment.is_empty())
        .collect();
    let [owner, last] = segments[..] else {
        return None;
    };
    Some((owner, repo_name(last)?))
}

/// The repo name a last path segment spells: less every trailing `.git`, any case, as
/// `GitHubKeys` reads it. `None` for nothing left, or a dot segment (raw or
/// percent-encoded), which a URL parser drops, naming the owner or site instead.
fn repo_name(segment: &str) -> Option<&str> {
    let name = without_git_suffixes(segment);
    let is_dot_segment = matches!(
        name.to_ascii_lowercase().as_str(),
        "." | ".." | "%2e" | ".%2e" | "%2e." | "%2e%2e"
    );
    (!name.is_empty() && !is_dot_segment).then_some(name)
}

/// `segment` less every trailing `.git`, in any case: `Repo.git.GIT` is `Repo`.
fn without_git_suffixes(segment: &str) -> &str {
    let mut name = segment;
    while let Some(shorter) = strip_git_suffix(name) {
        name = shorter;
    }
    name
}

fn strip_git_suffix(segment: &str) -> Option<&str> {
    let split = segment.len().checked_sub(".git".len())?;
    let (name, suffix) = (segment.get(..split)?, segment.get(split..)?);
    suffix.eq_ignore_ascii_case(".git").then_some(name)
}

/// What sets a repo note's homepage line apart from the rest, which `cleanup` reads back.
const HOMEPAGE_SEPARATOR: &str = "\n\nProject homepage: ";

/// A repo's notes, for sync and cleanup alike: the `description` in a `<blockquote>`, which
/// Pinboard renders, else the repo URL, then the homepage outside the quote.
fn github_note(description: Option<&str>, homepage: Option<&str>, html_url: &str) -> String {
    let mut note = match description.filter(|s| !s.is_empty()) {
        Some(desc) => blockquote(desc),
        None => html_url.to_string(),
    };
    if let Some(home) = homepage.filter(|s| !s.is_empty()) {
        note = format!("{note}{HOMEPAGE_SEPARATOR}{home}");
    }
    note
}

impl GitHubRepo {
    /// The repo's owner: the part of `full_name` before the `/`.
    fn owner(&self) -> &str {
        self.full_name.split('/').next().unwrap_or_default()
    }

    /// The draft, undated; production dates it via [`GitHubStarredRepo::into_draft`].
    #[cfg(test)]
    fn into_draft(self, cfg: &GitHubConfig) -> Option<BookmarkDraft> {
        self.into_draft_with_date(cfg, None)
    }

    /// The draft dated `post_date`, or `None` (with a warning) if `html_url` doesn't parse.
    fn into_draft_with_date(
        self,
        cfg: &GitHubConfig,
        post_date: Option<OffsetDateTime>,
    ) -> Option<BookmarkDraft> {
        let url = Url::parse(&self.html_url)
            .map_err(|e| {
                warn!(
                    "skipping github repo with unparseable URL {}: {e}",
                    self.html_url
                );
            })
            .ok()?;
        let dedup_key = GitHubKeys
            .dedup_key(&url)
            .unwrap_or_else(|| url.as_str().into());

        let note = github_note(
            self.description.as_deref(),
            self.homepage.as_deref(),
            &self.html_url,
        );

        let mut tags = Tags::default();
        tags.push_all(&cfg.tags);
        if let Some(lang) = self.language.as_deref().filter(|s| !s.is_empty()) {
            tags.push_prefixed(&cfg.lang_prefix, &lang.to_lowercase());
        }
        tags.push_prefixed(&cfg.author_prefix, self.owner());

        Some(BookmarkDraft {
            bookmark: Bookmark {
                url,
                title: html_to_plain(&self.full_name),
                note,
                tags,
                timestamp: post_date,
                public: false,
                read_later: false,
            },
            dedup_key,
        })
    }
}

/// The explanation when a response is GitHub's rate limit, else `None`. Both limits answer
/// `403` or `429`, as a permission denial does, so only the headers tell: an exhausted
/// primary quota zeroes `x-ratelimit-remaining`, a secondary limit sends `retry-after`.
/// Only those signatures match: a false positive stops a pass over a permission error, a
/// false negative sends the operator after the token.
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
        // Either delta-seconds or an HTTP-date.
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
    // A secondary limit may carry neither header, leaving only its message.
    if body.to_lowercase().contains("secondary rate limit") {
        return Some("GitHub secondary rate limit hit; wait a minute before retrying".to_string());
    }
    None
}

/// Why a `/repos/{owner}/{name}` refusal will repeat on every run, for the warning, or
/// `None` for one that may clear (a rate limit, an outage, an ordinary permission error).
///
/// A blocked repo answers `451` (DMCA) or `403` (Terms of Service), both with a `block`
/// object, which tells that 403 from a permission denial. An org enforcing SAML SSO the
/// token isn't authorized for answers `403` with `X-GitHub-SSO: required`: only the
/// operator clears it, and the token is valid elsewhere, so the pass goes on.
fn permanent_refusal(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &str,
) -> Option<&'static str> {
    if status == reqwest::StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS {
        return Some("is blocked (451)");
    }
    if status != reqwest::StatusCode::FORBIDDEN
        || rate_limit_message(status, headers, body).is_some()
    {
        return None;
    }
    let sso_required = headers
        .get("x-github-sso")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim_start().starts_with("required"));
    if sso_required {
        return Some(
            "is behind an organization's SAML SSO, which the token isn't authorized for (403)",
        );
    }
    let blocked = serde_json::from_str::<serde_json::Value>(body).is_ok_and(|refusal| {
        refusal
            .get("block")
            .is_some_and(serde_json::Value::is_object)
    });
    blocked.then_some("is blocked (403)")
}

/// The body of a refused response. On a 403 or 429 whose headers mark no rate limit the
/// body decides (a block, or a secondary limit known by its message), so a cut-off one
/// fails rather than reads as an ordinary refusal.
async fn refusal_body(what: &str, resp: reqwest::Response) -> Result<String, SourceError> {
    let status = resp.status();
    let decides = (status == reqwest::StatusCode::FORBIDDEN
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS)
        && rate_limit_message(status, resp.headers(), "").is_none();
    Ok(read_body(resp, decides, || {
        format!("reading {what} response ({status})")
    })
    .await?)
}

/// Reads a user's stars and starred gists with a personal access token.
pub struct GitHubClient {
    http: RetryingClient,
    config: GitHubConfig,
    base: String,
    /// The setting the token came from, for messages; never the token itself.
    token_source: &'static str,
    /// Only to [redact](crate::http::redact) it from a logged or quoted body.
    token: String,
}

impl GitHubClient {
    pub fn new(token: ConfiguredSecret, config: GitHubConfig) -> anyhow::Result<Self> {
        Self::build(token, config, GITHUB_API_BASE.to_string())
    }

    fn build(token: ConfiguredSecret, config: GitHubConfig, base: String) -> anyhow::Result<Self> {
        use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION};

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
            HeaderValue::from_static(GITHUB_API_VERSION),
        );

        let http = client_builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http: RetryingClient::new(http, DEFAULT_BACKOFF),
            config,
            base,
            token_source: token.source,
            token: token.value,
        })
    }

    fn token_refused(&self) -> SourceError {
        SourceError::Halted(Halt::Reauth(format!(
            "GitHub returned 401 — the token from {} is invalid or expired.",
            self.token_source
        )))
    }

    /// The error for a failed non-401 response: [`Halt::RateLimited`] when
    /// [`rate_limit_message`] says so, [`refusal_body`]'s read error, else the status with
    /// an excerpt of the body, the token redacted.
    async fn refusal(&self, what: &str, resp: reqwest::Response) -> SourceError {
        let status = resp.status();
        let headers = resp.headers().clone();
        match refusal_body(what, resp).await {
            Ok(body) => self.refusal_of(what, status, &headers, &body),
            Err(error) => error,
        }
    }

    /// [`Self::refusal`] for a response already read.
    fn refusal_of(
        &self,
        what: &str,
        status: reqwest::StatusCode,
        headers: &reqwest::header::HeaderMap,
        body: &str,
    ) -> SourceError {
        if let Some(message) = rate_limit_message(status, headers, body) {
            return SourceError::Halted(Halt::RateLimited(message));
        }
        crate::http::error_with_body(&format!("{what} returned {status}"), body, &[&self.token])
            .into()
    }

    /// A repo, following renames and transfers, or `None` when it is gone for good: a 404,
    /// or a [`permanent_refusal`].
    async fn repo(&self, owner: &str, name: &str) -> Result<Option<GitHubRepo>, SourceError> {
        let endpoint = format!("{}/repos/{owner}/{name}", self.base);
        self.lookup(
            "github repo",
            &endpoint,
            &format!("{owner}/{name}"),
            "its stored title, notes and tags",
        )
        .await
    }

    /// A gist, or `None` when gone for good: a 404, or a [`permanent_refusal`].
    async fn gist(&self, id: &str) -> Result<Option<GitHubGist>, SourceError> {
        let endpoint = format!("{}/gists/{id}", self.base);
        self.lookup(
            "github gist",
            &endpoint,
            id,
            "its stored URL, title, notes and tags",
        )
        .await
    }

    /// GET `endpoint`, `None` for a 404 or a [`permanent_refusal`]. A refusal is logged,
    /// naming `subject` and what is `kept`: unlike a 404, the thing still exists.
    async fn lookup<T: serde::de::DeserializeOwned>(
        &self,
        label: &str,
        endpoint: &str,
        subject: &str,
        kept: &str,
    ) -> Result<Option<T>, SourceError> {
        let resp = self.http.send(label, || self.http.get(endpoint)).await?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(self.token_refused());
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            let headers = resp.headers().clone();
            let body = refusal_body(label, resp).await?;
            if let Some(why) = permanent_refusal(status, &headers, &body) {
                warn!("{label} {subject} {why}; keeping {kept}");
                return Ok(None);
            }
            return Err(self.refusal_of(label, status, &headers, &body));
        }
        let found = resp
            .json()
            .await
            .with_context(|| format!("parsing {label} response"))?;
        Ok(Some(found))
    }

    /// [`Source::fetch`], pushing each page body to `sink`. With `synced_keys`, each stream
    /// stops at its own first page with nothing new ([`page_is_caught_up`]). Stars come
    /// first, so a numeric `limit` writes them first.
    async fn fetch_capturing(
        &self,
        synced_keys: Option<&HashSet<DedupKey>>,
        sink: &mut RawSink,
    ) -> Result<Vec<BookmarkDraft>, SourceError> {
        let mut drafts = self.fetch_stars(synced_keys, sink).await?;
        if self.config.gists.enabled {
            drafts.extend(self.fetch_gists(synced_keys, sink).await?);
        } else {
            // Without it a backup would keep last run's gists file beside a gist-less
            // normalized half.
            sink.push(STARRED_GISTS.raw_stream, RawKind::Json, "[]");
        }
        Ok(drafts)
    }

    /// The starred gists, a second stream: it stops at its own caught-up page.
    async fn fetch_gists(
        &self,
        synced_keys: Option<&HashSet<DedupKey>>,
        sink: &mut RawSink,
    ) -> Result<Vec<BookmarkDraft>, SourceError> {
        self.walk_drafts(&STARRED_GISTS, synced_keys, sink, |gist: GitHubGist| {
            gist.into_draft(&self.config.gists)
        })
        .await
    }

    /// Every starred gist, keyed by lowercase id, so `cleanup github` plans from one read,
    /// not a lookup per bookmark.
    async fn starred_gists(&self) -> Result<HashMap<String, GitHubGist>, SourceError> {
        let mut gists = HashMap::new();
        self.walk(
            &STARRED_GISTS,
            &mut RawSink::disabled(),
            |page: Vec<GitHubGist>| {
                gists.extend(
                    page.into_iter()
                        .map(|gist| (gist.id.to_ascii_lowercase(), gist)),
                );
                ControlFlow::Continue(())
            },
        )
        .await?;
        Ok(gists)
    }

    /// The starred repos, each dated by its star time.
    async fn fetch_stars(
        &self,
        synced_keys: Option<&HashSet<DedupKey>>,
        sink: &mut RawSink,
    ) -> Result<Vec<BookmarkDraft>, SourceError> {
        self.walk_drafts(&STARS, synced_keys, sink, |starred: GitHubStarredRepo| {
            starred.into_draft(&self.config)
        })
        .await
    }

    /// [`Self::walk`] `stream` into drafts, stopping, with `synced_keys`, at the first page
    /// with nothing new ([`page_is_caught_up`]).
    async fn walk_drafts<T: serde::de::DeserializeOwned>(
        &self,
        stream: &PagedStream,
        synced_keys: Option<&HashSet<DedupKey>>,
        sink: &mut RawSink,
        shape: impl Fn(T) -> Option<BookmarkDraft>,
    ) -> Result<Vec<BookmarkDraft>, SourceError> {
        let mut drafts = Vec::new();
        self.walk(stream, sink, |items: Vec<T>| {
            let page_len = items.len();
            let page_drafts: Vec<BookmarkDraft> = items.into_iter().filter_map(&shape).collect();
            let caught_up =
                synced_keys.is_some_and(|keys| page_is_caught_up(page_len, &page_drafts, keys));
            drafts.extend(page_drafts);
            if caught_up {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .await?;
        Ok(drafts)
    }

    /// Walk `stream`'s `Link: rel="next"` pages, handing each page to `on_page`, which
    /// breaks once caught up. A bad element is skipped with a warning; a body that isn't a
    /// JSON array fails the walk, as does a page whose every element fails
    /// ([`deserialize_lenient`]), a schema break.
    async fn walk<T: serde::de::DeserializeOwned>(
        &self,
        stream: &PagedStream,
        sink: &mut RawSink,
        mut on_page: impl FnMut(Vec<T>) -> ControlFlow<()>,
    ) -> Result<(), SourceError> {
        let PagedStream {
            label,
            raw_stream,
            path,
            accept,
            query,
        } = *stream;
        let endpoint = format!("{}{path}", self.base);
        let mut page: u32 = 1;
        let mut visited = HashSet::new();

        loop {
            if page > MAX_PAGES {
                warn!("{label}: hit the {MAX_PAGES}-page cap; stopping (some may be missing)");
                sink.mark_truncated(format!("{label} stopped at the {MAX_PAGES}-page cap"));
                break;
            }
            if !visited.insert(page) {
                warn!("{label}: 'next' link looped back to page {page}; stopping");
                break;
            }
            let resp = self
                .http
                .send(label, || {
                    self.http
                        .get(&endpoint)
                        .header(reqwest::header::ACCEPT, accept)
                        .query(query)
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
                return Err(self.refusal(label, resp).await);
            }

            let body = resp
                .text()
                .await
                .with_context(|| format!("reading {label} response"))?;
            sink.push(raw_stream, RawKind::Json, &body);
            let elements: Vec<serde_json::Value> =
                serde_json::from_str(&body).with_context(|| format!("parsing {label} response"))?;
            let items: Vec<T> =
                deserialize_lenient(elements, &format!("{label} element"), |count| {
                    SourceError::Other(anyhow::anyhow!(
                        "{label} page {page}: all {count} element(s) failed to \
                         deserialize — the API response shape may have changed"
                    ))
                })?;
            if on_page(items).is_break() {
                debug!("{label}: page {page} has nothing new; stopping");
                break;
            }

            match next {
                Some(p) => page = p,
                None => break,
            }
        }
        Ok(())
    }
}

impl Source for GitHubClient {
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError> {
        self.fetch_capturing(None, &mut RawSink::disabled()).await
    }

    /// Stars and gists list newest first, so each stream stops paging at its first page
    /// that would give `sync` nothing to write.
    async fn fetch_incremental(
        &self,
        existing: &[Bookmark],
    ) -> Result<Vec<BookmarkDraft>, SourceError> {
        self.fetch_capturing(Some(&self.synced_keys(existing)), &mut RawSink::disabled())
            .await
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
    /// [`GitHubKeys`], so a `www.` or `.git` form of a repo dedups against its star.
    fn dedup_key(&self, url: &Url) -> Option<DedupKey> {
        GitHubKeys.dedup_key(url)
    }
}

/// The `page` of a `Link` header's `rel="next"`, if any.
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
impl GitHubConfig {
    /// The defaults with the gists off, for a test that mocks only the stars.
    pub(crate) fn stars_only() -> Self {
        let mut config = Self::default();
        config.gists.enabled = false;
        config
    }
}

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
mod tests;

/// Integration tests against a `wiremock` server. These bind a TCP socket, so they
/// can't run in the Nix build sandbox -- the flake skips `net_tests` there.
#[cfg(test)]
mod net_tests;
