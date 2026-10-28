//! The dedup keys: how each source keys a URL, so `sync` can tell a draft from a bookmark
//! already on Pinboard. Each source's scheme is a [`UrlKey`] type carrying only what it
//! needs ([`GitHubKeys`], [`RedditKeys`], [`HackerNewsKeys`]), built on the generic
//! host+path [`url_key`]. A key must survive `cleanup`'s canonicalization, or `sync`
//! re-adds what `cleanup` moved.

use url::Url;

use crate::github::gists::{gist_root_id, GIST_HOST};
use crate::hackernews::HackerNewsItemId;
use crate::reddit::model::over18_dest;
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
/// the bare host and a repo root's `.git` suffix dropped. Those are the forms
/// `cleanup github` canonicalizes to `github.com/owner/repo`, so a bookmark keys the same
/// before and after it does. A gist root keys by its id alone (`gist.github.com/<id>`), so
/// its owner-less form matches the `gist.github.com/<owner>/<id>` form cleanup moves it
/// to. Other subdomains, and a gist's deep links, keep their own host and path.
#[derive(Debug, Clone, Copy, Default)]
pub struct GitHubKeys;

impl UrlKey for GitHubKeys {
    fn dedup_key(&self, url: &Url) -> Option<DedupKey> {
        if let Some(id) = gist_root_id(url) {
            return Some(format!("{GIST_HOST}/{id}").into());
        }
        let key = url_key(url).filter(|_| url.host_is("github.com"))?;
        let key = match key.strip_prefix("www.") {
            Some(bare) => bare.to_string(),
            None => key,
        };
        let is_repo_root = key
            .strip_prefix("github.com/")
            .is_some_and(|path| path.split('/').count() == 2);
        let key = match key.strip_suffix(".git") {
            Some(root) if is_repo_root => root.to_string(),
            _ => key,
        };
        Some(key.into())
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
    fn dedup_key(&self, url: &Url) -> Option<DedupKey> {
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
        if let Some(dest) = over18_dest(url) {
            return self.dedup_key(&dest);
        }
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
    fn reddit_keys_reject_non_reddit_and_empty() {
        assert_eq!(key("https://example.com/r/rust/comments/abc/"), None);
        // "reddit.com" only in the path of another host must not match.
        assert_eq!(key("https://example.com/reddit.com/x"), None);
        assert_eq!(key("https://www.reddit.com/"), None);
    }
}
