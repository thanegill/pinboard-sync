use super::*;
use crate::test_support::{strings, url, FakePinboard};

/// A stored bookmark at `url`, titled "T", with `note`: the fixture every test here
/// builds its account from.
fn at(url: &str, note: &str) -> Bookmark {
    Bookmark {
        title: "T".into(),
        note: note.into(),
        ..crate::test_support::stored_at(url)
    }
}

#[tokio::test]
async fn a_merge_reports_the_write_and_every_delete_that_landed() {
    let pinboard = FakePinboard::default();
    let old_a = url("https://old-a/");
    let old_b = url("https://old-b/");

    let out = pinboard
        .apply_merge(&at("https://target/", ""), &[&old_a, &old_b])
        .await;

    assert!(out.wrote);
    assert_eq!(out.deleted, vec![old_a, old_b]);
    assert!(out.error.is_none());
}

#[tokio::test]
async fn a_merge_never_deletes_its_own_target() {
    // The merged record is written to the target, so deleting that URL as "absorbed"
    // would undo the write. Callers build `old_urls` excluding it, but this is a public
    // default method and the guard is what makes it safe to call with anything.
    let pinboard = FakePinboard::default();
    let target = url("https://target/");
    let absorbed = url("https://absorbed/");

    let outcome = pinboard
        .apply_merge(&at("https://target/", ""), &[&target, &absorbed])
        .await;

    assert!(outcome.error.is_none());
    assert_eq!(outcome.deleted, vec![absorbed]);
    assert_eq!(
        *pinboard.deleted.borrow(),
        strings(&["https://absorbed/"]),
        "the target must not even be attempted"
    );
}

#[tokio::test]
async fn a_merge_returns_the_first_delete_error_and_still_tries_the_rest() {
    // Only one error can be carried out to the caller, and it is the *first* — later
    // ones are logged instead. Asserting which one it is matters: the caller reports it
    // verbatim, so keeping the last would name a different URL than the one that first
    // went wrong.
    let pinboard = FakePinboard {
        fail_delete_urls: ["https://a/".to_string(), "https://b/".to_string()]
            .into_iter()
            .collect(),
        ..FakePinboard::default()
    };
    let a = url("https://a/");
    let b = url("https://b/");
    let c = url("https://c/");

    let outcome = pinboard
        .apply_merge(&at("https://target/", ""), &[&a, &b, &c])
        .await;

    assert!(outcome.wrote);
    let reported = outcome.error.expect("the caller still sees a failure");
    assert!(
        format!("{reported:#}").contains("https://a/"),
        "the first failure is the one reported, not the last: {reported:#}"
    );
    assert_eq!(outcome.deleted, vec![c], "and the one that worked landed");
    assert_eq!(
        *pinboard.deleted.borrow(),
        strings(&["https://a/", "https://b/", "https://c/"]),
        "every absorbed URL is still attempted"
    );
}

#[tokio::test]
async fn a_failed_delete_does_not_strand_the_remaining_absorbed_urls() {
    // Absorbed URLs are independent. Abandoning the rest on the first failure leaves
    // more duplicate records behind than necessary, and the caller still has to know
    // the write itself landed so it doesn't treat the target as untouched.
    let old_a = url("https://old-a/");
    let old_b = url("https://old-b/");
    let old_c = url("https://old-c/");
    let pinboard = FakePinboard {
        fail_delete_urls: [old_b.to_string()].into_iter().collect(),
        ..Default::default()
    };

    let out = pinboard
        .apply_merge(&at("https://target/", ""), &[&old_a, &old_b, &old_c])
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
    let old = url("https://old/");

    let out = pinboard
        .apply_merge(&at("https://target/", ""), &[&old])
        .await;

    assert!(!out.wrote);
    assert!(out.deleted.is_empty());
    assert!(out.error.is_some());
    assert!(pinboard.deleted.borrow().is_empty());
}

#[tokio::test]
async fn the_snapshot_reflects_this_runs_writes() {
    // A later pass plans from the snapshot, so it has to show what an earlier pass
    // wrote, which the `posts/all` read before the run does not.
    let pinboard = FakePinboard::default();
    let store = CleanupStore::new(
        &pinboard,
        AccountView::new(vec![at("https://a/", "seed")]),
        false,
    );
    store.update(&at("https://a/", "written")).await.unwrap();

    let all = store.snapshot();

    assert_eq!(all.len(), 1);
    assert_eq!(all[0].note, "written");
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
        .apply_update(&at("https://b/", "new"), Some(&url("https://a/")))
        .await;
    assert!(out.error.is_none());

    assert_eq!(store.resident(&url("https://b/")).unwrap().note, "new");
    assert!(store.resident(&url("https://a/")).is_none());
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
        .apply_update(&at("https://b/", "new"), Some(&url("https://a/")))
        .await;
    assert!(out.error.is_none());

    assert!(store.resident(&url("https://b/")).is_some());
    assert!(store.resident(&url("https://a/")).is_none());
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
        .apply_update(&at("https://b/", "new"), Some(&url("https://a/")))
        .await;

    assert!(!out.wrote);
    assert!(store.resident(&url("https://b/")).is_none());
    assert!(
        store.resident(&url("https://a/")).is_some(),
        "the old URL was never deleted, so it is still there"
    );
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
    view.remove(&url("https://a/"));
    assert_eq!(urls(&view), vec!["https://c/", "https://b/", "https://d/"]);
}

#[test]
fn reinserting_a_removed_url_does_not_duplicate_it() {
    // A tombstone left in the order would let a later `insert` of the same URL append a
    // second entry, and `iter()` would yield it twice for a later pass to plan twice.
    let mut view = AccountView::new(vec![at("https://a/", ""), at("https://b/", "")]);
    view.remove(&url("https://a/"));
    view.insert(&at("https://a/", "back"));

    assert_eq!(urls(&view), vec!["https://b/", "https://a/"]);
}

#[test]
fn insert_upserts_and_remove_drops() {
    let mut view = AccountView::new(vec![at("https://a/", "old"), at("https://b/", "")]);
    view.insert(&at("https://a/", "new"));
    view.remove(&url("https://b/"));

    assert_eq!(view.get(&url("https://a/")).unwrap().note, "new");
    assert!(view.get(&url("https://b/")).is_none());
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
        view.get(&url("https://dup.example/")).unwrap().note,
        "first"
    );
}

fn urls(view: &AccountView) -> Vec<String> {
    view.iter().map(|b| b.url.to_string()).collect()
}

fn bookmark() -> Bookmark {
    Bookmark {
        title: "Title".into(),
        note: "note".into(),
        tags: vec!["a".into(), "b".into()],
        ..crate::test_support::stored_at("https://example.com/")
    }
}

#[test]
fn diff_compares_timestamps_by_instant_not_by_rendering() {
    // The same moment written in two zones renders differently but is not a change;
    // treating it as one would rewrite the bookmark every run.
    let mut stored = bookmark();
    stored.timestamp = crate::timefmt::parse_rfc3339("2020-01-01T00:00:00Z");
    let mut plan = stored.clone();
    plan.timestamp = crate::timefmt::parse_rfc3339("2020-01-01T01:00:00+01:00");
    assert_eq!(stored.timestamp, plan.timestamp);
    assert!(
        stored.diff(&plan).is_empty(),
        "an equivalent time in another offset is not a change"
    );
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
