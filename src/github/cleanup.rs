//! `cleanup github`: canonicalize GitHub repo-root bookmarks and refresh each one from the
//! API (which follows renames and transfers), rewriting a moved repo's URL and refreshing
//! its title, notes and language tag.

use std::collections::HashMap;

use anyhow::Context;
use time::OffsetDateTime;
use url::Url;

use super::{github_note, GitHubClient, GitHubConfig, GitHubRepo};
use crate::bookmark::{AccountState, Bookmark, BookmarkStore};
use crate::cleanup_pass::{run_pass, CleanupPass, Plan};
use crate::htmltext::html_to_plain;
use crate::source::{Source, SourceError};
use crate::tags::{extend_unique, push_prefixed};
use crate::timefmt::DateOpts;
use crate::urls::{url_key, UrlExt};

/// Normalize existing GitHub repo bookmarks. Only genuine repo-root URLs on the bare
/// `github.com` host (`owner/repo`) are touched; deep links (issues/PRs/blob/tree) and
/// gist or other subdomains are left alone. For each repo root, look it up via the API
/// (which follows renames/transfers), rewriting a moved repo's URL to the current one,
/// setting the title to the current `owner/repo`, and refreshing the `lang:` tag. A
/// URL change updates + deletes the old; notes are kept and creation time preserved.
/// A repo that no longer exists (404) keeps just the URL canonicalization. Under
/// `use_post_date`, bookmarks are re-dated to when the repo was starred.
pub async fn run<S: BookmarkStore + AccountState>(
    store: &S,
    client: &GitHubClient,
    dates: DateOpts,
) -> Result<(), SourceError> {
    // Taken from the run's live view, so an earlier source's writes are already reflected.
    let gh_bms: Vec<Bookmark> = store
        .snapshot()
        .into_iter()
        .filter(|bookmark| bookmark.url.host_is("github.com"))
        .collect();

    // `starred_at` isn't on the per-repo lookup, so for use_post_date fetch the star
    // list once and map url_key -> star time. A repo no longer starred won't be here
    // (it keeps its existing time).
    let star_dates: HashMap<String, OffsetDateTime> = if dates.use_post_date {
        client
            .fetch()
            .await?
            .into_iter()
            .filter_map(|d| Some((url_key(&d.bookmark.url)?, d.bookmark.timestamp?)))
            .collect()
    } else {
        HashMap::new()
    };

    let pass = GitHubCleanupPass { client, star_dates };
    run_pass(store, &gh_bms, "github", dates, &pass)
        .await
        .into_result()
}

/// Re-shapes one GitHub repo-root bookmark: skip anything that isn't a bare
/// `github.com/owner/repo` URL, else canonicalize it and refresh from the API (which
/// follows renames/transfers) — current URL/title, rebuilt `<blockquote>` notes, and
/// language tag. A 404 keeps just the canonicalization.
struct GitHubCleanupPass<'a> {
    client: &'a GitHubClient,
    star_dates: HashMap<String, OffsetDateTime>,
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
        // A failed lookup is surfaced to the driver, which logs and counts it (or, for a
        // dead credential, aborts the pass).
        if let Some(info) = self.client.repo(&owner, &repo).await? {
            title = html_to_plain(&info.full_name);
            // Rebuild the notes from fresh data so old bookmarks retrofit to the
            // <blockquote> shape (sync skips already-present bookmarks).
            note = github_note(
                info.description.as_deref(),
                info.homepage.as_deref(),
                &info.html_url,
            );
            tags = refresh_tags(bookmark.tags.clone(), &info, &self.client.config);
            url = Url::parse(&info.html_url).unwrap_or(url); // follows renames/transfers
        }

        let timestamp = url_key(&url).and_then(|k| self.star_dates.get(&k).copied());
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
    let [owner, repo] = segments[..] else {
        return None;
    };
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    Some((owner.to_string(), repo.to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::url;

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
}

#[cfg(test)]
mod net_tests;
