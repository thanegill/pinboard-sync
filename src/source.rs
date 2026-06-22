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
