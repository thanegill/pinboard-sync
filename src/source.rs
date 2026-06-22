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

/// Append `prefix + value` to `tags` unless either side is empty.
pub fn push_prefixed(tags: &mut Vec<String>, prefix: &str, value: &str) {
    if !prefix.is_empty() && !value.is_empty() {
        tags.push(format!("{prefix}{value}"));
    }
}
