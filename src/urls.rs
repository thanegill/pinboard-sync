//! URL helpers the sources share: whether a host is a domain or one of its subdomains, and
//! the generic host+path dedup key that [`crate::domains`] refines for a few hosts.

use url::Url;

/// Whether `host` is `domain` or a subdomain of it (`*.domain`). `host` is assumed
/// already lowercased (as [`Url::host_str`] returns it for http(s)); `domain` must be too.
pub fn host_matches(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|rest| rest.ends_with('.'))
}

/// Extension methods on [`Url`] shared across the sources.
pub trait UrlExt {
    /// Whether the URL's host is `domain` or a `*.domain` subdomain — the "is this URL
    /// one I manage?" test used by the per-source bookmark filters.
    fn host_is(&self, domain: &str) -> bool;
}

impl UrlExt for Url {
    fn host_is(&self, domain: &str) -> bool {
        self.host_str()
            .is_some_and(|host| host_matches(host, domain))
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::url;

    #[test]
    fn host_matches_domain_and_subdomains_only() {
        assert!(host_matches("github.com", "github.com"));
        assert!(host_matches("api.github.com", "github.com"));
        assert!(host_matches("old.reddit.com", "reddit.com"));
        // Lookalikes must not match.
        assert!(!host_matches("evilgithub.com", "github.com"));
        assert!(!host_matches("github.com.evil.com", "github.com"));
        assert!(!host_matches("notreddit.com", "reddit.com"));
    }

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
}
