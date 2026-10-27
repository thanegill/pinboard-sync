//! Date conversions for the `use_post_date` option: between unix epoch seconds (how
//! source post dates are carried internally, so the age cap is a plain integer
//! compare) and the RFC 3339 string Pinboard's `dt` expects (e.g.
//! `2010-12-11T19:48:02Z`) — plus the option itself, [`DateOpts`], which sync and every
//! cleanup pass apply, and its resolution through the config tiers ([`resolve_dates`]).

use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use crate::config::{self, resolve_setting, Config};

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

/// Format an [`OffsetDateTime`] as RFC 3339, keeping its offset and any fractional
/// seconds; Pinboard's `dt` goes through [`to_pinboard_dt`] instead. `None` if formatting
/// fails. (Can't be a `From`/`Into` impl: `String` and `OffsetDateTime` are both foreign
/// to this crate, so the orphan rule forbids it — and it's fallible.)
pub fn to_rfc3339(dt: OffsetDateTime) -> Option<String> {
    dt.format(&Rfc3339).ok()
}

/// Format an [`OffsetDateTime`] as Pinboard's `dt`, which its API documents as
/// `CCYY-MM-DDThh:mm:ssZ`: UTC, at whole seconds. `None` if it can't be represented.
pub fn to_pinboard_dt(dt: OffsetDateTime) -> Option<String> {
    dt.checked_to_offset(UtcOffset::UTC)
        .map(OffsetDateTime::truncate_to_second)
        .and_then(to_rfc3339)
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
/// once per account in [`resolve_dates`]; `sync` applies the first two via
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
    /// stale, `stale_to_now` is set and `existing` is unknown or itself outside the cap;
    /// otherwise `existing` (unchanged). The age cap compares by instant; the RFC3339
    /// formatting happens at the write boundary.
    pub fn cleanup_date(
        self,
        candidate: Option<OffsetDateTime>,
        now: OffsetDateTime,
        existing: Option<OffsetDateTime>,
    ) -> Option<OffsetDateTime> {
        if !self.use_post_date {
            return existing;
        }
        let within_cap = |ts: OffsetDateTime| {
            within_age_cap(now.unix_timestamp(), ts.unix_timestamp(), self.max_age_days)
        };
        match candidate {
            Some(ts) if within_cap(ts) => Some(ts),
            Some(_) if self.stale_to_now && !existing.is_some_and(within_cap) => Some(now),
            _ => existing,
        }
    }

    /// Whether [`Self::cleanup_date`] would re-date a stale bookmark on every run rather
    /// than at most once per cap period: with a zero cap every stored date is outside it.
    pub fn redates_every_run(self) -> bool {
        self.stale_to_now && self.max_age_days == 0
    }
}

/// CLI-flag overrides for the date settings, sitting at the top of the tier
/// (flag → account → per-source default → `[pinboard]` global). All-`None` means
/// "no flag given", so resolution falls through to the existing tiers unchanged.
#[derive(Default, Clone)]
pub struct DateOverrides {
    pub use_post_date: Option<bool>,
    pub max_age_days: Option<u64>,
    pub stale_to_now: Option<bool>,
}

impl DateOverrides {
    /// Let a date flag placed before the source subcommand (on `CleanupCmd`) take
    /// effect, falling back to this per-source value. Mirrors sync's
    /// `SyncOverrides::with_top_level_hook`.
    pub fn with_top_level(self, top: &DateOverrides) -> DateOverrides {
        DateOverrides {
            use_post_date: top.use_post_date.or(self.use_post_date),
            max_age_days: top.max_age_days.or(self.max_age_days),
            stale_to_now: top.stale_to_now.or(self.stale_to_now),
        }
    }
}

/// The `use_post_date` trio for an account, each resolved through [`resolve_setting`]
/// (flag → account → per-source default → `[pinboard]` global, with `max_age_days`
/// defaulting to [`config::DEFAULT_MAX_AGE_DAYS`]). Shared by every `sync`/`cleanup`
/// builder.
pub fn resolve_dates(
    over: &DateOverrides,
    account: Option<&impl config::Account>,
    src: &config::SourceDefaults,
    config: &Config,
) -> DateOpts {
    DateOpts {
        use_post_date: resolve_setting(
            over.use_post_date,
            account.and_then(|a| a.use_post_date()),
            src.use_post_date,
            config.pinboard.use_post_date,
        ),
        max_age_days: resolve_setting(
            over.max_age_days,
            account.and_then(|a| a.max_age_days()),
            src.post_date_max_age_days,
            config
                .pinboard
                .post_date_max_age_days
                .unwrap_or(config::DEFAULT_MAX_AGE_DAYS),
        ),
        stale_to_now: resolve_setting(
            over.stale_to_now,
            account.and_then(|a| a.stale_to_now()),
            src.cleanup_stale_to_now,
            config.pinboard.cleanup_stale_to_now,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RedditAccount;

    #[test]
    fn date_settings_stale_to_now_flag_tops_the_tier() {
        let config = Config::default(); // global cleanup_stale_to_now = false
        let src = &config.defaults.reddit;

        // flag forces it on against a false global
        let over = DateOverrides {
            stale_to_now: Some(true),
            ..Default::default()
        };
        let d = resolve_dates(&over, None::<&RedditAccount>, src, &config);
        assert!(d.stale_to_now);

        // flag forces it off against a true global
        let mut config = Config::default();
        config.pinboard.cleanup_stale_to_now = true;
        let src = &config.defaults.reddit;
        let over = DateOverrides {
            stale_to_now: Some(false),
            ..Default::default()
        };
        let d = resolve_dates(&over, None::<&RedditAccount>, src, &config);
        assert!(!d.stale_to_now);
    }

    #[test]
    fn date_settings_max_age_days_flag_tops_the_tier() {
        let mut config = Config::default();
        config.pinboard.post_date_max_age_days = Some(10); // global
        let src = &config.defaults.reddit; // per-source default None

        // flag wins
        let over = DateOverrides {
            max_age_days: Some(99),
            ..Default::default()
        };
        let d = resolve_dates(&over, None::<&RedditAccount>, src, &config);
        assert_eq!(d.max_age_days, 99);

        // no flag → global
        let d = resolve_dates(
            &DateOverrides::default(),
            None::<&RedditAccount>,
            src,
            &config,
        );
        assert_eq!(d.max_age_days, 10);

        // global unset → built-in default
        let config = Config::default();
        let src = &config.defaults.reddit;
        let d = resolve_dates(
            &DateOverrides::default(),
            None::<&RedditAccount>,
            src,
            &config,
        );
        assert_eq!(d.max_age_days, config::DEFAULT_MAX_AGE_DAYS);
    }

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
    fn pinboard_dt_is_utc_at_whole_seconds() {
        let instant = from_unix(1_292_096_882).unwrap() + time::Duration::milliseconds(661);
        let offset = UtcOffset::from_hms(1, 0, 0).unwrap();
        assert_eq!(
            to_pinboard_dt(instant.to_offset(offset)).unwrap(),
            "2010-12-11T19:48:02Z"
        );
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
        // Stale + stale_to_now, but already dated within the cap (e.g. by the previous
        // run's "now") → keep existing, so the pass settles.
        assert_eq!(
            dates(true, true).cleanup_date(Some(old), now, Some(recent)),
            Some(recent)
        );
        // Stale + stale_to_now with no stored date → now.
        assert_eq!(
            dates(true, true).cleanup_date(Some(old), now, None),
            Some(now)
        );
        // No source date → keep existing.
        assert_eq!(
            dates(true, true).cleanup_date(None, now, existing),
            existing
        );
    }

    #[test]
    fn stale_to_now_redates_at_most_once_per_cap_period() {
        let dates = DateOpts {
            use_post_date: true,
            max_age_days: 30,
            stale_to_now: true,
        };
        let stale_source = from_unix(1_500_000_000);
        let first_run = from_unix(1_700_000_000).unwrap();
        let redate = |now: OffsetDateTime, stored| dates.cleanup_date(stale_source, now, stored);

        let after_first = redate(first_run, from_unix(1_546_300_800));
        assert_eq!(after_first, Some(first_run));
        // A run soon after keeps that date.
        let soon_after = first_run + time::Duration::hours(1);
        assert_eq!(redate(soon_after, after_first), after_first);
        // A run more than a cap later re-dates once more, then settles again.
        let a_cap_later = first_run + time::Duration::days(31);
        let after_cap = redate(a_cap_later, after_first);
        assert_eq!(after_cap, Some(a_cap_later));
        assert_eq!(
            redate(a_cap_later + time::Duration::hours(1), after_cap),
            after_cap
        );
    }
}
