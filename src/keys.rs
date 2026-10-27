//! The dedup keys: how each source keys a URL, so `sync` can tell a draft from a bookmark
//! already on Pinboard. A key must survive `cleanup`'s canonicalization, or `sync`
//! re-adds what `cleanup` moved.

use url::Url;

use crate::hackernews::HackerNewsItemId;
use crate::reddit::model::over18_dest;
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

/// The GitHub dedup key: [`url_key`], gated to github.com hosts, with the `www.` alias
/// folded onto the bare host and a repo root's `.git` suffix dropped. Those are the forms
/// `cleanup github` canonicalizes to `github.com/owner/repo`, so a bookmark keys the same
/// before and after it does. Other subdomains (`gist.`) keep their own host.
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

/// Build a dedup key from a full Reddit URL: the path, lowercased, with query/fragment
/// and trailing slash removed. Returns `None` for non-Reddit URLs. Only the path is
/// used, so the same post saved under any reddit host (`old.`/`www.`/`m.`) maps to the
/// same key. `domain` is the account's `reddit_domain`, a Reddit host even outside
/// reddit.com (a mirror), since that is where sync writes the account's bookmarks.
pub fn reddit_key(url: &Url, domain: &str) -> Option<String> {
    if !is_reddit_host(url, domain) {
        return None;
    }
    let key = url.path().trim_end_matches('/').to_ascii_lowercase();
    (!key.is_empty()).then_some(key)
}

/// Whether `url` is on a Reddit host: reddit.com, a subdomain of it, or `domain`, the
/// account's `reddit_domain`.
fn is_reddit_host(url: &Url, domain: &str) -> bool {
    url.host_str()
        .is_some_and(|host| host_matches(host, "reddit.com") || host.eq_ignore_ascii_case(domain))
}

/// The key a draft and a stored bookmark at `url` share. An HN item permalink keys on its
/// id (`hn:<id>`), even as a story's article URL (a meta post), since a host+path key
/// drops the id query. An article on Reddit (a reddit.com host, or one of
/// `reddit_domains`, the configured accounts' `reddit_domain`s) or GitHub keys as that
/// source does, so it still matches once `cleanup reddit` or `cleanup github` has moved it
/// to its canonical URL. Any other article falls back to the host+path key.
///
/// Reddit's `over18` interstitial keys as its `dest`, which `cleanup reddit` unwraps it
/// to, and the bare Reddit root, which `reddit_key` leaves unkeyed, keys as `/`, the path
/// cleanup keeps when it moves it onto the account's domain.
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
}
