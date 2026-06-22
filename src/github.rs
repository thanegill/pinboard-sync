//! The GitHub source: reads the authenticated user's starred repositories
//! (`/user/starred`, token-authenticated) and shapes each into a Pinboard draft.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::http::send_retrying;
use crate::pinboard::{Bookmark, BookmarkStore, RATE_LIMIT_SECS};
use crate::source::{push_prefixed, push_tag, url_key, BookmarkDraft, Source, SourceError};

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
        for tag in &cfg.tags {
            push_tag(&mut tags, tag);
        }
        if let Some(lang) = self.language.filter(|s| !s.is_empty()) {
            push_prefixed(&mut tags, &cfg.lang_prefix, &lang.to_lowercase());
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
}

impl Source for GitHubClient {
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError> {
        let endpoint = format!("{}/user/starred", self.base);
        let mut out = Vec::new();
        let mut page: u32 = 1;

        loop {
            let resp = send_retrying("github starred", MAX_RETRIES, RETRY_DELAY, || {
                self.http
                    .get(&endpoint)
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

/// Options for `cleanup github`.
pub struct GhCleanupOpts {
    pub dry_run: bool,
    pub verbose: bool,
}

/// Normalize existing GitHub repo bookmarks: canonicalize each repo-root URL to
/// `https://github.com/<owner>/<repo>` (updating + deleting the old URL when it
/// changes), preserving the title, notes, tags, and creation time.
pub async fn cleanup<P: BookmarkStore>(
    pinboard: &P,
    opts: &GhCleanupOpts,
    bookmarks: &[Bookmark],
) -> Result<()> {
    let gh_bms: Vec<_> = bookmarks
        .iter()
        .filter(|b| is_github_url(&b.url))
        .cloned()
        .collect();
    println!(
        "Scanning {} github bookmark(s){}...",
        gh_bms.len(),
        if opts.dry_run { " (dry run)" } else { "" }
    );

    let mut changed = 0usize;
    let mut wrote = false;
    for bm in &gh_bms {
        let Some(new_url) = canonical_repo_url(&bm.url) else {
            continue;
        };
        changed += 1;

        if opts.dry_run {
            println!("[dry-run] {}", bm.url);
            println!("          url -> {new_url}");
            continue;
        }

        if wrote {
            tokio::time::sleep(Duration::from_secs(RATE_LIMIT_SECS)).await;
        }
        pinboard
            .update(
                &new_url,
                &bm.description,
                &bm.extended,
                &bm.tag_list(),
                bm.is_shared(),
                bm.is_toread(),
                &bm.time,
            )
            .await
            .with_context(|| format!("updating bookmark {new_url}"))?;
        pinboard
            .delete(&bm.url)
            .await
            .with_context(|| format!("deleting old URL {}", bm.url))?;
        wrote = true;
        if opts.verbose {
            eprintln!("updated {} -> {new_url}", bm.url);
        }
    }

    if opts.dry_run {
        println!("{changed} bookmark(s) would change.");
    } else {
        println!("Done. Updated {changed} bookmark(s).");
    }
    Ok(())
}

/// Whether `url`'s host is github.com or a `*.github.com` subdomain.
fn is_github_url(url: &str) -> bool {
    let after = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = after.split('/').next().unwrap_or(after);
    let host = host
        .rsplit('@')
        .next()
        .unwrap_or(host)
        .split(':')
        .next()
        .unwrap_or(host)
        .to_ascii_lowercase();
    host == "github.com" || host.ends_with(".github.com")
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
    let after = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let path = after.split('/').skip(1).collect::<Vec<_>>().join("/");
    let path = path.split(['?', '#']).next().unwrap_or(&path);
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
            "A thing\n\nProject homepage: https://example.com"
        );
        assert_eq!(d.tags, vec!["github-star", "lang:rust"]);
        assert_eq!(d.dedup_key, "github.com/owner/repo");
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

    #[tokio::test]
    async fn cleanup_canonicalizes_url_and_deletes_old() {
        use crate::pinboard::Bookmark;
        use crate::test_support::FakePinboard;

        let pinboard = FakePinboard {
            all: vec![Bookmark {
                url: "https://www.github.com/Owner/Repo.git".into(),
                description: "Owner/Repo".into(),
                extended: "notes".into(),
                tags: "github-star lang:rust".into(),
                time: "2020-01-01T00:00:00Z".into(),
                shared: "no".into(),
                toread: "no".into(),
            }],
            ..Default::default()
        };
        let bookmarks = pinboard.all.clone();
        cleanup(
            &pinboard,
            &GhCleanupOpts {
                dry_run: false,
                verbose: false,
            },
            &bookmarks,
        )
        .await
        .unwrap();

        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://github.com/Owner/Repo");
        // Tags are preserved as-is (phase 1 only touches the URL).
        assert_eq!(updated[0].tags, vec!["github-star", "lang:rust"]);
        assert_eq!(
            pinboard.deleted.borrow().as_slice(),
            &["https://www.github.com/Owner/Repo.git".to_string()]
        );
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
