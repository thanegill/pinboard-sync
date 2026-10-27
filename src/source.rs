//! The generic source port. Every service (Reddit, …) implements [`Source`]:
//! it yields [`BookmarkDraft`]s to write to Pinboard and maps an existing Pinboard
//! URL back to a dedup key, so the sync loop stays service-agnostic.

use std::collections::HashSet;

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
    /// The service is refusing further requests until a reset. Distinct from
    /// [`SourceError::ReauthRequired`] because no credential change fixes it, so it must
    /// not fire the auth-failure hook — but it is alike in that every *remaining* request
    /// would fail the same way, so a caller should stop rather than work through them.
    #[error("rate limited: {0}")]
    RateLimited(String),
    /// The service's edge refused the request as a matter of policy (Reddit's "blocked by
    /// network security" page), whatever the credential. Like
    /// [`SourceError::RateLimited`] it must not fire the auth-failure hook and it fails
    /// every remaining request to the same endpoint the same way; unlike it, waiting is
    /// not known to help.
    #[error("blocked: {0}")]
    Blocked(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl SourceError {
    /// Flatten into a plain `anyhow::Error`: the re-auth, rate-limit or block message on
    /// its own (without the variant's prefix) or the inner error unwrapped.
    ///
    /// Only once nothing needs the variant any more: `hook::handle_source_err`, after it
    /// has decided on the hook; `doctor`, which reports each credential rather than acting
    /// on it; and HackerNews, which is public and cannot need re-authentication. Everywhere
    /// else the variant is what decides whether the `--on-auth-failure` hook fires, so
    /// flattening early silently loses that hook — don't reach for this to make a type
    /// mismatch go away.
    pub fn into_anyhow(self) -> anyhow::Error {
        match self {
            SourceError::ReauthRequired(m)
            | SourceError::RateLimited(m)
            | SourceError::Blocked(m) => anyhow::anyhow!(m),
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

/// Maps an existing Pinboard URL to a source's dedup key (matched against the `dedup_key`
/// of fresh drafts), or `None` if the source doesn't manage that URL. Each source defines
/// its own shape on top of the generic host+path [`url_key`](crate::keys::url_key): GitHub
/// gates it to github.com hosts, Reddit uses the host-agnostic permalink path, and
/// HackerNews uses `hn:<id>`, else for an article bookmark the owning source's key on a
/// Reddit or GitHub URL and the host+path key otherwise.
pub trait UrlKey {
    fn dedup_key(&self, url: &Url) -> Option<String>;

    /// The dedup keys of the `existing` bookmarks this source manages: a draft whose
    /// `dedup_key` is among them is already synced.
    fn synced_keys(&self, existing: &[Bookmark]) -> HashSet<String> {
        existing
            .iter()
            .filter_map(|bookmark| self.dedup_key(&bookmark.url))
            .collect()
    }
}

/// A service that yields saveable items as [`BookmarkDraft`]s. Abstracted from the
/// concrete client so the `sync` loop can be exercised with an in-memory fake.
pub trait Source: UrlKey {
    /// All saveable items as drafts, newest first.
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError>;

    /// [`Source::fetch`] for `sync`, which already holds the destination's bookmarks:
    /// a source may use `existing` to stop paging once it reaches items already synced.
    /// What it returns still goes through the caller's dedup, so it may include items in
    /// `existing`. By default, the full fetch.
    async fn fetch_incremental(
        &self,
        existing: &[Bookmark],
    ) -> Result<Vec<BookmarkDraft>, SourceError> {
        let _ = existing;
        self.fetch().await
    }
}

/// Whether a page of `page_len` source items, shaped into `page_drafts`, would give
/// `sync` nothing to write: every draft's `dedup_key` is among `synced_keys`. Sources list
/// newest first, so the pages after it hold nothing new either. Items that make no draft
/// add nothing, so a page of only those is caught up too; an empty page proves nothing.
pub fn page_is_caught_up<'a>(
    page_len: usize,
    page_drafts: impl IntoIterator<Item = &'a BookmarkDraft>,
    synced_keys: &HashSet<String>,
) -> bool {
    page_len > 0
        && page_drafts
            .into_iter()
            .all(|draft| synced_keys.contains(&draft.dedup_key))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_is_caught_up_only_when_it_has_no_new_draft() {
        let draft = |key: &str| BookmarkDraft {
            bookmark: crate::test_support::stored_at("https://example.com/"),
            dedup_key: key.into(),
        };
        let synced_keys: HashSet<String> = ["a".into(), "b".into()].into();
        let caught_up =
            |page_len, drafts: &[BookmarkDraft]| page_is_caught_up(page_len, drafts, &synced_keys);

        assert!(caught_up(2, &[draft("a"), draft("b")]), "all synced");
        assert!(!caught_up(2, &[draft("c"), draft("a")]), "a new item");
        assert!(
            !caught_up(2, &[draft("a"), draft("c")]),
            "an unsynced older one"
        );
        assert!(
            caught_up(3, &[draft("a")]),
            "items that make no draft add nothing"
        );
        assert!(caught_up(1, &[]), "a page of only items that make no draft");
        assert!(!caught_up(0, &[]), "an empty page");
        assert!(
            !page_is_caught_up(1, &[draft("a")], &HashSet::new()),
            "nothing synced"
        );
    }
}
