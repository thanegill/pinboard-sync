//! The sync write path: pick the drafts not already on Pinboard, and write them
//! sequentially (Pinboard rate-limits `posts/add`). Source reads happen in the
//! caller (so an `--all` run can fetch services concurrently); only the writes are
//! serialized here. Generic over the [`Source`]/[`BookmarkStore`] ports so it can
//! be unit-tested with in-memory fakes.

use std::collections::HashSet;

use log::{debug, error};

use crate::bookmark::{Bookmark, BookmarkStore};
use crate::config::SyncLimit;
use crate::source::{BookmarkDraft, Source};
use crate::timefmt::DateOpts;

/// The drafts not already present on Pinboard, matching each existing bookmark URL
/// through this source's [`UrlKey::dedup_key`].
pub fn filter_new<S: Source>(
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

/// The per-account settings resolved for one sync job: how much to read and write, the
/// flags stamped onto every new bookmark, and the post-date policy. A `SyncJob` carries one
/// next to its client, so the per-job transform can run without the concrete client.
#[derive(Debug, Clone, Copy)]
pub struct JobSettings {
    pub limit: SyncLimit,
    /// To-read flag for this account's new bookmarks.
    pub toread: bool,
    /// Public/shared flag for this account's new bookmarks.
    pub shared: bool,
    pub dates: DateOpts,
}

/// The new drafts for one account, ready to write: those not already on Pinboard
/// ([`filter_new`]) and collapsed so each `dedup_key` appears once (keep-first, matching
/// the cross-job [`merge_deduped`] so `limit` counts distinct items rather than letting
/// duplicate-key drafts consume slots), each stamped with the account's resolved
/// `toread`/`shared` flags and post date (kept only when `use_post_date` is on and the
/// post is within the age cap relative to `now`, else cleared so Pinboard defaults to
/// "now"), and truncated to the per-job `limit`, if any, which keeps the newest.
///
/// Returned oldest first, the reverse of the source's newest-first order, so a run cut
/// short leaves the *newest* items unwritten: they sit on the source's first pages, where
/// the next run's caught-up check sees them and keeps paging down to the written ones.
/// Newest first, an interrupted backfill would leave page 1 fully written, and the next
/// run would stop there and never reach the older items. `now` is a parameter so tests
/// are deterministic.
pub fn prepare_new_drafts<S: Source>(
    source: &S,
    drafts: Vec<BookmarkDraft>,
    existing: &[Bookmark],
    settings: &JobSettings,
    now: i64,
) -> Vec<BookmarkDraft> {
    let mut new = filter_new(source, drafts, existing);
    let mut seen = HashSet::new();
    new.retain(|d| seen.insert(d.dedup_key.clone()));
    for d in &mut new {
        d.bookmark.read_later = settings.toread;
        d.bookmark.public = settings.shared;
        d.bookmark.timestamp = if settings.dates.use_post_date {
            d.bookmark.timestamp.filter(|t| {
                crate::timefmt::within_age_cap(now, t.unix_timestamp(), settings.dates.max_age_days)
            })
        } else {
            None
        };
    }
    if let Some(limit) = settings.limit.write_cap() {
        new.truncate(limit.get());
    }
    new.reverse();
    new
}

/// Flatten each job's prepared drafts into one write batch, keeping the first occurrence
/// of any `dedup_key` so two drafts the dedup logic treats as one item are written once
/// (matching [`filter_new`]'s existing-key check — otherwise two fresh drafts whose URLs
/// differ byte-for-byte but share a key, e.g. two HN favorites of the same article stored
/// as `http://x` vs `https://x`, would each be written on a first-time run). Order is
/// preserved: jobs in order, drafts in order within a job.
pub fn merge_deduped(per_job: Vec<Vec<BookmarkDraft>>) -> Vec<BookmarkDraft> {
    let mut merged: Vec<BookmarkDraft> = Vec::new();
    let mut seen = HashSet::new();
    for drafts in per_job {
        for draft in drafts {
            if seen.insert(draft.dedup_key.clone()) {
                merged.push(draft);
            }
        }
    }
    merged
}

/// Tally of a write pass: how many drafts were written vs. failed.
#[must_use]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WriteTally {
    pub written: usize,
    pub failed: usize,
}

/// Write `drafts` to Pinboard sequentially (the store spaces its own `posts/add` calls).
/// A single bookmark that fails to write (e.g. a rejected URL) is logged and skipped so
/// the rest still go through; the failure is reflected in the returned [`WriteTally`]
/// for a non-zero exit. Writes nothing in `dry_run`.
pub async fn write_drafts<P: BookmarkStore>(
    pinboard: &P,
    drafts: &[BookmarkDraft],
    dry_run: bool,
) -> WriteTally {
    let mut outcome = WriteTally::default();
    for draft in drafts {
        let bm = &draft.bookmark;
        if dry_run {
            // The source post date as RFC3339, when set (the sync loop has already
            // applied the use_post_date flag + age cap); empty = let Pinboard default.
            let dt = bm
                .timestamp
                .and_then(crate::timefmt::to_rfc3339)
                .unwrap_or_default();
            println!("[dry-run] {}", bm.url);
            println!("          title: {}", bm.title);
            if !bm.note.is_empty() {
                println!("          notes: {}", crate::htmltext::preview(&bm.note));
            }
            println!("          tags:  [{}]", bm.tags.join(" "));
            if !dt.is_empty() {
                println!("          date:  {dt}");
            }
            continue;
        }

        // The sync loop already resolved `public`/`read_later` and the post date on the
        // draft's bookmark, so write it straight through (the client paces itself).
        match pinboard.add(bm).await {
            Ok(()) => {
                outcome.written += 1;
                debug!("added {}  [{}]", bm.url, bm.tags.join(" "));
            }
            // Log and skip — one bad bookmark shouldn't abort the rest of the run.
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
