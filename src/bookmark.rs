//! The service-agnostic bookmark domain type. [`Bookmark`] is what the `cleanup`
//! driver reads (from Pinboard) and plans an end-state in, independent of any service's
//! wire format: tags split out, the creation time as a real [`OffsetDateTime`], and the
//! flags as plain `bool`s. The Pinboard wire shape it's converted from (`TryFrom`) lives
//! next to the client in [`crate::pinboard::PinboardBookmark`]; the formatting back to
//! Pinboard fields happens at the write boundary in `pinboard::post_add`.

use std::collections::HashMap;

use anyhow::{Context, Result};
use log::warn;
use time::OffsetDateTime;
use url::Url;

use crate::tags::tags_differ;

/// A bookmark in service-agnostic domain form. The field names are the domain's
/// (`title`/`note`/`public`/`read_later`), not Pinboard's wire names
/// (`description`/`extended`/`shared`/`toread` — those stay on
/// [`PinboardBookmark`](crate::pinboard::PinboardBookmark)). The
/// URL is a parsed [`Url`], so consumers don't re-parse it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub url: Url,
    pub title: String,
    pub note: String,
    pub tags: Vec<String>,
    /// Creation time, or `None` when none was set (an empty wire `time`) or a non-empty
    /// `time` that won't parse (logged on read). It stays `None` rather than dropping the
    /// whole bookmark so the record remains visible to sync's dedup set (see the `TryFrom`
    /// impl below).
    pub timestamp: Option<OffsetDateTime>,
    /// Whether the bookmark is public (Pinboard's `shared=yes`).
    pub public: bool,
    /// Whether the bookmark is queued to read (Pinboard's `toread=yes`).
    pub read_later: bool,
}

impl Bookmark {
    /// The written fields where `new` differs from `self` (the stored bookmark), each as a
    /// `(label, rendered new value)` pair for the cleanup dry-run. Empty when nothing a
    /// write would change differs — so `cleanup` skips the bookmark. `timestamp` compares
    /// by instant (a re-formatted but equivalent time isn't a change). The `public` and
    /// `read_later` flags are not compared, because on the paths that call this they cannot
    /// differ, though for different reasons. A single plan carries the stored values,
    /// forced there before diffing. In a merge, `public` is enforced — a member that
    /// disagrees with the record staying at the target is refused and never joins — while
    /// `read_later` is *restored* from that record afterwards, because members are free to
    /// disagree about it and the merge's any()-rule would otherwise flip it on. See
    /// `cleanup_pass::hold_back_visibility_mismatches` and `cleanup_pass::write_merge`.
    pub fn diff(&self, new: &Bookmark) -> Vec<(&'static str, String)> {
        let mut changes = Vec::new();
        if new.url != self.url {
            changes.push(("url", new.url.to_string()));
        }
        if new.title != self.title {
            changes.push(("title", new.title.clone()));
        }
        if new.note != self.note {
            let value = if new.note.is_empty() {
                "(removed)".to_string()
            } else {
                new.note.clone()
            };
            changes.push(("notes", value));
        }
        if tags_differ(&self.tags, &new.tags) {
            changes.push(("tags", format!("[{}]", new.tags.join(" "))));
        }
        if new.timestamp != self.timestamp {
            let value = new
                .timestamp
                .and_then(crate::timefmt::to_pinboard_dt)
                .unwrap_or_default();
            changes.push(("date", value));
        }
        changes
    }
}

/// The bookmark-store writes the sync/cleanup loops depend on. Abstracted from the concrete
/// Pinboard client so those loops can be exercised with an in-memory fake. A write takes a
/// whole [`Bookmark`]; the client maps it to the Pinboard `posts/add` parameters at the
/// boundary (see `pinboard::post_add`). Reading the account is not part of the port: a run
/// reads `posts/all` once, through `PinboardClient::all`, before any loop starts.
pub trait BookmarkStore {
    /// Add a new bookmark. A `None` `timestamp` lets Pinboard default the date to now.
    async fn add(&self, b: &Bookmark) -> Result<()>;
    /// Re-add an existing bookmark with normalized fields.
    async fn update(&self, b: &Bookmark) -> Result<()>;
    /// Delete a bookmark by URL.
    async fn delete(&self, url: &Url) -> Result<()>;

    /// The cleanup re-write step: `update` the bookmark, then `delete` the old URL when it
    /// changed (`old_url`). Any inter-write pacing is the store's own concern (the Pinboard
    /// client spaces its `posts/add` calls internally).
    async fn apply_update(&self, update: &Bookmark, old_url: Option<&Url>) -> WriteOutcome {
        if let Err(e) = self
            .update(update)
            .await
            .with_context(|| format!("updating bookmark {}", update.url))
        {
            return WriteOutcome::failed(e);
        }
        let mut outcome = WriteOutcome::wrote();
        if let Some(old) = old_url {
            outcome.record_delete(
                old,
                self.delete(old)
                    .await
                    .with_context(|| format!("deleting old URL {old}")),
            );
        }
        outcome
    }

    /// Write a merged bookmark that absorbs one or more colliding bookmarks: `update` the
    /// merged record at its URL, then `delete` every absorbed `old_urls` entry that isn't
    /// the merge target. Deleting the absorbed URLs is what makes a later cleanup run see a
    /// single bookmark at the target and converge.
    ///
    /// A failed delete does not abandon the rest: the absorbed URLs are independent, and
    /// stopping early strands duplicate records the store could have cleared. The caller
    /// gets back what actually happened rather than a bare `Result`, because a merge whose
    /// write landed but whose delete failed has still changed the account.
    async fn apply_merge(&self, update: &Bookmark, old_urls: &[&Url]) -> WriteOutcome {
        if let Err(e) = self
            .update(update)
            .await
            .with_context(|| format!("updating merged bookmark {}", update.url))
        {
            return WriteOutcome::failed(e);
        }
        let mut outcome = WriteOutcome::wrote();
        for old in old_urls {
            if **old == update.url {
                continue;
            }
            outcome.record_delete(
                old,
                self.delete(old)
                    .await
                    .with_context(|| format!("deleting absorbed URL {old}")),
            );
        }
        outcome
    }
}

/// What a write actually accomplished. Not a `Result<()>`, because a merge whose
/// `posts/add` landed but whose delete failed has changed the account, and a caller that
/// tracks the account's state has to be able to tell that apart from "nothing happened".
#[must_use]
#[derive(Debug, Default)]
pub struct WriteOutcome {
    /// The record was written to its URL.
    pub wrote: bool,
    /// URLs the store confirmed it removed — a subset of those asked for.
    pub deleted: Vec<Url>,
    /// The first failure, if any. Later deletes are still attempted.
    pub error: Option<anyhow::Error>,
}

impl WriteOutcome {
    fn wrote() -> Self {
        Self {
            wrote: true,
            ..Self::default()
        }
    }

    fn failed(error: anyhow::Error) -> Self {
        Self {
            error: Some(error),
            ..Self::default()
        }
    }

    fn record_delete(&mut self, url: &Url, result: Result<()>) {
        match result {
            Ok(()) => self.deleted.push(url.clone()),
            Err(e) => {
                // Only the first error is carried out, since the caller reports one — but
                // the later ones are separate failures with their own causes, so log them
                // here rather than letting them vanish.
                if self.error.is_none() {
                    self.error = Some(e);
                } else {
                    warn!("deleting absorbed URL {url}: {e:#}");
                }
            }
        }
    }
}

/// The account as a run currently believes it to be: seeded once from `posts/all` and
/// updated as passes write and delete, so a later pass never plans against — or merges
/// with — a record an earlier one already changed.
///
/// `cleanup --all` fetches `posts/all` once and runs github, then reddit, then
/// hackernews, each of which writes; a plain snapshot leaves the later two reasoning
/// about a state that no longer exists. Order is preserved because a merge takes the
/// first member's note verbatim, so which bookmark comes first has to be stable rather
/// than a `HashMap`'s iteration order.
pub struct AccountView {
    /// Insertion order, so a URL that is re-written keeps its original position.
    order: Vec<Url>,
    by_url: HashMap<Url, Bookmark>,
}

impl AccountView {
    pub fn new(bookmarks: Vec<Bookmark>) -> Self {
        let mut view = Self {
            order: Vec::with_capacity(bookmarks.len()),
            by_url: HashMap::with_capacity(bookmarks.len()),
        };
        for bookmark in bookmarks {
            // Two wire rows can normalize onto one parsed `Url` (`posts/all` isn't
            // deduped). Keeping the first and saying so beats silently losing one, which
            // would hide it from every pass's slice.
            if let Some(kept) = view.by_url.get(&bookmark.url) {
                warn!(
                    "pinboard returned {} more than once; keeping the first ({})",
                    bookmark.url, kept.title
                );
                continue;
            }
            view.order.push(bookmark.url.clone());
            view.by_url.insert(bookmark.url.clone(), bookmark);
        }
        view
    }

    /// The bookmark stored at `url`, if any.
    pub fn get(&self, url: &Url) -> Option<&Bookmark> {
        self.by_url.get(url)
    }

    /// Every bookmark, in `posts/all` order.
    pub fn iter(&self) -> impl Iterator<Item = &Bookmark> {
        self.order.iter().filter_map(|url| self.by_url.get(url))
    }

    /// Record that `bookmark` now occupies its URL. A URL already present keeps its
    /// position, so a re-written bookmark doesn't jump to the end of the order.
    pub fn insert(&mut self, bookmark: &Bookmark) {
        if !self.by_url.contains_key(&bookmark.url) {
            self.order.push(bookmark.url.clone());
        }
        self.by_url.insert(bookmark.url.clone(), bookmark.clone());
    }

    /// Record that nothing occupies `url` any more. Drops it from the order too: leaving
    /// a tombstone there would let a later `insert` of the same URL append a second entry
    /// and yield the bookmark twice.
    pub fn remove(&mut self, url: &Url) {
        self.by_url.remove(url);
        self.order.retain(|ordered| ordered != url);
    }
}

/// The cleanup driver's read side of the account. Separate from [`BookmarkStore`] because
/// only the driver needs it, and it must answer from the run's live view rather than a
/// fetch. `dry_run` lives here rather than being passed alongside so the driver's preview
/// cannot disagree with what the store actually does.
pub trait AccountState {
    /// The bookmark stored at `url` right now, if any.
    fn resident(&self, url: &Url) -> Option<Bookmark>;
    /// Every bookmark, in `posts/all` order.
    fn snapshot(&self) -> Vec<Bookmark>;
    /// Whether writes are simulated rather than sent.
    fn dry_run(&self) -> bool;
}

/// A [`BookmarkStore`] that keeps an [`AccountView`] in step with what it writes.
///
/// Every write goes through here, so there is no separate bookkeeping call for a caller
/// to forget — and because [`BookmarkStore::apply_update`] and
/// [`BookmarkStore::apply_merge`] are default methods built from `update` and `delete`,
/// intercepting just those two covers both.
///
/// Under `dry_run` the view is still updated but nothing reaches the network: a preview
/// has to show the same set of changes a real run would make, including the ones a later
/// pass only makes because an earlier one already wrote.
pub struct CleanupStore<'a, P> {
    inner: &'a P,
    view: std::cell::RefCell<AccountView>,
    dry_run: bool,
}

impl<'a, P: BookmarkStore> CleanupStore<'a, P> {
    pub fn new(inner: &'a P, view: AccountView, dry_run: bool) -> Self {
        Self {
            inner,
            view: std::cell::RefCell::new(view),
            dry_run,
        }
    }
}

impl<P> AccountState for CleanupStore<'_, P> {
    fn resident(&self, url: &Url) -> Option<Bookmark> {
        self.view.borrow().get(url).cloned()
    }

    fn snapshot(&self) -> Vec<Bookmark> {
        self.view.borrow().iter().cloned().collect()
    }

    fn dry_run(&self) -> bool {
        self.dry_run
    }
}

impl<P: BookmarkStore> BookmarkStore for CleanupStore<'_, P> {
    async fn add(&self, b: &Bookmark) -> Result<()> {
        if !self.dry_run {
            self.inner.add(b).await?;
        }
        self.view.borrow_mut().insert(b);
        Ok(())
    }

    async fn update(&self, b: &Bookmark) -> Result<()> {
        if !self.dry_run {
            self.inner.update(b).await?;
        }
        self.view.borrow_mut().insert(b);
        Ok(())
    }

    async fn delete(&self, url: &Url) -> Result<()> {
        if !self.dry_run {
            self.inner.delete(url).await?;
        }
        self.view.borrow_mut().remove(url);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
