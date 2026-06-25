//! The GitHub source: reads the authenticated user's starred repositories
//! (`/user/starred`, token-authenticated) and shapes each into a Pinboard draft.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::cleanup_pass::{run_pass, CleanupPass, DateOpts, PlannedCleanupPass};
use crate::htmltext::{blockquote, html_to_plain};
use crate::http::send_retrying;
use crate::pinboard::{Bookmark, BookmarkStore};
use crate::source::{
    extend_unique, host_matches, push_prefixed, push_tags, url_key, BookmarkDraft, Source,
    SourceError, UrlExt, UrlKey,
};
use url::Url;

const UA: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
const API_BASE: &str = "https://api.github.com";
const MAX_RETRIES: u32 = 4;
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Tag vocabulary for GitHub stars. `tags` are applied to every bookmark
/// (defaulting to `["github-star"]`); `lang_prefix` defaults to its built-in value,
/// and an empty string disables that tag.
#[derive(Debug, Clone)]
pub struct GithubConfig {
    pub tags: Vec<String>,
    pub lang_prefix: String,
}

impl Default for GithubConfig {
    fn default() -> Self {
        Self {
            tags: vec!["github-star".into()],
            lang_prefix: "lang:".into(),
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

/// A `/user/starred` element when requested with the star+json media type: the repo
/// plus the time the user starred it.
#[derive(Debug, Clone, Deserialize)]
struct StarredRepo {
    starred_at: String,
    repo: Repo,
}

impl StarredRepo {
    /// Shape into a draft, dating it by the (RFC3339) star time.
    fn into_draft(self, cfg: &GithubConfig) -> BookmarkDraft {
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

impl Repo {
    /// Shape the repo into a Pinboard draft (no source date). Test-only convenience;
    /// the production path dates each draft via [`StarredRepo::into_draft`].
    #[cfg(test)]
    fn into_draft(self, cfg: &GithubConfig) -> BookmarkDraft {
        self.into_draft_with_date(cfg, None)
    }

    /// Shape the repo into a Pinboard draft, carrying `post_date` (the star time).
    fn into_draft_with_date(self, cfg: &GithubConfig, post_date: Option<i64>) -> BookmarkDraft {
        let dedup_key = Url::parse(&self.html_url)
            .ok()
            .as_ref()
            .and_then(url_key)
            .unwrap_or_else(|| self.html_url.clone());

        let extended = github_extended(
            self.description.as_deref(),
            self.homepage.as_deref(),
            &self.html_url,
        );

        let mut tags = Vec::new();
        push_tags(&mut tags, &cfg.tags);
        if let Some(lang) = self.language.filter(|s| !s.is_empty()) {
            push_prefixed(&mut tags, &cfg.lang_prefix, &lang.to_lowercase());
        }

        BookmarkDraft {
            url: self.html_url,
            description: html_to_plain(&self.full_name),
            extended,
            tags,
            dedup_key,
            toread: false,
            shared: false,
            post_date,
        }
    }
}

/// Reads a user's starred repos via a personal access token.
pub struct GitHubClient {
    http: reqwest::Client,
    config: GithubConfig,
    /// API base (overridden in tests).
    base: String,
}

impl GitHubClient {
    pub fn new(token: String, config: GithubConfig) -> anyhow::Result<Self> {
        Self::build(token, config, API_BASE.to_string())
    }

    fn build(token: String, config: GithubConfig, base: String) -> anyhow::Result<Self> {
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
    /// transfers, returning the current name/URL). `None` if it no longer exists.
    async fn repo(&self, owner: &str, name: &str) -> Result<Option<Repo>, SourceError> {
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
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("github repo returned {status}: {}", body.trim()).into());
        }
        let repo: Repo = resp.json().await.context("parsing github repo response")?;
        Ok(Some(repo))
    }
}

impl Source for GitHubClient {
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError> {
        let endpoint = format!("{}/user/starred", self.base);
        let mut out = Vec::new();
        let mut page: u32 = 1;

        loop {
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
                let body = resp.text().await.unwrap_or_default();
                return Err(
                    anyhow::anyhow!("github starred returned {status}: {}", body.trim()).into(),
                );
            }

            let starred: Vec<StarredRepo> = resp
                .json()
                .await
                .context("parsing github starred response")?;
            out.extend(starred.into_iter().map(|s| s.into_draft(&self.config)));

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
    /// never produces a key.
    fn dedup_key(&self, url: &Url) -> Option<String> {
        url_key(url).filter(|_| url.host_is("github.com"))
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

/// Normalize existing GitHub repo bookmarks: look each repo up via the API (which
/// follows renames/transfers), rewriting a moved repo's URL to the current one,
/// setting the title to the current `owner/repo`, and refreshing the `lang:` tag. A
/// URL change updates + deletes the old; notes are kept and creation time preserved.
/// A repo that no longer exists (404) keeps just the URL canonicalization.
pub async fn cleanup<P: BookmarkStore>(
    pinboard: &P,
    client: &GitHubClient,
    config: &GithubConfig,
    opts: &GitHubCleanupOpts,
    bookmarks: &[Bookmark],
) -> Result<()> {
    let gh_bms: Vec<_> = bookmarks
        .iter()
        .filter(|bookmark| Url::parse(&bookmark.url).is_ok_and(|url| url.host_is("github.com")))
        .cloned()
        .collect();

    // `starred_at` isn't on the per-repo lookup, so for use_post_date fetch the star
    // list once and map url_key -> star epoch. A repo no longer starred won't be here
    // (it keeps its existing time).
    let star_dates: HashMap<String, i64> = if opts.use_post_date {
        client
            .fetch()
            .await
            .map_err(SourceError::into_anyhow)?
            .into_iter()
            .filter_map(|d| {
                let key = Url::parse(&d.url).ok().as_ref().and_then(url_key)?;
                Some((key, d.post_date?))
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
    let failed = run_pass(
        pinboard,
        &gh_bms,
        opts.dry_run,
        "github",
        opts.date_opts(),
        &pass,
    )
    .await;
    if failed > 0 {
        bail!("{failed} bookmark(s) failed to update");
    }
    Ok(())
}

/// Re-shapes one GitHub repo bookmark: canonicalize the URL, then refresh from the API
/// (which follows renames/transfers) — current URL/title, rebuilt `<blockquote>` notes,
/// and language tag. A 404 keeps just the canonicalization.
struct GitHubCleanupPass<'a> {
    client: &'a GitHubClient,
    config: &'a GithubConfig,
    star_dates: HashMap<String, i64>,
}

impl CleanupPass for GitHubCleanupPass<'_> {
    async fn plan(&self, bookmark: &Bookmark) -> Result<Option<PlannedCleanupPass>> {
        // Parse the stored URL once (the pass is filtered to github URLs).
        let Some(original) = Url::parse(&bookmark.url).ok() else {
            return Ok(None);
        };
        let canonical = canonical_repo_url(&original).unwrap_or(original);

        // Default to the canonicalization, then refresh from the API when the repo
        // still exists (a 404 keeps just the canonical URL).
        let mut url = canonical.clone();
        let mut description = html_to_plain(&bookmark.description);
        let mut extended = bookmark.extended.clone();
        let mut tags = bookmark.tag_list();
        if let Some((owner, repo)) = owner_repo(&canonical) {
            match self.client.repo(&owner, &repo).await {
                Ok(Some(info)) => {
                    description = html_to_plain(&info.full_name);
                    // Rebuild the notes from fresh data so old bookmarks retrofit to the
                    // <blockquote> shape (sync skips already-present bookmarks).
                    extended = github_extended(
                        info.description.as_deref(),
                        info.homepage.as_deref(),
                        &info.html_url,
                    );
                    tags = refresh_tags(bookmark.tag_list(), &info, self.config);
                    url = Url::parse(&info.html_url).unwrap_or(url); // follows renames/transfers
                }
                Ok(None) => {}
                // A failed lookup is surfaced to the driver, which logs and counts it.
                Err(e) => return Err(SourceError::into_anyhow(e)),
            }
        }

        let src_date = url_key(&url).and_then(|k| self.star_dates.get(&k).copied());
        Ok(Some(PlannedCleanupPass {
            url: url.into(),
            description,
            extended,
            tags,
            src_date,
        }))
    }
}

/// The `(owner, repo)` of a canonical `https://github.com/owner/repo` URL.
fn owner_repo(url: &Url) -> Option<(String, String)> {
    let mut segments = url.path_segments()?;
    let owner = segments.next().filter(|s| !s.is_empty())?.to_string();
    let repo = segments.next().filter(|s| !s.is_empty())?.to_string();
    Some((owner, repo))
}

/// Refresh the language tag from the current repo, keeping existing tags and
/// ensuring the base tags: drop any old `lang_prefix` tag, then re-add the base
/// tags and the current `lang:` tag.
fn refresh_tags(existing: Vec<String>, repo: &Repo, cfg: &GithubConfig) -> Vec<String> {
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

/// Canonicalize a GitHub *repo-root* URL to `https://github.com/<owner>/<repo>`
/// (lowercasing the host, forcing https, dropping a `.git` suffix, trailing slash,
/// and any query/fragment). Returns `Some(new)` only if it changed; `None` for a
/// non-GitHub host, an already-canonical URL, or a deeper path (e.g. `/tree/...`,
/// `/issues`) which is left untouched.
pub fn canonical_repo_url(url: &Url) -> Option<Url> {
    if !host_matches(url.host_str()?, "github.com") {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    // Only act on a bare owner/repo (don't mangle deeper links).
    if segments.len() != 2 {
        return None;
    }
    let owner = segments[0];
    let repo = segments[1].strip_suffix(".git").unwrap_or(segments[1]);
    let canonical = Url::parse(&format!("https://github.com/{owner}/{repo}")).ok()?;
    (canonical != *url).then_some(canonical)
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
        .into_draft(&GithubConfig::default());

        assert_eq!(d.url, "https://github.com/owner/Repo");
        assert_eq!(d.description, "owner/Repo");
        assert_eq!(
            d.extended,
            "<blockquote>A thing</blockquote>\n\nProject homepage: https://example.com"
        );
        assert_eq!(d.tags, vec!["github-star", "lang:rust"]);
        assert_eq!(d.dedup_key, "github.com/owner/repo");
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
        .into_draft(&GithubConfig::default());
        assert_eq!(d.tags, vec!["github-star", "lang:jupyter-notebook"]);
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
    fn tags_list_replaces_the_default_base() {
        let cfg = GithubConfig {
            tags: vec!["github-star".into(), "account:work".into()],
            ..GithubConfig::default()
        };
        let d = repo(json!({ "full_name": "o/r", "html_url": "https://github.com/o/r" }))
            .into_draft(&cfg);
        assert_eq!(d.tags, vec!["github-star", "account:work"]);
    }

    #[test]
    fn canonical_repo_url_normalizes_repo_roots_only() {
        // Scheme, host case, .git, trailing slash, and query are all normalized.
        for u in [
            "http://github.com/Owner/Repo",
            "https://www.github.com/Owner/Repo/",
            "https://github.com/Owner/Repo.git",
            "https://github.com/Owner/Repo?tab=stars",
        ] {
            assert_eq!(
                canonical_repo_url(&url(u)).map(String::from).as_deref(),
                Some("https://github.com/Owner/Repo"),
                "url: {u}"
            );
        }
        // Already canonical → no change.
        assert_eq!(canonical_repo_url(&url("https://github.com/o/r")), None);
        // Non-GitHub and deeper paths are left untouched.
        assert_eq!(canonical_repo_url(&url("https://example.com/o/r")), None);
        assert_eq!(
            canonical_repo_url(&url("https://github.com/o/r/issues/5")),
            None
        );
        assert_eq!(canonical_repo_url(&url("https://github.com/o")), None);
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
        let c = GitHubClient::new("t".into(), GithubConfig::default()).unwrap();
        assert_eq!(
            c.dedup_key(&url("https://github.com/o/r")).as_deref(),
            Some("github.com/o/r")
        );
        assert!(c.dedup_key(&url("https://example.com/o/r")).is_none());
        // Subdomains of github.com are recognized too.
        assert_eq!(
            c.dedup_key(&url("https://www.github.com/o/r")).as_deref(),
            Some("www.github.com/o/r")
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
            GitHubClient::with_base_url("tok".into(), GithubConfig::default(), server.uri());
        let drafts = client.fetch().await.unwrap();
        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[0].description, "a/one");
        assert_eq!(drafts[0].tags, vec!["github-star", "lang:rust"]);
        // The star time becomes the draft's post_date (RFC3339 → epoch).
        assert_eq!(
            drafts[0].post_date,
            crate::timefmt::rfc3339_to_unix("2023-01-02T03:04:05Z")
        );
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

    #[tokio::test]
    async fn cleanup_refresh_rewrites_renamed_repo_and_language() {
        use crate::pinboard::Bookmark;
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
            all: vec![Bookmark {
                url: "https://github.com/old/name".into(),
                description: "old/name".into(),
                extended: "notes".into(),
                tags: "github-star lang:python mine".into(),
                time: "2020-01-01T00:00:00Z".into(),
                shared: "no".into(),
                toread: "no".into(),
            }],
            ..Default::default()
        };
        let bookmarks = pinboard.all.clone();
        let client =
            GitHubClient::with_base_url("tok".into(), GithubConfig::default(), server.uri());
        cleanup(
            &pinboard,
            &client,
            &GithubConfig::default(),
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
    async fn cleanup_rewrites_notes_only_to_retrofit_the_blockquote() {
        use crate::pinboard::Bookmark;
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
            all: vec![Bookmark {
                url: "https://github.com/o/r".into(),
                description: "o/r".into(),
                extended: "A thing".into(), // pre-blockquote notes
                tags: "github-star".into(),
                time: "2020-01-01T00:00:00Z".into(),
                shared: "no".into(),
                toread: "no".into(),
            }],
            ..Default::default()
        };
        let bookmarks = pinboard.all.clone();
        let client =
            GitHubClient::with_base_url("tok".into(), GithubConfig::default(), server.uri());
        cleanup(
            &pinboard,
            &client,
            &GithubConfig::default(),
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
