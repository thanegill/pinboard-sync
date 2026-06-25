//! The generic source port. Every service (Reddit, …) implements [`Source`]:
//! it yields [`BookmarkDraft`]s to write to Pinboard and maps an existing Pinboard
//! URL back to a dedup key, so the sync loop stays service-agnostic.

use url::Url;

/// Errors from a source, separating the "operator must re-authenticate" case (an
/// expired/missing credential → a 401/403) from transient/other failures, because
/// only the former should fire the auth-failure hook.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// The service rejected the request (401/403); a credential needs refreshing.
    #[error("re-authentication required: {0}")]
    ReauthRequired(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl SourceError {
    /// Flatten into a plain `anyhow::Error`: the re-auth message on its own (without
    /// the variant's prefix) or the inner error unwrapped. Used by the cleanup paths,
    /// which surface failures as `anyhow` and don't fire the auth-failure hook.
    pub fn into_anyhow(self) -> anyhow::Error {
        match self {
            SourceError::ReauthRequired(m) => anyhow::anyhow!(m),
            SourceError::Other(e) => e,
        }
    }
}

/// A bookmark ready to write to Pinboard, plus the key used to tell whether it is
/// already present there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookmarkDraft {
    /// Pinboard `url`.
    pub url: String,
    /// Pinboard `description` (the bookmark title).
    pub description: String,
    /// Pinboard `extended` (the notes body).
    pub extended: String,
    pub tags: Vec<String>,
    /// Key matched against existing Pinboard bookmarks via [`Source::existing_key`].
    pub dedup_key: String,
    /// Whether to write the bookmark queued-to-read (Pinboard's `toread`). Sources build
    /// this `false`; the sync loop stamps the per-account resolved value before writing.
    pub read_later: bool,
    /// Whether to write the bookmark public (Pinboard's `shared`). Sources build this
    /// `false`; the sync loop stamps the per-account resolved value before writing.
    pub public: bool,
    /// The source post's creation time as a unix epoch (UTC), when known. Used as the
    /// Pinboard `dt` if `use_post_date` is on and the post is within the age cap; the
    /// sync loop clears it otherwise. `None` when the source exposes no date.
    pub post_date: Option<i64>,
}

/// Maps an existing Pinboard URL to a source's dedup key (matched against the
/// `dedup_key` of fresh drafts), or `None` if the source doesn't manage that URL. Each
/// source defines its own shape on top of the generic host+path [`url_key`]: GitHub
/// gates it to github.com hosts, Reddit uses the host-agnostic permalink path, and
/// HackerNews uses `hn:<id>` (else the host+path key for article bookmarks).
pub trait UrlKey {
    fn dedup_key(&self, url: &Url) -> Option<String>;
}

/// A service that yields saveable items as [`BookmarkDraft`]s. Abstracted from the
/// concrete client so the `sync` loop can be exercised with an in-memory fake.
/// (Crate-internal, never spawned across threads, so the missing `Send` bound from
/// `async fn` in a trait is irrelevant here.)
#[allow(async_fn_in_trait)]
pub trait Source: UrlKey {
    /// All saveable items as drafts, newest first.
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError>;
}

/// Append `key` to `tags` unless it is empty (empty = the tag is disabled).
pub fn push_tag(tags: &mut Vec<String>, key: &str) {
    if !key.is_empty() {
        tags.push(key.to_string());
    }
}

/// Append every non-empty key in `keys` to `tags` (via [`push_tag`]). Used to seed a
/// draft's tags from a source's configurable base `tags` list.
pub fn push_tags(tags: &mut Vec<String>, keys: &[String]) {
    for key in keys {
        push_tag(tags, key);
    }
}

/// Append `prefix + value` to `tags`, collapsing internal whitespace in `value` to
/// `-` (Pinboard tags can't contain spaces — the API splits on them). Skipped if
/// `prefix` is empty or `value` is empty/all-whitespace.
pub fn push_prefixed(tags: &mut Vec<String>, prefix: &str, value: &str) {
    let value = value.split_whitespace().collect::<Vec<_>>().join("-");
    if !prefix.is_empty() && !value.is_empty() {
        tags.push(format!("{prefix}{value}"));
    }
}

/// Append each tag in `add` that isn't already in `tags` (order-preserving).
pub fn extend_unique(tags: &mut Vec<String>, add: &[String]) {
    for tag in add {
        if !tags.contains(tag) {
            tags.push(tag.clone());
        }
    }
}

/// Whether two tag lists differ as sets (order- and duplicate-insensitive after sort).
pub fn tags_differ(a: &[String], b: &[String]) -> bool {
    let mut a = a.to_vec();
    let mut b = b.to_vec();
    a.sort();
    b.sort();
    a != b
}

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
/// `https://GitHub.com/Owner/Repo/?tab=x` → `github.com/owner/repo`. Returns `None` for
/// a URL without a host.
pub fn url_key(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    let path = url.path().trim_end_matches('/').to_ascii_lowercase();
    Some(format!("{host}{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_prefixed_slugs_internal_whitespace() {
        let mut tags = Vec::new();
        push_prefixed(&mut tags, "lang:", "Jupyter Notebook");
        push_prefixed(&mut tags, "x:", "  spaced  out ");
        push_prefixed(&mut tags, "y:", "   "); // all whitespace → skipped
        push_prefixed(&mut tags, "", "v"); // empty prefix → skipped
        assert_eq!(tags, vec!["lang:Jupyter-Notebook", "x:spaced-out"]);
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

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
}
