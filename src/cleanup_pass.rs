//! The shared `cleanup` driver. Each source describes how to re-shape one bookmark
//! (a [`CleanupPass`]); this module owns the loop common to all of them: plan every
//! bookmark's end-state, then group the plans by their target URL and write each group.
//! A lone plan is diffed against its stored bookmark, skipped when nothing changed, and
//! written via [`BookmarkStore::apply_update`] (deleting the old URL on a rewrite). Two or
//! more plans landing on one URL are a collision: they are field-merged (see
//! [`merge_bookmarks`]) into a single record written via [`BookmarkStore::apply_merge`],
//! which deletes the absorbed URLs. A rewrite's old URL is never deleted when it is
//! itself the target of another planned write in the same pass, so colliding/chained
//! rewrites can't clobber each other's record and the pass is order-independent.
//! `run_pass` renders the dry-run lines, tallies, and returns the number of bookmarks
//! that failed (logged and skipped) so a caller running several passes can aggregate and
//! bail once.

use std::collections::HashMap;

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
///
/// Two phases so that several bookmarks whose plans normalize to the *same* URL don't
/// clobber each other: phase 1 plans every bookmark (resolving date + privacy), phase 2
/// groups the plans by target URL and writes each group. A lone group is a normal
/// rewrite; a group of more than one is field-merged (see [`merge_bookmarks`]) into a
/// single bookmark that absorbs the others.
///
/// A rewrite's old URL is only deleted when it is not itself the target of another
/// planned write in the pass: if bookmark X normalizes onto a URL that is bookmark Y's
/// stored URL while Y moves elsewhere, Y's delete would otherwise remove the record X
/// just wrote there. Precomputing the set of all target URLs and refusing to delete any
/// of them makes the pass order-independent.
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

    let mut planned_pairs: Vec<(&Bookmark, Bookmark)> = Vec::new();
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

        // Privacy flags are never re-shaped by cleanup: take them from the stored
        // bookmark so a plan can't silently flip them.
        planned.public = bookmark.public;
        planned.read_later = bookmark.read_later;

        planned_pairs.push((bookmark, planned));
    }

    // First-appearance order of the groups (and snapshot order within each group).
    let mut group_order: Vec<&url::Url> = Vec::new();
    let mut groups: HashMap<&url::Url, Vec<(&Bookmark, &Bookmark)>> = HashMap::new();
    for pair in &planned_pairs {
        let (original, planned) = (pair.0, &pair.1);
        let key = &planned.url;
        if !groups.contains_key(key) {
            group_order.push(key);
        }
        groups.entry(key).or_default().push((original, planned));
    }

    // Every group's target URL. A rewrite's old URL is never deleted when it is one of
    // these, because some other planned write owns that URL; deleting it would clobber
    // that write's record. This keeps the pass order-independent.
    let targets: std::collections::HashSet<&url::Url> = group_order.iter().copied().collect();

    for key in group_order {
        let group = groups.remove(key).expect("key came from group_order");
        if group.len() == 1 {
            let (original, planned) = group[0];
            // The written fields that differ; empty means nothing a write would change.
            let changes = original.diff(planned);
            if changes.is_empty() {
                continue;
            }
            // Delete the old URL only when it changed and no other planned write targets
            // it; otherwise that write's record lives there and must not be removed.
            let url_changed = planned.url != original.url;
            let delete_old = url_changed && !targets.contains(&original.url);

            if dry_run {
                changed += 1;
                println!("[dry-run] {}", original.url);
                for (label, value) in &changes {
                    println!("          {label:<6}-> {value}");
                }
                continue;
            }

            // `planned` carries the stored `public`/`read_later` and the driver-resolved
            // `timestamp`, so it's the complete write model.
            // Log and skip a single failed update so the rest of the pass still runs.
            match pinboard
                .apply_update(planned, delete_old.then_some(&original.url))
                .await
            {
                Ok(()) => {
                    changed += 1;
                    debug!(
                        "updated {} -> {} [{}]",
                        original.url,
                        planned.url,
                        planned.tags.join(" ")
                    );
                }
                Err(e) => {
                    failed += 1;
                    error!("updating bookmark {}: {e:#}", original.url);
                }
            }
            continue;
        }

        // Field-merge and delete the absorbed URLs so a later run sees a single bookmark at
        // the target and converges.
        let target_url = group[0].1.url.clone();
        // The primary keeps its own privacy at the target: the member already resident at
        // the target URL, else (a brand-new URL) the first in stable order.
        let primary = group
            .iter()
            .find(|(original, _)| original.url == target_url)
            .map(|(_, planned)| *planned)
            .unwrap_or(group[0].1);
        let plans: Vec<&Bookmark> = group.iter().map(|(_, planned)| *planned).collect();
        let merged = merge_bookmarks(&plans, primary);
        let target = &merged.url;

        // Absorbed URLs to delete: an original that is neither the target nor another
        // planned write's target (deleting the latter would clobber that write's record).
        let mut old_urls: Vec<&url::Url> = Vec::new();
        for (original, _) in &group {
            if original.url != *target
                && !targets.contains(&original.url)
                && !old_urls.contains(&&original.url)
            {
                old_urls.push(&original.url);
            }
        }

        if dry_run {
            changed += 1;
            println!("[dry-run] {target}");
            println!("          {:<6}-> {}", "title", merged.title);
            let note_value = if merged.note.is_empty() {
                "(removed)"
            } else {
                &merged.note
            };
            println!("          {:<6}-> {note_value}", "notes");
            println!("          {:<6}-> [{}]", "tags", merged.tags.join(" "));
            if let Some(date) = merged.timestamp.and_then(crate::timefmt::to_rfc3339) {
                println!("          {:<6}-> {date}", "date");
            }
            println!("          {:<6}-> {}", "public", merged.public);
            println!("          {:<6}-> {}", "toread", merged.read_later);
            for old in &old_urls {
                println!("          absorb {old}");
            }
            continue;
        }

        match pinboard.apply_merge(&merged, &old_urls).await {
            Ok(()) => {
                changed += 1;
                let absorbed: Vec<String> = old_urls.iter().map(|u| u.to_string()).collect();
                debug!("merged {target} <- [{}]", absorbed.join(" "));
            }
            Err(e) => {
                failed += 1;
                error!("updating bookmark {target}: {e:#}");
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

/// Field-merge the PLANNED bookmarks of a collision group (all sharing `url`, given in
/// stable order) into one bookmark. Tags are an order-preserving union (first occurrence
/// wins, case-sensitive); notes are the distinct non-empty notes joined by a blank line;
/// title is the first non-empty; timestamp the earliest present. The privacy flags come
/// from `primary` (the member that stays at the target URL, else the first in order), never
/// OR'd across the group, so an absorbed bookmark can't widen the survivor's privacy.
fn merge_bookmarks(group: &[&Bookmark], primary: &Bookmark) -> Bookmark {
    let url = group[0].url.clone();

    let mut tags: Vec<String> = Vec::new();
    for b in group {
        for tag in &b.tags {
            if !tags.contains(tag) {
                tags.push(tag.clone());
            }
        }
    }

    let mut notes: Vec<&str> = Vec::new();
    for b in group {
        if !b.note.is_empty() && !notes.contains(&b.note.as_str()) {
            notes.push(&b.note);
        }
    }
    let note = notes.join("\n\n");

    let title = group
        .iter()
        .map(|b| b.title.as_str())
        .find(|t| !t.is_empty())
        .unwrap_or_default()
        .to_string();

    let timestamp = group.iter().filter_map(|b| b.timestamp).min();

    Bookmark {
        url,
        title,
        note,
        tags,
        timestamp,
        public: primary.public,
        read_later: primary.read_later,
    }
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

    #[test]
    fn merge_bookmarks_applies_field_rules() {
        let ts_early = crate::timefmt::from_unix(1_000);
        let ts_late = crate::timefmt::from_unix(2_000);
        let a = Bookmark {
            url: u("https://collide/"),
            title: String::new(),
            note: "from A".into(),
            tags: vec!["x".into(), "shared".into()],
            timestamp: ts_late,
            public: false,
            read_later: true,
        };
        let b = Bookmark {
            url: u("https://collide/"),
            title: "Title B".into(),
            note: "from B".into(),
            tags: vec!["shared".into(), "y".into()],
            timestamp: ts_early,
            public: true,
            read_later: false,
        };
        let c = Bookmark {
            url: u("https://collide/"),
            title: "Title C".into(),
            // A duplicate of B's note must not be repeated in the join.
            note: "from B".into(),
            tags: vec!["z".into()],
            timestamp: None,
            public: false,
            read_later: false,
        };

        // Primary is A (resident at the target); its privacy is kept verbatim, not OR'd,
        // even though B is public.
        let merged = merge_bookmarks(&[&a, &b, &c], &a);
        assert_eq!(merged.url.as_str(), "https://collide/");
        // Order-preserving, case-sensitive union across the group.
        assert_eq!(merged.tags, vec!["x", "shared", "y", "z"]);
        // Distinct non-empty notes in order, joined by a blank line.
        assert_eq!(merged.note, "from A\n\nfrom B");
        // First non-empty title.
        assert_eq!(merged.title, "Title B");
        // Earliest present timestamp.
        assert_eq!(merged.timestamp, ts_early);
        // Privacy taken from the primary (A), not OR'd: A is private, so the merge is too.
        assert!(!merged.public);
        assert!(merged.read_later);
    }

    #[test]
    fn merge_bookmarks_passes_a_single_note_through() {
        let a = Bookmark {
            note: "only note".into(),
            ..bookmark("https://collide/")
        };
        let b = Bookmark {
            note: String::new(),
            ..bookmark("https://collide/")
        };
        let merged = merge_bookmarks(&[&a, &b], &a);
        assert_eq!(merged.note, "only note");
    }

    #[tokio::test]
    async fn colliding_rewrites_are_field_merged() {
        // Two stored bookmarks whose plans land on the same URL: A normalizes onto B's URL
        // and B stays put. The driver must field-merge them into one record at that URL and
        // delete A's old URL, rather than clobbering either.
        let mut stored_a = bookmark("https://old-a/");
        stored_a.tags = vec!["x".into()];
        stored_a.note = "from A".into();
        let mut stored_b = bookmark("https://collide/");
        stored_b.tags = vec!["y".into()];
        stored_b.note = "from B".into();
        let books = vec![stored_a, stored_b];

        let pass = FakePass(|bookmark: &Bookmark| {
            if bookmark.url.as_str().contains("old-a") {
                Ok(Some(Bookmark {
                    url: u("https://collide/"),
                    ..unchanged_plan(bookmark)
                }))
            } else {
                Ok(Some(unchanged_plan(bookmark)))
            }
        });
        let pinboard = FakePinboard::default();

        let failed = run_pass(&pinboard, &books, false, "test", NO_DATING, &pass).await;
        assert_eq!(failed, 0);

        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://collide/");
        // Union in snapshot order: A's tags first, then B's.
        assert_eq!(updated[0].tags, vec!["x".to_string(), "y".to_string()]);
        assert!(updated[0].extended.contains("from A"));
        assert!(updated[0].extended.contains("from B"));
        // Only A's old URL is absorbed; the shared target is not deleted.
        assert_eq!(
            *pinboard.deleted.borrow(),
            vec!["https://old-a/".to_string()]
        );
    }

    #[tokio::test]
    async fn chained_rewrite_does_not_delete_another_writes_target() {
        // X at old-a rewrites to collide-b; Y at collide-b rewrites to new-z. X's target is
        // Y's stored URL, so Y's post-rewrite delete of collide-b would clobber X's record.
        // The pass must leave collide-b (a planned target) undeleted, in either order.
        async fn run_in_order(books: Vec<Bookmark>) {
            let pass = FakePass(|bookmark: &Bookmark| {
                let next = if bookmark.url.as_str().contains("old-a") {
                    "https://collide-b/"
                } else {
                    "https://new-z/"
                };
                Ok(Some(Bookmark {
                    url: u(next),
                    ..unchanged_plan(bookmark)
                }))
            });
            let pinboard = FakePinboard::default();

            let failed = run_pass(&pinboard, &books, false, "test", NO_DATING, &pass).await;
            assert_eq!(failed, 0);

            let updated = pinboard.updated.borrow();
            let mut written: Vec<&str> = updated.iter().map(|c| c.url.as_str()).collect();
            written.sort_unstable();
            assert_eq!(written, vec!["https://collide-b/", "https://new-z/"]);

            let deleted = pinboard.deleted.borrow();
            // old-a is deleted (no other write targets it); collide-b is X's target and
            // must survive Y's rewrite.
            assert!(deleted.contains(&"https://old-a/".to_string()));
            assert!(!deleted.contains(&"https://collide-b/".to_string()));
        }

        let x = bookmark("https://old-a/");
        let y = bookmark("https://collide-b/");
        run_in_order(vec![x.clone(), y.clone()]).await;
        run_in_order(vec![y, x]).await;
    }

    #[tokio::test]
    async fn colliding_rewrite_keeps_resident_privacy_not_widened() {
        // A (public) normalizes onto B's URL; B (private) is the resident survivor. The merge
        // must keep B's privacy: an absorbed public bookmark can't widen the survivor to public.
        let mut stored_a = bookmark("https://old-a/");
        stored_a.public = true;
        let mut stored_b = bookmark("https://collide/");
        stored_b.public = false;
        let books = vec![stored_a, stored_b];

        let pass = FakePass(|bookmark: &Bookmark| {
            if bookmark.url.as_str().contains("old-a") {
                Ok(Some(Bookmark {
                    url: u("https://collide/"),
                    ..unchanged_plan(bookmark)
                }))
            } else {
                Ok(Some(unchanged_plan(bookmark)))
            }
        });
        let pinboard = FakePinboard::default();

        let failed = run_pass(&pinboard, &books, false, "test", NO_DATING, &pass).await;
        assert_eq!(failed, 0);

        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://collide/");
        // Not widened: the resident private bookmark stays private.
        assert!(!updated[0].shared);
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
