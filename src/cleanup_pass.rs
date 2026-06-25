//! The shared `cleanup` driver. Each source describes how to re-shape one bookmark
//! (a [`CleanupPass`]); this module owns the loop common to all of them: diff the
//! planned end-state against the stored bookmark, skip when nothing changed, render
//! the dry-run lines, write via [`apply_update`] (deleting the old URL on a rewrite),
//! and tally. `run_pass` returns the number of bookmarks that failed (logged and
//! skipped) so a caller running several passes can aggregate and bail once.

use anyhow::Result;
use log::{debug, error, info};

use crate::pinboard::{apply_update, Bookmark, BookmarkStore, BookmarkUpdate};
use crate::source::tags_differ;

/// The desired end-state for one bookmark. `shared`/`toread` are always preserved
/// from the stored bookmark, so a pass only supplies the mutable fields.
pub struct Planned {
    pub url: String,
    pub description: String,
    pub extended: String,
    pub tags: Vec<String>,
    /// Pinboard `dt` (RFC3339), empty to leave Pinboard's default.
    pub dt: String,
}

/// How a source re-shapes one bookmark during `cleanup`.
#[allow(async_fn_in_trait)]
pub trait CleanupPass {
    /// The end-state for `bm`, or `None` to leave it unchanged outright. `Err` marks a
    /// per-item failure (logged and counted; the pass continues with the next bookmark).
    /// The driver still skips an unchanged `Some` (one whose fields all match `bm`), so
    /// a pass can return the computed end-state without checking for changes itself.
    async fn plan(&self, bm: &Bookmark) -> Result<Option<Planned>>;
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
    pass: &C,
) -> usize {
    info!(
        "scanning {} {noun} bookmark(s){}",
        bookmarks.len(),
        if dry_run { " (dry run)" } else { "" }
    );

    let mut changed = 0usize;
    let mut failed = 0usize;
    let mut wrote = false;
    for bm in bookmarks {
        let planned = match pass.plan(bm).await {
            Ok(Some(p)) => p,
            Ok(None) => continue,
            // Log and skip a single failed plan so the rest of the pass still runs.
            Err(e) => {
                failed += 1;
                error!("updating bookmark {}: {e:#}", bm.url);
                continue;
            }
        };

        let url_changed = planned.url != bm.url;
        if !changed_at_all(bm, &planned) {
            continue;
        }

        if dry_run {
            changed += 1;
            print_diff(bm, &planned);
            continue;
        }

        // Log and skip a single failed update so the rest of the pass still runs.
        match apply_update(
            pinboard,
            &mut wrote,
            BookmarkUpdate {
                url: &planned.url,
                description: &planned.description,
                extended: &planned.extended,
                tags: &planned.tags,
                shared: bm.is_shared(),
                toread: bm.is_toread(),
                dt: &planned.dt,
            },
            url_changed.then_some(bm.url.as_str()),
        )
        .await
        {
            Ok(()) => {
                changed += 1;
                debug!(
                    "updated {} -> {} [{}]",
                    bm.url,
                    planned.url,
                    planned.tags.join(" ")
                );
            }
            Err(e) => {
                failed += 1;
                error!("updating bookmark {}: {e:#}", bm.url);
            }
        }
    }

    if dry_run {
        println!("{changed} bookmark(s) would change.");
    } else {
        info!("done: updated {changed} bookmark(s)");
    }
    failed
}

/// Whether the plan differs from the stored bookmark in any written field.
fn changed_at_all(bm: &Bookmark, p: &Planned) -> bool {
    p.url != bm.url
        || p.description != bm.description
        || p.extended != bm.extended
        || p.dt != bm.time
        || tags_differ(&bm.tag_list(), &p.tags)
}

/// Print the changed fields of a planned update (dry-run output).
fn print_diff(bm: &Bookmark, p: &Planned) {
    println!("[dry-run] {}", bm.url);
    if p.url != bm.url {
        println!("          url   -> {}", p.url);
    }
    if p.description != bm.description {
        println!("          title -> {}", p.description);
    }
    if p.extended != bm.extended {
        if p.extended.is_empty() {
            println!("          notes -> (removed)");
        } else {
            println!("          notes -> {}", p.extended);
        }
    }
    if tags_differ(&bm.tag_list(), &p.tags) {
        println!("          tags  -> [{}]", p.tags.join(" "));
    }
    if p.dt != bm.time {
        println!("          date  -> {}", p.dt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::FakePinboard;
    use anyhow::anyhow;

    /// A `CleanupPass` whose `plan` is a closure, so each test scripts its own outcome.
    struct FakePass<F>(F);
    impl<F: Fn(&Bookmark) -> Result<Option<Planned>>> CleanupPass for FakePass<F> {
        async fn plan(&self, bm: &Bookmark) -> Result<Option<Planned>> {
            (self.0)(bm)
        }
    }

    fn bm(url: &str) -> Bookmark {
        Bookmark {
            url: url.into(),
            description: "Title".into(),
            extended: "notes".into(),
            tags: "a b".into(),
            time: "2020-01-01T00:00:00Z".into(),
            shared: "no".into(),
            toread: "no".into(),
        }
    }

    /// A plan identical to `b` (the driver should treat it as unchanged).
    fn unchanged_plan(b: &Bookmark) -> Planned {
        Planned {
            url: b.url.clone(),
            description: b.description.clone(),
            extended: b.extended.clone(),
            tags: b.tag_list(),
            dt: b.time.clone(),
        }
    }

    #[tokio::test]
    async fn err_plan_counts_failed_and_continues() {
        // The first bookmark's plan fails; the second still gets written.
        let books = vec![bm("https://x/bad"), bm("https://x/good")];
        let pass = FakePass(|b: &Bookmark| {
            if b.url.contains("bad") {
                Err(anyhow!("boom"))
            } else {
                Ok(Some(Planned {
                    description: "New".into(),
                    ..unchanged_plan(b)
                }))
            }
        });
        let pinboard = FakePinboard::default();

        let failed = run_pass(&pinboard, &books, false, "test", &pass).await;
        assert_eq!(failed, 1);
        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://x/good");
        assert_eq!(updated[0].description, "New");
    }

    #[tokio::test]
    async fn none_plan_is_skipped() {
        let books = vec![bm("https://x/")];
        let pass = FakePass(|_: &Bookmark| Ok(None));
        let pinboard = FakePinboard::default();

        let failed = run_pass(&pinboard, &books, false, "test", &pass).await;
        assert_eq!(failed, 0);
        assert!(pinboard.updated.borrow().is_empty());
        assert!(pinboard.deleted.borrow().is_empty());
    }

    #[tokio::test]
    async fn unchanged_plan_is_skipped() {
        let books = vec![bm("https://x/")];
        let pass = FakePass(|b: &Bookmark| Ok(Some(unchanged_plan(b))));
        let pinboard = FakePinboard::default();

        let failed = run_pass(&pinboard, &books, false, "test", &pass).await;
        assert_eq!(failed, 0);
        assert!(pinboard.updated.borrow().is_empty());
    }

    #[tokio::test]
    async fn url_change_updates_and_deletes_old_preserving_privacy() {
        let books = vec![bm("https://old/")];
        let pass = FakePass(|b: &Bookmark| {
            Ok(Some(Planned {
                url: "https://new/".into(),
                description: "New".into(),
                tags: vec!["x".into()],
                ..unchanged_plan(b)
            }))
        });
        let pinboard = FakePinboard::default();

        let failed = run_pass(&pinboard, &books, false, "test", &pass).await;
        assert_eq!(failed, 0);
        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://new/");
        // shared/toread are carried over from the stored bookmark (both "no").
        assert!(!updated[0].shared && !updated[0].toread);
        // The old URL is deleted after the rewrite.
        assert_eq!(*pinboard.deleted.borrow(), vec!["https://old/".to_string()]);
    }

    #[tokio::test]
    async fn apply_update_failure_is_logged_and_counted() {
        let books = vec![bm("https://x/")];
        // A desc-only change (URL unchanged, so no delete); the update itself fails.
        let pass = FakePass(|b: &Bookmark| {
            Ok(Some(Planned {
                description: "New".into(),
                ..unchanged_plan(b)
            }))
        });
        let mut pinboard = FakePinboard::default();
        pinboard.fail_update_urls.insert("https://x/".into());

        let failed = run_pass(&pinboard, &books, false, "test", &pass).await;
        assert_eq!(failed, 1);
        assert!(pinboard.updated.borrow().is_empty());
    }

    #[tokio::test]
    async fn dry_run_renders_every_field_and_writes_nothing() {
        // Two bookmarks so the dry-run renderer hits both notes branches: the first
        // empties the notes ("(removed)"), the second sets new non-empty notes.
        let books = vec![bm("https://empty/"), bm("https://full/")];
        let pass = FakePass(|b: &Bookmark| {
            let extended = if b.url.contains("empty") {
                String::new()
            } else {
                "new notes".into()
            };
            Ok(Some(Planned {
                url: format!("{}new", b.url),
                description: "New".into(),
                extended,
                tags: vec!["x".into()],
                dt: "2024-01-01T00:00:00Z".into(),
            }))
        });
        let pinboard = FakePinboard::default();

        let failed = run_pass(&pinboard, &books, true, "test", &pass).await;
        assert_eq!(failed, 0);
        assert!(pinboard.updated.borrow().is_empty());
        assert!(pinboard.deleted.borrow().is_empty());
    }
}
