//! URL helpers the sources share: whether a host is a domain or one of its subdomains.

use url::Url;

/// Whether `host` is `domain` or a subdomain of it. Both must be lowercase, as
/// [`Url::host_str`] returns an http(s) host.
pub fn host_matches(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|rest| rest.ends_with('.'))
}

pub trait UrlExt {
    /// Whether the host is `domain` or a subdomain of it.
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
