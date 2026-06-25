//! The GitHub source: reads the authenticated user's starred repositories
//! (`/user/starred`, token-authenticated) and shapes each into a Pinboard draft.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use log::{debug, error, info};
use serde::Deserialize;

use crate::htmltext::{blockquote, html_to_plain};
use crate::http::send_retrying;
use crate::pinboard::{apply_update, Bookmark, BookmarkStore, BookmarkUpdate};
use crate::source::{
    extend_unique, host_matches, push_prefixed, push_tags, split_host_path, tags_differ, url_key,
    BookmarkDraft, Source, SourceError,
};

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
        let dedup_key = url_key(&self.html_url).unwrap_or_else(|| self.html_url.clone());

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

    fn existing_key(&self, url: &str) -> Option<String> {
        let key = url_key(url)?;
        let (host, _) = split_host_path(url);
        host_matches(&host, "github.com").then_some(key)
    }
}

/// Options for `cleanup github`.
pub struct GhCleanupOpts {
    pub dry_run: bool,
    /// Re-date bookmarks to when the repo was starred (within the age cap). Since the
    /// per-repo lookup doesn't carry `starred_at`, this fetches the star list to map it.
    pub use_post_date: bool,
    /// Backdate age cap, in days.
    pub max_age_days: u64,
    /// Re-date repos starred longer ago than the cap to "now" instead of leaving them.
    pub cleanup_stale_to_now: bool,
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
    opts: &GhCleanupOpts,
    bookmarks: &[Bookmark],
) -> Result<()> {
    let gh_bms: Vec<_> = bookmarks
        .iter()
        .filter(|b| is_github_url(&b.url))
        .cloned()
        .collect();
    info!(
        "scanning {} github bookmark(s){}",
        gh_bms.len(),
        if opts.dry_run { " (dry run)" } else { "" }
    );

    // `starred_at` isn't on the per-repo lookup, so for use_post_date fetch the star
    // list once and map url_key -> star epoch. A repo no longer starred won't be here
    // (it keeps its existing time).
    let star_dates: HashMap<String, i64> = if opts.use_post_date {
        client
            .fetch()
            .await
            .map_err(SourceError::into_anyhow)?
            .into_iter()
            .filter_map(|d| Some((url_key(&d.url)?, d.post_date?)))
            .collect()
    } else {
        HashMap::new()
    };

    let now = crate::timefmt::now_unix();
    let mut changed = 0usize;
    let mut failed = 0usize;
    let mut wrote = false;
    for bm in &gh_bms {
        let canonical = canonical_repo_url(&bm.url).unwrap_or_else(|| bm.url.clone());

        // Default to the canonicalization, then refresh from the API when the repo
        // still exists (a 404 keeps just the canonical URL).
        let mut url = canonical.clone();
        let mut description = bm.description.clone();
        let mut tags = bm.tag_list();
        if let Some((owner, repo)) = owner_repo(&canonical) {
            // Log and skip a single repo whose lookup fails so the rest still run.
            match client.repo(owner, repo).await {
                Ok(Some(info)) => {
                    url = info.html_url.clone(); // follows renames/transfers
                    description = info.full_name.clone();
                    tags = refresh_tags(bm.tag_list(), &info, config);
                }
                Ok(None) => {}
                Err(e) => {
                    failed += 1;
                    error!("looking up {}: {:#}", bm.url, SourceError::into_anyhow(e));
                    continue;
                }
            }
        }

        let dt = crate::timefmt::cleanup_dt(
            opts.use_post_date,
            opts.max_age_days,
            opts.cleanup_stale_to_now,
            url_key(&url).and_then(|k| star_dates.get(&k).copied()),
            now,
            &bm.time,
        );

        let url_changed = url != bm.url;
        let desc_changed = description != bm.description;
        let tags_changed = tags_differ(&bm.tag_list(), &tags);
        let date_changed = dt != bm.time;
        if !(url_changed || desc_changed || tags_changed || date_changed) {
            continue;
        }

        if opts.dry_run {
            changed += 1;
            println!("[dry-run] {}", bm.url);
            if url_changed {
                println!("          url   -> {url}");
            }
            if desc_changed {
                println!("          title -> {description}");
            }
            if tags_changed {
                println!("          tags  -> [{}]", tags.join(" "));
            }
            if date_changed {
                println!("          date  -> {dt}");
            }
            continue;
        }

        // Log and skip a single failed update so the rest of the pass still runs.
        match apply_update(
            pinboard,
            &mut wrote,
            BookmarkUpdate {
                url: &url,
                description: &description,
                extended: &bm.extended,
                tags: &tags,
                shared: bm.is_shared(),
                toread: bm.is_toread(),
                dt: &dt,
            },
            url_changed.then_some(bm.url.as_str()),
        )
        .await
        {
            Ok(()) => {
                changed += 1;
                debug!("updated {} -> {url}", bm.url);
            }
            Err(e) => {
                failed += 1;
                error!("updating bookmark {}: {e:#}", bm.url);
            }
        }
    }

    if opts.dry_run {
        println!("{changed} bookmark(s) would change.");
    } else {
        info!("done: updated {changed} bookmark(s)");
    }
    if failed > 0 {
        bail!("{failed} bookmark(s) failed to update");
    }
    Ok(())
}

/// The `(owner, repo)` of a canonical `https://github.com/owner/repo` URL.
fn owner_repo(url: &str) -> Option<(&str, &str)> {
    let after = url.split_once("github.com/")?.1;
    let mut segments = after.split('/');
    let owner = segments.next().filter(|s| !s.is_empty())?;
    let repo = segments.next().filter(|s| !s.is_empty())?;
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

/// Whether `url`'s host is github.com or a `*.github.com` subdomain.
fn is_github_url(url: &str) -> bool {
    let (host, _) = split_host_path(url);
    host_matches(&host, "github.com")
}

/// Canonicalize a GitHub *repo-root* URL to `https://github.com/<owner>/<repo>`
/// (lowercasing the host, forcing https, dropping a `.git` suffix, trailing slash,
/// and any query/fragment). Returns `Some(new)` only if it changed; `None` for a
/// non-GitHub host, an already-canonical URL, or a deeper path (e.g. `/tree/...`,
/// `/issues`) which is left untouched.
pub fn canonical_repo_url(url: &str) -> Option<String> {
    if !is_github_url(url) {
        return None;
    }
    let (_host, path) = split_host_path(url);
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // Only act on a bare owner/repo (don't mangle deeper links).
    if segments.len() != 2 {
        return None;
    }
    let owner = segments[0];
    let repo = segments[1].strip_suffix(".git").unwrap_or(segments[1]);
    let canonical = format!("https://github.com/{owner}/{repo}");
    (canonical != url).then_some(canonical)
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
        for url in [
            "http://github.com/Owner/Repo",
            "https://www.github.com/Owner/Repo/",
            "https://github.com/Owner/Repo.git",
            "https://github.com/Owner/Repo?tab=stars",
        ] {
            assert_eq!(
                canonical_repo_url(url).as_deref(),
                Some("https://github.com/Owner/Repo"),
                "url: {url}"
            );
        }
        // Already canonical → no change.
        assert_eq!(canonical_repo_url("https://github.com/o/r"), None);
        // Non-GitHub and deeper paths are left untouched.
        assert_eq!(canonical_repo_url("https://example.com/o/r"), None);
        assert_eq!(canonical_repo_url("https://github.com/o/r/issues/5"), None);
        assert_eq!(canonical_repo_url("https://github.com/o"), None);
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
        // Subdomains of github.com are recognized too (consistent with is_github_url).
        assert_eq!(
            c.existing_key("https://www.github.com/o/r").as_deref(),
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
            &GhCleanupOpts {
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
}
