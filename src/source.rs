//! The generic source port. Every service (Reddit, …) implements [`Source`]:
//! it yields [`BookmarkDraft`]s to write to Pinboard and maps an existing Pinboard
//! URL back to a dedup key, so the sync loop stays service-agnostic.

use url::Url;

use crate::bookmark::Bookmark;

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
/// already present there. It's a [`Bookmark`] (the thing written) and the extra
/// `dedup_key` that only matters before the write. Sources build `bookmark.public`/
/// `read_later` `false` and set `bookmark.timestamp` to the source post date (when
/// known); the sync loop stamps the resolved flags and applies the age cap before
/// writing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookmarkDraft {
    /// The bookmark to write.
    pub bookmark: Bookmark,
    /// Key matched against existing Pinboard bookmarks via [`UrlKey::dedup_key`].
    pub dedup_key: String,
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

/// Append each non-empty tag in `add` that isn't already in `tags` (order-preserving).
/// Empty tags are skipped for parity with [`push_tag`] (empty = the tag is disabled):
/// stored tags derive via `split_whitespace` and so can never contain `""`, so emitting
/// one would make a cleanup plan diff against storage on every run and never converge.
pub fn extend_unique(tags: &mut Vec<String>, add: &[String]) {
    for tag in add {
        if !tag.is_empty() && !tags.contains(tag) {
            tags.push(tag.clone());
        }
    }
}

/// Whether two tag lists differ as sets (order- and duplicate-insensitive after sort).
pub fn tags_differ(a: &[String], b: &[String]) -> bool {
    let mut a = a.to_vec();
    let mut b = b.to_vec();
    a.sort();
    a.dedup();
    b.sort();
    b.dedup();
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

/// Deserialize each element of `values` into `T`, skipping (with a warning naming `what`)
/// any element that fails, so one malformed item can't discard a whole page. A non-empty
/// input where *every* element fails is treated as an error (a schema break, e.g. a renamed
/// required field) rather than a silently empty result: `on_all_failed` builds that error in
/// the caller's own error type. An empty input yields an empty vec. Shared by the GitHub
/// starred fetch and the Reddit listing deserializer.
pub fn deserialize_lenient<T, E>(
    values: Vec<serde_json::Value>,
    what: &str,
    on_all_failed: impl FnOnce(usize) -> E,
) -> Result<Vec<T>, E>
where
    T: serde::de::DeserializeOwned,
{
    let count = values.len();
    let parsed: Vec<T> = values
        .into_iter()
        .filter_map(|value| {
            serde_json::from_value::<T>(value)
                .map_err(|e| log::warn!("skipping malformed {what}: {e}"))
                .ok()
        })
        .collect();
    if count > 0 && parsed.is_empty() {
        return Err(on_all_failed(count));
    }
    Ok(parsed)
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

    #[test]
    fn extend_unique_skips_empty_and_duplicate_tags() {
        let tags = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut acc = tags(&["reddit"]);
        // An empty base tag (config `tags = [""]`) must not add a literal "" — stored
        // tags can never contain one, so it would diff against storage forever.
        extend_unique(&mut acc, &tags(&["", "reddit", "rust"]));
        assert_eq!(acc, tags(&["reddit", "rust"]));
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn tags_differ_is_set_based() {
        let tags = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // A duplicate on one side must not read as a difference.
        assert!(!tags_differ(
            &tags(&["reddit"]),
            &tags(&["reddit", "reddit"])
        ));
        // Order-insensitive.
        assert!(!tags_differ(&tags(&["a", "b"]), &tags(&["b", "a"])));
        // Genuinely different sets still differ.
        assert!(tags_differ(&tags(&["reddit"]), &tags(&["reddit", "rust"])));
        assert!(tags_differ(&tags(&["a", "a"]), &tags(&["a", "b"])));
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
