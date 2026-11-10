//! The service-agnostic [`Bookmark`], the [`BookmarkStore`] port and the run's live view of
//! the account. Pinboard's wire shape stays with its client
//! ([`crate::pinboard::PinboardBookmark`]), converted on read and in `pinboard::post_add`.

use std::collections::HashMap;

use anyhow::{Context, Result};
use log::warn;
use time::OffsetDateTime;
use url::Url;

use crate::tags::Tags;

/// A bookmark, with domain field names (`title`, `note`, `public`, `read_later`) for
/// Pinboard's `description`, `extended`, `shared` and `toread`, and a parsed [`Url`], so
/// nothing re-parses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub url: Url,
    pub title: String,
    pub note: String,
    pub tags: Tags,
    /// Creation time, or `None` when unset or unparseable: the bookmark is kept, so sync's
    /// dedup still sees it.
    pub timestamp: Option<OffsetDateTime>,
    /// Pinboard's `shared=yes`.
    pub public: bool,
    /// Pinboard's `toread=yes`.
    pub read_later: bool,
}

impl Bookmark {
    /// The written fields where `new` differs from the stored `self`, for the dry run as
    /// `(label, new value)`; empty means `cleanup` skips it. `timestamp` compares by
    /// instant. `public` and `read_later` aren't compared: the driver makes them match
    /// first, in `cleanup_pass::HoldBacks::hold_back_visibility_mismatches` and
    /// `cleanup_pass::merge_members`, before any diff is taken.
    pub fn diff(&self, new: &Bookmark) -> Vec<(&'static str, String)> {
        let mut changes = Vec::new();
        if new.url != self.url {
            changes.push(("url", new.url.to_string()));
        }
        if new.title != self.title {
            changes.push(("title", new.title.clone()));
        }
        if new.note != self.note {
            changes.push(("notes", crate::preview::note(&new.note)));
        }
        if self.tags.differs_from(&new.tags) {
            changes.push(("tags", crate::preview::tags(&new.tags)));
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

/// The writes the sync and cleanup loops make. Reading is not part of it: a run reads
/// `posts/all` once, through `PinboardClient::all`, before any loop starts.
pub trait BookmarkStore {
    /// Add a new bookmark; a `None` `timestamp` lets Pinboard date it now.
    async fn add(&self, b: &Bookmark) -> Result<()>;
    /// Re-add an existing bookmark with normalized fields, as [`Self::fit`] keeps it, or
    /// write nothing when the store refuses it as too long.
    async fn update(&self, b: &Bookmark) -> Result<Update>;
    async fn delete(&self, url: &Url) -> Result<()>;

    /// `b` as a write would store it, its note cut to fit the request URL. The driver diffs
    /// in this form, so a note stored cut reads as unchanged, not rewritten every run.
    fn fit(&self, b: Bookmark) -> Bookmark;

    /// The part [`Self::fit`] kept of `note`, when `note` ends in the store's cut marker;
    /// `None` from a store that never cuts. A whole note may end in the same text, so a
    /// caller treating `Some` as proof of a cut checks the kept part against what it could
    /// have been cut from. A note `fit` drops whole, with no room for the marker, is empty
    /// and carries none.
    fn kept_of_cut<'n>(&self, note: &'n str) -> Option<&'n str>;

    /// Whether a write of `b` can be sent: [`TooLongToSend`] when too long even with no
    /// note, which [`Self::update`] refuses as [`Update::TooLong`] and [`Self::add`] fails.
    /// The driver asks before any write, to refuse such a plan up front.
    fn can_write(&self, b: &Bookmark) -> Result<(), TooLongToSend>;

    /// `update` the bookmark, then `delete` `old_url`, unless the write was refused
    /// ([`WriteOutcome::too_long`]). Pacing is the store's concern.
    async fn apply_update(&self, update: &Bookmark, old_url: Option<&Url>) -> WriteOutcome {
        match self
            .update(update)
            .await
            .with_context(|| format!("updating bookmark {}", update.url))
        {
            Err(e) => return WriteOutcome::failed(e),
            Ok(Update::TooLong) => return WriteOutcome::too_long(),
            Ok(Update::Written) => {}
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

    /// `update` the merged record, then delete each absorbed `old_urls` entry but the
    /// target, so a later run sees one bookmark and converges. A failed delete doesn't stop
    /// the independent rest. A refused write deletes nothing ([`WriteOutcome::too_long`]).
    async fn apply_merge(&self, update: &Bookmark, old_urls: &[&Url]) -> WriteOutcome {
        match self
            .update(update)
            .await
            .with_context(|| format!("updating merged bookmark {}", update.url))
        {
            Err(e) => WriteOutcome::failed(e),
            Ok(Update::TooLong) => WriteOutcome::too_long(),
            Ok(Update::Written) => self.apply_absorb(&update.url, old_urls).await,
        }
    }

    /// The deleting half of [`Self::apply_merge`], for a merge that stores nothing new: the
    /// record at `target` already is the merged one, so it counts as written.
    async fn apply_absorb(&self, target: &Url, old_urls: &[&Url]) -> WriteOutcome {
        let mut outcome = WriteOutcome::wrote();
        for old in old_urls {
            if *old == target {
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

/// A write too long to send even with no note: its request URL's length without the note
/// and the limit, in bytes. `Display` gives only the lengths, never the URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooLongToSend {
    pub without_note: usize,
    pub limit: usize,
}

impl std::fmt::Display for TooLongToSend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} bytes without the note, over the {}-byte limit",
            self.without_note, self.limit
        )
    }
}

/// Warn that a write to `url` stores `to` in place of the note `from`: trimmed, or dropped
/// when it keeps none of it. Silent when they match.
pub fn warn_note_cut<S: BookmarkStore>(store: &S, url: &Url, from: &str, to: &str) {
    if to == from {
        return;
    }
    if store.kept_of_cut(to).unwrap_or(to).is_empty() {
        warn!(
            "note dropped to fit the request URL for {url}: none of its {} bytes fit",
            from.len()
        );
        return;
    }
    warn!(
        "note trimmed to fit the request URL for {url}: {} bytes -> {}",
        from.len(),
        to.len()
    );
}

/// What a store did with a rewrite of a stored bookmark.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    /// Stored, with the note as [`BookmarkStore::fit`] keeps it.
    Written,
    /// Refused as too long, by a 414 or by the client up front: nothing was written. A
    /// shorter note instead would cut text the bookmark already holds.
    TooLong,
}

/// What a write accomplished. Not a `Result<()>`: a merge whose write landed and whose
/// delete failed changed the account, which a caller must tell from "nothing happened".
#[must_use]
#[derive(Debug, Default)]
pub struct WriteOutcome {
    /// The record is at its URL: written there, or already there for an absorb.
    pub wrote: bool,
    /// The store refused the write as too long ([`Update::TooLong`]), so nothing was
    /// written or deleted.
    pub too_long: bool,
    /// URLs the store confirmed it removed.
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

    fn too_long() -> Self {
        Self {
            too_long: true,
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
                // The caller reports one error, so log each later one here.
                if self.error.is_none() {
                    self.error = Some(e);
                } else {
                    warn!("deleting absorbed URL {url}: {e:#}");
                }
            }
        }
    }
}

/// The account as the run believes it to be: seeded from `posts/all` and updated as passes
/// write, so no pass plans against or merges with a record an earlier one changed.
/// `cleanup --all` runs three writing passes over one read. Ordered, since a merge takes
/// its lead's note verbatim, so which bookmark leads must be stable.
pub struct AccountView {
    /// Insertion order: a rewritten URL keeps its place.
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
            // Two wire rows can parse to one `Url`: keep the first and say so.
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

    /// Record that `bookmark` occupies its URL, keeping an existing URL's place.
    pub fn insert(&mut self, bookmark: &Bookmark) {
        if !self.by_url.contains_key(&bookmark.url) {
            self.order.push(bookmark.url.clone());
        }
        self.by_url.insert(bookmark.url.clone(), bookmark.clone());
    }

    /// Record that nothing occupies `url`, dropping it from the order too: a tombstone
    /// would let a later `insert` of it yield the bookmark twice.
    pub fn remove(&mut self, url: &Url) {
        self.by_url.remove(url);
        self.order.retain(|ordered| ordered != url);
    }
}

/// Whether a run writes, or only previews what it would write (`--dry-run`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    #[default]
    Live,
    DryRun,
}

/// The mode a `--dry-run` flag sets: a dry run when it was given.
impl From<bool> for WriteMode {
    fn from(dry_run: bool) -> Self {
        if dry_run {
            WriteMode::DryRun
        } else {
            WriteMode::Live
        }
    }
}

impl WriteMode {
    /// A dry run when either one is: `--dry-run` may come before or after the subcommand.
    #[must_use]
    pub fn or(self, other: WriteMode) -> Self {
        if self.is_dry_run() {
            self
        } else {
            other
        }
    }

    pub fn is_dry_run(self) -> bool {
        self == WriteMode::DryRun
    }
}

/// The cleanup driver's read side of the account, from the live view, not a fetch. The
/// write mode lives here, so the preview can't disagree with what the store does.
pub trait AccountState {
    /// The bookmark stored at `url` right now, if any.
    fn resident(&self, url: &Url) -> Option<Bookmark>;
    /// Every bookmark `keep` accepts, in `posts/all` order, as earlier passes left it.
    fn snapshot(&self, keep: impl Fn(&Bookmark) -> bool) -> Vec<Bookmark>;
    /// Whether writes are sent, or only previewed.
    fn write_mode(&self) -> WriteMode;
}

/// A [`BookmarkStore`] that keeps an [`AccountView`] in step with what it writes.
///
/// Every write goes through it, so there is no bookkeeping to forget; intercepting `update`
/// and `delete` covers the `apply_*` defaults built on them.
///
/// A dry run still updates the view, without the network, so its preview shows what a real
/// run would do, a later pass's changes included. A 414 is the one thing it can't foresee.
pub struct CleanupStore<'a, P> {
    inner: &'a P,
    view: std::cell::RefCell<AccountView>,
    mode: WriteMode,
}

impl<'a, P: BookmarkStore> CleanupStore<'a, P> {
    pub fn new(inner: &'a P, view: AccountView, mode: WriteMode) -> Self {
        Self {
            inner,
            view: std::cell::RefCell::new(view),
            mode,
        }
    }
}

impl<P> AccountState for CleanupStore<'_, P> {
    fn resident(&self, url: &Url) -> Option<Bookmark> {
        self.view.borrow().get(url).cloned()
    }

    fn snapshot(&self, keep: impl Fn(&Bookmark) -> bool) -> Vec<Bookmark> {
        self.view
            .borrow()
            .iter()
            .filter(|bookmark| keep(bookmark))
            .cloned()
            .collect()
    }

    fn write_mode(&self) -> WriteMode {
        self.mode
    }
}

impl<P: BookmarkStore> BookmarkStore for CleanupStore<'_, P> {
    async fn add(&self, b: &Bookmark) -> Result<()> {
        if !self.mode.is_dry_run() {
            self.inner.add(b).await?;
        }
        self.view.borrow_mut().insert(b);
        Ok(())
    }

    async fn update(&self, b: &Bookmark) -> Result<Update> {
        let update = if self.mode.is_dry_run() {
            Update::Written
        } else {
            self.inner.update(b).await?
        };
        if update == Update::Written {
            self.view.borrow_mut().insert(b);
        }
        Ok(update)
    }

    async fn delete(&self, url: &Url) -> Result<()> {
        if !self.mode.is_dry_run() {
            self.inner.delete(url).await?;
        }
        self.view.borrow_mut().remove(url);
        Ok(())
    }

    fn fit(&self, b: Bookmark) -> Bookmark {
        self.inner.fit(b)
    }

    fn kept_of_cut<'n>(&self, note: &'n str) -> Option<&'n str> {
        self.inner.kept_of_cut(note)
    }

    fn can_write(&self, b: &Bookmark) -> Result<(), TooLongToSend> {
        self.inner.can_write(b)
    }
}

#[cfg(test)]
mod tests;
