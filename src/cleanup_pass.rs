//! The shared `cleanup` driver. Each source describes how to re-shape one bookmark
//! (a [`CleanupPass`]); this module owns the loop common to all of them: diff the
//! planned end-state against the stored bookmark, skip when nothing changed, render
//! the dry-run lines, write via [`BookmarkStore::apply_update`] (deleting the old URL on a rewrite),
//! and tally. `run_pass` returns the number of bookmarks that failed (logged and
//! skipped) so a caller running several passes can aggregate and bail once.

use anyhow::Result;
use log::{debug, error, info};
use time::OffsetDateTime;

use crate::bookmark::{Bookmark, BookmarkStore};

/// The `use_post_date` policy applied uniformly across a pass: whether to re-date by
/// the source date, the backdate age cap, and whether to push stale (older-than-cap)
/// items to "now". Resolved once by the caller from its source's cleanup options.
#[derive(Clone, Copy)]
pub struct DateOpts {
    pub use_post_date: bool,
    pub max_age_days: u64,
    pub stale_to_now: bool,
}

/// How a source re-shapes one bookmark during `cleanup`.
#[allow(async_fn_in_trait)]
pub trait CleanupPass {
    /// The end-state for `bookmark` as a [`Bookmark`], or `None` to leave it unchanged
    /// outright. `Err` marks a per-item failure (logged and counted; the pass continues
    /// with the next bookmark). The plan's `src_date` is the *candidate* source date; the
    /// driver resolves the final date from it via the pass's [`DateOpts`], and always
    /// takes `shared`/`toread` from the stored bookmark — so those two fields on the
    /// returned `Bookmark` are ignored. The driver still skips an unchanged plan (one
    /// whose fields all match `bookmark`), so a pass can return the computed end-state
    /// without checking for changes itself.
    async fn plan(&self, bookmark: &Bookmark) -> Result<Option<Bookmark>>;
}

/// Run `pass` over `bookmarks` (already filtered to the source). Re-writes each
/// changed bookmark (deleting the old URL when it changed), or prints the diff under
/// `dry_run`. `noun` names the source in the scan log. Returns the count that failed
/// to update.
pub async fn run_pass<P: BookmarkStore, C: CleanupPass>(
    pinboard: &P,
    bookmarks: &[Bookmark],
    dry_run: bool,
    noun: &str,
    dates: DateOpts,
    pass: &C,
) -> usize {
    // Dry-run output is consistently prefixed with `[dry-run]`.
    let dry_prefix = if dry_run { "[dry-run] " } else { "" };
    info!(
        "{dry_prefix}scanning {} {noun} bookmark(s)",
        bookmarks.len()
    );

    let now = OffsetDateTime::now_utc();
    let mut changed = 0usize;
    let mut failed = 0usize;
    for bookmark in bookmarks {
        let mut planned = match pass.plan(bookmark).await {
            Ok(Some(p)) => p,
            Ok(None) => continue,
            // Log and skip a single failed plan so the rest of the pass still runs.
            Err(e) => {
                failed += 1;
                error!("updating bookmark {}: {e:#}", bookmark.url);
                continue;
            }
        };

        // Resolve the final creation time here, at the write boundary: the candidate
        // source time when dating is on and within the cap, else "now"/preserve per the
        // policy. Comparable by instant to the stored `timestamp`.
        planned.timestamp = crate::timefmt::cleanup_date(
            dates.use_post_date,
            dates.max_age_days,
            dates.stale_to_now,
            planned.timestamp,
            now,
            bookmark.timestamp,
        );

        // The written fields that differ; empty means nothing a write would change.
        let changes = bookmark.diff(&planned);
        if changes.is_empty() {
            continue;
        }
        let url_changed = planned.url != bookmark.url;

        if dry_run {
            changed += 1;
            println!("[dry-run] {}", bookmark.url);
            for (label, value) in &changes {
                println!("          {label:<6}-> {value}");
            }
            continue;
        }

        // `planned` already carries the stored `public`/`read_later` (the planners copy
        // them) and the driver-resolved `timestamp`, so it's the complete write model.
        // Log and skip a single failed update so the rest of the pass still runs.
        match pinboard
            .apply_update(&planned, url_changed.then_some(&bookmark.url))
            .await
        {
            Ok(()) => {
                changed += 1;
                debug!(
                    "updated {} -> {} [{}]",
                    bookmark.url,
                    planned.url,
                    planned.tags.join(" ")
                );
            }
            Err(e) => {
                failed += 1;
                error!("updating bookmark {}: {e:#}", bookmark.url);
            }
        }
    }

    if dry_run {
        println!("{dry_prefix}{changed} bookmark(s) would change.");
    } else {
        info!("done: updated {changed} bookmark(s)");
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::FakePinboard;
    use anyhow::anyhow;
    use url::Url;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    /// A `CleanupPass` whose `plan` is a closure, so each test scripts its own outcome.
    struct FakePass<F>(F);
    impl<F: Fn(&Bookmark) -> Result<Option<Bookmark>>> CleanupPass for FakePass<F> {
        async fn plan(&self, bookmark: &Bookmark) -> Result<Option<Bookmark>> {
            (self.0)(bookmark)
        }
    }

    fn bookmark(url: &str) -> Bookmark {
        Bookmark {
            url: u(url),
            title: "Title".into(),
            note: "notes".into(),
            tags: vec!["a".into(), "b".into()],
            timestamp: crate::timefmt::from_unix(1_577_836_800), // 2020-01-01T00:00:00Z
            public: false,
            read_later: false,
        }
    }

    /// A plan identical to `bookmark` (the driver should treat it as unchanged under
    /// [`NO_DATING`], which leaves the stored date intact).
    fn unchanged_plan(bookmark: &Bookmark) -> Bookmark {
        bookmark.clone()
    }

    /// Dating off: the driver preserves each bookmark's existing date.
    const NO_DATING: DateOpts = DateOpts {
        use_post_date: false,
        max_age_days: 0,
        stale_to_now: false,
    };

    #[tokio::test]
    async fn err_plan_counts_failed_and_continues() {
        // The first bookmark's plan fails; the second still gets written.
        let books = vec![bookmark("https://x/bad"), bookmark("https://x/good")];
        let pass = FakePass(|bookmark: &Bookmark| {
            if bookmark.url.as_str().contains("bad") {
                Err(anyhow!("boom"))
            } else {
                Ok(Some(Bookmark {
                    title: "New".into(),
                    ..unchanged_plan(bookmark)
                }))
            }
        });
        let pinboard = FakePinboard::default();

        let failed = run_pass(&pinboard, &books, false, "test", NO_DATING, &pass).await;
        assert_eq!(failed, 1);
        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://x/good");
        assert_eq!(updated[0].description, "New");
    }

    #[tokio::test]
    async fn none_plan_is_skipped() {
        let books = vec![bookmark("https://x/")];
        let pass = FakePass(|_: &Bookmark| Ok(None));
        let pinboard = FakePinboard::default();

        let failed = run_pass(&pinboard, &books, false, "test", NO_DATING, &pass).await;
        assert_eq!(failed, 0);
        assert!(pinboard.updated.borrow().is_empty());
        assert!(pinboard.deleted.borrow().is_empty());
    }

    #[tokio::test]
    async fn unchanged_plan_is_skipped() {
        let books = vec![bookmark("https://x/")];
        let pass = FakePass(|bookmark: &Bookmark| Ok(Some(unchanged_plan(bookmark))));
        let pinboard = FakePinboard::default();

        let failed = run_pass(&pinboard, &books, false, "test", NO_DATING, &pass).await;
        assert_eq!(failed, 0);
        assert!(pinboard.updated.borrow().is_empty());
    }

    #[tokio::test]
    async fn url_change_updates_and_deletes_old_preserving_privacy() {
        let books = vec![bookmark("https://old/")];
        let pass = FakePass(|bookmark: &Bookmark| {
            Ok(Some(Bookmark {
                url: u("https://new/"),
                title: "New".into(),
                tags: vec!["x".into()],
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();

        let failed = run_pass(&pinboard, &books, false, "test", NO_DATING, &pass).await;
        assert_eq!(failed, 0);
        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://new/");
        // shared/toread are carried over from the stored bookmark (both off).
        assert!(!updated[0].shared && !updated[0].toread);
        // The old URL is deleted after the rewrite.
        assert_eq!(*pinboard.deleted.borrow(), vec!["https://old/".to_string()]);
    }

    #[tokio::test]
    async fn apply_update_failure_is_logged_and_counted() {
        let books = vec![bookmark("https://x/")];
        // A desc-only change (URL unchanged, so no delete); the update itself fails.
        let pass = FakePass(|bookmark: &Bookmark| {
            Ok(Some(Bookmark {
                title: "New".into(),
                ..unchanged_plan(bookmark)
            }))
        });
        let mut pinboard = FakePinboard::default();
        pinboard.fail_update_urls.insert("https://x/".into());

        let failed = run_pass(&pinboard, &books, false, "test", NO_DATING, &pass).await;
        assert_eq!(failed, 1);
        assert!(pinboard.updated.borrow().is_empty());
    }

    #[tokio::test]
    async fn dry_run_renders_every_field_and_writes_nothing() {
        // Two bookmarks so the dry-run renderer hits both notes branches: the first
        // empties the notes ("(removed)"), the second sets new non-empty notes.
        let books = vec![bookmark("https://empty/"), bookmark("https://full/")];
        let pass = FakePass(|bookmark: &Bookmark| {
            let note = if bookmark.url.as_str().contains("empty") {
                String::new()
            } else {
                "new notes".into()
            };
            Ok(Some(Bookmark {
                url: u(&format!("{}new", bookmark.url)),
                title: "New".into(),
                note,
                tags: vec!["x".into()],
                // A datable candidate source time, so the driver re-dates and the
                // `date ->` line renders.
                timestamp: crate::timefmt::from_unix(1_700_000_000),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();

        // Dating on with a huge cap, so the source date is always applied.
        let dates = DateOpts {
            use_post_date: true,
            max_age_days: 1_000_000,
            stale_to_now: false,
        };
        let failed = run_pass(&pinboard, &books, true, "test", dates, &pass).await;
        assert_eq!(failed, 0);
        assert!(pinboard.updated.borrow().is_empty());
        assert!(pinboard.deleted.borrow().is_empty());
    }
}
