use super::*;
use crate::bookmark::{AccountState, AccountView, CleanupStore, WriteMode};
use crate::pinboard::TRUNCATION_MARKER;
use crate::test_support::{stored_at, strings, tag_list, url as u, FakePinboard, Op, NO_DATING};
use anyhow::anyhow;

/// Wrap the fake in the production store so every test exercises the same view
/// bookkeeping the real run does, rather than a stand-in that can drift from it.
fn store<'a>(
    pinboard: &'a FakePinboard,
    all: &[Bookmark],
    mode: WriteMode,
) -> CleanupStore<'a, FakePinboard> {
    CleanupStore::new(pinboard, AccountView::new(all.to_vec()), mode)
}

/// [`run_pass`] over `books`, with the account seeded from them and dating off.
async fn run<C: CleanupPass>(pinboard: &FakePinboard, books: &[Bookmark], pass: &C) -> PassOutcome {
    run_pass(
        &store(pinboard, books, WriteMode::Live),
        books,
        "test",
        NO_DATING,
        pass,
    )
    .await
}

/// [`run`] as a dry run.
async fn run_dry<C: CleanupPass>(
    pinboard: &FakePinboard,
    books: &[Bookmark],
    pass: &C,
) -> PassOutcome {
    run_pass(
        &store(pinboard, books, WriteMode::DryRun),
        books,
        "test",
        NO_DATING,
        pass,
    )
    .await
}

/// A `CleanupPass` whose `plan` is a closure, so each test scripts its own outcome.
struct FakePass<F>(F);
impl<F: Fn(&Bookmark) -> Result<Plan, SourceError>> CleanupPass for FakePass<F> {
    async fn plan(&self, bookmark: &Bookmark) -> Result<Plan, SourceError> {
        (self.0)(bookmark)
    }
}

/// A per-item read failure: the kind that must not fail the whole run.
fn item_error() -> SourceError {
    SourceError::Other(anyhow!("boom"))
}

fn bookmark(url: &str) -> Bookmark {
    Bookmark {
        title: "Title".into(),
        note: "notes".into(),
        tags: tag_list(&["a", "b"]),
        timestamp: crate::timefmt::from_unix(1_577_836_800), // 2020-01-01T00:00:00Z
        ..stored_at(url)
    }
}

/// A plan identical to `bookmark` (the driver should treat it as unchanged under
/// [`NO_DATING`], which leaves the stored date intact).
fn unchanged_plan(bookmark: &Bookmark) -> Bookmark {
    bookmark.clone()
}

#[tokio::test]
async fn err_plan_counts_failed_and_continues() {
    // The first bookmark's plan fails; the second still gets written.
    let books = vec![bookmark("https://x/bad"), bookmark("https://x/good")];
    let pass = FakePass(|bookmark: &Bookmark| {
        if bookmark.url.as_str().contains("bad") {
            Err(item_error())
        } else {
            Ok(Plan::Bookmark(Bookmark {
                title: "New".into(),
                ..unchanged_plan(bookmark)
            }))
        }
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.plan_failed, 1);
    assert_eq!(outcome.write_failed, 0);
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://x/good");
    assert_eq!(updated[0].description, "New");
}

#[tokio::test]
async fn one_unreachable_link_does_not_fail_the_run() {
    // The heart of the policy: a single link we couldn't look up is logged, but the
    // run still succeeds, because everything we *could* do, we did. The good ones
    // outnumber it, so this stays clear of the majority-failure guard.
    let books = vec![
        bookmark("https://x/bad"),
        bookmark("https://x/good-1"),
        bookmark("https://x/good-2"),
    ];
    let pass = FakePass(|bookmark: &Bookmark| {
        if bookmark.url.as_str().contains("bad") {
            Err(item_error())
        } else {
            Ok(Plan::Bookmark(unchanged_plan(bookmark)))
        }
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    outcome.into_result().unwrap();
}

#[tokio::test]
async fn a_failed_write_fails_the_run() {
    // The destination is what we cannot sync to, so this one is fatal.
    let books = vec![bookmark("https://x/")];
    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            title: "New".into(),
            ..unchanged_plan(bookmark)
        }))
    });
    let mut pinboard = FakePinboard::default();
    pinboard.fail_update_urls.insert("https://x/".into());

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.write_failed, 1);
    assert!(
        pinboard.updated.borrow().is_empty(),
        "{:?}",
        pinboard.updated.borrow()
    );
    let err = outcome.into_result().unwrap_err();
    assert!(err.to_string().contains("failed to write"), "{err}");
}

#[tokio::test]
async fn every_plan_failing_fails_the_run() {
    // Not one bad link but an unreachable source: nothing got through, so the run
    // must not report success.
    let books = vec![bookmark("https://x/one"), bookmark("https://x/two")];
    let pass = FakePass(|_: &Bookmark| Err(item_error()));
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.plan_failed, 2);
    let err = outcome.into_result().unwrap_err();
    assert!(err.to_string().contains("2 of 2"), "{err}");
}

#[tokio::test]
async fn a_lone_failed_lookup_never_fails_the_run() {
    // A pass can have a tiny population -- `cleanup hackernews --link-discussions`
    // strips its own marker tag, so in the steady state it looks up one or two
    // bookmarks. One transient hiccup there must not fail the run, or a single bad
    // lookup wedges the scheduled service exactly as before.
    let books = vec![bookmark("https://x/only")];
    let pass = FakePass(|_: &Bookmark| Err(item_error()));
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!((outcome.reached, outcome.plan_failed), (0, 1));
    outcome.into_result().unwrap();
}

#[tokio::test]
async fn a_majority_of_lookups_failing_fails_the_run() {
    // Not one bad link but widespread trouble -- a rate limit or an outage part-way
    // through the run. Some lookups got through, so the source isn't flat down, but
    // reporting success would hide most of the work never happening.
    let books = vec![
        bookmark("https://x/ok"),
        bookmark("https://x/bad-1"),
        bookmark("https://x/bad-2"),
        bookmark("https://x/bad-3"),
    ];
    let pass = FakePass(|bookmark: &Bookmark| {
        if bookmark.url.as_str().contains("bad") {
            Err(item_error())
        } else {
            Ok(Plan::Unchanged)
        }
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!((outcome.reached, outcome.plan_failed), (1, 3));
    let err = outcome.into_result().unwrap_err();
    assert!(err.to_string().contains("3 of 4"), "{err}");
}

#[test]
fn a_halt_reports_the_lookups_and_writes_that_failed_before_it() {
    let message = |plan_failed, write_failed| {
        let outcome = PassOutcome {
            halted: Some(Halt::Reauth("cookie expired".into())),
            plan_failed,
            write_failed,
            ..PassOutcome::default()
        };
        outcome.into_result().unwrap_err().to_string()
    };
    assert_eq!(message(0, 0), "re-authentication required: cookie expired");
    assert_eq!(
        message(2, 1),
        "re-authentication required: cookie expired (also failed: 2 lookup(s), 1 write(s))"
    );
    assert_eq!(
        message(0, 3),
        "re-authentication required: cookie expired (also failed: 3 write(s))"
    );
}

#[tokio::test]
async fn a_rate_limit_stops_the_pass_like_a_dead_credential() {
    // Being rate limited fails every remaining lookup just as a dead credential does,
    // so stop rather than spend one doomed request per bookmark -- while still writing
    // the plans already made.
    let books = vec![
        bookmark("https://x/one"),
        bookmark("https://x/limited"),
        bookmark("https://x/never"),
    ];
    let planned = std::cell::Cell::new(0usize);
    let pass = FakePass(|bookmark: &Bookmark| {
        planned.set(planned.get() + 1);
        if bookmark.url.as_str().ends_with("one") {
            return Ok(Plan::Bookmark(Bookmark {
                title: "New".into(),
                ..unchanged_plan(bookmark)
            }));
        }
        Err(SourceError::Halted(Halt::RateLimited(
            "resets at 14:23".into(),
        )))
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(planned.get(), 2, "should stop at the rate limit");
    assert_eq!(
        outcome.halted,
        Some(Halt::RateLimited("resets at 14:23".into()))
    );
    assert_eq!(pinboard.updated.borrow().len(), 1);
    let err = outcome.into_result().unwrap_err().to_string();
    assert!(err.contains("resets at 14:23"), "{err}");
}

#[tokio::test]
async fn an_edge_block_stops_the_pass_and_is_reported_as_a_block() {
    // A block refuses every remaining lookup too, and must come back out as a block:
    // turned into a re-auth it would fire the hook, into a plain error it would be
    // counted as one dead link and the pass would grind on.
    let books = vec![
        bookmark("https://x/one"),
        bookmark("https://x/blocked"),
        bookmark("https://x/never"),
    ];
    let planned = std::cell::Cell::new(0usize);
    let pass = FakePass(|bookmark: &Bookmark| {
        planned.set(planned.get() + 1);
        if bookmark.url.as_str().ends_with("one") {
            return Ok(Plan::Bookmark(Bookmark {
                title: "New".into(),
                ..unchanged_plan(bookmark)
            }));
        }
        Err(SourceError::Halted(Halt::Blocked("edge block".into())))
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(planned.get(), 2, "should stop at the block");
    assert_eq!(outcome.halted, Some(Halt::Blocked("edge block".into())));
    assert_eq!(outcome.plan_failed, 0, "a block is not a per-item failure");
    assert_eq!(pinboard.updated.borrow().len(), 1);
    let err = outcome.into_result().unwrap_err();
    assert!(
        matches!(&err, SourceError::Halted(Halt::Blocked(m)) if m == "edge block"),
        "got {err:?}"
    );
}

#[tokio::test]
async fn reauth_stops_planning_but_still_writes_what_was_planned() {
    // A dead credential fails every remaining lookup, so stop rather than burn a
    // request per bookmark -- but the plans already made are good, so write them
    // rather than throwing the work away.
    let books = vec![
        bookmark("https://x/one"),
        bookmark("https://x/two"),
        bookmark("https://x/three"),
    ];
    let planned = std::cell::Cell::new(0usize);
    let pass = FakePass(|bookmark: &Bookmark| {
        planned.set(planned.get() + 1);
        if bookmark.url.as_str().ends_with("one") {
            return Ok(Plan::Bookmark(Bookmark {
                title: "New".into(),
                ..unchanged_plan(bookmark)
            }));
        }
        Err(SourceError::Halted(Halt::Reauth("token expired".into())))
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(planned.get(), 2, "should stop at the reauth failure");
    let updated = pinboard.updated.borrow();
    assert_eq!(
        updated.len(),
        1,
        "the plan made before it died is still written"
    );
    assert_eq!(updated[0].url, "https://x/one");
    let err = outcome.into_result().unwrap_err();
    assert!(err.to_string().contains("token expired"), "{err}");
}

#[tokio::test]
async fn a_reauth_alongside_failed_writes_reports_both() {
    // Planning stops at the dead credential but the earlier plans are still written,
    // so both failures can be real. Reporting only the credential would hide that
    // writes were also being rejected.
    let books = vec![bookmark("https://x/one"), bookmark("https://x/dead")];
    let pass = FakePass(|bookmark: &Bookmark| {
        if bookmark.url.as_str().ends_with("dead") {
            return Err(SourceError::Halted(Halt::Reauth("token expired".into())));
        }
        Ok(Plan::Bookmark(Bookmark {
            title: "New".into(),
            ..unchanged_plan(bookmark)
        }))
    });
    let mut pinboard = FakePinboard::default();
    pinboard.fail_update_urls.insert("https://x/one".into());

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.write_failed, 1);
    let err = outcome.into_result().unwrap_err().to_string();
    assert!(err.contains("token expired"), "{err}");
    assert!(err.contains("1 write(s)"), "{err}");
}

#[tokio::test]
async fn a_reauth_does_not_hide_lookups_that_already_failed() {
    // Ten lookups 5xx'd before the credential died; reporting only the credential
    // would lose the fact that the source was already misbehaving.
    let books = vec![
        bookmark("https://x/bad"),
        bookmark("https://x/dead"),
        bookmark("https://x/never"),
    ];
    let pass = FakePass(|bookmark: &Bookmark| {
        if bookmark.url.as_str().ends_with("dead") {
            return Err(SourceError::Halted(Halt::Reauth("token expired".into())));
        }
        Err(item_error())
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.plan_failed, 1);
    let err = outcome.into_result().unwrap_err().to_string();
    assert!(err.contains("token expired"), "{err}");
    assert!(err.contains("1 lookup(s)"), "{err}");
}

#[tokio::test]
async fn bookmarks_a_pass_skipped_do_not_count_as_reaching_the_source() {
    // A skipped bookmark never touched the source, so it must not dilute the ratio:
    // `cleanup github` skips deep links without an API call, and those must not vouch for a
    // source that is actually down. Two failures, to clear the one-failure floor and
    // isolate what's being tested here.
    let books = vec![
        bookmark("https://x/not-mine-1"),
        bookmark("https://x/not-mine-2"),
        bookmark("https://x/not-mine-3"),
        bookmark("https://x/mine-1"),
        bookmark("https://x/mine-2"),
    ];
    let pass = FakePass(|bookmark: &Bookmark| {
        if bookmark.url.as_str().contains("not-mine") {
            Ok(Plan::Skipped)
        } else {
            Err(item_error())
        }
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.reached, 0);
    // "2 of 2", not "2 of 5": the three skipped bookmarks are not in the denominator,
    // which is what stops them padding it into a passing ratio.
    let err = outcome.into_result().unwrap_err();
    assert!(err.to_string().contains("2 of 2"), "{err}");
}

#[tokio::test]
async fn an_unchanged_plan_counts_as_reaching_the_source() {
    // The pass looked this one up and found nothing to do -- that answer proves the
    // source is up, so a separate failure is just one bad link.
    let books = vec![bookmark("https://x/looked-up"), bookmark("https://x/bad")];
    let pass = FakePass(|bookmark: &Bookmark| {
        if bookmark.url.as_str().contains("bad") {
            Err(item_error())
        } else {
            Ok(Plan::Unchanged)
        }
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.reached, 1);
    outcome.into_result().unwrap();
}

#[tokio::test]
async fn none_plan_is_skipped() {
    let books = vec![bookmark("https://x/")];
    let pass = FakePass(|_: &Bookmark| Ok(Plan::Skipped));
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.plan_failed + outcome.write_failed, 0);
    assert!(
        pinboard.updated.borrow().is_empty(),
        "{:?}",
        pinboard.updated.borrow()
    );
    assert!(
        pinboard.deleted.borrow().is_empty(),
        "{:?}",
        pinboard.deleted.borrow()
    );
}

#[tokio::test]
async fn unchanged_plan_is_skipped() {
    let books = vec![bookmark("https://x/")];
    let pass = FakePass(|bookmark: &Bookmark| Ok(Plan::Bookmark(unchanged_plan(bookmark))));
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.plan_failed + outcome.write_failed, 0);
    assert!(
        pinboard.updated.borrow().is_empty(),
        "{:?}",
        pinboard.updated.borrow()
    );
}

#[tokio::test]
async fn url_change_updates_and_deletes_old_preserving_privacy() {
    let books = vec![bookmark("https://old/")];
    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            url: u("https://new/"),
            title: "New".into(),
            tags: tag_list(&["x"]),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.plan_failed + outcome.write_failed, 0);
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://new/");
    // shared/toread are carried over from the stored bookmark (both off).
    assert!(!updated[0].shared && !updated[0].toread);
    // The old URL is deleted after the rewrite.
    assert_eq!(*pinboard.deleted.borrow(), strings(&["https://old/"]));
}

#[tokio::test]
async fn colliding_rewrites_are_field_merged() {
    // Two stored bookmarks whose plans land on the same URL: A normalizes onto B's URL
    // and B stays put. The driver must field-merge them into one record at that URL and
    // delete A's old URL, rather than clobbering either.
    let mut stored_a = bookmark("https://old-a/");
    stored_a.tags = tag_list(&["x"]);
    stored_a.note = "from A".into();
    let mut stored_b = bookmark("https://collide/");
    stored_b.tags = tag_list(&["y"]);
    stored_b.note = "from B".into();
    let books = vec![stored_a, stored_b];

    let pass = FakePass(|bookmark: &Bookmark| {
        if bookmark.url.as_str().contains("old-a") {
            Ok(Plan::Bookmark(Bookmark {
                url: u("https://collide/"),
                ..unchanged_plan(bookmark)
            }))
        } else {
            Ok(Plan::Bookmark(unchanged_plan(bookmark)))
        }
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.plan_failed + outcome.write_failed, 0);

    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://collide/");
    // B leads the union, not A: B is the record that stays at this URL, so it is the
    // base the others extend. A led only because it happened to come first in the
    // slice, which is an ordering the user has no way to see or control.
    assert_eq!(updated[0].tags, strings(&["y", "x"]));
    assert_eq!(updated[0].extended, "from B\n\nfrom A");
    // Only A's old URL is absorbed; the shared target is not deleted.
    assert_eq!(*pinboard.deleted.borrow(), strings(&["https://old-a/"]));
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
            Ok(Plan::Bookmark(Bookmark {
                url: u(next),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard::default();

        let outcome = run(&pinboard, &books, &pass).await;
        assert_eq!(outcome.plan_failed + outcome.write_failed, 0);

        let updated = pinboard.updated.borrow();
        let mut written: Vec<&str> = updated.iter().map(|c| c.url.as_str()).collect();
        written.sort_unstable();
        assert_eq!(written, vec!["https://collide-b/", "https://new-z/"]);

        let deleted = pinboard.deleted.borrow();
        // old-a is deleted (no other write targets it); collide-b is X's target and
        // must survive Y's rewrite.
        assert!(
            deleted.contains(&"https://old-a/".to_string()),
            "{deleted:?}"
        );
        assert!(
            !deleted.contains(&"https://collide-b/".to_string()),
            "{deleted:?}"
        );
    }

    let x = bookmark("https://old-a/");
    let y = bookmark("https://collide-b/");
    run_in_order(vec![x.clone(), y.clone()]).await;
    run_in_order(vec![y, x]).await;
}

#[tokio::test]
async fn a_refused_rewrite_leaves_its_own_record_protected() {
    // C's lookup fails, so C is untouchable. A wants C's URL and is refused -- meaning
    // A stays put, so A's own URL is occupied after all, and B's rewrite onto it would
    // replace the very record A's refusal just saved. The protection has to propagate.
    let mut a = bookmark("https://x/a");
    a.note = "A's notes".into();
    let b = bookmark("https://x/b");
    let c = bookmark("https://x/c");
    let slice = vec![a.clone(), b.clone(), c.clone()];
    let all = vec![a, b, c];

    let pass = FakePass(|bookmark: &Bookmark| {
        let target = match bookmark.url.as_str() {
            url if url.ends_with("/a") => "https://x/c",
            url if url.ends_with("/b") => "https://x/a",
            _ => return Err(item_error()),
        };
        Ok(Plan::Bookmark(Bookmark {
            url: u(target),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run_pass(
        &store(&pinboard, &all, WriteMode::Live),
        &slice,
        "test",
        NO_DATING,
        &pass,
    )
    .await;
    assert_eq!(outcome.refused, 2, "both rewrites must be refused");
    assert!(
        pinboard.updated.borrow().is_empty(),
        "A's record must survive its own refusal: {:?}",
        pinboard.updated.borrow()
    );
}

#[tokio::test]
async fn a_matching_public_group_merges_and_stays_public() {
    // The mirror of the refusal: when every member agrees with the resident, the merge
    // runs and neither flag moves. This is what lets the converged-state guard use
    // `diff`, which ignores `public`/`read_later` -- those two cannot differ here, so
    // there is nothing for it to be blind to.
    let mut mover = bookmark("https://x/mine");
    mover.public = true;
    mover.read_later = true;
    mover.note = "shared annotation".into();
    let mut resident = bookmark("https://other/resident");
    resident.public = true;
    resident.read_later = false;
    resident.note = "resident note".into();
    let slice = vec![mover.clone()];
    let all = vec![mover, resident];

    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            url: u("https://other/resident"),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run_pass(
        &store(&pinboard, &all, WriteMode::Live),
        &slice,
        "test",
        NO_DATING,
        &pass,
    )
    .await;
    assert_eq!(outcome.refused, 0);
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert!(
        updated[0].extended.contains("shared annotation"),
        "{:?}",
        updated[0].extended
    );
    assert!(updated[0].shared, "a matching group must stay public");
    assert!(
        !updated[0].toread,
        "the resident's reading state survives the merge"
    );
}

#[tokio::test]
async fn a_converged_check_uses_the_incumbents_stored_record_not_its_plan() {
    // The guard asks "does the account already hold what this merge produces?", so it
    // has to compare the *stored* record. Comparing the incumbent's plan instead makes
    // the merge look converged the moment it equals its own plan -- which it does by
    // construction -- and the incumbent's pending change is silently dropped.
    let mut sitting = bookmark("https://collide/");
    sitting.title = "stale".into();
    let mover = bookmark("https://x/mover");
    let other = bookmark("https://x/other");
    let books = vec![sitting, mover, other];

    let pass = FakePass(|bookmark: &Bookmark| {
        let target = match bookmark.url.as_str() {
            // Keeps the mover's URL a target, so it can't be absorbed and `old_urls`
            // stays empty -- the only way to reach the guard.
            "https://x/other" => "https://x/mover",
            _ => "https://collide/",
        };
        Ok(Plan::Bookmark(Bookmark {
            url: u(target),
            title: "fresh".into(),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.write_failed, 0);
    let updated = pinboard.updated.borrow();
    let merged = updated
        .iter()
        .find(|call| call.url == "https://collide/")
        .expect("the incumbent's own change must still be written");
    assert_eq!(merged.description, "fresh");
}

#[tokio::test]
async fn a_converged_incumbent_merge_is_not_rewritten_every_run() {
    // The incumbent half of the converged-state guard. The bookmark at the target
    // already holds everything the merge produces, and the mover's old URL is another
    // plan's target so it can't be absorbed -- so the group re-merges every run and must
    // recognise that there is nothing left to write.
    let mut sitting = bookmark("https://collide/");
    sitting.note = "settled".into();
    let mover = bookmark("https://x/mover");
    let other = bookmark("https://x/other");
    let books = vec![sitting, mover, other];

    let pass = FakePass(|bookmark: &Bookmark| {
        let target = match bookmark.url.path() {
            // `other` heads for the mover's URL, which keeps the mover's URL a target
            // and so protects it from being deleted as absorbed.
            "/other" => "https://x/mover",
            _ => "https://collide/",
        };
        Ok(Plan::Bookmark(Bookmark {
            url: u(target),
            note: "settled".into(),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.write_failed, 0);
    assert_eq!(
        outcome.refused, 0,
        "nothing here is refused — it is settled"
    );
    assert!(
        !pinboard
            .updated
            .borrow()
            .iter()
            .any(|call| call.url == "https://collide/"),
        "a converged incumbent merge must not rewrite itself every run"
    );
}

#[tokio::test]
async fn the_view_never_drifts_from_the_account() {
    // The view is what every later pass plans against, and nothing ever checks it back
    // against Pinboard mid-run. So a missed, doubled or mis-ordered recording raises no
    // error at all -- it surfaces one pass later as a plan built on an account that never
    // existed. Run a mixed workload twice over one store and assert the view still
    // equals the account rebuilt from the writes that actually landed.
    let seed = vec![
        bookmark("https://a/old"),
        bookmark("https://b/mover"),
        bookmark("https://r/resident"),
        bookmark("https://c/mover"),
        bookmark("https://d/blocked"),
        bookmark("https://f/fail"),
        bookmark("https://e/mover"),
    ];
    // Two different passes, as `cleanup --all` really runs: the second plans against
    // what the first left, and reuses a URL the first retired.
    let first_pass = FakePass(|bookmark: &Bookmark| {
        let target = match bookmark.url.as_str() {
            // A plain rewrite, whose old URL is deleted.
            "https://a/old" => "https://a/new",
            // A merge onto a resident, absorbing the mover's URL.
            "https://b/mover" => "https://r/resident",
            // A rewrite refused because its target could not be read.
            "https://c/mover" => "https://d/blocked",
            // A write Pinboard rejects, so the view must *not* record it.
            "https://f/fail" => "https://f/moved",
            "https://d/blocked" => return Err(item_error()),
            _ => return Ok(Plan::Skipped),
        };
        Ok(Plan::Bookmark(Bookmark {
            url: u(target),
            ..unchanged_plan(bookmark)
        }))
    });
    let second_pass = FakePass(|bookmark: &Bookmark| {
        let target = match bookmark.url.as_str() {
            "https://a/new" => "https://a/newer",
            // Onto the URL the first pass retired -- a real `--all` shape, and the one
            // that catches a delete which drops the record but leaves its slot behind.
            "https://e/mover" => "https://a/old",
            _ => return Ok(Plan::Skipped),
        };
        Ok(Plan::Bookmark(Bookmark {
            url: u(target),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard {
        fail_update_urls: ["https://f/moved".to_string()].into_iter().collect(),
        ..FakePinboard::default()
    };
    let store = store(&pinboard, &seed, WriteMode::Live);

    // The resident is held out of the first slice, so `b/mover` is a genuine resident
    // merge rather than a collision between two plans.
    let first: Vec<Bookmark> = seed
        .iter()
        .filter(|b| b.url.as_str() != "https://r/resident")
        .cloned()
        .collect();
    let first_outcome = run_pass(&store, &first, "test", NO_DATING, &first_pass).await;
    // The second pass plans against whatever the first left behind -- which is the whole
    // reason the view has to be right.
    let second = store.snapshot();
    let second_outcome = run_pass(&store, &second, "test", NO_DATING, &second_pass).await;

    // Pin the workload: an invariant test that silently stopped exercising these would
    // keep passing while checking nothing.
    assert_eq!(
        (
            first_outcome.changed,
            first_outcome.refused,
            first_outcome.plan_failed,
            first_outcome.write_failed
        ),
        (2, 1, 1, 1),
        "the first pass must make a rewrite, a merge, a refusal, a failed lookup and a \
         rejected write"
    );
    assert_eq!(
        (second_outcome.changed, second_outcome.write_failed),
        (2, 0)
    );
    let ops = pinboard.ops.borrow();
    assert!(
        ops.iter().any(|op| matches!(op, Op::Delete(_))),
        "and deletes have to reach the store, or the replay side is trivial: {ops:?}"
    );
    assert_eq!(
        store.snapshot(),
        crate::test_support::replay(&seed, &ops),
        "the view diverged from the account the writes actually produced"
    );
}

#[tokio::test]
async fn a_mismatched_member_drops_out_while_the_rest_of_the_group_merges() {
    // The point of refusing per member rather than per group: two bookmarks head for the
    // resident, one agreeing about visibility and one not. The agreeing one still merges;
    // only the other is held back. Every other mismatch test leaves a group of one, which
    // takes the single-plan path and so never exercises this at all.
    let mut resident = bookmark("https://other/resident");
    resident.public = true;
    resident.note = "resident note".into();
    resident.tags = tag_list(&["rt"]);
    let mut agreeing = bookmark("https://x/ok");
    agreeing.public = true;
    agreeing.note = "ok note".into();
    agreeing.tags = tag_list(&["okt"]);
    let mut disagreeing = bookmark("https://x/secret");
    disagreeing.public = false;
    disagreeing.note = "SECRET".into();
    disagreeing.tags = tag_list(&["secrett"]);
    let slice = vec![agreeing.clone(), disagreeing.clone()];
    let all = vec![agreeing, disagreeing, resident];

    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            url: u("https://other/resident"),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run_pass(
        &store(&pinboard, &all, WriteMode::Live),
        &slice,
        "test",
        NO_DATING,
        &pass,
    )
    .await;
    assert_eq!(outcome.refused, 1, "only the private one is held back");
    assert_eq!(outcome.changed, 1);
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].extended, "resident note\n\nok note");
    assert_eq!(updated[0].tags, strings(&["rt", "okt"]));
    assert!(updated[0].shared);
    assert!(
        !updated[0].extended.contains("SECRET"),
        "the held-back bookmark's note must not reach a public record"
    );
    assert_eq!(
        *pinboard.deleted.borrow(),
        strings(&["https://x/ok"]),
        "only the bookmark that actually merged is absorbed"
    );
}

#[tokio::test]
async fn refusals_propagate_through_a_chain_not_just_one_hop() {
    // `propagate_refusals` is a fixpoint, and a single iteration is indistinguishable
    // from one unless the chain is at least two deep. Blocked is unreadable, so C is
    // refused there, so C's URL is occupied, so B is refused there, so B's URL is
    // occupied, so A is refused there. One pass of the loop settles only the first hop.
    let books = vec![
        bookmark("https://blocked/"),
        bookmark("https://c/"),
        bookmark("https://b/"),
        bookmark("https://a/"),
    ];
    let pass = FakePass(|bookmark: &Bookmark| {
        let target = match bookmark.url.as_str() {
            "https://blocked/" => return Err(item_error()),
            "https://c/" => "https://blocked/",
            "https://b/" => "https://c/",
            _ => "https://b/",
        };
        Ok(Plan::Bookmark(Bookmark {
            url: u(target),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.refused, 3, "every hop of the chain must be refused");
    assert!(
        pinboard.updated.borrow().is_empty(),
        "no link in the chain moved, so nothing may be written over any of them"
    );
}

#[tokio::test]
async fn a_halt_protects_the_bookmark_whose_lookup_raised_it() {
    // The halting bookmark is itself unread -- the lookup that raised the halt is the one
    // that failed -- so the protected range has to start at it, not after it.
    // The mover is planned first, so it forms a group targeting the halting bookmark's
    // URL; the halt then has to protect that URL from it.
    let books = vec![bookmark("https://mover/"), bookmark("https://halts/")];
    let pass = FakePass(|bookmark: &Bookmark| match bookmark.url.as_str() {
        "https://halts/" => Err(SourceError::Halted(Halt::Reauth("cookie expired".into()))),
        _ => Ok(Plan::Bookmark(Bookmark {
            url: u("https://halts/"),
            ..unchanged_plan(bookmark)
        })),
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert!(outcome.halted.is_some());
    assert_eq!(outcome.refused, 1);
    assert!(
        pinboard.updated.borrow().is_empty(),
        "the halting bookmark was never read, so nothing may be written over it"
    );
}

#[tokio::test]
async fn an_unchanged_bookmark_is_merged_with_not_refused() {
    // `Plan::Unchanged` means the source answered and the record is current, so a
    // rewrite landing on it merges -- the same as `Plan::Skipped`. Treating it as
    // unreadable instead would refuse HN's commonest answer (`search_by_url` finding no
    // discussion) and leave the account unconverged.
    let mut settled = bookmark("https://target/");
    settled.note = "settled note".into();
    let mover = bookmark("https://mover/");
    let books = vec![settled, mover];

    let pass = FakePass(|bookmark: &Bookmark| match bookmark.url.as_str() {
        "https://target/" => Ok(Plan::Unchanged),
        _ => Ok(Plan::Bookmark(Bookmark {
            url: u("https://target/"),
            note: "incoming".into(),
            ..unchanged_plan(bookmark)
        })),
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.refused, 0);
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(
        updated[0].extended, "settled note\n\nincoming",
        "the unchanged bookmark stays and leads the merge"
    );
}

#[tokio::test]
async fn a_merge_whose_delete_fails_reports_the_write_that_landed() {
    // The merge's half-applied arm: the record is written and an absorbed URL survives.
    // Saying "merging into {target} failed" would send the operator after a record that
    // is actually fine.
    let mut resident = bookmark("https://target/");
    resident.note = "resident".into();
    let mut mover = bookmark("https://mover/");
    mover.note = "mover".into();
    let slice = vec![mover.clone()];
    let all = vec![mover, resident];

    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            url: u("https://target/"),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard {
        fail_delete_urls: ["https://mover/".to_string()].into_iter().collect(),
        ..FakePinboard::default()
    };

    let outcome = run_pass(
        &store(&pinboard, &all, WriteMode::Live),
        &slice,
        "test",
        NO_DATING,
        &pass,
    )
    .await;
    assert_eq!(
        outcome.write_failed, 1,
        "the stranded URL still fails the run"
    );
    assert_eq!(
        outcome.changed, 1,
        "but the merge landed, so it counts as a change — as the rewrite path does"
    );
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1, "and the merged record did land");
    assert_eq!(updated[0].extended, "resident\n\nmover");
}

#[tokio::test]
async fn a_plan_can_never_change_the_privacy_flags() {
    // The half of the invariant that makes `Bookmark::diff`'s blindness to these two
    // fields safe. `diff` ignores them *because* the driver forces them from the stored
    // record -- so if the forcing goes, a plan's flags are written with `replace=yes`
    // and the dry-run doesn't even render the change. Every other test builds its plan
    // with `unchanged_plan`, which clones the stored flags and so can't catch this.
    let mut stored = bookmark("https://x/keep");
    stored.public = false;
    stored.read_later = true;
    let books = vec![stored];

    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            title: "changed".into(),
            public: true,
            read_later: false,
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.changed, 1);
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].description, "changed");
    assert!(
        !updated[0].shared,
        "a plan must not publish a private bookmark"
    );
    assert!(
        updated[0].toread,
        "nor clear a to-read flag the user had set"
    );
}

#[tokio::test]
async fn a_rewrite_whose_delete_fails_still_counts_as_written() {
    // The rewrite landed at the new URL and only the old-URL delete failed, so the
    // account did change. Reporting it as a plain failure under-counts a write that
    // really happened and points the operator at the old URL, where nothing is wrong.
    let books = vec![bookmark("https://old/")];
    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            url: u("https://new/"),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard {
        fail_delete_urls: ["https://old/".to_string()].into_iter().collect(),
        ..FakePinboard::default()
    };

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(pinboard.updated.borrow().len(), 1, "the write did land");
    assert_eq!(outcome.changed, 1, "so it must be counted as a change");
    assert_eq!(outcome.write_failed, 1, "and still fail the run");
}

#[tokio::test]
async fn a_failed_merge_protects_a_member_another_group_targets() {
    // The merge fails, so M never left `m/`. G is heading there. Protecting only the
    // URLs the merge would have *absorbed* misses M entirely -- those are exactly the
    // URLs no other group targets, so the protection covers only cases that were never
    // in danger, while the one case that loses data is filtered out of it.
    let mut stranded = bookmark("https://m/");
    stranded.title = "M original".into();
    let other = bookmark("https://n/");
    let taker = bookmark("https://g/");
    let books = vec![stranded, other, taker];

    let pass = FakePass(|bookmark: &Bookmark| {
        let target = match bookmark.url.as_str() {
            "https://g/" => "https://m/",
            _ => "https://collide/",
        };
        Ok(Plan::Bookmark(Bookmark {
            url: u(target),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard {
        fail_update_urls: ["https://collide/".to_string()].into_iter().collect(),
        ..FakePinboard::default()
    };

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.write_failed, 1);
    assert!(
        !pinboard
            .updated
            .borrow()
            .iter()
            .any(|call| call.url == "https://m/"),
        "a record stranded by a failed merge must not be overwritten"
    );
}

#[tokio::test]
async fn a_failed_write_refusal_propagates_like_any_other() {
    // A's move fails, so `a/` is occupied and P1+P2 are refused there -- which means P1
    // and P2 never moved either, and H1/H2 are heading for *their* URLs. Without
    // re-closing the refusal set, protecting A costs two records instead of saving one:
    // strictly worse than not protecting it at all.
    let books = vec![
        bookmark("https://a/"),
        bookmark("https://p1/"),
        bookmark("https://p2/"),
        bookmark("https://h1/"),
        bookmark("https://h2/"),
    ];

    let pass = FakePass(|bookmark: &Bookmark| {
        let target = match bookmark.url.as_str() {
            "https://a/" => "https://b/",
            "https://p1/" | "https://p2/" => "https://a/",
            "https://h1/" => "https://p1/",
            _ => "https://p2/",
        };
        Ok(Plan::Bookmark(Bookmark {
            url: u(target),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard {
        fail_update_urls: ["https://b/".to_string()].into_iter().collect(),
        ..FakePinboard::default()
    };

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.write_failed, 1);
    let updated = pinboard.updated.borrow();
    for stranded in ["https://p1/", "https://p2/"] {
        assert!(
            !updated.iter().any(|call| call.url == stranded),
            "{stranded} never moved, so nothing may be written over it"
        );
    }
}

#[tokio::test]
async fn a_failed_move_protects_the_record_it_left_behind() {
    // Y fails to move to its new URL, so its record is still sitting at the old one.
    // A is heading there. Refusals are settled before any write, so nothing had marked
    // that URL -- and `replace=yes` would overwrite Y with A, losing Y from the account
    // entirely: never written to its new URL, and clobbered at its old one. With A first
    // in the snapshot its group appears first, and only the write order keeps A from
    // landing before Y has tried to leave.
    async fn run_in_order(books: Vec<Bookmark>) {
        let pass = FakePass(|bookmark: &Bookmark| {
            let target = match bookmark.url.as_str() {
                "https://collide/" => "https://elsewhere/",
                _ => "https://collide/",
            };
            Ok(Plan::Bookmark(Bookmark {
                url: u(target),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard {
            fail_update_urls: ["https://elsewhere/".to_string()].into_iter().collect(),
            ..FakePinboard::default()
        };

        let outcome = run(&pinboard, &books, &pass).await;
        assert_eq!(outcome.write_failed, 1);
        assert_eq!(
            outcome.refused, 1,
            "the URL the failed move left occupied must refuse the next rewrite"
        );
        assert!(
            !pinboard
                .updated
                .borrow()
                .iter()
                .any(|call| call.url == "https://collide/"),
            "a record stranded by a failed write must not be overwritten"
        );
    }

    let mut stranded = bookmark("https://collide/");
    stranded.title = "Y original".into();
    let mover = bookmark("https://a/");
    run_in_order(vec![stranded.clone(), mover.clone()]).await;
    run_in_order(vec![mover, stranded]).await;
}

/// A chain of three rewrites, c -> b -> a -> elsewhere, listed so that each group appears
/// before the group that moves its target's occupant away.
fn chain_of_three() -> (Vec<Bookmark>, impl CleanupPass) {
    let books = vec![
        bookmark("https://c/"),
        bookmark("https://b/"),
        bookmark("https://a/"),
    ];
    let pass = FakePass(|bookmark: &Bookmark| {
        let target = match bookmark.url.as_str() {
            "https://c/" => "https://b/",
            "https://b/" => "https://a/",
            _ => "https://elsewhere/",
        };
        Ok(Plan::Bookmark(Bookmark {
            url: u(target),
            ..unchanged_plan(bookmark)
        }))
    });
    (books, pass)
}

#[tokio::test]
async fn a_chain_is_written_from_its_far_end() {
    let (books, pass) = chain_of_three();
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.changed, 3);
    let written: Vec<String> = pinboard
        .updated
        .borrow()
        .iter()
        .map(|call| call.url.clone())
        .collect();
    assert_eq!(
        written,
        strings(&["https://elsewhere/", "https://a/", "https://b/"]),
        "each URL must be vacated before a record is written over it"
    );
}

#[tokio::test]
async fn a_failed_merge_protects_every_url_its_members_left() {
    // x1 and x2 merge into k, and z1 and z2 head for x1 and x2. The merge must be written
    // before either, so its failure can refuse both. Each order of the movers puts a
    // different member's URL first, so both members must count as leaving.
    async fn run_in_order(books: Vec<Bookmark>) {
        let pass = FakePass(|bookmark: &Bookmark| {
            let target = match bookmark.url.as_str() {
                "https://z1/" => "https://x1/",
                "https://z2/" => "https://x2/",
                _ => "https://k/",
            };
            Ok(Plan::Bookmark(Bookmark {
                url: u(target),
                ..unchanged_plan(bookmark)
            }))
        });
        let pinboard = FakePinboard {
            fail_update_urls: ["https://k/".to_string()].into_iter().collect(),
            ..FakePinboard::default()
        };

        let outcome = run(&pinboard, &books, &pass).await;
        assert_eq!(outcome.write_failed, 1);
        assert_eq!(
            outcome.refused, 2,
            "both rewrites onto the members are refused"
        );
        assert!(
            pinboard.updated.borrow().is_empty(),
            "nothing may be written over a record that did not move"
        );
    }

    let [z1, z2, x1, x2] =
        ["https://z1/", "https://z2/", "https://x1/", "https://x2/"].map(bookmark);
    run_in_order(vec![z1.clone(), z2.clone(), x1.clone(), x2.clone()]).await;
    run_in_order(vec![z2, z1, x1, x2]).await;
}

#[tokio::test]
async fn a_swap_still_writes_both_bookmarks() {
    // A cycle has no safe order, but it must still be written, once each, and neither
    // URL deleted: each is the other's target.
    let books = vec![bookmark("https://x/"), bookmark("https://y/")];
    let pass = FakePass(|bookmark: &Bookmark| {
        let target = match bookmark.url.as_str() {
            "https://x/" => "https://y/",
            _ => "https://x/",
        };
        Ok(Plan::Bookmark(Bookmark {
            url: u(target),
            title: format!("from {}", bookmark.url),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.changed, 2);
    let mut written: Vec<String> = pinboard
        .updated
        .borrow()
        .iter()
        .map(|call| call.url.clone())
        .collect();
    written.sort_unstable();
    assert_eq!(written, strings(&["https://x/", "https://y/"]));
    assert!(
        pinboard.deleted.borrow().is_empty(),
        "{:?}",
        pinboard.deleted.borrow()
    );
}

#[tokio::test]
async fn a_failed_move_protects_every_link_of_the_chain_behind_it() {
    // a fails to leave, so b must not land on it; b then never leaves, so c must not land
    // on b. In first-appearance order c and b were both written before a even tried, and
    // a's failure then lost a and b.
    let (books, pass) = chain_of_three();
    let pinboard = FakePinboard {
        fail_update_urls: ["https://elsewhere/".to_string()].into_iter().collect(),
        ..FakePinboard::default()
    };

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.write_failed, 1);
    assert_eq!(
        outcome.refused, 2,
        "both rewrites behind the failure are refused"
    );
    assert!(
        pinboard.updated.borrow().is_empty(),
        "nothing may be written over a record that did not move"
    );
}

#[tokio::test]
async fn a_visibility_mismatch_refuses_only_the_mover() {
    // The record that stays usually has a plan of its own, and that plan has nothing to
    // do with the mover's visibility. Refusing the whole group would let one mismatched
    // duplicate freeze an unrelated bookmark's cleanup for good -- every run, with no
    // way out short of the user editing a visibility by hand.
    let mut sitting = bookmark("https://github.com/new/name");
    sitting.public = false;
    sitting.title = "stale".into();
    let mut mover = bookmark("https://github.com/old/name");
    mover.public = true;
    let target = sitting.url.clone();
    let books = vec![mover, sitting];
    let pass = FakePass(move |bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            url: target.clone(),
            title: "fresh".into(),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.refused, 1, "only the mover is refused");
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://github.com/new/name");
    assert_eq!(
        updated[0].description, "fresh",
        "the bookmark that stays still gets its own cleanup"
    );
    assert!(
        !updated[0].shared,
        "and it is not merged with, so its visibility is untouched"
    );
    assert!(
        pinboard.deleted.borrow().is_empty(),
        "the refused mover must not be absorbed"
    );
}

#[tokio::test]
async fn a_visibility_mismatch_refuses_instead_of_merging() {
    // Merging a private mover into a public resident has no safe answer: keeping the
    // resident public republishes the mover's private annotation, and narrowing it
    // silently unpublishes a bookmark the user chose to share. Refuse instead, and
    // leave both records exactly as they are.
    let mut mover = bookmark("https://x/mine");
    mover.public = false;
    mover.note = "private annotation".into();
    let mut resident = bookmark("https://other/resident");
    resident.public = true;
    let slice = vec![mover.clone()];
    let all = vec![mover, resident];

    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            url: u("https://other/resident"),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run_pass(
        &store(&pinboard, &all, WriteMode::Live),
        &slice,
        "test",
        NO_DATING,
        &pass,
    )
    .await;
    assert_eq!(outcome.refused, 1);
    assert_eq!(outcome.changed, 0);
    assert!(
        pinboard.updated.borrow().is_empty(),
        "a mismatched merge must write nothing at all"
    );
    assert!(
        pinboard.deleted.borrow().is_empty(),
        "{:?}",
        pinboard.deleted.borrow()
    );
}

#[tokio::test]
async fn a_visibility_mismatch_protects_the_movers_own_url() {
    // The refused mover stays at its own URL, so a second rewrite heading there would
    // overwrite the record the refusal just preserved. The mismatch has to be settled
    // before the occupied-by-refused fixpoint, not after it.
    let mut mover = bookmark("https://x/mine");
    mover.public = false;
    let mut resident = bookmark("https://other/resident");
    resident.public = true;
    let second = bookmark("https://x/second");
    let slice = vec![mover.clone(), second.clone()];
    let all = vec![mover, second, resident];

    let pass = FakePass(|bookmark: &Bookmark| {
        let target = match bookmark.url.path() {
            "/mine" => "https://other/resident",
            _ => "https://x/mine",
        };
        Ok(Plan::Bookmark(Bookmark {
            url: u(target),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run_pass(
        &store(&pinboard, &all, WriteMode::Live),
        &slice,
        "test",
        NO_DATING,
        &pass,
    )
    .await;
    assert_eq!(outcome.refused, 2);
    assert!(
        pinboard.updated.borrow().is_empty(),
        "the mover's own URL is still occupied, so nothing may be written there"
    );
}

#[tokio::test]
async fn merging_does_not_reset_a_residents_unparseable_date() {
    // A stored date that wouldn't parse reads back as `None`, and writing `None` omits
    // `dt` -- under `replace=yes` Pinboard then dates the record to today. Keep the
    // date the merge found rather than losing the record's age.
    let mut mover = bookmark("https://x/mine");
    mover.timestamp = crate::timefmt::from_unix(1_000_000);
    let mut outsider = bookmark("https://other/resident");
    outsider.timestamp = None;
    let slice = vec![mover.clone()];
    let all = vec![mover, outsider];

    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            url: u("https://other/resident"),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run_pass(
        &store(&pinboard, &all, WriteMode::Live),
        &slice,
        "test",
        NO_DATING,
        &pass,
    )
    .await;
    assert_eq!(outcome.refused, 0);
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert!(
        !updated[0].dt.is_empty(),
        "an empty dt lets Pinboard re-date the record to now"
    );
}

#[tokio::test]
async fn merging_a_resident_keeps_its_read_later_and_date() {
    // The resident is the record that survives at this URL, so its own reading state
    // and stored date stay put: `merge_bookmarks`'s any()/earliest rules are for two
    // plans, and with dating off nothing may re-date a bookmark at all.
    let mut mover = bookmark("https://x/mine");
    mover.read_later = true;
    mover.timestamp = crate::timefmt::from_unix(1_000_000);
    let mut outsider = bookmark("https://other/resident");
    outsider.read_later = false;
    outsider.timestamp = crate::timefmt::from_unix(1_700_000_000);
    let slice = vec![mover.clone()];
    let all = vec![mover, outsider];

    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            url: u("https://other/resident"),
            title: "New".into(),
            // Something the merge adds, so the resident is written rather than only
            // absorbing the mover.
            tags: tag_list(&["a", "b", "moved"]),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run_pass(
        &store(&pinboard, &all, WriteMode::Live),
        &slice,
        "test",
        NO_DATING,
        &pass,
    )
    .await;
    assert_eq!(outcome.refused, 0);
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert!(!updated[0].toread, "must not pick up the mover's to-read");
    assert_eq!(
        updated[0].dt,
        crate::timefmt::to_rfc3339(crate::timefmt::from_unix(1_700_000_000).unwrap()).unwrap(),
        "must not be backdated to the mover's date"
    );
}

#[tokio::test]
async fn a_converged_merge_is_not_rewritten_every_run() {
    // The resident already holds exactly what the merge produces and there is nothing
    // left to absorb (the mover's old URL is another plan's target, so it is not
    // deleted), so the pass must leave it alone rather than rewriting it forever.
    let mut resident = bookmark("https://collide/");
    resident.note = "settled".into();
    let mover = bookmark("https://x/mover");
    let other = bookmark("https://x/other");
    let slice = vec![mover.clone(), other.clone()];
    let all = vec![resident.clone(), mover, other];

    let pass = FakePass(move |bookmark: &Bookmark| {
        if bookmark.url.as_str().ends_with("/mover") {
            // Plans exactly the resident's content onto the resident's URL.
            return Ok(Plan::Bookmark(Bookmark {
                url: u("https://collide/"),
                note: "settled".into(),
                ..unchanged_plan(bookmark)
            }));
        }
        // Targets the mover's URL, so the mover's URL is never absorbed.
        Ok(Plan::Bookmark(Bookmark {
            url: u("https://x/mover"),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run_pass(
        &store(&pinboard, &all, WriteMode::Live),
        &slice,
        "test",
        NO_DATING,
        &pass,
    )
    .await;
    let updated = pinboard.updated.borrow();
    let written: Vec<&str> = updated.iter().map(|c| c.url.as_str()).collect();
    assert!(
        !written.contains(&"https://collide/"),
        "the settled record must not be rewritten: {written:?}"
    );
    assert_eq!(outcome.refused, 0);
}

#[tokio::test]
async fn a_bookmark_is_not_a_resident_of_its_own_plan() {
    // Reading residency from a live view makes a bookmark that stays at its own URL
    // find *itself* sitting there. Merging with itself would union its stored record
    // back into the plan and resurrect exactly what the plan was computed to remove.
    let mut stored = bookmark("https://x/mine");
    stored.tags = tag_list(&["keep", "drop-me"]);
    stored.note = "stale note".into();
    let books = vec![stored];

    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            tags: tag_list(&["keep"]),
            note: "fresh note".into(),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.refused, 0);
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert!(
        !updated[0].tags.contains(&"drop-me".to_string()),
        "the plan dropped this tag; merging with itself would bring it back: {updated:?}"
    );
    assert_eq!(
        updated[0].extended, "fresh note",
        "the stale note must not be merged back in"
    );
}

#[tokio::test]
async fn rewrite_onto_a_resident_outside_the_slice_merges_with_it() {
    // A plan can target a URL its own filter would have excluded -- HackerNews
    // rewrites a story bookmark to its article -- so the resident sitting there is
    // invisible to a residency check that only looks at the slice. Pinboard keys on
    // URL, so the end state is one record: merge, keeping what the resident had.
    let mut mover = bookmark("https://x/mine");
    mover.note = "from the mover".into();
    mover.tags = tag_list(&["moved"]);
    let mut outsider = bookmark("https://other/resident");
    outsider.note = "hand written".into();
    outsider.tags = tag_list(&["keepme"]);
    outsider.title = "Resident title".into();
    let slice = vec![mover.clone()];
    let all = vec![mover, outsider];

    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            url: u("https://other/resident"),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run_pass(
        &store(&pinboard, &all, WriteMode::Live),
        &slice,
        "test",
        NO_DATING,
        &pass,
    )
    .await;
    assert_eq!(outcome.refused, 0);
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://other/resident");
    // The resident goes into the merge first, so its title survives and its notes and
    // tags are kept alongside the mover's rather than replaced by them.
    assert_eq!(updated[0].description, "Resident title");
    assert!(
        updated[0].extended.contains("hand written"),
        "{:?}",
        updated[0].extended
    );
    assert!(
        updated[0].extended.contains("from the mover"),
        "{:?}",
        updated[0].extended
    );
    assert!(
        updated[0].tags.contains(&"keepme".to_string()),
        "{:?}",
        updated[0].tags
    );
    assert!(
        updated[0].tags.contains(&"moved".to_string()),
        "{:?}",
        updated[0].tags
    );
    // The mover's old URL is absorbed, so a later run sees one bookmark.
    assert_eq!(*pinboard.deleted.borrow(), strings(&["https://x/mine"]));
}

#[tokio::test]
async fn a_rewrite_onto_an_unoccupied_url_still_goes_through() {
    // The other half of the guard: protecting residents must not block the ordinary
    // rewrite onto a URL nobody has bookmarked.
    let mover = bookmark("https://x/mine");
    let slice = vec![mover.clone()];
    let all = vec![mover];

    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            url: u("https://other/vacant"),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run_pass(
        &store(&pinboard, &all, WriteMode::Live),
        &slice,
        "test",
        NO_DATING,
        &pass,
    )
    .await;
    assert_eq!(outcome.refused, 0);
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://other/vacant");
    assert_eq!(*pinboard.deleted.borrow(), strings(&["https://x/mine"]));
}

#[tokio::test]
async fn rewrite_onto_skipped_resident_merges_with_it() {
    // A is skipped (it stays put at T with its own record); B rewrites onto T. A being
    // skipped means its stored record is current, so B's plan merges into it rather
    // than replacing A's title/note/tags -- and B's old URL is absorbed.
    let mut stored_a = bookmark("https://collide/");
    stored_a.tags = tag_list(&["x"]);
    stored_a.note = "from A".into();
    let mut stored_b = bookmark("https://old-b/");
    stored_b.tags = tag_list(&["y"]);
    stored_b.note = "from B".into();
    let books = vec![stored_a, stored_b];

    let pass = FakePass(|bookmark: &Bookmark| {
        if bookmark.url.as_str().contains("collide") {
            Ok(Plan::Skipped)
        } else {
            Ok(Plan::Bookmark(Bookmark {
                url: u("https://collide/"),
                ..unchanged_plan(bookmark)
            }))
        }
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.plan_failed + outcome.write_failed, 0);
    assert_eq!(outcome.refused, 0);
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://collide/");
    assert!(updated[0].extended.contains("from A"), "{updated:?}");
    assert!(updated[0].extended.contains("from B"), "{updated:?}");
    assert!(
        updated[0].tags.contains(&"x".to_string()),
        "{:?}",
        updated[0].tags
    );
    assert!(
        updated[0].tags.contains(&"y".to_string()),
        "{:?}",
        updated[0].tags
    );
    assert_eq!(*pinboard.deleted.borrow(), strings(&["https://old-b/"]));
}

#[tokio::test]
async fn rewrite_onto_a_bookmark_whose_lookup_failed_is_refused() {
    // A failed lookup leaves us not knowing A's end-state, so it is just as much an
    // untouchable resident as a skipped one. Without that, B's rewrite lands on A's
    // URL with replace=yes and destroys A's record -- and now that a failed lookup no
    // longer fails the run, it would do so while reporting success.
    let mut stored_a = bookmark("https://collide/");
    stored_a.note = "hand written".into();
    let stored_b = bookmark("https://old-b/");
    let books = vec![stored_a, stored_b];

    let pass = FakePass(|bookmark: &Bookmark| {
        if bookmark.url.as_str().contains("collide") {
            Err(item_error())
        } else {
            Ok(Plan::Bookmark(Bookmark {
                url: u("https://collide/"),
                ..unchanged_plan(bookmark)
            }))
        }
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.plan_failed, 1);
    assert!(
        pinboard.updated.borrow().is_empty(),
        "must not write over a bookmark whose own lookup failed"
    );
    assert!(
        pinboard.deleted.borrow().is_empty(),
        "{:?}",
        pinboard.deleted.borrow()
    );
}

#[tokio::test]
async fn rewrite_onto_a_bookmark_left_unplanned_by_reauth_is_refused() {
    // Planning stops at the dead credential, so the third bookmark is never planned.
    // It must still be protected: the first bookmark's rewrite targets its URL, and
    // writing there would clobber a record we never even looked at.
    let mover = bookmark("https://old-b/");
    let dead = bookmark("https://dead/");
    let mut resident = bookmark("https://collide/");
    resident.note = "hand written".into();
    let books = vec![mover, dead, resident];

    let pass = FakePass(|bookmark: &Bookmark| {
        if bookmark.url.as_str().contains("dead") {
            return Err(SourceError::Halted(Halt::Reauth("token expired".into())));
        }
        Ok(Plan::Bookmark(Bookmark {
            url: u("https://collide/"),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert!(
        matches!(outcome.halted, Some(Halt::Reauth(_))),
        "{:?}",
        outcome.halted
    );
    assert_eq!(
        outcome.refused, 1,
        "and the rewrite is reported, not just dropped"
    );
    assert!(
        pinboard.updated.borrow().is_empty(),
        "must not write over a bookmark reauth left unplanned"
    );
    assert!(
        pinboard.deleted.borrow().is_empty(),
        "{:?}",
        pinboard.deleted.borrow()
    );
}

/// Two bookmarks in the slice, both planned onto the second one's own URL -- a GitHub
/// repo rename, where the old and new names are separately starred. The bookmark
/// already stored at the target is the record that stays there, exactly like an
/// unplanned resident; being inside the slice is an implementation detail the user
/// can't see, so it must not decide whether their bookmark is protected.
async fn run_rename_collision(
    pinboard: &FakePinboard,
    sitting: Bookmark,
    mover: Bookmark,
) -> PassOutcome {
    let target = sitting.url.clone();
    let books = vec![mover, sitting];
    let pass = FakePass(move |bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            url: target.clone(),
            // A real change, so a test asserting "nothing was written" is asserting the
            // refusal and not merely that the plan was a no-op.
            title: "refreshed".into(),
            ..unchanged_plan(bookmark)
        }))
    });
    run_pass(
        &store(pinboard, &books, WriteMode::Live),
        &books,
        "test",
        NO_DATING,
        &pass,
    )
    .await
}

#[tokio::test]
async fn a_collision_onto_a_stored_bookmark_refuses_on_a_visibility_mismatch() {
    // The public bookmark is the one that stays; merging a private one onto it would
    // unshare it. Same hazard as the resident case, so the same refusal.
    let mut sitting = bookmark("https://github.com/new/name");
    sitting.public = true;
    let mut mover = bookmark("https://github.com/old/name");
    mover.public = false;
    mover.note = "private annotation".into();
    let pinboard = FakePinboard::default();

    let outcome = run_rename_collision(&pinboard, sitting, mover).await;
    assert_eq!(outcome.refused, 1);
    let updated = pinboard.updated.borrow();
    assert_eq!(
        updated.len(),
        1,
        "the bookmark that stays still gets its plan"
    );
    assert_eq!(updated[0].url, "https://github.com/new/name");
    assert!(
        updated[0].shared,
        "a public bookmark must not be silently unshared by a collision"
    );
    assert!(
        !updated[0].extended.contains("private annotation"),
        "and the held-back bookmark's note must not reach it"
    );
    assert!(
        pinboard.deleted.borrow().is_empty(),
        "the held-back bookmark must not be absorbed"
    );
}

#[tokio::test]
async fn a_collision_onto_a_stored_bookmark_keeps_its_date_and_reading_state() {
    // Visibility matches, so the merge runs -- and the bookmark already at the target
    // keeps its own date and to-read state rather than picking up the absorbed one's
    // under `merge_bookmarks`'s earliest/any rules. Its note leads, too.
    let mut sitting = bookmark("https://github.com/new/name");
    sitting.read_later = false;
    sitting.timestamp = crate::timefmt::from_unix(1_700_000_000);
    sitting.note = "sitting".into();
    let mut mover = bookmark("https://github.com/old/name");
    mover.read_later = true;
    mover.timestamp = crate::timefmt::from_unix(1_000_000);
    mover.note = "mover".into();
    let pinboard = FakePinboard::default();

    let outcome = run_rename_collision(&pinboard, sitting, mover).await;
    assert_eq!(outcome.refused, 0);
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://github.com/new/name");
    assert!(!updated[0].toread, "must not pick up the mover's to-read");
    assert_eq!(
        updated[0].dt,
        crate::timefmt::to_rfc3339(crate::timefmt::from_unix(1_700_000_000).unwrap()).unwrap(),
        "must not be backdated to the mover's date"
    );
    assert_eq!(
        updated[0].extended, "sitting\n\nmover",
        "the record that stays leads the merge, so its note is the base"
    );
    assert_eq!(
        *pinboard.deleted.borrow(),
        strings(&["https://github.com/old/name"])
    );
}

#[tokio::test]
async fn a_collision_refuses_when_a_public_mover_lands_on_a_private_bookmark() {
    // The mirror direction of the mismatch: A (public) normalizes onto B's URL, and B
    // (private) is the record that stays. Refused rather than merged -- narrowing A into
    // a private record is the safe direction for disclosure, but it still makes a
    // bookmark the user chose to share unreachable, and then deletes A.
    let mut stored_a = bookmark("https://old-a/");
    stored_a.public = true;
    let mut stored_b = bookmark("https://collide/");
    stored_b.public = false;
    let books = vec![stored_a, stored_b];

    let pass = FakePass(|bookmark: &Bookmark| {
        if bookmark.url.as_str().contains("old-a") {
            Ok(Plan::Bookmark(Bookmark {
                url: u("https://collide/"),
                ..unchanged_plan(bookmark)
            }))
        } else {
            // A real change, so "B was not merged into" is not confused with
            // "B had nothing to write".
            Ok(Plan::Bookmark(Bookmark {
                title: "refreshed".into(),
                ..unchanged_plan(bookmark)
            }))
        }
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.plan_failed + outcome.write_failed, 0);
    assert_eq!(outcome.refused, 1);
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1, "B still gets its own plan");
    assert_eq!(updated[0].url, "https://collide/");
    assert!(!updated[0].shared, "B is not widened by the held-back A");
    assert!(
        pinboard.deleted.borrow().is_empty(),
        "the refused mover must not be deleted"
    );
}

#[tokio::test]
async fn an_emptied_group_is_not_counted_as_a_refusal_of_its_own() {
    // Four plans are held back, so four writes are abandoned. The group at `a/` empties
    // *and* its key is marked occupied by the pair that stayed there -- reporting it as
    // a refusal too would count five, and print a dry-run stanza for a group with no
    // members left in it.
    let mut pub_to_t = bookmark("https://p1/");
    pub_to_t.public = true;
    let mut priv_to_t = bookmark("https://p2/");
    priv_to_t.public = false;
    let mut pub_to_a = bookmark("https://p3/");
    pub_to_a.public = true;
    let mut priv_to_a = bookmark("https://p4/");
    priv_to_a.public = false;
    let books = vec![pub_to_t, priv_to_t, pub_to_a, priv_to_a];

    let pass = FakePass(|bookmark: &Bookmark| {
        let target = match bookmark.url.as_str() {
            "https://p1/" | "https://p2/" => "https://a/",
            _ => "https://p1/",
        };
        Ok(Plan::Bookmark(Bookmark {
            url: u(target),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run_dry(&pinboard, &books, &pass).await;
    assert_eq!(outcome.refused, 4, "one per abandoned write, not per group");
    assert!(
        !outcome
            .preview
            .iter()
            .any(|line| line.contains("https://p1/")
                && line.starts_with("[dry-run]")
                && outcome.preview.iter().filter(|l| *l == line).count() > 1),
        "no group is previewed twice: {:?}",
        outcome.preview
    );
    assert!(
        pinboard.updated.borrow().is_empty(),
        "{:?}",
        pinboard.updated.borrow()
    );
}

#[tokio::test]
async fn a_group_emptied_by_a_refusal_no_longer_protects_its_url() {
    // The delete-guard exists because "some other planned write owns that URL". When a
    // refusal empties a group, nobody owns its target any more -- so a bookmark moving
    // away from that URL must delete its old record. Leaving the guard in place strands
    // the bookmark at both URLs, silently and for good: the next run refuses the same
    // pair, protects the same URL, and finds the copy already converged.
    let mut leaving = bookmark("https://t/");
    leaving.public = false;
    let mut public_peer = bookmark("https://a/");
    public_peer.public = true;
    let mut private_peer = bookmark("https://b/");
    private_peer.public = false;
    let books = vec![leaving, public_peer, private_peer];

    let pass = FakePass(|bookmark: &Bookmark| {
        let target = match bookmark.url.as_str() {
            "https://t/" => "https://u/",
            _ => "https://t/",
        };
        Ok(Plan::Bookmark(Bookmark {
            url: u(target),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.refused, 2, "the disagreeing peers stay put");
    assert_eq!(
        *pinboard.deleted.borrow(),
        strings(&["https://t/"]),
        "and the bookmark that did move must not be left behind at its old URL"
    );
}

#[tokio::test]
async fn colliding_peers_that_disagree_are_all_refused() {
    // Neither member is stored at the target: both normalize to a brand-new URL, so
    // there is no record whose visibility the others should defer to. Merging anyway
    // would have to pick one -- and picking the private one silently unshares a bookmark
    // the user chose to share, then deletes it. With nothing to prefer, none of them
    // may write.
    let mut stored_a = bookmark("https://old-a/");
    stored_a.public = true;
    stored_a.note = "public note".into();
    let mut stored_b = bookmark("https://old-b/");
    stored_b.public = false;
    let books = vec![stored_a, stored_b];

    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            url: u("https://new/"),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.plan_failed + outcome.write_failed, 0);
    assert_eq!(outcome.refused, 2, "neither peer may write");
    assert!(
        pinboard.updated.borrow().is_empty(),
        "{:?}",
        pinboard.updated.borrow()
    );
    assert!(
        pinboard.deleted.borrow().is_empty(),
        "and neither is absorbed — the public one must not vanish"
    );
}

#[tokio::test]
async fn colliding_peers_that_agree_still_merge() {
    // The mirror: peers that agree have nothing to arbitrate, so the merge runs exactly
    // as before. Without this, refusing on disagreement could be over-applied and no
    // test would notice.
    let mut stored_a = bookmark("https://old-a/");
    stored_a.public = true;
    stored_a.note = "from A".into();
    let mut stored_b = bookmark("https://old-b/");
    stored_b.public = true;
    stored_b.note = "from B".into();
    let books = vec![stored_a, stored_b];

    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            url: u("https://new/"),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run(&pinboard, &books, &pass).await;
    assert_eq!(outcome.refused, 0);
    let updated = pinboard.updated.borrow();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].url, "https://new/");
    assert!(
        updated[0].shared,
        "agreeing peers keep the visibility they share"
    );
    // Both notes survive. *Which leads* is first-appearance order in the slice, which
    // the user has no way to see or control, so it is not something to pin here.
    assert!(
        updated[0].extended.contains("from A"),
        "{:?}",
        updated[0].extended
    );
    assert!(
        updated[0].extended.contains("from B"),
        "{:?}",
        updated[0].extended
    );
    assert_eq!(pinboard.deleted.borrow().len(), 2);
}

#[tokio::test]
async fn a_dry_run_pass_still_advances_the_view() {
    // A preview has to show the changes a real run would make, including the ones a
    // later pass only makes because an earlier one wrote. So a dry run routes through
    // the store as usual -- the store is what withholds the network, not this loop.
    let books = vec![bookmark("https://x/mine")];
    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            url: u("https://x/moved"),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();
    let store = store(&pinboard, &books, WriteMode::DryRun);

    let outcome = run_pass(&store, &books, "test", NO_DATING, &pass).await;
    assert_eq!(outcome.changed, 1);

    assert!(
        store.resident(&u("https://x/moved")).is_some(),
        "the next pass must preview from the state this one would leave"
    );
    assert_eq!(store.resident(&u("https://x/mine")), None);
    assert!(pinboard.updated.borrow().is_empty(), "and write nothing");
    assert!(
        pinboard.deleted.borrow().is_empty(),
        "{:?}",
        pinboard.deleted.borrow()
    );
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
        Ok(Plan::Bookmark(Bookmark {
            url: u(&format!("{}new", bookmark.url)),
            title: "New".into(),
            note,
            tags: tag_list(&["x"]),
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
    let outcome = run_pass(
        &store(&pinboard, &books, WriteMode::DryRun),
        &books,
        "test",
        dates,
        &pass,
    )
    .await;
    assert_eq!(outcome.plan_failed + outcome.write_failed, 0);
    assert!(
        pinboard.updated.borrow().is_empty(),
        "{:?}",
        pinboard.updated.borrow()
    );
    assert!(
        pinboard.deleted.borrow().is_empty(),
        "{:?}",
        pinboard.deleted.borrow()
    );
    // What the operator actually reads before letting a real run touch the account.
    assert_eq!(
        outcome.preview,
        vec![
            "[dry-run] https://empty/",
            "          url   -> https://empty/new",
            "          title -> New",
            "          notes -> (removed)",
            "          tags  -> [x]",
            "          date  -> 2023-11-14T22:13:20Z",
            "[dry-run] https://full/",
            "          url   -> https://full/new",
            "          title -> New",
            "          notes -> new notes",
            "          tags  -> [x]",
            "          date  -> 2023-11-14T22:13:20Z",
        ]
    );
}

#[tokio::test]
async fn a_dry_run_previews_a_merge_and_its_absorbed_urls() {
    // The merge preview renders every written field plus one `absorb` line per URL the
    // merge would delete -- the deletions are the destructive half, and the preview is
    // where an operator gets to see them coming.
    let mut resident = bookmark("https://target/");
    resident.note = "resident".into();
    resident.tags = tag_list(&["rt"]);
    let mut mover = bookmark("https://mover/");
    mover.note = "mover".into();
    mover.tags = tag_list(&["mt"]);
    let slice = vec![mover.clone()];
    let all = vec![mover, resident];

    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            url: u("https://target/"),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run_pass(
        &store(&pinboard, &all, WriteMode::DryRun),
        &slice,
        "test",
        NO_DATING,
        &pass,
    )
    .await;
    assert!(
        pinboard.updated.borrow().is_empty(),
        "{:?}",
        pinboard.updated.borrow()
    );
    assert!(
        pinboard.deleted.borrow().is_empty(),
        "{:?}",
        pinboard.deleted.borrow()
    );
    assert_eq!(
        outcome.preview,
        vec![
            "[dry-run] https://target/",
            "          title -> Title",
            "          notes -> resident\n\nmover",
            "          tags  -> [rt mt]",
            "          date  -> 2020-01-01T00:00:00Z",
            "          public-> false",
            "          toread-> false",
            "          absorb https://mover/",
        ]
    );
}

/// A note of 400 short blocks, too long for a `posts/add` URL.
fn note_pinboard_trims() -> String {
    (0..400)
        .map(|n| format!("block {n:03}"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[tokio::test]
async fn a_merge_onto_a_trimmed_note_is_not_rewritten_every_run() {
    // The resident holds the trimmed form of the note the mover plans onto it, and the
    // mover's old URL is another plan's target, so it is never absorbed. Merging appends
    // the blocks the trim cut off; fitted, the merge is the stored record again.
    let pinboard = FakePinboard {
        trims_notes: true,
        ..Default::default()
    };
    let resident = pinboard.fit(Bookmark {
        note: note_pinboard_trims(),
        ..bookmark("https://collide/")
    });
    assert!(
        resident.note.ends_with(TRUNCATION_MARKER),
        "{:?}",
        resident.note
    );
    let mover = bookmark("https://x/mover");
    let other = bookmark("https://x/other");
    let slice = vec![mover.clone(), other.clone()];
    let all = vec![resident, mover, other];

    let pass = FakePass(move |bookmark: &Bookmark| {
        let (url, note) = if bookmark.url.as_str().ends_with("/mover") {
            ("https://collide/", note_pinboard_trims())
        } else {
            ("https://x/mover", bookmark.note.clone())
        };
        Ok(Plan::Bookmark(Bookmark {
            url: u(url),
            note,
            ..unchanged_plan(bookmark)
        }))
    });

    let outcome = run_pass(
        &store(&pinboard, &all, WriteMode::Live),
        &slice,
        "test",
        NO_DATING,
        &pass,
    )
    .await;
    let updated = pinboard.updated.borrow();
    let written: Vec<&str> = updated.iter().map(|c| c.url.as_str()).collect();
    assert!(
        !written.contains(&"https://collide/"),
        "the trimmed record must not be rewritten: {written:?}"
    );
    assert_eq!(outcome.write_failed, 0);
}

#[tokio::test]
async fn a_whole_note_too_long_to_write_is_not_cut_when_nothing_changed() {
    // Saved through Pinboard's web form, which has no URL limit. Cleanup has nothing to
    // change, so it must not write the bookmark back, cut.
    let pinboard = FakePinboard {
        trims_notes: true,
        ..Default::default()
    };
    let mut stored = bookmark("https://long/");
    stored.note = note_pinboard_trims();
    let mut resident = bookmark("https://collide/");
    resident.note = note_pinboard_trims();
    let incoming = bookmark("https://x/incoming");
    let other = bookmark("https://x/other");
    let slice = vec![stored.clone(), incoming.clone(), other.clone()];
    let all = vec![stored, resident, incoming, other];

    let pass = FakePass(|bookmark: &Bookmark| {
        let url = match bookmark.url.as_str() {
            // Plans the resident's own content onto it, from a URL another plan targets,
            // so there is nothing to absorb.
            "https://x/incoming" => {
                return Ok(Plan::Bookmark(Bookmark {
                    url: u("https://collide/"),
                    note: note_pinboard_trims(),
                    ..unchanged_plan(bookmark)
                }));
            }
            "https://x/other" => u("https://x/incoming"),
            _ => bookmark.url.clone(),
        };
        Ok(Plan::Bookmark(Bookmark {
            url,
            ..unchanged_plan(bookmark)
        }))
    });

    let outcome = run_pass(
        &store(&pinboard, &all, WriteMode::Live),
        &slice,
        "test",
        NO_DATING,
        &pass,
    )
    .await;
    let updated = pinboard.updated.borrow();
    let written: Vec<&str> = updated.iter().map(|c| c.url.as_str()).collect();
    assert_eq!(
        written,
        ["https://x/incoming"],
        "only the real move is written"
    );
    assert_eq!(outcome.write_failed, 0);
}

#[tokio::test]
async fn a_dry_run_previews_the_note_as_pinboard_will_store_it() {
    let pinboard = FakePinboard {
        trims_notes: true,
        ..Default::default()
    };
    let mut resident = bookmark("https://target/");
    resident.note = "resident".into();
    let books = vec![bookmark("https://single/"), bookmark("https://mover/")];
    let mut all = books.clone();
    all.push(resident);
    let pass = FakePass(|bookmark: &Bookmark| {
        let url = match bookmark.url.as_str() {
            "https://mover/" => u("https://target/"),
            _ => bookmark.url.clone(),
        };
        Ok(Plan::Bookmark(Bookmark {
            url,
            note: note_pinboard_trims(),
            ..unchanged_plan(bookmark)
        }))
    });

    let (outcome, logs) = crate::test_support::capturing_logs(run_pass(
        &store(&pinboard, &all, WriteMode::DryRun),
        &books,
        "test",
        NO_DATING,
        &pass,
    ))
    .await;
    let notes: Vec<&String> = outcome
        .preview
        .iter()
        .filter(|line| line.trim_start().starts_with("notes ->"))
        .collect();
    assert_eq!(notes.len(), 2, "{:?}", outcome.preview);
    for line in notes {
        assert!(line.ends_with(TRUNCATION_MARKER), "{line}");
    }
    let cut_warnings = crate::test_support::warnings(&logs)
        .into_iter()
        .filter(|line| line.contains("note trimmed to fit the request URL"))
        .count();
    assert_eq!(cut_warnings, 2, "{logs:#?}");
}

#[tokio::test]
async fn a_change_wholly_past_the_cut_is_not_written() {
    // Writing it would only cut the stored note: the appended line is cut off again.
    let pinboard = FakePinboard {
        trims_notes: true,
        ..Default::default()
    };
    let mut stored = bookmark("https://long/");
    stored.note = note_pinboard_trims();
    let books = vec![stored];
    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            note: format!(
                "{}\n\nHN Link: https://news.ycombinator.com/item?id=1",
                bookmark.note
            ),
            ..unchanged_plan(bookmark)
        }))
    });

    let (outcome, logs) = crate::test_support::capturing_logs(run(&pinboard, &books, &pass)).await;
    assert_eq!(outcome.changed, 0);
    assert!(
        pinboard.updated.borrow().is_empty(),
        "{:?}",
        pinboard.updated.borrow()
    );
    assert!(crate::test_support::warnings(&logs).is_empty(), "{logs:#?}");
}

#[tokio::test]
async fn a_dry_run_previews_a_refused_target() {
    // The other refusal branch: the whole group is refused because its target holds a
    // record the pass couldn't read. Rendered against the *target*, where the mismatch
    // branch renders against the mover -- two call sites, and only one had a test.
    let books = vec![bookmark("https://mover/"), bookmark("https://unreadable/")];
    let pass = FakePass(|bookmark: &Bookmark| match bookmark.url.as_str() {
        "https://unreadable/" => Err(item_error()),
        _ => Ok(Plan::Bookmark(Bookmark {
            url: u("https://unreadable/"),
            ..unchanged_plan(bookmark)
        })),
    });
    let pinboard = FakePinboard::default();

    let outcome = run_dry(&pinboard, &books, &pass).await;
    assert_eq!(outcome.refused, 1);
    assert_eq!(
        outcome.preview,
        vec![
            "[dry-run] https://unreadable/",
            "          (refused: occupied by a bookmark this pass could not read)",
        ]
    );
}

#[tokio::test]
async fn a_dry_run_previews_a_refusal() {
    // A refusal is work the real run would decline to do, so it belongs in the preview
    // too -- it was invisible there while refusals only reached the log.
    let mut resident = bookmark("https://target/");
    resident.public = true;
    let mut mover = bookmark("https://mover/");
    mover.public = false;
    let slice = vec![mover.clone()];
    let all = vec![mover, resident];

    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            url: u("https://target/"),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run_pass(
        &store(&pinboard, &all, WriteMode::DryRun),
        &slice,
        "test",
        NO_DATING,
        &pass,
    )
    .await;
    assert_eq!(outcome.refused, 1);
    assert_eq!(
        outcome.preview,
        vec![
            "[dry-run] https://mover/",
            "          (refused: would merge into https://target/, which is public)",
        ]
    );
}

#[tokio::test]
async fn a_dry_run_names_a_target_occupied_by_a_refused_rewrite() {
    // A is refused (its target couldn't be read), so A stays put and B, heading for A's
    // URL, is refused in turn. The two reasons send the operator to different places, so
    // each refusal has to name its own.
    let books = vec![
        bookmark("https://unreadable/"),
        bookmark("https://a/"),
        bookmark("https://b/"),
    ];
    let pass = FakePass(|bookmark: &Bookmark| match bookmark.url.as_str() {
        "https://unreadable/" => Err(item_error()),
        "https://a/" => Ok(Plan::Bookmark(Bookmark {
            url: u("https://unreadable/"),
            ..unchanged_plan(bookmark)
        })),
        _ => Ok(Plan::Bookmark(Bookmark {
            url: u("https://a/"),
            ..unchanged_plan(bookmark)
        })),
    });
    let pinboard = FakePinboard::default();

    let outcome = run_dry(&pinboard, &books, &pass).await;
    assert_eq!(outcome.refused, 2);
    assert_eq!(
        outcome.preview,
        vec![
            "[dry-run] https://unreadable/",
            "          (refused: occupied by a bookmark this pass could not read)",
            "[dry-run] https://a/",
            "          (refused: occupied by a rewrite that was itself refused)",
        ]
    );
}

#[tokio::test]
async fn a_refusal_behind_a_failed_write_says_the_write_failed() {
    // A's move fails, so A is still at its URL and B, heading there, must be refused.
    // Only a real run can fail a write, so the reason shows in the log, not a preview.
    let books = vec![bookmark("https://a/"), bookmark("https://b/")];
    let pass = FakePass(|bookmark: &Bookmark| {
        let target = match bookmark.url.as_str() {
            "https://a/" => "https://new/",
            _ => "https://a/",
        };
        Ok(Plan::Bookmark(Bookmark {
            url: u(target),
            ..unchanged_plan(bookmark)
        }))
    });
    let mut pinboard = FakePinboard::default();
    pinboard.fail_update_urls.insert("https://new/".into());

    let (outcome, logs) = crate::test_support::capturing_logs(run(&pinboard, &books, &pass)).await;
    assert_eq!(outcome.refused, 1);
    assert_eq!(outcome.write_failed, 1);
    assert!(pinboard.ops.borrow().is_empty(), "nothing may land");
    assert!(
        logs.contains(
            &"WARN skipping cleanup write to https://a/: occupied by a bookmark whose own \
              rewrite failed"
                .to_string()
        ),
        "{logs:#?}"
    );
}

#[tokio::test]
async fn a_dry_run_names_a_target_held_by_a_visibility_refusal() {
    // A is held back for disagreeing with the public record staying at the target, so A
    // stays put and B, heading for A's URL, is refused for the same reason as any other
    // rewrite that was itself refused.
    let mut resident = bookmark("https://target/");
    resident.public = true;
    let slice = vec![bookmark("https://a/"), bookmark("https://b/")];
    let all = vec![slice[0].clone(), slice[1].clone(), resident];
    let pass = FakePass(|bookmark: &Bookmark| {
        let target = match bookmark.url.as_str() {
            "https://a/" => "https://target/",
            _ => "https://a/",
        };
        Ok(Plan::Bookmark(Bookmark {
            url: u(target),
            ..unchanged_plan(bookmark)
        }))
    });
    let pinboard = FakePinboard::default();

    let outcome = run_pass(
        &store(&pinboard, &all, WriteMode::DryRun),
        &slice,
        "test",
        NO_DATING,
        &pass,
    )
    .await;
    assert_eq!(outcome.refused, 2);
    assert_eq!(
        outcome.preview,
        vec![
            "[dry-run] https://a/",
            "          (refused: would merge into https://target/, which is public)",
            "[dry-run] https://a/",
            "          (refused: occupied by a rewrite that was itself refused)",
        ]
    );
}

#[tokio::test]
async fn stale_to_now_settles_after_one_run() {
    // The source date is past the cap, so the first run re-dates the bookmark to now. That
    // stored "now" is within the cap, so the second run must leave it alone rather than
    // re-dating it to its own "now" forever.
    let books = vec![bookmark("https://stale/")];
    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            timestamp: crate::timefmt::from_unix(1_500_000_000),
            ..unchanged_plan(bookmark)
        }))
    });
    let dates = DateOpts {
        use_post_date: true,
        max_age_days: 30,
        stale_to_now: true,
    };

    let first = FakePinboard::default();
    let outcome = run_pass(
        &store(&first, &books, WriteMode::Live),
        &books,
        "test",
        dates,
        &pass,
    )
    .await;
    assert_eq!(outcome.changed, 1);
    let after_first = crate::test_support::replay(&books, &first.ops.borrow());

    let second = FakePinboard::default();
    let outcome = run_pass(
        &store(&second, &after_first, WriteMode::Live),
        &after_first,
        "test",
        dates,
        &pass,
    )
    .await;
    assert_eq!(outcome.changed, 0);
    assert!(
        second.updated.borrow().is_empty(),
        "{:?}",
        second.updated.borrow()
    );
}

#[tokio::test]
async fn stale_to_now_dates_at_whole_seconds() {
    // Pinboard keeps whole seconds, so a fractional "now" could never equal the date it
    // stores back.
    let books = vec![bookmark("https://stale/")];
    let pass = FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            timestamp: crate::timefmt::from_unix(1_500_000_000),
            ..unchanged_plan(bookmark)
        }))
    });
    let dates = DateOpts {
        use_post_date: true,
        max_age_days: 30,
        stale_to_now: true,
    };
    let pinboard = FakePinboard::default();
    let outcome = run_pass(
        &store(&pinboard, &books, WriteMode::Live),
        &books,
        "test",
        dates,
        &pass,
    )
    .await;
    assert_eq!(outcome.changed, 1);

    let ops = pinboard.ops.borrow();
    let [Op::Write(written)] = ops.as_slice() else {
        panic!("expected one write, got {ops:?}");
    };
    assert_eq!(written.timestamp.map(|ts| ts.nanosecond()), Some(0));
}

/// A resident at `https://target/` with a whole note too long for a write, and a mover
/// heading there whose own note is short, so all it adds is cut off by the fit.
fn long_resident_and_short_mover() -> (Bookmark, Bookmark) {
    let mut resident = bookmark("https://target/");
    resident.note = note_pinboard_trims();
    let mut mover = bookmark("https://mover/");
    mover.note = "mover".into();
    (resident, mover)
}

/// Plans every bookmark onto `https://target/`, as it is.
fn onto_target() -> FakePass<impl Fn(&Bookmark) -> Result<Plan, SourceError>> {
    FakePass(|bookmark: &Bookmark| {
        Ok(Plan::Bookmark(Bookmark {
            url: u("https://target/"),
            ..unchanged_plan(bookmark)
        }))
    })
}

#[tokio::test]
async fn a_merge_that_stores_nothing_new_only_absorbs() {
    // Writing the merge would cut the resident's whole note and keep nothing of the
    // mover's, so the mover is absorbed by deleting it and the resident is not written.
    let pinboard = FakePinboard {
        trims_notes: true,
        ..Default::default()
    };
    let (resident, mover) = long_resident_and_short_mover();
    let all = vec![mover.clone(), resident.clone()];

    let outcome = run_pass(
        &store(&pinboard, &all, WriteMode::Live),
        &[mover],
        "test",
        NO_DATING,
        &onto_target(),
    )
    .await;
    assert!(
        pinboard.updated.borrow().is_empty(),
        "{:?}",
        pinboard.updated.borrow()
    );
    assert_eq!(*pinboard.deleted.borrow(), strings(&["https://mover/"]));
    assert_eq!(outcome.changed, 1);
    assert_eq!(outcome.write_failed, 0);
    let account = crate::test_support::replay(&all, &pinboard.ops.borrow());
    assert_eq!(account, [resident]);
}

#[tokio::test]
async fn an_absorb_whose_delete_fails_is_half_applied() {
    let pinboard = FakePinboard {
        trims_notes: true,
        fail_delete_urls: ["https://mover/".to_string()].into(),
        ..Default::default()
    };
    let (resident, mover) = long_resident_and_short_mover();
    let all = vec![mover.clone(), resident];

    let outcome = run_pass(
        &store(&pinboard, &all, WriteMode::Live),
        &[mover],
        "test",
        NO_DATING,
        &onto_target(),
    )
    .await;
    assert!(pinboard.updated.borrow().is_empty());
    assert_eq!(outcome.changed, 0, "nothing in the account changed");
    assert_eq!(outcome.write_failed, 1);
}

#[tokio::test]
async fn an_absorb_warns_that_the_movers_note_does_not_fit() {
    let pinboard = FakePinboard {
        trims_notes: true,
        ..Default::default()
    };
    let (resident, mover) = long_resident_and_short_mover();
    let all = vec![mover.clone(), resident];

    let (_, logs) = crate::test_support::capturing_logs(run_pass(
        &store(&pinboard, &all, WriteMode::Live),
        &[mover],
        "test",
        NO_DATING,
        &onto_target(),
    ))
    .await;
    assert!(
        crate::test_support::warnings(&logs)
            .iter()
            .any(|line| line.contains("note trimmed to fit the request URL for https://target/")),
        "{logs:#?}"
    );
}

#[tokio::test]
async fn a_dry_run_previews_an_absorb_without_fields() {
    let pinboard = FakePinboard {
        trims_notes: true,
        ..Default::default()
    };
    let (resident, mover) = long_resident_and_short_mover();
    let all = vec![mover.clone(), resident];

    let outcome = run_pass(
        &store(&pinboard, &all, WriteMode::DryRun),
        &[mover],
        "test",
        NO_DATING,
        &onto_target(),
    )
    .await;
    assert_eq!(
        outcome.preview,
        [
            "[dry-run] https://target/",
            "          absorb https://mover/"
        ]
    );
}
