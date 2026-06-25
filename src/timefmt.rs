//! Date conversions for the `use_post_date` option: between unix epoch seconds (how
//! source post dates are carried internally, so the age cap is a plain integer
//! compare) and the RFC 3339 string Pinboard's `dt` expects (e.g.
//! `2010-12-11T19:48:02Z`).

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Format unix epoch seconds (UTC) as an RFC 3339 string, e.g. `2010-12-11T19:48:02Z`.
/// `None` if the timestamp is out of range.
pub fn unix_to_rfc3339(secs: i64) -> Option<String> {
    OffsetDateTime::from_unix_timestamp(secs)
        .ok()?
        .format(&Rfc3339)
        .ok()
}

/// Parse an RFC 3339 timestamp (e.g. GitHub's `starred_at`) to unix epoch seconds.
pub fn rfc3339_to_unix(s: &str) -> Option<i64> {
    OffsetDateTime::parse(s, &Rfc3339)
        .ok()
        .map(|t| t.unix_timestamp())
}

/// Parse an RFC 3339 timestamp into an [`OffsetDateTime`] — the `cleanup` domain form of
/// a bookmark's creation time. `None` if it doesn't parse.
pub fn parse_rfc3339(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339).ok()
}

/// Format an [`OffsetDateTime`] as RFC 3339 for Pinboard's `dt`. `None` if formatting
/// fails.
pub fn to_rfc3339(dt: OffsetDateTime) -> Option<String> {
    dt.format(&Rfc3339).ok()
}

/// An [`OffsetDateTime`] from unix epoch seconds — used to lift a source's epoch post
/// date into the `cleanup` domain form. `None` if out of range.
pub fn from_unix(secs: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(secs).ok()
}

/// Current time as unix epoch seconds (UTC). Kept as the single `now` call site; pure
/// logic (the age cap) takes the value as a parameter so it stays testable.
pub fn now_unix() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

/// Current time as an [`OffsetDateTime`] (UTC) — the `cleanup` domain `now`.
pub fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// Whether a post created at `timestamp` (epoch) is at most `max_age_days` old relative
/// to `now` (epoch) — the `use_post_date` backdate cap. Shared by `sync` (which clears
/// out-of-cap dates) and `cleanup` ([`cleanup_date`]).
pub fn within_age_cap(now: i64, timestamp: i64, max_age_days: u64) -> bool {
    now - timestamp <= max_age_days as i64 * 86_400
}

/// The creation time a `cleanup` re-write should set, honoring `use_post_date`, the age
/// cap (`max_age_days`), and the stale fallback (`stale_to_now`). `candidate` is the
/// source post time (if known) and `existing` the bookmark's current time. Returns the
/// source time when dating is on and the post is within the cap; otherwise `now` when
/// the post is stale and `stale_to_now`; otherwise `existing` (unchanged). The age cap
/// compares by instant; the RFC3339 formatting happens at the write boundary.
pub fn cleanup_date(
    use_post_date: bool,
    max_age_days: u64,
    stale_to_now: bool,
    candidate: Option<OffsetDateTime>,
    now: OffsetDateTime,
    existing: Option<OffsetDateTime>,
) -> Option<OffsetDateTime> {
    if !use_post_date {
        return existing;
    }
    match candidate {
        Some(ts) if within_age_cap(now.unix_timestamp(), ts.unix_timestamp(), max_age_days) => {
            Some(ts)
        }
        Some(_) if stale_to_now => Some(now),
        _ => existing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_known_instant_with_trailing_z() {
        // 2010-12-11T19:48:02Z (the example from Pinboard's API docs).
        let secs = 1_292_096_882;
        assert_eq!(unix_to_rfc3339(secs).unwrap(), "2010-12-11T19:48:02Z");
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
        assert_eq!(unix_to_rfc3339(secs).unwrap(), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(rfc3339_to_unix("not a date"), None);
    }

    #[test]
    fn cleanup_date_honors_flag_cap_and_stale_fallback() {
        let now = from_unix(1_700_000_000).unwrap();
        let recent = from_unix(1_700_000_000 - 5 * 86_400).unwrap(); // 5 days old
        let old = from_unix(1_700_000_000 - 60 * 86_400).unwrap(); // 60 days old
        let existing = from_unix(1_546_300_800); // Some(2019-01-01T00:00:00Z)

        // Dating off → keep existing.
        assert_eq!(
            cleanup_date(false, 30, true, Some(recent), now, existing),
            existing
        );
        // Within cap → source date.
        assert_eq!(
            cleanup_date(true, 30, false, Some(recent), now, existing),
            Some(recent)
        );
        // Stale + default (no stale_to_now) → keep existing.
        assert_eq!(
            cleanup_date(true, 30, false, Some(old), now, existing),
            existing
        );
        // Stale + stale_to_now → now.
        assert_eq!(
            cleanup_date(true, 30, true, Some(old), now, existing),
            Some(now)
        );
        // No source date → keep existing.
        assert_eq!(cleanup_date(true, 30, true, None, now, existing), existing);
    }
}
