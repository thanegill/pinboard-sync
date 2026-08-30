//! The service-agnostic bookmark domain type. [`Bookmark`] is what the `cleanup`
//! driver reads (from Pinboard) and plans an end-state in, independent of any service's
//! wire format: tags split out, the creation time as a real [`OffsetDateTime`], and the
//! flags as plain `bool`s. The Pinboard wire shape it's converted `From` lives next to
//! the client in [`crate::pinboard::PinboardBookmark`]; the formatting back to Pinboard
//! fields happens at the write boundary in `pinboard::post_add`.

use std::collections::HashMap;

use anyhow::{Context, Result};
use log::warn;
use time::OffsetDateTime;
use url::Url;

use crate::pinboard::PinboardBookmark;
use crate::source::tags_differ;

/// A bookmark in service-agnostic domain form. The field names are the domain's
/// (`title`/`note`/`public`/`read_later`), not Pinboard's wire names
/// (`description`/`extended`/`shared`/`toread` — those stay on [`PinboardBookmark`]). The
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

impl TryFrom<PinboardBookmark> for Bookmark {
    /// Fails only when the wire `href` doesn't parse as a URL; `all()` skips (and warns
    /// on) such entries. A non-empty `time` that won't parse is *not* fatal: it becomes
    /// `timestamp: None` (logged) so the bookmark stays in the set sync dedups against —
    /// dropping it would make sync re-add the URL and reset its date/title/tags/notes.
    type Error = url::ParseError;
    fn try_from(b: PinboardBookmark) -> Result<Self, Self::Error> {
        let url = Url::parse(&b.url)?;
        let timestamp = if b.time.is_empty() {
            None
        } else {
            let parsed = crate::timefmt::parse_rfc3339(&b.time);
            if parsed.is_none() {
                warn!(
                    "bookmark {url}: unparseable creation time {:?}, dropping date",
                    b.time
                );
            }
            parsed
        };
        Ok(Bookmark {
            url,
            title: b.description,
            note: b.extended,
            tags: b.tags.split_whitespace().map(String::from).collect(),
            timestamp,
            public: b.shared == "yes",
            read_later: b.toread == "yes",
        })
    }
}

impl Bookmark {
    /// The written fields where `new` differs from `self` (the stored bookmark), each as
    /// a `(label, rendered new value)` pair for the cleanup dry-run. Empty when nothing a
    /// write would change differs — so `cleanup` skips the bookmark. `timestamp` compares
    /// by instant (a re-formatted but equivalent time isn't a change). The `public` and
    /// `read_later` flags are not compared, because on the paths that call this they
    /// cannot differ: a single plan carries the stored values, forced there before diffing,
    /// and a merge onto a record already stored at the target reaches the write only when
    /// every member already matches it (see `cleanup_pass::run_pass`).
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
                .and_then(crate::timefmt::to_rfc3339)
                .unwrap_or_default();
            changes.push(("date", value));
        }
        changes
    }
}

/// The bookmark-store operations the sync/cleanup loops depend on. Abstracted from the
/// concrete Pinboard client so those loops can be exercised with an in-memory fake. A
/// write takes a whole [`Bookmark`]; the client maps it to the Pinboard `posts/add`
/// parameters at the boundary (see `pinboard::post_add`).
/// (Crate-internal, never spawned across threads, so the missing `Send` bound from
/// `async fn` in a trait is irrelevant here.)
#[allow(async_fn_in_trait)]
pub trait BookmarkStore {
    /// Every bookmark in the account (`posts/all`).
    async fn all(&self) -> Result<Vec<Bookmark>>;
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
                if self.error.is_none() {
                    self.error = Some(e);
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
    /// The run's own view, not a fetch: `posts/all` is read once, before the passes.
    async fn all(&self) -> Result<Vec<Bookmark>> {
        Ok(self.snapshot())
    }

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
mod write_outcome_tests {
    use super::*;
    use crate::test_support::FakePinboard;

    fn at(url: &str) -> Bookmark {
        Bookmark {
            url: Url::parse(url).unwrap(),
            title: "T".into(),
            note: String::new(),
            tags: Vec::new(),
            timestamp: None,
            public: false,
            read_later: false,
        }
    }

    #[tokio::test]
    async fn a_merge_reports_the_write_and_every_delete_that_landed() {
        let pinboard = FakePinboard::default();
        let old_a = Url::parse("https://old-a/").unwrap();
        let old_b = Url::parse("https://old-b/").unwrap();

        let out = pinboard
            .apply_merge(&at("https://target/"), &[&old_a, &old_b])
            .await;

        assert!(out.wrote);
        assert_eq!(out.deleted, vec![old_a, old_b]);
        assert!(out.error.is_none());
    }

    #[tokio::test]
    async fn a_failed_delete_does_not_strand_the_remaining_absorbed_urls() {
        // Absorbed URLs are independent. Abandoning the rest on the first failure leaves
        // more duplicate records behind than necessary, and the caller still has to know
        // the write itself landed so it doesn't treat the target as untouched.
        let old_a = Url::parse("https://old-a/").unwrap();
        let old_b = Url::parse("https://old-b/").unwrap();
        let old_c = Url::parse("https://old-c/").unwrap();
        let pinboard = FakePinboard {
            fail_delete_urls: [old_b.to_string()].into_iter().collect(),
            ..Default::default()
        };

        let out = pinboard
            .apply_merge(&at("https://target/"), &[&old_a, &old_b, &old_c])
            .await;

        assert!(out.wrote, "the write landed and the caller must know");
        assert_eq!(
            out.deleted,
            vec![old_a, old_c],
            "the delete after the failing one must still be attempted"
        );
        assert!(out.error.is_some());
    }

    #[tokio::test]
    async fn a_failed_write_reports_nothing_written_and_deletes_nothing() {
        let pinboard = FakePinboard {
            fail_update_urls: ["https://target/".to_string()].into_iter().collect(),
            ..Default::default()
        };
        let old = Url::parse("https://old/").unwrap();

        let out = pinboard.apply_merge(&at("https://target/"), &[&old]).await;

        assert!(!out.wrote);
        assert!(out.deleted.is_empty());
        assert!(out.error.is_some());
        assert!(pinboard.deleted.borrow().is_empty());
    }
}

#[cfg(test)]
mod cleanup_store_tests {
    use super::*;
    use crate::test_support::FakePinboard;

    fn at(url: &str, note: &str) -> Bookmark {
        Bookmark {
            url: Url::parse(url).unwrap(),
            title: "T".into(),
            note: note.into(),
            tags: Vec::new(),
            timestamp: None,
            public: false,
            read_later: false,
        }
    }

    #[tokio::test]
    async fn a_write_is_visible_to_the_next_read_without_refetching() {
        // The whole point: a later pass in the same run asks what occupies a URL and gets
        // what an earlier pass put there, not what `posts/all` said before it ran.
        let fake = FakePinboard::default();
        let store = CleanupStore::new(
            &fake,
            AccountView::new(vec![at("https://a/", "old")]),
            false,
        );

        let out = store
            .apply_update(
                &at("https://b/", "new"),
                Some(&Url::parse("https://a/").unwrap()),
            )
            .await;
        assert!(out.error.is_none());

        assert_eq!(
            store
                .resident(&Url::parse("https://b/").unwrap())
                .unwrap()
                .note,
            "new"
        );
        assert!(store.resident(&Url::parse("https://a/").unwrap()).is_none());
        assert_eq!(fake.updated.borrow().len(), 1);
    }

    #[tokio::test]
    async fn a_dry_run_updates_the_view_but_writes_nothing() {
        // A preview must show the same changes a real run makes, including ones a later
        // pass only makes because an earlier one wrote — so the view moves, the store
        // does not.
        let fake = FakePinboard::default();
        let store = CleanupStore::new(&fake, AccountView::new(vec![at("https://a/", "old")]), true);

        let out = store
            .apply_update(
                &at("https://b/", "new"),
                Some(&Url::parse("https://a/").unwrap()),
            )
            .await;
        assert!(out.error.is_none());

        assert!(store.resident(&Url::parse("https://b/").unwrap()).is_some());
        assert!(store.resident(&Url::parse("https://a/").unwrap()).is_none());
        assert!(fake.updated.borrow().is_empty(), "dry run must not write");
        assert!(fake.deleted.borrow().is_empty(), "dry run must not delete");
    }

    #[tokio::test]
    async fn a_failed_write_leaves_the_view_alone() {
        let fake = FakePinboard {
            fail_update_urls: ["https://b/".to_string()].into_iter().collect(),
            ..Default::default()
        };
        let store = CleanupStore::new(
            &fake,
            AccountView::new(vec![at("https://a/", "old")]),
            false,
        );

        let out = store
            .apply_update(
                &at("https://b/", "new"),
                Some(&Url::parse("https://a/").unwrap()),
            )
            .await;

        assert!(!out.wrote);
        assert!(store.resident(&Url::parse("https://b/").unwrap()).is_none());
        assert!(
            store.resident(&Url::parse("https://a/").unwrap()).is_some(),
            "the old URL was never deleted, so it is still there"
        );
    }
}

#[cfg(test)]
mod account_view_tests {
    use super::*;

    fn at(url: &str, note: &str) -> Bookmark {
        Bookmark {
            url: Url::parse(url).unwrap(),
            title: "T".into(),
            note: note.into(),
            tags: Vec::new(),
            timestamp: None,
            public: false,
            read_later: false,
        }
    }

    #[test]
    fn preserves_posts_all_order_across_writes_and_deletes() {
        // Order is load-bearing, not cosmetic: a merge takes the first group member's
        // note verbatim, so iteration order decides which note survives. A HashMap alone
        // would make that vary run to run.
        let mut view = AccountView::new(vec![at("https://c/", ""), at("https://a/", "")]);
        assert_eq!(urls(&view), vec!["https://c/", "https://a/"]);

        // An in-place update keeps its position rather than moving to the end.
        view.insert(&at("https://c/", "updated"));
        assert_eq!(urls(&view), vec!["https://c/", "https://a/"]);

        // A brand-new URL appends.
        view.insert(&at("https://b/", ""));
        assert_eq!(urls(&view), vec!["https://c/", "https://a/", "https://b/"]);

        view.insert(&at("https://d/", ""));
        view.remove(&Url::parse("https://a/").unwrap());
        assert_eq!(urls(&view), vec!["https://c/", "https://b/", "https://d/"]);
    }

    #[test]
    fn reinserting_a_removed_url_does_not_duplicate_it() {
        // `remove` used to leave the order entry behind as a tombstone, so a later
        // `insert` of the same URL appended a second one and `iter()` yielded the
        // bookmark twice — which a later pass would then plan twice.
        let mut view = AccountView::new(vec![at("https://a/", ""), at("https://b/", "")]);
        view.remove(&Url::parse("https://a/").unwrap());
        view.insert(&at("https://a/", "back"));

        assert_eq!(urls(&view), vec!["https://b/", "https://a/"]);
    }

    #[test]
    fn insert_upserts_and_remove_drops() {
        let mut view = AccountView::new(vec![at("https://a/", "old"), at("https://b/", "")]);
        view.insert(&at("https://a/", "new"));
        view.remove(&Url::parse("https://b/").unwrap());

        assert_eq!(
            view.get(&Url::parse("https://a/").unwrap()).unwrap().note,
            "new"
        );
        assert!(view.get(&Url::parse("https://b/").unwrap()).is_none());
        assert_eq!(view.iter().count(), 1);
    }

    #[test]
    fn duplicate_urls_in_posts_all_keep_the_first_rather_than_vanishing() {
        // `wire_to_bookmarks` does not dedup and `Url::parse` normalizes (default ports,
        // a root path's trailing slash), so two wire rows can land on one parsed URL.
        // Dropping one silently would hide a bookmark from every pass.
        let view = AccountView::new(vec![
            at("https://dup.example/", "first"),
            at("https://dup.example:443/", "second"),
        ]);
        assert_eq!(view.iter().count(), 1);
        assert_eq!(
            view.get(&Url::parse("https://dup.example/").unwrap())
                .unwrap()
                .note,
            "first"
        );
    }

    fn urls(view: &AccountView) -> Vec<String> {
        view.iter().map(|b| b.url.to_string()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bookmark() -> Bookmark {
        Bookmark {
            url: Url::parse("https://example.com/").unwrap(),
            title: "Title".into(),
            note: "note".into(),
            tags: vec!["a".into(), "b".into()],
            timestamp: None,
            public: false,
            read_later: false,
        }
    }

    #[test]
    fn diff_ignores_privacy_flags() {
        let stored = bookmark();
        let planned = Bookmark {
            public: true,
            read_later: true,
            ..bookmark()
        };
        assert!(stored.diff(&planned).is_empty());
    }

    fn wire(time: &str) -> PinboardBookmark {
        PinboardBookmark {
            url: "https://example.com/".into(),
            description: "T".into(),
            extended: String::new(),
            tags: String::new(),
            time: time.into(),
            shared: "no".into(),
            toread: "no".into(),
        }
    }

    #[test]
    fn unparseable_time_keeps_the_bookmark_with_no_timestamp() {
        // A non-empty `time` that won't parse must NOT drop the record: it stays visible
        // (as `timestamp: None`) so sync's dedup set still contains its URL.
        let bookmark = Bookmark::try_from(wire("not a date")).unwrap();
        assert_eq!(bookmark.url.as_str(), "https://example.com/");
        assert_eq!(bookmark.timestamp, None);
    }

    #[test]
    fn parseable_time_becomes_a_timestamp() {
        let bookmark = Bookmark::try_from(wire("2020-01-01T00:00:00Z")).unwrap();
        assert_eq!(bookmark.timestamp, crate::timefmt::from_unix(1_577_836_800));
    }

    #[test]
    fn unparseable_url_is_the_only_fatal_conversion() {
        let mut bad = wire("");
        bad.url = "not a url".into();
        assert!(Bookmark::try_from(bad).is_err());
    }
}
