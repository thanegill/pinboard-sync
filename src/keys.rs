//! The dedup keys: how each source keys a URL, so `sync` can tell a draft from a bookmark
//! already on Pinboard. Each source's scheme is a [`UrlKey`] type carrying only what it
//! needs ([`GitHubKeys`], [`RedditKeys`], [`HackerNewsKeys`]), built on the generic
//! host+path [`url_key`]. A key must survive `cleanup`'s canonicalization, or `sync`
//! re-adds what `cleanup` moved.

use url::Url;

use crate::hackernews::HackerNewsItemId;
use crate::reddit::model::over18_dest;
use crate::source::UrlKey;
use crate::urls::{host_matches, UrlExt};

/// A host+path dedup key for a URL: scheme and query/fragment dropped, host lowercased
/// (by the `url` crate), path lowercased with any trailing slash removed — e.g.
/// `https://GitHub.com/Owner/Repo/?tab=x` → `github.com/owner/repo`. Dropping the query
/// is deliberate canonicalization; the exception is a non-source domain whose identity
/// lives in the query string (e.g. YouTube `watch?v=…`), for which
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
/// the bare host and a repo root's `.git` suffix dropped. Those are the forms
/// `cleanup github` canonicalizes to `github.com/owner/repo`, so a bookmark keys the same
/// before and after it does. Other subdomains (`gist.`) keep their own host.
#[derive(Debug, Clone, Copy, Default)]
pub struct GitHubKeys;

impl UrlKey for GitHubKeys {
    fn dedup_key(&self, url: &Url) -> Option<String> {
        let key = url_key(url).filter(|_| url.host_is("github.com"))?;
        let key = match key.strip_prefix("www.") {
            Some(bare) => bare.to_string(),
            None => key,
        };
        let is_repo_root = key
            .strip_prefix("github.com/")
            .is_some_and(|path| path.split('/').count() == 2);
        Some(match key.strip_suffix(".git") {
            Some(root) if is_repo_root => root.to_string(),
            _ => key,
        })
    }
}

/// Reddit's key: the path, lowercased, with query/fragment and trailing slash removed, or
/// `None` off Reddit and for the bare root. Only the path is used, so the same post saved
/// under any Reddit host (`old.`/`www.`/`m.`) maps to the same key. A Reddit host is
/// reddit.com, a subdomain of it, or one of `domains`, the `reddit_domain`s of the
/// accounts in play: a mirror outside reddit.com counts too, since that is where sync
/// writes its account's bookmarks.
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
    fn dedup_key(&self, url: &Url) -> Option<String> {
        if !self.is_reddit_host(url) {
            return None;
        }
        let key = url.path().trim_end_matches('/').to_ascii_lowercase();
        (!key.is_empty()).then_some(key)
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
    fn dedup_key(&self, url: &Url) -> Option<String> {
        if let Some(dest) = over18_dest(url) {
            return self.dedup_key(&dest);
        }
        if let Ok(id) = HackerNewsItemId::try_from(url) {
            return Some(id.dedup_key());
        }
        if self.reddit.is_reddit_host(url) {
            let key = self.reddit.dedup_key(url);
            return Some(key.unwrap_or_else(|| "/".to_string()));
        }
        self.github.dedup_key(url).or_else(|| url_key(url))
    }
}

/// [`GitHubKeys`].
pub fn github_key(url: &Url) -> Option<String> {
    GitHubKeys.dedup_key(url)
}

/// [`RedditKeys`] on the one `domain`.
pub fn reddit_key(url: &Url, domain: &str) -> Option<String> {
    RedditKeys::new([domain]).dedup_key(url)
}

/// [`HackerNewsKeys`] with `reddit_domains` as Reddit hosts.
pub fn hackernews_key(url: &Url, reddit_domains: &[String]) -> Option<String> {
    HackerNewsKeys::new(RedditKeys::new(reddit_domains)).dedup_key(url)
}

#[cfg(test)]
mod tests {
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
    fn github_key_folds_the_forms_cleanup_canonicalizes() {
        let key = |s: &str| github_key(&url(s));
        for alias in [
            "https://github.com/Owner/Repo",
            "https://www.github.com/Owner/Repo",
            "https://github.com/Owner/Repo.git",
            "https://www.github.com/owner/repo.git/",
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
        assert_eq!(
            key("https://gist.github.com/user/abcd.git").as_deref(),
            Some("gist.github.com/user/abcd.git")
        );
        assert_eq!(key("https://example.com/o/r"), None);
    }

    fn key(url: &str) -> Option<String> {
        reddit_key(&Url::parse(url).unwrap(), "old.reddit.com")
    }

    #[test]
    fn reddit_key_treats_the_configured_domain_as_reddit() {
        let mirror = |url: &str| reddit_key(&Url::parse(url).unwrap(), "redlib.example.org");
        let want = Some("/r/rust/comments/abc/title".to_string());
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
            reddit_key(
                &Url::parse("https://Redlib.Example.org/r/rust/comments/abc/title/").unwrap(),
                "Redlib.Example.org"
            ),
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
    fn reddit_key_matches_across_subdomains_and_query_and_trailing_slash() {
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
    fn reddit_key_rejects_non_reddit_and_empty() {
        assert_eq!(key("https://example.com/r/rust/comments/abc/"), None);
        // "reddit.com" only in the path of another host must not match.
        assert_eq!(key("https://example.com/reddit.com/x"), None);
        assert_eq!(key("https://www.reddit.com/"), None);
    }

    /// The key functions as they stood before the per-source schemes, verbatim: the
    /// oracle the schemes must reproduce exactly.
    mod before {
        use super::*;

        pub fn github_key(url: &Url) -> Option<String> {
            let key = url_key(url).filter(|_| url.host_is("github.com"))?;
            let key = match key.strip_prefix("www.") {
                Some(bare) => bare.to_string(),
                None => key,
            };
            let is_repo_root = key
                .strip_prefix("github.com/")
                .is_some_and(|path| path.split('/').count() == 2);
            Some(match key.strip_suffix(".git") {
                Some(root) if is_repo_root => root.to_string(),
                _ => key,
            })
        }

        pub fn reddit_key(url: &Url, domain: &str) -> Option<String> {
            if !is_reddit_host(url, domain) {
                return None;
            }
            let key = url.path().trim_end_matches('/').to_ascii_lowercase();
            (!key.is_empty()).then_some(key)
        }

        fn is_reddit_host(url: &Url, domain: &str) -> bool {
            url.host_str().is_some_and(|host| {
                host_matches(host, "reddit.com") || host.eq_ignore_ascii_case(domain)
            })
        }

        pub fn hackernews_key(url: &Url, reddit_domains: &[String]) -> Option<String> {
            if let Some(dest) = over18_dest(url) {
                return hackernews_key(&dest, reddit_domains);
            }
            HackerNewsItemId::try_from(url)
                .ok()
                .map(|id| id.dedup_key())
                .or_else(|| {
                    let domain = reddit_domains
                        .iter()
                        .map(String::as_str)
                        .chain(["reddit.com"])
                        .find(|domain| is_reddit_host(url, domain))?;
                    Some(reddit_key(url, domain).unwrap_or_else(|| "/".to_string()))
                })
                .or_else(|| github_key(url))
                .or_else(|| url_key(url))
        }
    }

    const URLS: &[&str] = &[
        "https://github.com/Owner/Repo",
        "https://github.com/Owner/Repo/",
        "https://www.github.com/Owner/Repo",
        "https://github.com/Owner/Repo.git",
        "https://www.github.com/owner/repo.git/",
        "https://GitHub.com/o/r?tab=stars#readme",
        "https://github.com/o/r.git/tree/main",
        "https://github.com/o",
        "https://github.com/",
        "https://github.com",
        "https://gist.github.com/user/abcd.git",
        "https://api.github.com/repos/o/r",
        "https://evilgithub.com/o/r",
        "https://github.com.evil.com/o/r",
        "https://old.reddit.com/r/rust/comments/abc/title/",
        "https://www.reddit.com/r/Rust/comments/abc/Title/",
        "http://m.reddit.com/r/rust/comments/abc/title",
        "https://reddit.com/r/rust/comments/abc/title/?utm_source=x",
        "https://REDDIT.com/r/rust/",
        "https://www.reddit.com/",
        "https://www.reddit.com",
        "https://reddit.com/r/rust/comments/abc/title/def/",
        "https://notreddit.com/r/rust",
        "https://example.com/reddit.com/x",
        "https://redlib.example.org/r/rust/comments/abc/title/",
        "https://Redlib.Example.org/r/rust/comments/abc/title/",
        "https://redlib.example.org/",
        "https://sub.redlib.example.org/r/rust/",
        "https://example.org/r/rust/comments/abc/title/",
        "http://127.0.0.1/r/rust/comments/abc/x/",
        "http://127.0.0.1/",
        "http://[::1]/r/rust/comments/abc/x/",
        "https://xn--bcher-kva.example/r/rust/comments/abc/x/",
        "https://bücher.example/r/rust/comments/abc/x/",
        "https://old.reddit.com/over18/?dest=https%3A%2F%2Fold.reddit.com%2Fr%2Fnsfw%2Fcomments%2Fx%2Fy%2F",
        "https://www.reddit.com/over18?dest=https%3A%2F%2Fexample.com%2Fa%3Fb%3Dc",
        "https://www.reddit.com/over18?dest=https%3A%2F%2Fgithub.com%2FO%2FR.git",
        "https://www.reddit.com/over18?dest=https%3A%2F%2Fnews.ycombinator.com%2Fitem%3Fid%3D7",
        "https://www.reddit.com/over18?dest=https%3A%2F%2Fwww.reddit.com%2Fover18%3Fdest%3Dhttps%253A%252F%252Fredlib.example.org%252Fr%252Fx%252F",
        "https://www.reddit.com/over18?dest=https%3A%2F%2Fwww.reddit.com%2F",
        "https://www.reddit.com/over18?dest=not%20a%20url",
        "https://www.reddit.com/over18/",
        "https://redlib.example.org/over18?dest=https%3A%2F%2Fexample.com%2Fz",
        "https://news.ycombinator.com/item?id=42",
        "http://news.ycombinator.com/item?id=42&p=2",
        "https://news.ycombinator.com/item?id=42abc",
        "https://news.ycombinator.com/item?id=",
        "https://news.ycombinator.com/item",
        "https://news.ycombinator.com/user?id=pg",
        "https://NEWS.ycombinator.com/item?id=9",
        "https://www.youtube.com/watch?v=AAAAAAAAAAA",
        "https://youtu.be/AbCdEfGhIjK",
        "https://Example.com/Some/Path/?q=1#frag",
        "https://example.com",
        "file:///etc/passwd",
        "mailto:someone@example.com",
        "data:text/plain,hello",
    ];

    const DOMAIN_LISTS: &[&[&str]] = &[
        &[],
        &["old.reddit.com"],
        &["reddit.com"],
        &["redlib.example.org"],
        &["Redlib.Example.org"],
        &["127.0.0.1"],
        &["[::1]"],
        &["xn--bcher-kva.example"],
        &[
            "old.reddit.com",
            "redlib.example.org",
            "127.0.0.1",
            "[::1]",
            "xn--bcher-kva.example",
        ],
    ];

    #[test]
    fn schemes_reproduce_the_functions_they_replace() {
        for raw in URLS {
            let url = url(raw);
            assert_eq!(
                GitHubKeys.dedup_key(&url),
                before::github_key(&url),
                "github {raw}"
            );
            for domains in DOMAIN_LISTS {
                let owned: Vec<String> = domains.iter().map(|d| d.to_string()).collect();
                for domain in *domains {
                    assert_eq!(
                        RedditKeys::new([*domain]).dedup_key(&url),
                        before::reddit_key(&url, domain),
                        "reddit {raw} on {domain}"
                    );
                }
                if !domains.is_empty() {
                    assert_eq!(
                        RedditKeys::new(domains.iter().copied()).dedup_key(&url),
                        domains
                            .iter()
                            .find_map(|domain| before::reddit_key(&url, domain)),
                        "reddit client {raw} on {domains:?}"
                    );
                }
                assert_eq!(
                    HackerNewsKeys::new(RedditKeys::new(domains.iter().copied())).dedup_key(&url),
                    before::hackernews_key(&url, &owned),
                    "hackernews {raw} on {domains:?}"
                );
            }
        }
    }
}
