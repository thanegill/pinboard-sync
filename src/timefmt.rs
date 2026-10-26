//! Date conversions for the `use_post_date` option: between unix epoch seconds (how
//! source post dates are carried internally, so the age cap is a plain integer
//! compare) and the RFC 3339 string Pinboard's `dt` expects (e.g.
//! `2010-12-11T19:48:02Z`) — plus the option itself, [`DateOpts`], which sync and every
//! cleanup pass apply.

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Parse an RFC 3339 timestamp (e.g. GitHub's `starred_at`) to unix epoch seconds.
pub fn rfc3339_to_unix(s: &str) -> Option<i64> {
    OffsetDateTime::parse(s, &Rfc3339)
        .ok()
        .map(|t| t.unix_timestamp())
}

/// Parse an RFC 3339 timestamp into an [`OffsetDateTime`] — the `cleanup` domain form of
/// a bookmark's creation time. `None` if it doesn't parse. (Can't be a `From`/`TryFrom`
/// impl: `OffsetDateTime` is foreign to this crate, so the orphan rule forbids it.)
pub fn parse_rfc3339(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339).ok()
}

/// Format an [`OffsetDateTime`] as RFC 3339 for Pinboard's `dt`. `None` if formatting
/// fails. (Can't be a `From`/`Into` impl: `String` and `OffsetDateTime` are both foreign
/// to this crate, so the orphan rule forbids it — and it's fallible.)
pub fn to_rfc3339(dt: OffsetDateTime) -> Option<String> {
    dt.format(&Rfc3339).ok()
}

/// An [`OffsetDateTime`] from unix epoch seconds — used to lift a source's epoch post
/// date into the `cleanup` domain form. `None` if out of range. (Thin `.ok()` over the
/// `time` crate's own `from_unix_timestamp`; can't be a `From` impl — `OffsetDateTime` is
/// foreign and the conversion is fallible.)
pub fn from_unix(secs: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(secs).ok()
}

/// Current time as unix epoch seconds (UTC). Kept as the single `now` call site; pure
/// logic (the age cap) takes the value as a parameter so it stays testable.
pub fn now_unix() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

/// Whether a post created at `timestamp` (epoch) is at most `max_age_days` old relative
/// to `now` (epoch) — the `use_post_date` backdate cap. Shared by
/// [`DateOpts::sync_date`] and [`DateOpts::cleanup_date`].
fn within_age_cap(now: i64, timestamp: i64, max_age_days: u64) -> bool {
    now as i128 - timestamp as i128 <= max_age_days as i128 * 86_400
}

/// The `use_post_date` policy: whether to date bookmarks by the source date, the backdate
/// age cap, and whether `cleanup` pushes stale (older-than-cap) items to "now". Resolved
/// once per account in `jobs::resolve_dates`; `sync` applies the first two via
/// [`DateOpts::sync_date`], and a `cleanup` pass applies all three uniformly via
/// [`DateOpts::cleanup_date`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateOpts {
    pub use_post_date: bool,
    /// Backdate age cap, in days.
    pub max_age_days: u64,
    pub stale_to_now: bool,
}

impl DateOpts {
    /// The creation time a new `sync` bookmark is written with. `candidate` is the source
    /// post time (if known) and `now` the run's time (epoch). Returns the source time when
    /// dating is on and the post is within the cap; otherwise `None`, so Pinboard dates the
    /// bookmark "now".
    pub fn sync_date(self, candidate: Option<OffsetDateTime>, now: i64) -> Option<OffsetDateTime> {
        if !self.use_post_date {
            return None;
        }
        candidate.filter(|ts| within_age_cap(now, ts.unix_timestamp(), self.max_age_days))
    }

    /// The creation time a `cleanup` re-write should set. `candidate` is the source post
    /// time (if known) and `existing` the bookmark's current time. Returns the source time
    /// when dating is on and the post is within the cap; otherwise `now` when the post is
    /// stale and `stale_to_now`; otherwise `existing` (unchanged). The age cap compares by
    /// instant; the RFC3339 formatting happens at the write boundary.
    pub fn cleanup_date(
        self,
        candidate: Option<OffsetDateTime>,
        now: OffsetDateTime,
        existing: Option<OffsetDateTime>,
    ) -> Option<OffsetDateTime> {
        if !self.use_post_date {
            return existing;
        }
        match candidate {
            Some(ts)
                if within_age_cap(now.unix_timestamp(), ts.unix_timestamp(), self.max_age_days) =>
            {
                Some(ts)
            }
            Some(_) if self.stale_to_now => Some(now),
            _ => existing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_known_instant_with_trailing_z() {
        // 2010-12-11T19:48:02Z (the example from Pinboard's API docs).
        let secs = 1_292_096_882;
        assert_eq!(
            from_unix(secs).and_then(to_rfc3339).unwrap(),
            "2010-12-11T19:48:02Z"
        );
        assert_eq!(rfc3339_to_unix("2010-12-11T19:48:02Z"), Some(secs));
    }

    #[test]
    fn parses_an_offset_back_to_utc_epoch() {
        // Same instant expressed with a +00:00 offset parses to the same epoch.
        assert_eq!(
            rfc3339_to_unix("2010-12-11T19:48:02+00:00"),
            Some(1_292_096_882)
        );
    }

    #[test]
    fn reddit_float_truncates_to_whole_seconds() {
        // Reddit's created_utc is a float; callers pass `f as i64`.
        let secs = 1_700_000_000.7_f64 as i64;
        assert_eq!(secs, 1_700_000_000);
        assert_eq!(
            from_unix(secs).and_then(to_rfc3339).unwrap(),
            "2023-11-14T22:13:20Z"
        );
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(rfc3339_to_unix("not a date"), None);
    }

    #[test]
    fn within_age_cap_handles_extreme_values_without_overflow() {
        let now = 1_700_000_000;
        // A huge cap never treats any post as too old, and must not overflow.
        assert!(within_age_cap(now, 0, u64::MAX));
        assert!(within_age_cap(now, now, u64::MAX));
        // Normal behavior is unchanged: 5 days in, 30-day cap holds; 60 days out fails.
        assert!(within_age_cap(now, now - 5 * 86_400, 30));
        assert!(!within_age_cap(now, now - 60 * 86_400, 30));
        // A zero cap admits only same-instant posts.
        assert!(within_age_cap(now, now, 0));
        assert!(!within_age_cap(now, now - 1, 0));
    }

    #[test]
    fn cleanup_date_honors_flag_cap_and_stale_fallback() {
        let now = from_unix(1_700_000_000).unwrap();
        let recent = from_unix(1_700_000_000 - 5 * 86_400).unwrap(); // 5 days old
        let old = from_unix(1_700_000_000 - 60 * 86_400).unwrap(); // 60 days old
        let existing = from_unix(1_546_300_800); // Some(2019-01-01T00:00:00Z)
        let dates = |use_post_date, stale_to_now| DateOpts {
            use_post_date,
            max_age_days: 30,
            stale_to_now,
        };

        // Dating off → keep existing.
        assert_eq!(
            dates(false, true).cleanup_date(Some(recent), now, existing),
            existing
        );
        // Within cap → source date.
        assert_eq!(
            dates(true, false).cleanup_date(Some(recent), now, existing),
            Some(recent)
        );
        // Stale + default (no stale_to_now) → keep existing.
        assert_eq!(
            dates(true, false).cleanup_date(Some(old), now, existing),
            existing
        );
        // Stale + stale_to_now → now.
        assert_eq!(
            dates(true, true).cleanup_date(Some(old), now, existing),
            Some(now)
        );
        // No source date → keep existing.
        assert_eq!(
            dates(true, true).cleanup_date(None, now, existing),
            existing
        );
    }
}
