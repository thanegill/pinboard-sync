//! The dedup keys: how each source keys a URL, so `sync` can tell a draft from a bookmark
//! already on Pinboard. Each source's scheme is a [`UrlKey`] type carrying only what it
//! needs ([`GitHubKeys`], [`RedditKeys`], [`HackerNewsKeys`]), built on the generic
//! host+path [`url_key`]. A key must survive `cleanup`'s canonicalization, or `sync`
//! re-adds what `cleanup` moved.

use url::Url;

use crate::github::gists::{gist_root_id, GIST_HOST};
use crate::github::repo_path;
use crate::hackernews::HackerNewsItemId;
use crate::reddit::model::unwrap_over18;
use crate::source::{DedupKey, UrlKey};
use crate::urls::{host_matches, UrlExt};

/// A host+path dedup key for a URL: scheme and query/fragment dropped, host lowercased
/// (by the `url` crate), path lowercased with any trailing slash removed -- e.g.
/// `https://GitHub.com/Owner/Repo/?tab=x` -> `github.com/owner/repo`. Dropping the query
/// is deliberate canonicalization; the exception is a non-source domain whose identity
/// lives in the query string (e.g. YouTube `watch?v=...`), for which
/// [`crate::domains::dedup_key_suffix`] appends a query-derived suffix so distinct
/// resources don't collapse to one key. Returns `None` for a URL without a host.
pub fn url_key(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    let path = url.path().trim_end_matches('/').to_ascii_lowercase();
    let mut key = format!("{host}{path}");
    if let Some(suffix) = crate::domains::dedup_key_suffix(host, &path, url) {
        key.push_str(&suffix);
    }
    Some(key)
}

/// GitHub's key: [`url_key`], gated to github.com hosts, with the `www.` alias folded onto
/// the bare host, and a repo root read as its [`repo_path`], so its empty segments and
/// `.git` suffixes are dropped. Those are the forms `cleanup github` canonicalizes to
/// `github.com/owner/repo`, so a bookmark keys the same before and after it does. A gist
/// root keys by its id alone (`gist.github.com/<id>`), so its owner-less form matches the
/// `gist.github.com/<owner>/<id>` form cleanup moves it to. Anything else, a deep link,
/// an owner's page or another subdomain, keeps its own host and path, less its empty
/// segments, so `o//r/issues/5` dedups with `o/r/issues/5` though cleanup moves neither.
#[derive(Debug, Clone, Copy, Default)]
pub struct GitHubKeys;

impl UrlKey for GitHubKeys {
    fn dedup_key(&self, url: &Url) -> Option<DedupKey> {
        if let Some(id) = gist_root_id(url) {
            return Some(format!("{GIST_HOST}/{id}").into());
        }
        let key = url_key(url).filter(|_| url.host_is("github.com"))?;
        let key = key.strip_prefix("www.").unwrap_or(&key);
        let (host, path) = key.split_once('/').unwrap_or((key, ""));
        let mut segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let repo_root = repo_path(segments.iter().copied()).filter(|_| host == "github.com");
        if let Some((owner, name)) = repo_root {
            return Some(format!("github.com/{owner}/{name}").into());
        }
        segments.insert(0, host);
        Some(segments.join("/").into())
    }
}

/// Reddit's key: the path, lowercased, with query/fragment and trailing slash removed, or
/// `None` off Reddit and for the bare root. Only the path is used, so the same post saved
/// under any Reddit host (`old.`/`www.`/`m.`) maps to the same key. A Reddit host is
/// reddit.com, a subdomain of it, or one of `domains`, the `reddit_domain`s of the
/// accounts in play: a mirror outside reddit.com counts too, since that is where sync
/// writes its account's bookmarks. An `over18` interstitial keys as its `dest`, the URL
/// `cleanup reddit` unwraps it to.
#[derive(Debug, Clone, Default)]
pub struct RedditKeys {
    domains: Vec<String>,
}

impl RedditKeys {
    pub fn new<D: Into<String>>(domains: impl IntoIterator<Item = D>) -> Self {
        Self {
            domains: domains.into_iter().map(Into::into).collect(),
        }
    }

    fn is_reddit_host(&self, url: &Url) -> bool {
        url.host_str().is_some_and(|host| {
            host_matches(host, "reddit.com")
                || self
                    .domains
                    .iter()
                    .any(|domain| host.eq_ignore_ascii_case(domain))
        })
    }
}

impl UrlKey for RedditKeys {
    fn dedup_key(&self, url: &Url) -> Option<DedupKey> {
        let url = &unwrap_over18(url);
        if !self.is_reddit_host(url) {
            return None;
        }
        let key = url.path().trim_end_matches('/').to_ascii_lowercase();
        (!key.is_empty()).then(|| key.into())
    }
}

/// HackerNews' key, shared by a draft and a stored bookmark at the same URL. An HN item
/// permalink keys on its id (`hn:<id>`), even as a story's article URL (a meta post),
/// since a host+path key drops the id query. An article on Reddit or GitHub keys as that
/// source's scheme does, so it still matches once `cleanup reddit` or `cleanup github` has
/// moved it to its canonical URL. Any other article falls back to the host+path key.
///
/// Reddit's `over18` interstitial keys as its `dest`, which `cleanup reddit` unwraps it
/// to, and the bare Reddit root, which [`RedditKeys`] leaves unkeyed, keys as `/`, the
/// path cleanup keeps when it moves it onto the account's domain.
#[derive(Debug, Clone, Default)]
pub struct HackerNewsKeys {
    reddit: RedditKeys,
    github: GitHubKeys,
}

impl HackerNewsKeys {
    /// Keys an article on Reddit by `reddit`, whose domains are the configured Reddit
    /// accounts' `reddit_domain`s.
    pub fn new(reddit: RedditKeys) -> Self {
        Self {
            reddit,
            github: GitHubKeys,
        }
    }
}

impl UrlKey for HackerNewsKeys {
    fn dedup_key(&self, url: &Url) -> Option<DedupKey> {
        let url = &unwrap_over18(url);
        if let Ok(id) = HackerNewsItemId::try_from(url) {
            return Some(id.dedup_key());
        }
        if self.reddit.is_reddit_host(url) {
            let key = self.reddit.dedup_key(url);
            return Some(key.unwrap_or_else(|| "/".into()));
        }
        self.github
            .dedup_key(url)
            .or_else(|| url_key(url).map(DedupKey::from))
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::test_support::url;

    #[test]
    fn url_key_normalizes_host_and_path() {
        assert_eq!(
            url_key(&url("https://GitHub.com/Owner/Repo/?tab=stars")).as_deref(),
            Some("github.com/owner/repo")
        );
        assert_eq!(
            url_key(&url("http://news.ycombinator.com/item?id=42")).as_deref(),
            Some("news.ycombinator.com/item")
        );
    }

    #[test]
    fn url_key_delegates_to_domains_for_query_significant_hosts() {
        // Two YouTube videos differing only in `?v=` must not collapse to one key
        // (the domains rule appends the id); a GitHub `?tab=` still drops the query.
        assert_ne!(
            url_key(&url("https://www.youtube.com/watch?v=AAAAAAAAAAA")),
            url_key(&url("https://www.youtube.com/watch?v=BBBBBBBBBBB"))
        );
        assert_eq!(
            url_key(&url("https://github.com/owner/repo?tab=stars")),
            url_key(&url("https://github.com/owner/repo?tab=issues"))
        );
    }

    #[test]
    fn github_keys_fold_the_forms_cleanup_canonicalizes() {
        let key = |s: &str| {
            GitHubKeys
                .dedup_key(&url(s))
                .map(|key| key.as_str().to_string())
        };
        for alias in [
            "https://github.com/Owner/Repo",
            "https://www.github.com/Owner/Repo",
            "https://github.com/Owner/Repo.git",
            "https://www.github.com/owner/repo.git/",
            "https://github.com/owner//repo",
            "https://github.com//owner/repo/",
        ] {
            assert_eq!(
                key(alias).as_deref(),
                Some("github.com/owner/repo"),
                "{alias}"
            );
        }
        assert_eq!(
            key("https://github.com/o/r.git/tree/main").as_deref(),
            Some("github.com/o/r.git/tree/main"),
            "only a repo root drops `.git`"
        );
        for nameless in [
            "https://github.com/o/.git",
            "https://github.com/o/..git",
            "https://github.com/o/.git.git",
        ] {
            assert_eq!(
                key(nameless).as_deref(),
                Some(&nameless["https://".len()..]),
                "a name `.git` leaves empty or a dot segment is no repo's, so nothing is dropped"
            );
        }
        assert_eq!(key("https://example.com/o/r"), None);
    }

    #[test]
    fn github_keys_key_a_gist_root_by_its_id() {
        let key = |s: &str| {
            GitHubKeys
                .dedup_key(&url(s))
                .map(|key| key.as_str().to_string())
        };
        let id = "aa5a315d61ae9438b18d";
        for alias in [
            format!("https://gist.github.com/{id}"),
            format!("https://gist.github.com/Someone/{id}"),
            format!("http://gist.github.com/someone/{id}/"),
            format!("https://gist.github.com/someone/{}.git", id.to_uppercase()),
            format!("https://gist.github.com/someone/{id}.Git.GIT"),
            format!("https://gist.github.com/someone/{id}?file=x#y"),
        ] {
            assert_eq!(
                key(&alias),
                Some(format!("gist.github.com/{id}")),
                "{alias}"
            );
        }
        assert_eq!(
            key("https://gist.github.com/1234567").as_deref(),
            Some("gist.github.com/1234567"),
            "an old numeric id"
        );
        // Not gist roots: an owner's gist list (even an all-hex name), a revision.
        assert_eq!(
            key("https://gist.github.com/cafe").as_deref(),
            Some("gist.github.com/cafe")
        );
        assert_eq!(
            key(&format!("https://gist.github.com/someone/{id}/0123abc")),
            Some(format!("gist.github.com/someone/{id}/0123abc"))
        );
    }

    #[test]
    fn github_keys_collapse_empty_segments_and_change_nothing_else() {
        let key = |s: &str| {
            GitHubKeys
                .dedup_key(&url(s))
                .map(|key| key.as_str().to_string())
        };
        let id = "aa5a315d61ae9438b18d";
        for (doubled, single) in [
            (
                "https://github.com/o//r/issues/5",
                "github.com/o/r/issues/5",
            ),
            (
                "https://www.github.com//O/R//tree/main//",
                "github.com/o/r/tree/main",
            ),
            ("https://github.com//owner", "github.com/owner"),
            (
                "https://docs.github.com/en//actions",
                "docs.github.com/en/actions",
            ),
            (
                &format!("https://gist.github.com/someone//{id}/raw"),
                &format!("gist.github.com/someone/{id}/raw"),
            ),
        ] {
            assert_eq!(key(doubled).as_deref(), Some(single), "{doubled}");
            assert_eq!(
                key(&format!("https://{single}")).as_deref(),
                Some(single),
                "a key without empty segments is the URL's host and path"
            );
        }
        assert_eq!(
            key("https://github.com/o/r.git//tree/main").as_deref(),
            Some("github.com/o/r.git/tree/main"),
            "a deep link keeps its `.git`"
        );
        assert_eq!(
            HackerNewsKeys::default().dedup_key(&url("https://github.com/o//r/issues/5")),
            HackerNewsKeys::default().dedup_key(&url("https://github.com/o/r/issues/5")),
        );
    }

    /// A GitHub path segment that is no dot segment, which a URL parser would resolve
    /// against its neighbours.
    fn github_segment() -> impl Strategy<Value = String> {
        "[A-Za-z0-9._-]{1,8}".prop_filter("not a dot segment", |segment| {
            !matches!(segment.as_str(), "." | "..")
        })
    }

    proptest! {
        #![proptest_config(crate::test_support::proptest_config())]

        #[test]
        fn a_github_url_keys_the_same_with_doubled_slashes(
            host in prop::sample::select(vec![
                "github.com",
                "www.github.com",
                "GitHub.com",
                "gist.github.com",
                "docs.github.com",
            ]),
            segments in prop::collection::vec(
                (prop::sample::select(vec!["/", "//"]), github_segment()),
                1..=5,
            ),
            trailing in prop::sample::select(vec!["", "/", "//"]),
            tail in prop::sample::select(vec!["", "?tab=readme-ov-file", "#L10"]),
        ) {
            let path = |doubled: bool| {
                segments
                    .iter()
                    .map(|(separator, segment)| {
                        format!("{}{segment}", if doubled { separator } else { "/" })
                    })
                    .collect::<String>()
            };
            let doubled = url(&format!("https://{host}{}{trailing}{tail}", path(true)));
            let single = url(&format!("https://{host}{}{tail}", path(false)));
            prop_assert_eq!(
                GitHubKeys.dedup_key(&doubled),
                GitHubKeys.dedup_key(&single),
                "{} vs {}",
                doubled,
                single
            );
            let hackernews = HackerNewsKeys::default();
            prop_assert_eq!(
                hackernews.dedup_key(&doubled),
                hackernews.dedup_key(&single),
                "{} vs {}",
                doubled,
                single
            );
        }
    }

    #[test]
    fn hackernews_keys_a_gist_article_as_github_does() {
        let id = "aa5a315d61ae9438b18d";
        let hn = |s: &str| HackerNewsKeys::default().dedup_key(&url(s));
        assert_eq!(
            hn(&format!("https://gist.github.com/{id}")),
            hn(&format!("https://gist.github.com/someone/{id}")),
            "a story's owner-less gist link still matches once cleanup adds the owner"
        );
        assert_eq!(
            hn(&format!("https://gist.github.com/{id}")),
            GitHubKeys.dedup_key(&url(&format!("https://gist.github.com/someone/{id}")))
        );
    }

    fn key(raw: &str) -> Option<String> {
        RedditKeys::new(["old.reddit.com"])
            .dedup_key(&url(raw))
            .map(|key| key.as_str().to_string())
    }

    #[test]
    fn reddit_keys_treat_the_configured_domain_as_reddit() {
        let mirror = |raw: &str| RedditKeys::new(["redlib.example.org"]).dedup_key(&url(raw));
        let want = Some(DedupKey::from("/r/rust/comments/abc/title"));
        assert_eq!(
            mirror("https://redlib.example.org/r/rust/comments/abc/title/"),
            want
        );
        assert_eq!(
            mirror("https://www.reddit.com/r/Rust/comments/abc/title/"),
            want
        );
        // The URL's host comes back lowercased, whatever case the config spells it in.
        assert_eq!(
            RedditKeys::new(["Redlib.Example.org"]).dedup_key(&url(
                "https://Redlib.Example.org/r/rust/comments/abc/title/"
            )),
            want
        );
        // Only the configured host itself, not its parent or a sibling.
        assert_eq!(
            mirror("https://example.org/r/rust/comments/abc/title/"),
            None
        );
        assert_eq!(
            key("https://redlib.example.org/r/rust/comments/abc/title/"),
            None
        );
    }

    #[test]
    fn reddit_keys_match_across_subdomains_and_query_and_trailing_slash() {
        let want = "/r/rust/comments/abc/title";
        // The same post bookmarked under any host / case / query / no trailing slash.
        for url in [
            "https://old.reddit.com/r/rust/comments/abc/title/",
            "https://www.reddit.com/r/Rust/comments/abc/Title/",
            "http://m.reddit.com/r/rust/comments/abc/title",
            "https://reddit.com/r/rust/comments/abc/title/?utm_source=x",
        ] {
            assert_eq!(key(url).as_deref(), Some(want), "url: {url}");
        }
    }

    #[test]
    fn reddit_keys_an_over18_interstitial_as_its_dest() {
        let interstitial = "https://www.reddit.com/over18/?dest=http%3A%2F%2Freddit.com%2Fr%2FA0a%2Fcomments%2F00a0a%2F0";
        for domain in ["old.reddit.com", "www.reddit.com"] {
            let cleaned = crate::reddit::cleanup::normalize_url(&url(interstitial), domain)
                .expect("cleanup reddit moves an interstitial");
            assert_eq!(key(interstitial), key(cleaned.as_str()), "{cleaned}");
        }
        assert_eq!(
            key(interstitial).as_deref(),
            Some("/r/a0a/comments/00a0a/0")
        );
        let nested = "https://www.reddit.com/over18?dest=https%3A%2F%2Fwww.reddit.com%2Fover18%3Fdest%3Dhttps%253A%252F%252Fwww.reddit.com%252Fr%252Fx%252Fcomments%252Fa%252Ft%252F";
        assert_eq!(key(nested).as_deref(), Some("/r/x/comments/a/t"));
        assert_eq!(
            key("https://www.reddit.com/over18?dest=https%3A%2F%2Fexample.com%2Fpost"),
            None,
            "an interstitial in front of another site is that site's link"
        );
    }

    #[test]
    fn reddit_keys_reject_non_reddit_and_empty() {
        assert_eq!(key("https://example.com/r/rust/comments/abc/"), None);
        // "reddit.com" only in the path of another host must not match.
        assert_eq!(key("https://example.com/reddit.com/x"), None);
        assert_eq!(key("https://www.reddit.com/"), None);
    }
}
