//! `cleanup github`: canonicalize GitHub repo-root bookmarks and refresh each one from the
//! API (which follows renames and transfers), rewriting a moved repo's URL. Only what the
//! tool wrote is refreshed: notes in the shape `sync github` writes, and the title and tags
//! of bookmarks carrying the base tag.

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
/// (which follows renames/transfers) and rewrite a moved repo's URL to the current one.
/// Notes are rebuilt only when the tool wrote them ([`note_is_tool_generated`]); the base
/// and `lang:` tags only on a bookmark `sync github` wrote ([`is_synced_star`]), its title
/// only while it is still `owner/repo`, and (under `use_post_date`) its star date only
/// while its notes are the tool's too. Any other bookmark of a repo -- an HN story linking
/// it, one saved by hand -- keeps its title, notes, tags and date. A URL change updates
/// and deletes the old, preserving the creation time. A repo that no longer exists (404)
/// keeps just the URL canonicalization.
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
/// follows renames/transfers) — the current URL, plus what the tool owns of it: notes it
/// wrote, and a synced star's title and tags. A 404 keeps just the canonicalization.
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
        let synced = is_synced_star(bookmark, &self.client.config);
        // A failed lookup is surfaced to the driver, which logs and counts it (or, for a
        // dead credential, aborts the pass).
        let lookup = self.client.repo(&owner, &repo).await?;
        let names = repo_names(&bookmark.url, lookup.as_ref());
        // A rename can merge a star into another source's bookmark of the repo, leaving a
        // record with the base tag but that source's title or notes: it is not the star's
        // to re-title or re-date.
        let star_owns_title = synced && title_is_tool_generated(&title, &names);
        let note_is_tools = lookup
            .as_ref()
            .is_some_and(|info| note_is_tool_generated(&bookmark.note, info, &names, synced));
        let star_owns_date = star_owns_title && note_is_tools;
        if let Some(info) = lookup {
            if note_is_tools {
                note = github_note(
                    info.description.as_deref(),
                    info.homepage.as_deref(),
                    &info.html_url,
                );
            }
            if star_owns_title {
                title = html_to_plain(&info.full_name);
            }
            if synced {
                tags = refresh_tags(bookmark.tags.clone(), &info, &self.client.config);
            }
            url = Url::parse(&info.html_url).unwrap_or(url); // follows renames/transfers
        }

        let timestamp = url_key(&url)
            .filter(|_| star_owns_date)
            .and_then(|k| self.star_dates.get(&k).copied());
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

/// Whether `sync github` wrote this bookmark: it carries the base tag, the first non-empty
/// entry of `tags`. That makes its tags the tool's to refresh, and its title and star date
/// too while they still look the tool's (see [`run`]). With every base tag disabled
/// nothing marks a star, so no bookmark qualifies.
fn is_synced_star(bookmark: &Bookmark, cfg: &GitHubConfig) -> bool {
    cfg.tags
        .iter()
        .find(|tag| !tag.is_empty())
        .is_some_and(|base| bookmark.tags.contains(base))
}

/// Whether a repo bookmark's stored `note` is one the tool wrote, so a rebuild from fresh
/// repo data may replace it, rather than a hand-written note (or another source's, like an
/// HN story's `HN Link:`) to keep. The recognized shapes are [`github_note`]'s, each with
/// or without its `Project homepage:` line: empty, the current description in a
/// `<blockquote>`, a bare link to this repo (the no-description fallback), or the current
/// description unwrapped, as syncs before the `<blockquote>` retrofit wrote it. On a
/// `synced` star any lone `<blockquote>` counts, so an upstream description edit still
/// refreshes; elsewhere it may be a passage the user quoted when saving the page.
fn note_is_tool_generated(
    note: &str,
    repo: &GitHubRepo,
    names: &[(String, String)],
    synced: bool,
) -> bool {
    let body = match note.rsplit_once("\n\nProject homepage: ") {
        Some((body, homepage)) if !homepage.contains('\n') => body,
        _ => note,
    }
    .trim();
    let description = repo
        .description
        .as_deref()
        .map(str::trim)
        .filter(|description| !description.is_empty());
    body.is_empty()
        || (synced && is_lone_blockquote(body))
        || description.is_some_and(|description| {
            body == description || body == crate::htmltext::blockquote(description)
        })
        || links_to_repo(body, names)
}

/// Whether a synced star's (plain-text) `title` is still the `owner/repo` sync gave it,
/// under either of the repo's `names`, rather than one the user edited.
fn title_is_tool_generated(title: &str, names: &[(String, String)]) -> bool {
    let title = title.trim();
    title.is_empty()
        || title
            .split_once('/')
            .is_some_and(|(owner, name)| is_one_of(owner, name, names))
}

/// Whether `text` is exactly one `<blockquote>` element and nothing else.
fn is_lone_blockquote(text: &str) -> bool {
    text.strip_prefix("<blockquote>")
        .and_then(|rest| rest.strip_suffix("</blockquote>"))
        .is_some_and(|inner| !inner.contains("<blockquote>") && !inner.contains("</blockquote>"))
}

/// The `(owner, repo)` a bookmark's repo goes by: under its stored URL and, after a
/// rename or transfer, its current one.
fn repo_names(stored_url: &Url, repo: Option<&GitHubRepo>) -> Vec<(String, String)> {
    let current = repo.and_then(|repo| Url::parse(&repo.html_url).ok());
    [Some(stored_url), current.as_ref()]
        .into_iter()
        .flatten()
        .filter_map(repo_root)
        .collect()
}

/// Whether `owner/name` is one of `names`, compared as GitHub does, ignoring case.
fn is_one_of(owner: &str, name: &str, names: &[(String, String)]) -> bool {
    names.iter().any(|(known_owner, known_name)| {
        owner.eq_ignore_ascii_case(known_owner) && name.eq_ignore_ascii_case(known_name)
    })
}

/// Whether `text` is a bare URL of the root of the repo going by `names`.
fn links_to_repo(text: &str, names: &[(String, String)]) -> bool {
    Url::parse(text)
        .ok()
        .as_ref()
        .and_then(repo_root)
        .is_some_and(|(owner, name)| is_one_of(&owner, &name, names))
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

    fn repo(description: Option<&str>) -> GitHubRepo {
        GitHubRepo {
            full_name: "new/name".into(),
            html_url: "https://github.com/new/name".into(),
            description: description.map(String::from),
            homepage: None,
            language: None,
        }
    }

    /// Whether `note` reads as the tool's, for a star renamed old/name -> new/name whose
    /// description is now "Fresh".
    fn tool_shaped(note: &str, synced: bool) -> bool {
        let current = repo(Some("Fresh"));
        let names = repo_names(&url("https://github.com/old/name"), Some(&current));
        note_is_tool_generated(note, &current, &names, synced)
    }

    #[test]
    fn tool_shaped_notes_are_recognized_on_any_bookmark() {
        for note in [
            "",
            "  \n",
            "<blockquote>Fresh</blockquote>",
            "<blockquote>Fresh</blockquote>\n\nProject homepage: https://h.example",
            "https://github.com/old/name",
            "https://github.com/New/Name\n\nProject homepage: https://h.example",
            "Fresh",
            "Fresh\n\nProject homepage: https://h.example",
        ] {
            for synced in [true, false] {
                assert!(tool_shaped(note, synced), "{note:?} is tool-shaped");
            }
        }
    }

    #[test]
    fn a_stale_blockquote_is_the_tools_only_on_a_synced_star() {
        for note in [
            "<blockquote>An older description</blockquote>",
            "<blockquote>Stale</blockquote>\n\nProject homepage: https://old.example",
        ] {
            assert!(tool_shaped(note, true), "{note:?} on a star");
            assert!(
                !tool_shaped(note, false),
                "{note:?} may be a quoted passage"
            );
        }
    }

    #[test]
    fn other_notes_are_kept() {
        for note in [
            "notes",
            "HN Link: https://news.ycombinator.com/item?id=42",
            "HN Link: https://news.ycombinator.com/item?id=42\n\n<blockquote>Fresh</blockquote>",
            "<blockquote>Fresh</blockquote>\n\nmy own notes",
            "<blockquote>a</blockquote> and <blockquote>b</blockquote>",
            "https://github.com/someone/else",
            "https://github.com/old/name/issues/1",
            "An older description",
            "Fresh\n\nProject homepage: https://h.example\nmore of my notes",
        ] {
            assert!(!tool_shaped(note, true), "{note:?} is hand-written");
        }
    }

    #[test]
    fn only_an_owner_repo_title_is_the_tools() {
        let names = repo_names(&url("https://github.com/old/name"), Some(&repo(None)));
        for title in ["", "old/name", "New/Name", " new/name "] {
            assert!(title_is_tool_generated(title, &names), "{title:?}");
        }
        for title in [
            "Show HN: New",
            "my favourite tool",
            "someone/else",
            "old/name/x",
        ] {
            assert!(!title_is_tool_generated(title, &names), "{title:?}");
        }
    }

    #[test]
    fn a_synced_star_carries_the_first_enabled_base_tag() {
        let cfg = |tags: &[&str]| GitHubConfig {
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            ..GitHubConfig::default()
        };
        let tagged = |tags: &[&str]| Bookmark {
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            ..crate::test_support::stored_at("https://github.com/o/r")
        };
        assert!(is_synced_star(
            &tagged(&["github-star"]),
            &cfg(&["github-star"])
        ));
        assert!(is_synced_star(&tagged(&["star"]), &cfg(&["", "star", "x"])));
        assert!(!is_synced_star(&tagged(&["x"]), &cfg(&["star", "x"])));
        assert!(!is_synced_star(
            &tagged(&["hackernews"]),
            &cfg(&["github-star"])
        ));
        assert!(!is_synced_star(&tagged(&["github-star"]), &cfg(&[])));
        assert!(!is_synced_star(&tagged(&["github-star"]), &cfg(&[""])));
    }
}

#[cfg(test)]
mod net_tests;
