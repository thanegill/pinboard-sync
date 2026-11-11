//! The sync write path: the drafts not already on Pinboard, written in turn, since Pinboard
//! rate-limits `posts/add`. The caller reads the sources, concurrently. Generic over the
//! ports, for the in-memory fakes.

use std::collections::HashSet;

use log::{debug, error};
use time::OffsetDateTime;

use crate::bookmark::{Bookmark, BookmarkStore, WriteMode};
use crate::config::SyncLimit;
use crate::preview::{self, INDENT};
use crate::source::{BookmarkDraft, UrlKey};
use crate::stdout::outln;
use crate::timefmt::DateOpts;

/// The drafts not already on Pinboard, by `source`'s [`UrlKey::dedup_key`].
pub fn filter_new<S: UrlKey>(
    source: &S,
    drafts: Vec<BookmarkDraft>,
    existing: &[Bookmark],
) -> Vec<BookmarkDraft> {
    let keys = source.synced_keys(existing);
    drafts
        .into_iter()
        .filter(|d| !keys.contains(&d.dedup_key))
        .collect()
}

/// One sync job's resolved settings: how much to read and write, the flags stamped on each
/// new bookmark, and the post-date policy.
#[derive(Debug, Clone, Copy)]
pub struct JobSettings {
    pub limit: SyncLimit,
    /// Set by `--toread` / `toread`.
    pub read_later: bool,
    /// Set by `--public` / `public`.
    pub public: bool,
    pub dates: DateOpts,
}

/// The new drafts for one account, ready to write:
/// - those not already on Pinboard ([`filter_new`]);
/// - each `dedup_key` once, the first kept as in [`merge_deduped`], so `limit` counts
///   distinct items;
/// - stamped with the account's `read_later`/`public` and its post date, which
///   [`DateOpts::sync_date`] keeps, or clears so Pinboard dates the bookmark now;
/// - cut to the per-job `limit`, if any, keeping the newest.
///
/// Returned oldest first, so a run cut short leaves the newest unwritten: they sit on the
/// source's first pages, where the next run's caught-up check sees them and pages on down.
/// Newest first, an interrupted backfill would leave page 1 written, and the next run would
/// stop there and never reach the older items.
pub fn prepare_new_drafts<S: UrlKey>(
    source: &S,
    drafts: Vec<BookmarkDraft>,
    existing: &[Bookmark],
    settings: &JobSettings,
    now: OffsetDateTime,
) -> Vec<BookmarkDraft> {
    let mut new = filter_new(source, drafts, existing);
    let mut seen = HashSet::new();
    new.retain(|d| seen.insert(d.dedup_key.clone()));
    for d in &mut new {
        d.bookmark.read_later = settings.read_later;
        d.bookmark.public = settings.public;
        d.bookmark.timestamp = settings.dates.sync_date(d.bookmark.timestamp, now);
    }
    if let Some(limit) = settings.limit.write_cap() {
        new.truncate(limit.get());
    }
    new.reverse();
    new
}

/// Every job's drafts as one batch, in order, keeping the first of each `dedup_key`, as
/// [`filter_new`] matches: two fresh drafts of one item (an article as `http://x` and
/// `https://x`) would otherwise both be written.
pub fn merge_deduped(per_job: Vec<Vec<BookmarkDraft>>) -> Vec<BookmarkDraft> {
    let mut seen = HashSet::new();
    per_job
        .into_iter()
        .flatten()
        .filter(|draft| seen.insert(draft.dedup_key.clone()))
        .collect()
}

#[must_use]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WriteTally {
    pub written: usize,
    pub failed: usize,
}

/// Write `drafts` in turn (the store paces its own calls), or in [`WriteMode::DryRun`] only
/// print them. One that fails is logged and counted, and the rest still go.
pub async fn write_drafts<P: BookmarkStore>(
    pinboard: &P,
    drafts: &[BookmarkDraft],
    mode: WriteMode,
) -> WriteTally {
    let mut outcome = WriteTally::default();
    for draft in drafts {
        let bm = &draft.bookmark;
        if mode.is_dry_run() {
            outln!("{}", preview::header(&bm.url));
            outln!("{INDENT}title: {}", bm.title);
            if !bm.note.is_empty() {
                outln!("{INDENT}notes: {}", preview::excerpt(&bm.note));
            }
            outln!("{INDENT}tags:  {}", preview::tags(&bm.tags));
            if let Some(dt) = bm.timestamp.and_then(crate::timefmt::to_pinboard_dt) {
                outln!("{INDENT}date:  {dt}");
            }
            continue;
        }

        match pinboard.add(bm).await {
            Ok(()) => {
                outcome.written += 1;
                debug!("added {}  [{}]", bm.url, bm.tags.joined());
            }
            Err(e) => {
                outcome.failed += 1;
                error!("adding bookmark {}: {e:#}", bm.url);
            }
        }
    }
    outcome
}

#[cfg(test)]
mod tests;
