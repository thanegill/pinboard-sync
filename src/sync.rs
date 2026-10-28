//! The sync write path: pick the drafts not already on Pinboard, and write them
//! sequentially (Pinboard rate-limits `posts/add`). Source reads happen in the caller (so
//! an `--all` run can fetch services concurrently); only the writes are serialized here.
//! Generic over the [`UrlKey`]/[`BookmarkStore`] ports so it can be unit-tested with
//! in-memory fakes.

use std::collections::HashSet;

use log::{debug, error};
use time::OffsetDateTime;

use crate::bookmark::{Bookmark, BookmarkStore, WriteMode};
use crate::config::SyncLimit;
use crate::source::{BookmarkDraft, UrlKey};
use crate::timefmt::DateOpts;

/// The drafts not already present on Pinboard, matching each existing bookmark URL
/// through this source's [`UrlKey::dedup_key`].
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

/// The per-account settings resolved for one sync job: how much to read and write, the
/// flags stamped onto every new bookmark, and the post-date policy. A `SyncJob` carries one
/// next to its client, so the per-job transform can run without the concrete client.
#[derive(Debug, Clone, Copy)]
pub struct JobSettings {
    pub limit: SyncLimit,
    /// Whether this account's new bookmarks are queued to read (`--toread`, `toread`).
    pub read_later: bool,
    /// Whether this account's new bookmarks are public (`--public`, `public`).
    pub public: bool,
    pub dates: DateOpts,
}

/// The new drafts for one account, ready to write: those not already on Pinboard
/// ([`filter_new`]) and collapsed so each `dedup_key` appears once (keep-first, matching
/// the cross-job [`merge_deduped`] so `limit` counts distinct items rather than letting
/// duplicate-key drafts consume slots), each stamped with the account's resolved
/// `read_later`/`public` flags and post date (kept only when `use_post_date` is on and
/// the post is within the age cap relative to `now`, else cleared so Pinboard defaults to
/// "now"), and truncated to the per-job `limit`, if any, which keeps the newest.
///
/// Returned oldest first, the reverse of the source's newest-first order, so a run cut
/// short leaves the *newest* items unwritten: they sit on the source's first pages, where
/// the next run's caught-up check sees them and keeps paging down to the written ones.
/// Newest first, an interrupted backfill would leave page 1 fully written, and the next
/// run would stop there and never reach the older items. `now` is a parameter so tests
/// are deterministic.
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

/// Flatten each job's prepared drafts into one write batch, keeping the first occurrence
/// of any `dedup_key` so two drafts the dedup logic treats as one item are written once
/// (matching [`filter_new`]'s existing-key check -- otherwise two fresh drafts whose URLs
/// differ byte-for-byte but share a key, e.g. two HN favorites of the same article stored
/// as `http://x` vs `https://x`, would each be written on a first-time run). Order is
/// preserved: jobs in order, drafts in order within a job.
pub fn merge_deduped(per_job: Vec<Vec<BookmarkDraft>>) -> Vec<BookmarkDraft> {
    let mut seen = HashSet::new();
    per_job
        .into_iter()
        .flatten()
        .filter(|draft| seen.insert(draft.dedup_key.clone()))
        .collect()
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
/// for a non-zero exit. Writes nothing in [`WriteMode::DryRun`].
pub async fn write_drafts<P: BookmarkStore>(
    pinboard: &P,
    drafts: &[BookmarkDraft],
    mode: WriteMode,
) -> WriteTally {
    let mut outcome = WriteTally::default();
    for draft in drafts {
        let bm = &draft.bookmark;
        if mode.is_dry_run() {
            // The source post date as Pinboard's `dt`, when set (the sync loop has already
            // applied the use_post_date flag + age cap); empty = let Pinboard default.
            let dt = bm
                .timestamp
                .and_then(crate::timefmt::to_pinboard_dt)
                .unwrap_or_default();
            println!("[dry-run] {}", bm.url);
            println!("          title: {}", bm.title);
            if !bm.note.is_empty() {
                println!("          notes: {}", crate::htmltext::preview(&bm.note));
            }
            println!("          tags:  [{}]", bm.tags.joined());
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
                debug!("added {}  [{}]", bm.url, bm.tags.joined());
            }
            // Log and skip -- one bad bookmark shouldn't abort the rest of the run.
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
