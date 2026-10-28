//! The generic source port. Every service (Reddit, ...) implements [`Source`]:
//! it yields [`BookmarkDraft`]s to write to Pinboard and maps an existing Pinboard
//! URL back to a dedup key, so the sync loop stays service-agnostic.

use std::collections::HashSet;

use url::Url;

use crate::bookmark::Bookmark;

/// Errors from a source: a [`Halt`], which every remaining request would hit too, or any
/// other failure, which may be one item's.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error(transparent)]
    Halted(#[from] Halt),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Why a source can't go on: every remaining request would fail the same way, so a caller
/// stops rather than spend requests collecting identical errors. Only [`Halt::Reauth`]
/// fires the auth-failure hook, since no credential change clears a quota or an edge block.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Halt {
    /// The service rejected the request (401/403); a credential needs refreshing.
    #[error("re-authentication required: {0}")]
    Reauth(String),
    /// The service is refusing further requests until a reset.
    #[error("rate limited: {0}")]
    RateLimited(String),
    /// The service's edge refused the request as a matter of policy (Reddit's "blocked by
    /// network security" page), whatever the credential. Waiting is not known to help.
    #[error("blocked: {0}")]
    Blocked(String),
}

impl Halt {
    /// The operator-facing explanation, without the variant's prefix.
    pub fn message(&self) -> &str {
        match self {
            Halt::Reauth(message) | Halt::RateLimited(message) | Halt::Blocked(message) => message,
        }
    }

    /// The same halt with its message rewritten by `f`.
    #[must_use]
    pub fn map_message(self, f: impl FnOnce(String) -> String) -> Self {
        match self {
            Halt::Reauth(message) => Halt::Reauth(f(message)),
            Halt::RateLimited(message) => Halt::RateLimited(f(message)),
            Halt::Blocked(message) => Halt::Blocked(f(message)),
        }
    }
}

impl SourceError {
    /// Flatten into a plain `anyhow::Error`: a halt's message on its own (without the
    /// variant's prefix) or the inner error unwrapped.
    ///
    /// Only once nothing needs the variant any more, such as after
    /// `hook::handle_source_err` has decided on the hook. The variant is what decides
    /// whether the `--on-auth-failure` hook fires, so flattening early silently loses that
    /// hook -- don't reach for this to make a type mismatch go away.
    pub fn into_anyhow(self) -> anyhow::Error {
        match self {
            SourceError::Halted(halt) => anyhow::anyhow!(halt.message().to_string()),
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
    pub dedup_key: DedupKey,
}

/// What `sync` matches a draft against the bookmarks already on Pinboard by: a source's
/// [`UrlKey`] gives every URL for one item the same key, whatever its host alias, scheme or
/// tracking query. Comparable only with other keys, so a URL or title can't pass for one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DedupKey(String);

impl DedupKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for DedupKey {
    fn from(key: String) -> Self {
        Self(key)
    }
}

impl From<&str> for DedupKey {
    fn from(key: &str) -> Self {
        Self(key.to_string())
    }
}

#[cfg(test)]
impl PartialEq<&str> for DedupKey {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

#[cfg(test)]
impl PartialEq<String> for DedupKey {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}

/// Maps an existing Pinboard URL to a source's dedup key (matched against the `dedup_key`
/// of fresh drafts), or `None` if the source doesn't manage that URL. Each source's scheme
/// is a type in [`crate::keys`] that its client delegates to.
pub trait UrlKey {
    fn dedup_key(&self, url: &Url) -> Option<DedupKey>;

    /// The dedup keys of the `existing` bookmarks this source manages: a draft whose
    /// `dedup_key` is among them is already synced.
    fn synced_keys(&self, existing: &[Bookmark]) -> HashSet<DedupKey> {
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
    synced_keys: &HashSet<DedupKey>,
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
    fn into_anyhow_keeps_a_halts_message_without_its_prefix() {
        let flattened = |error: SourceError| error.into_anyhow().to_string();
        assert_eq!(flattened(Halt::Reauth("expired".into()).into()), "expired");
        assert_eq!(
            flattened(Halt::RateLimited("resets soon".into()).into()),
            "resets soon"
        );
        assert_eq!(flattened(Halt::Blocked("edge".into()).into()), "edge");
        assert_eq!(flattened(anyhow::anyhow!("other").into()), "other");
    }

    #[test]
    fn a_page_is_caught_up_only_when_it_has_no_new_draft() {
        let draft = |key: &str| BookmarkDraft {
            bookmark: crate::test_support::stored_at("https://example.com/"),
            dedup_key: key.into(),
        };
        let synced_keys: HashSet<DedupKey> = ["a".into(), "b".into()].into();
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
