//! The generic source port. Every service (Reddit, …) implements [`Source`]:
//! it yields [`BookmarkDraft`]s to write to Pinboard and maps an existing Pinboard
//! URL back to a dedup key, so the sync loop stays service-agnostic.

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
}

/// A service that yields saveable items as [`BookmarkDraft`]s. Abstracted from the
/// concrete client so the `sync` loop can be exercised with an in-memory fake.
/// (Crate-internal, never spawned across threads, so the missing `Send` bound from
/// `async fn` in a trait is irrelevant here.)
#[allow(async_fn_in_trait)]
pub trait Source {
    /// All saveable items as drafts, newest first.
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError>;
    /// Map an existing Pinboard bookmark URL to this source's dedup key, or `None`
    /// if this source doesn't manage that URL.
    fn existing_key(&self, url: &str) -> Option<String>;
}

/// Append `key` to `tags` unless it is empty (empty = the tag is disabled).
pub fn push_tag(tags: &mut Vec<String>, key: &str) {
    if !key.is_empty() {
        tags.push(key.to_string());
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

/// A host+path dedup key for a URL: scheme dropped, host lowercased (userinfo and
/// port stripped), path lowercased with any query/fragment and trailing slash
/// removed — e.g. `https://GitHub.com/Owner/Repo/?tab=x` → `github.com/owner/repo`.
/// Returns `None` for inputs without a host.
pub fn url_key(url: &str) -> Option<String> {
    let after = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let (host, path) = match after.find('/') {
        Some(i) => (&after[..i], &after[i..]),
        None => (after, "/"),
    };
    let host = host.rsplit('@').next().unwrap_or(host); // strip userinfo
    let host = host.split(':').next().unwrap_or(host); // strip port
    let host = host.to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let path = path.trim_end_matches('/').to_ascii_lowercase();
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
    fn url_key_normalizes_host_and_path() {
        assert_eq!(
            url_key("https://GitHub.com/Owner/Repo/?tab=stars").as_deref(),
            Some("github.com/owner/repo")
        );
        assert_eq!(
            url_key("http://news.ycombinator.com/item?id=42").as_deref(),
            Some("news.ycombinator.com/item")
        );
        assert_eq!(url_key("not a url").as_deref(), Some("not a url"));
    }
}
