//! URL helpers the sources share: whether a host is a domain or one of its subdomains.

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
