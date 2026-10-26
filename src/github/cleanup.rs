//! `cleanup github`: canonicalize GitHub repo-root bookmarks and refresh each one from the
//! API (which follows renames and transfers), rewriting a moved repo's URL and refreshing
//! its title, notes and language tag.

use std::collections::HashMap;

use anyhow::Context;
use time::OffsetDateTime;
use url::Url;

use super::{github_extended, GitHubClient, GitHubConfig, GitHubRepo};
use crate::bookmark::{AccountState, Bookmark, BookmarkStore};
use crate::cleanup_pass::{run_pass, CleanupPass, Plan};
use crate::htmltext::html_to_plain;
use crate::source::{extend_unique, push_prefixed, url_key, Source, SourceError, UrlExt};
use crate::timefmt::DateOpts;

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
    config: &GitHubConfig,
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

    let pass = GitHubCleanupPass {
        client,
        config,
        star_dates,
    };
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
    config: &'a GitHubConfig,
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
            note = github_extended(
                info.description.as_deref(),
                info.homepage.as_deref(),
                &info.html_url,
            );
            tags = refresh_tags(bookmark.tags.clone(), &info, self.config);
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

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
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
}

#[cfg(test)]
mod net_tests {
    use super::*;
    use crate::bookmark::{AccountView, CleanupStore};
    use serde_json::json;
    use wiremock::matchers::{method, path};
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

        let err = run(
            &CleanupStore::new(&pinboard, AccountView::new(bookmarks.clone()), false),
            &client,
            &GitHubConfig::default(),
            DateOpts {
                use_post_date: false,
                max_age_days: 30,
                stale_to_now: false,
            },
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

        let err = run(
            &CleanupStore::new(&pinboard, AccountView::new(bookmarks.clone()), false),
            &client,
            &GitHubConfig::default(),
            DateOpts {
                use_post_date: false,
                max_age_days: 30,
                stale_to_now: false,
            },
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("rate limit"), "{err}");
        // The repo read before the limit is still rewritten — the work isn't thrown away.
        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://github.com/new/name");
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

        run(
            &CleanupStore::new(&pinboard, AccountView::new(bookmarks.clone()), false),
            &client,
            &GitHubConfig::default(),
            DateOpts {
                use_post_date: false,
                max_age_days: 30,
                stale_to_now: false,
            },
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
        run(
            &CleanupStore::new(&pinboard, AccountView::new(bookmarks.clone()), false),
            &client,
            &GitHubConfig::default(),
            DateOpts {
                use_post_date: false,
                max_age_days: 30,
                stale_to_now: false,
            },
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
        run(
            &CleanupStore::new(&pinboard, AccountView::new(bookmarks.clone()), false),
            &client,
            &GitHubConfig::default(),
            DateOpts {
                use_post_date: false,
                max_age_days: 30,
                stale_to_now: false,
            },
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
        run(
            &CleanupStore::new(&pinboard, AccountView::new(bookmarks.clone()), false),
            &client,
            &GitHubConfig::default(),
            DateOpts {
                use_post_date: false,
                max_age_days: 30,
                stale_to_now: false,
            },
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
        run(
            &CleanupStore::new(&pinboard, AccountView::new(bookmarks.clone()), false),
            &client,
            &GitHubConfig::default(),
            DateOpts {
                use_post_date: false,
                max_age_days: 30,
                stale_to_now: false,
            },
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
        run(
            &CleanupStore::new(&pinboard, AccountView::new(bookmarks.clone()), false),
            &client,
            &GitHubConfig::default(),
            DateOpts {
                use_post_date: false,
                max_age_days: 30,
                stale_to_now: false,
            },
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
