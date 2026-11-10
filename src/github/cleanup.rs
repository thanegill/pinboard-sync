//! `cleanup github`: canonicalizes repo-root bookmarks and refreshes each from the API,
//! which follows renames and transfers, then does the same for gists ([`gists`]). Only what
//! the tool wrote is refreshed: notes in `sync github`'s shape, and the title, tags and
//! date of a bookmark carrying a configured account's base tag (a gist's markers, for a
//! gist). [`run`] gives the rules.

use std::collections::HashMap;

use anyhow::Context;
use time::OffsetDateTime;
use url::Url;

use super::{github_note, repo_path, GitHubClient, GitHubConfig, GitHubRepo, HOMEPAGE_SEPARATOR};
use crate::backup::RawSink;
use crate::bookmark::{AccountState, Bookmark, BookmarkStore};
use crate::cleanup_pass::{run_pass, two_pass_verdict, CleanupPass, Plan};
use crate::config::own_then_configured;
use crate::htmltext::{collapse_whitespace, html_to_plain};
use crate::keys::url_key;
use crate::source::SourceError;
use crate::tags::{prefixed, Tags};
use crate::timefmt::DateOpts;
use crate::urls::UrlExt;

mod gists;

/// Normalize the repo bookmarks, then the gists, each pass reaching its own verdict. A repo
/// root (`github.com/owner/repo`) is looked up and a moved one rewritten to its current
/// URL. Only what the tool wrote is refreshed: notes in its shape
/// ([`note_is_tool_generated`]), and on a synced star ([`is_synced_star`]) its tags, its
/// title while still `owner/repo` and, under `use_post_date`, its star date while title and
/// notes are both the tool's. Any other bookmark of a repo, such as an HN story linking it,
/// keeps its title, tags and date, and any note not in the tool's shape. A repo gone (404)
/// keeps only its canonical URL.
///
/// A star carrying any `configured` account's base tag counts as synced, and its tags are
/// refreshed by each such account's config, the client's own first, so a second account's
/// star doesn't gain the first's base tag.
pub async fn run<S: BookmarkStore + AccountState>(
    store: &S,
    client: &GitHubClient,
    dates: DateOpts,
    configured: &[GitHubConfig],
) -> Result<(), SourceError> {
    let gh_bms = store.snapshot(|bookmark| bookmark.url.host_is("github.com"));

    // The repo lookup carries no `starred_at`. A repo no longer starred keeps its date.
    let star_dates: HashMap<String, OffsetDateTime> = if dates.use_post_date {
        client
            .fetch_stars(None, &mut RawSink::disabled())
            .await?
            .into_iter()
            .filter_map(|d| Some((url_key(&d.bookmark.url)?, d.bookmark.timestamp?)))
            .collect()
    } else {
        HashMap::new()
    };

    let pass = GitHubCleanupPass {
        client,
        star_dates,
        star_configs: star_configs(&client.config, configured),
    };
    let repos = run_pass(store, &gh_bms, "github", dates, &pass)
        .await
        .into_result();
    // A halt (dead token, spent quota, edge block) would fail every gist lookup too.
    if let Err(halt @ SourceError::Halted(_)) = repos {
        return Err(halt);
    }
    let gists = gists::run(store, client, dates, configured).await;
    two_pass_verdict(("repos", repos), ("gists", gists))
}

/// `own`, then each of `configured` with a base tag no earlier one has, so each base tag is
/// refreshed by one config, the client's own where they share it.
fn star_configs(own: &GitHubConfig, configured: &[GitHubConfig]) -> Vec<GitHubConfig> {
    let mut configs: Vec<GitHubConfig> = Vec::new();
    for cfg in own_then_configured(own, configured) {
        let Some(tag) = base_tag(cfg) else {
            continue;
        };
        if !configs.iter().any(|seen| base_tag(seen) == Some(tag)) {
            configs.push(cfg.clone());
        }
    }
    configs
}

/// Re-shapes one repo-root bookmark: its current URL and what the tool owns ([`run`]).
struct GitHubCleanupPass<'a> {
    client: &'a GitHubClient,
    star_dates: HashMap<String, OffsetDateTime>,
    /// One per distinct base tag, from [`star_configs`].
    star_configs: Vec<GitHubConfig>,
}

impl CleanupPass for GitHubCleanupPass<'_> {
    async fn plan(&self, bookmark: &Bookmark) -> Result<Plan, SourceError> {
        // No lookup, so no evidence GitHub is reachable.
        let Some(root) = repo_root(&bookmark.url) else {
            return Ok(Plan::Skipped);
        };
        let mut url = root
            .canonical_url()
            .context("building the canonical github URL")?;
        let mut title = collapse_whitespace(&bookmark.title, " ");
        let mut note = bookmark.note.clone();
        let mut tags = bookmark.tags.clone();
        let syncing: Vec<&GitHubConfig> = self
            .star_configs
            .iter()
            .filter(|cfg| is_synced_star(bookmark, cfg))
            .collect();
        let synced = !syncing.is_empty();
        let lookup = self.client.repo(&root.owner, &root.name).await?;
        let names = repo_names(&bookmark.url, lookup.as_ref());
        // A rename can merge a star into another source's bookmark, leaving the base tag on
        // that source's title or notes, which are not the star's to change.
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
                tags = refresh_tags(tags, &info, &syncing);
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

/// Whether `host` serves repo pages: `github.com` or its `www.` alias, no other subdomain.
fn is_repo_host(host: &str) -> bool {
    matches!(host, "github.com" | "www.github.com")
}

/// A repo's `owner/name`, as a repo-root URL spells it.
#[derive(Debug, PartialEq, Eq)]
struct RepoName {
    owner: String,
    name: String,
}

impl RepoName {
    /// Whether this is `owner/name`, compared as GitHub does, ignoring case.
    fn is(&self, owner: &str, name: &str) -> bool {
        self.owner.eq_ignore_ascii_case(owner) && self.name.eq_ignore_ascii_case(name)
    }

    /// `https://github.com/<owner>/<name>`, written when the API has no such repo.
    fn canonical_url(&self) -> Result<Url, url::ParseError> {
        Url::parse(&format!("https://github.com/{}/{}", self.owner, self.name))
    }
}

/// The [`RepoName`] of a repo-root URL: an [`is_repo_host`] host and a [`repo_path`].
/// `None` for a deep link, another host, or a name that is no repo's (`/owner/.git`).
fn repo_root(url: &Url) -> Option<RepoName> {
    if !is_repo_host(url.host_str()?) {
        return None;
    }
    let (owner, name) = repo_path(url.path_segments()?)?;
    Some(RepoName {
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

/// Whether `sync github` wrote this bookmark under `cfg`: it carries that config's
/// [`base_tag`], so its tags are the tool's, and its title and star date while they still
/// look the tool's ([`run`]). With the base tags disabled nothing marks a star.
fn is_synced_star(bookmark: &Bookmark, cfg: &GitHubConfig) -> bool {
    base_tag(cfg).is_some_and(|base| bookmark.tags.has(base))
}

/// The tag marking a star synced under `cfg`: its first non-empty `tags` entry.
fn base_tag(cfg: &GitHubConfig) -> Option<&str> {
    cfg.tags
        .iter()
        .map(String::as_str)
        .find(|tag| !tag.is_empty())
}

/// Whether a repo bookmark's `note` is the tool's, so a rebuild may replace it, rather than
/// the user's or another source's (an HN `HN Link:`). The tool's shapes, each with or
/// without its `Project homepage:` line: empty, the current description in a `<blockquote>`
/// or bare (a legacy shape), or a bare link to the repo (the no-description fallback). On a
/// `synced` star any lone `<blockquote>` counts, so an upstream description edit still
/// refreshes; elsewhere it may be a passage the user quoted.
fn note_is_tool_generated(note: &str, repo: &GitHubRepo, names: &[RepoName], synced: bool) -> bool {
    let body = match note.rsplit_once(HOMEPAGE_SEPARATOR) {
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

/// Whether a synced star's `title` is still sync's `owner/repo`, under either of `names`.
fn title_is_tool_generated(title: &str, names: &[RepoName]) -> bool {
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

/// The repo's names: its stored URL's and, after a rename or transfer, its current one.
fn repo_names(stored_url: &Url, repo: Option<&GitHubRepo>) -> Vec<RepoName> {
    let current = repo.and_then(|repo| Url::parse(&repo.html_url).ok());
    [Some(stored_url), current.as_ref()]
        .into_iter()
        .flatten()
        .filter_map(repo_root)
        .collect()
}

/// Whether `owner/name` is one of `names` ([`RepoName::is`]).
fn is_one_of(owner: &str, name: &str, names: &[RepoName]) -> bool {
    names.iter().any(|known| known.is(owner, name))
}

/// Whether `text` is a bare URL of the root of the repo going by `names`.
fn links_to_repo(text: &str, names: &[RepoName]) -> bool {
    Url::parse(text)
        .ok()
        .as_ref()
        .and_then(repo_root)
        .is_some_and(|linked| is_one_of(&linked.owner, &linked.name, names))
}

/// The tags with each of `configs`' language and owner tags refreshed and base tags
/// ensured. Every old prefixed tag goes before any is added, so the order of `configs`
/// doesn't matter even when one prefix starts another (`lang:`, `lang:work:`).
fn refresh_tags(existing: Tags, repo: &GitHubRepo, configs: &[&GitHubConfig]) -> Tags {
    let prefixes: Vec<&str> = configs
        .iter()
        .flat_map(|cfg| [cfg.lang_prefix.as_str(), cfg.author_prefix.as_str()])
        .collect();
    let lang = repo.language.as_deref().filter(|lang| !lang.is_empty());
    let additions = configs.iter().flat_map(|cfg| {
        let lang_tag = lang.and_then(|lang| prefixed(&cfg.lang_prefix, &lang.to_lowercase()));
        let owner_tag = prefixed(&cfg.author_prefix, repo.owner());
        cfg.tags.iter().cloned().chain(lang_tag).chain(owner_tag)
    });
    retag(existing, &prefixes, additions)
}

/// `existing` without tags under `prefixes` (an empty one marks none), plus `additions`.
fn retag(mut tags: Tags, prefixes: &[&str], additions: impl IntoIterator<Item = String>) -> Tags {
    tags.retain(|tag| {
        !prefixes
            .iter()
            .any(|prefix| !prefix.is_empty() && tag.starts_with(prefix))
    });
    for tag in additions {
        tags.push(&tag);
    }
    tags
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::keys::{GitHubKeys, HackerNewsKeys};
    use crate::source::UrlKey;
    use crate::test_support::{stored_at, url};

    /// A deep link, a gist, a bare owner, or a name `.git` leaves empty or a dot segment is
    /// not this pass's bookmark: it is skipped before any lookup, so it says nothing about
    /// whether GitHub is reachable and must not count toward `PassOutcome::reached`.
    #[tokio::test]
    async fn a_bookmark_that_is_no_repo_root_is_skipped_without_a_lookup() {
        let client = GitHubClient::with_base_url(
            "tok".into(),
            GitHubConfig::default(),
            "http://unused.invalid".into(),
        );
        let pass = GitHubCleanupPass {
            client: &client,
            star_dates: HashMap::new(),
            star_configs: Vec::new(),
        };
        for stored in [
            "https://github.com/o/r/issues/5",
            "https://gist.github.com/user/abcd1234",
            "https://github.com/o",
            "https://github.com/o/.git",
            "https://github.com/o/..git",
            "https://github.com/o/%2E%2E.git",
            "https://github.com/o/.GIT",
            "https://github.com/o/.git.git",
        ] {
            let (plan, waits) = crate::http::recording_waits(pass.plan(&stored_at(stored))).await;
            assert_eq!(plan.unwrap(), Plan::Skipped, "{stored}");
            assert!(waits.is_empty(), "{stored} made a request");
        }
    }

    #[test]
    fn repo_root_only_matches_bare_github_repo_roots() {
        let owner_repo = Some(RepoName {
            owner: "Owner".into(),
            name: "Repo".into(),
        });
        // Bare owner/repo on github.com, with .git and trailing slash tolerated.
        assert_eq!(repo_root(&url("https://github.com/Owner/Repo")), owner_repo);
        assert_eq!(
            repo_root(&url("https://github.com/Owner/Repo.git")),
            owner_repo
        );
        assert_eq!(
            repo_root(&url("https://github.com/Owner/Repo.GIT")),
            owner_repo
        );
        assert_eq!(
            repo_root(&url("https://github.com/Owner/Repo.git.GIT")),
            owner_repo
        );
        assert_eq!(
            repo_root(&url("https://github.com/Owner/Repo/")),
            owner_repo
        );
        // The www. alias is the same repo host as the bare github.com.
        assert_eq!(
            repo_root(&url("https://www.github.com/Owner/Repo")),
            owner_repo
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
        // A name that `.git` leaves empty or a dot segment would build the owner's or the
        // site's URL.
        for stored in [
            "https://github.com/o/.git",
            "https://www.github.com/o/.git/",
            "https://github.com/o/..git",
            "https://github.com/o/...git",
            "https://github.com/o/%2e.git",
            "https://github.com/o/%2E%2E.git",
            "https://github.com/o/.GIT",
            "https://github.com/o/.git.git",
        ] {
            assert_eq!(repo_root(&url(stored)), None, "{stored}");
        }
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
            tags: crate::test_support::strings(tags),
            ..GitHubConfig::default()
        };
        let tagged = |tags: &[&str]| Bookmark {
            tags: crate::test_support::tag_list(tags),
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

    #[test]
    fn refreshed_tags_are_the_same_whichever_account_runs() {
        let cfg = |base: &str, lang_prefix: &str, author_prefix: &str| GitHubConfig {
            tags: vec![base.into()],
            lang_prefix: lang_prefix.into(),
            author_prefix: author_prefix.into(),
            ..GitHubConfig::default()
        };
        let (home, work) = (
            cfg("github-star", "lang:", "author:github:"),
            cfg("work-star", "lang:work:", "author:github:work:"),
        );
        let rust = GitHubRepo {
            language: Some("Rust".into()),
            ..repo(None)
        };
        let existing: Vec<String> = [
            "github-star",
            "work-star",
            "lang:go",
            "lang:work:go",
            "author:github:old",
            "author:github:work:old",
            "x",
        ]
        .map(String::from)
        .to_vec();
        let refreshed = |configs: &[&GitHubConfig]| {
            let mut tags = refresh_tags(existing.clone().into(), &rust, configs);
            tags.sort();
            tags
        };
        let expected = [
            "author:github:new",
            "author:github:work:new",
            "github-star",
            "lang:rust",
            "lang:work:rust",
            "work-star",
            "x",
        ];
        assert_eq!(refreshed(&[&home, &work]), expected);
        assert_eq!(refreshed(&[&work, &home]), expected);
    }

    #[test]
    fn star_configs_keep_one_config_per_base_tag_the_clients_first() {
        let cfg = |tags: &[&str], lang_prefix: &str| GitHubConfig {
            tags: crate::test_support::strings(tags),
            lang_prefix: lang_prefix.into(),
            ..GitHubConfig::default()
        };
        let own = cfg(&["github-star"], "own:");
        let configs = star_configs(
            &own,
            &[
                cfg(&["github-star"], "other:"),
                cfg(&[""], "off:"),
                cfg(&["", "work-star"], "work:"),
                cfg(&["work-star", "x"], "dup:"),
            ],
        );
        let kept: Vec<(Option<&str>, &str)> = configs
            .iter()
            .map(|cfg| (base_tag(cfg), cfg.lang_prefix.as_str()))
            .collect();
        assert_eq!(
            kept,
            [(Some("github-star"), "own:"), (Some("work-star"), "work:")]
        );
    }

    /// A repo-root URL in any of the forms `cleanup github` canonicalizes: either host,
    /// either scheme, any case, doubled slashes, with or without `.git` in either case, a
    /// trailing slash, a query or a fragment. The name may end in `.git`s too, as long as
    /// stripping them leaves a name, and not `.` or `..`.
    fn stored_repo_root() -> impl Strategy<Value = String> {
        (
            prop::sample::select(vec!["http", "https"]),
            prop::sample::select(vec!["github.com", "www.github.com", "GitHub.com"]),
            prop::sample::select(vec!["/", "//"]),
            "[A-Za-z0-9]([A-Za-z0-9-]{0,10}[A-Za-z0-9])?",
            prop::sample::select(vec!["/", "//"]),
            "([A-Za-z0-9._-]{1,12})(\\.[gG][iI][tT]){0,2}".prop_filter("a repo name", |name| {
                let mut bare = name.to_ascii_lowercase();
                while let Some(len) = bare.strip_suffix(".git").map(str::len) {
                    bare.truncate(len);
                }
                !matches!(bare.as_str(), "" | "." | "..")
            }),
            prop::sample::select(vec!["", ".git", ".GIT"]),
            prop::sample::select(vec!["", "/", "//"]),
            prop::sample::select(vec!["", "?tab=readme-ov-file", "#readme"]),
        )
            .prop_map(
                |(scheme, host, lead, owner, separator, name, suffix, slash, tail)| {
                    format!("{scheme}://{host}{lead}{owner}{separator}{name}{suffix}{slash}{tail}")
                },
            )
    }

    proptest! {
        #![proptest_config(crate::test_support::proptest_config())]

        #[test]
        fn a_repo_root_keys_the_same_once_canonicalized(stored in stored_repo_root()) {
            let stored = url(&stored);
            let root = repo_root(&stored).expect("a repo root");
            let canonical = root.canonical_url().expect("a canonical URL");
            let rerun = repo_root(&canonical);
            prop_assert_eq!(rerun.as_ref(), Some(&root), "{}", canonical);
            prop_assert!(GitHubKeys.dedup_key(&stored).is_some(), "{} has no key", stored);
            prop_assert_eq!(
                GitHubKeys.dedup_key(&canonical),
                GitHubKeys.dedup_key(&stored),
                "{} -> {}",
                stored,
                canonical
            );
            let hackernews = HackerNewsKeys::default();
            prop_assert_eq!(
                hackernews.dedup_key(&canonical),
                hackernews.dedup_key(&stored),
                "{} -> {}",
                stored,
                canonical
            );
        }
    }
}

#[cfg(test)]
mod net_tests;
