//! Date conversions for the `use_post_date` option: between unix epoch seconds (how
//! source post dates are carried internally, so the age cap is a plain integer
//! compare) and the RFC 3339 string Pinboard's `dt` expects (e.g.
//! `2010-12-11T19:48:02Z`).

// These helpers are wired into the sync/cleanup/github paths in the following
// commits; allow them to be unused until then.
#![allow(dead_code)]

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

/// Current time as unix epoch seconds (UTC). Kept as the single `now` call site; pure
/// logic (the age cap) takes the value as a parameter so it stays testable.
pub fn now_unix() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
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
}
