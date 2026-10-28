//! Resolving a setting through its tiers, highest first: the CLI flag or its env var, the
//! account, `[defaults.<source>]`, then the `[pinboard]` or `[hooks]` global.

use crate::cli::DateFlags;
use crate::config::{self, Config};
use crate::jobs::SyncOverrides;
use crate::sync::JobSettings;
use crate::timefmt::DateOpts;

/// The tier of the settings ladder a value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Flag,
    Account,
    SourceDefault,
    /// The `[pinboard]` global, or the built-in default it falls back to.
    Global,
}

impl Tier {
    /// The tier as a message names it, for the setting whose CLI flag is `flag`, of
    /// `source`'s accounts. It names where the value was set, never the value.
    pub fn describe(self, flag: &str, source: &str) -> String {
        match self {
            Tier::Flag => format!("`{flag}`"),
            Tier::Account => "the account".to_string(),
            Tier::SourceDefault => format!("`[defaults.{source}]`"),
            Tier::Global => "`[pinboard]`".to_string(),
        }
    }
}

/// A setting's value and the [`Tier`] it was resolved from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved<T> {
    pub value: T,
    pub from: Tier,
}

/// Resolve a setting through its precedence tiers, highest first: the CLI flag, else
/// the account override, else the per-source default, else the resolved `[pinboard]`
/// global. The first present (`Some`) value wins; the global is the guaranteed fallback.
pub fn resolve_setting<T>(
    flag: Option<T>,
    account: Option<T>,
    source: Option<T>,
    global: T,
) -> Resolved<T> {
    [
        (Tier::Flag, flag),
        (Tier::Account, account),
        (Tier::SourceDefault, source),
    ]
    .into_iter()
    .find_map(|(from, value)| {
        Some(Resolved {
            value: value?,
            from,
        })
    })
    .unwrap_or(Resolved {
        value: global,
        from: Tier::Global,
    })
}

/// The `use_post_date` trio as [`resolve_dates`] resolves it, each with its tier.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedDates {
    pub use_post_date: Resolved<bool>,
    pub max_age_days: Resolved<u64>,
    pub stale_to_now: Resolved<bool>,
}

impl ResolvedDates {
    pub fn opts(self) -> DateOpts {
        DateOpts {
            use_post_date: self.use_post_date.value,
            max_age_days: self.max_age_days.value,
            stale_to_now: self.stale_to_now.value,
        }
    }

    /// Refuse dates for which [`DateOpts::redates_every_run`] holds, naming `scope` and
    /// the tier of each half (`source` names its defaults table), never a value.
    pub fn check_redating(self, scope: &str, source: &str) -> anyhow::Result<()> {
        if !self.opts().redates_every_run() {
            return Ok(());
        }
        anyhow::bail!(
            "{scope} resolves `post_date_max_age_days` to 0 (from {}) with \
             `cleanup_stale_to_now` on (from {}), so cleanup would re-date its stale \
             bookmarks on every run; raise the cap or turn `cleanup_stale_to_now` off",
            self.max_age_days
                .from
                .describe("--post-date-max-age-days", source),
            self.stale_to_now
                .from
                .describe("--cleanup-stale-to-now", source),
        )
    }
}

/// The `use_post_date` trio for an account, each resolved through [`resolve_setting`]
/// (flag -> account -> per-source default -> `[pinboard]` global, with `max_age_days`
/// defaulting to [`config::DEFAULT_MAX_AGE_DAYS`]). Shared by every `sync`/`cleanup`
/// builder.
pub fn resolve_dates<A: config::Account>(
    overrides: &DateFlags,
    account: Option<&A>,
    config: &Config,
) -> ResolvedDates {
    let src = A::defaults(config);
    ResolvedDates {
        use_post_date: resolve_setting(
            overrides.use_post_date,
            account.and_then(|a| a.use_post_date()),
            src.use_post_date,
            config.pinboard.use_post_date,
        ),
        max_age_days: resolve_setting(
            overrides.max_age_days,
            account.and_then(|a| a.max_age_days()),
            src.post_date_max_age_days,
            config
                .pinboard
                .post_date_max_age_days
                .unwrap_or(config::DEFAULT_MAX_AGE_DAYS),
        ),
        stale_to_now: resolve_setting(
            overrides.stale_to_now,
            account.and_then(|a| a.stale_to_now()),
            src.cleanup_stale_to_now,
            config.pinboard.cleanup_stale_to_now,
        ),
    }
}

/// [`resolve_dates`] for a cleanup pass of an `A` account, refused by
/// [`ResolvedDates::check_redating`]. Config validation rules the pairing out for the
/// config tiers alone; this catches it when a flag supplies either half. `pass` names the
/// pass in the error, e.g. "`cleanup github`".
pub fn resolve_cleanup_dates<A: config::Account>(
    pass: &str,
    overrides: &DateFlags,
    account: Option<&A>,
    config: &Config,
) -> anyhow::Result<DateOpts> {
    let dates = resolve_dates(overrides, account, config);
    dates.check_redating(pass, A::SOURCE)?;
    Ok(dates.opts())
}

/// The per-account settings [`build_job`](crate::jobs::build_job) resolves: the
/// `limit`/`read_later`/`public` trio through [`resolve_setting`] plus the dates through
/// [`resolve_dates`], each on the flag -> account -> per-source default -> `[pinboard]`
/// global ladder. The client, its secrets and the hook come from
/// [`SourceAccount`](crate::jobs::SourceAccount) instead.
pub fn resolve_job_settings<A: config::Account>(
    account: Option<&A>,
    overrides: &SyncOverrides,
    config: &Config,
) -> JobSettings {
    let src = A::defaults(config);
    JobSettings {
        limit: resolve_setting(
            overrides.flags.limit,
            account.and_then(|a| a.limit()),
            src.limit,
            config.pinboard.limit.unwrap_or_default(),
        )
        .value,
        read_later: resolve_setting(
            overrides.flags.toread,
            account.and_then(|a| a.toread()),
            src.toread,
            config.pinboard.toread,
        )
        .value,
        public: resolve_setting(
            overrides.flags.public,
            account.and_then(|a| a.public()),
            src.public,
            config.pinboard.public,
        )
        .value,
        dates: resolve_dates(&DateFlags::from(&overrides.flags), account, config).opts(),
    }
}

/// The auth-failure hook: resolved flag/env (`overrides.on_auth_failure`) -> per-account
/// override -> per-source default -> `[hooks]` global.
pub fn resolve_hook(
    flag: Option<String>,
    account_override: Option<&str>,
    source_override: Option<&str>,
    config: &Config,
) -> Option<String> {
    flag.or_else(|| account_override.map(str::to_string))
        .or_else(|| source_override.map(str::to_string))
        .or_else(|| config.hooks.on_auth_failure.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RedditAccount;

    #[test]
    fn resolve_setting_picks_the_highest_present_tier() {
        // flag -> account -> source -> global, first `Some` wins, and says which it was.
        let resolved = |value, from| Resolved { value, from };
        assert_eq!(
            resolve_setting(Some(1), Some(2), Some(3), 5),
            resolved(1, Tier::Flag)
        );
        assert_eq!(
            resolve_setting(None, Some(2), Some(3), 5),
            resolved(2, Tier::Account)
        );
        assert_eq!(
            resolve_setting(None, None, Some(3), 5),
            resolved(3, Tier::SourceDefault)
        );
        assert_eq!(
            resolve_setting(None, None, None, 5),
            resolved(5, Tier::Global)
        );

        // A `Some(false)` flag is a real value, so it forces a `true` lower tier off --
        // the behavior a bare switch could not express (this is why `--public[=BOOL]`).
        assert!(!resolve_setting(Some(false), Some(true), Some(true), true).value);
        assert!(resolve_setting(Some(true), None, None, false).value);
        assert!(resolve_setting(None, None, None, true).value); // global fallback
    }

    #[test]
    fn date_settings_stale_to_now_flag_tops_the_tier() {
        let config = Config::default(); // global cleanup_stale_to_now = false

        // flag forces it on against a false global
        let overrides = DateFlags {
            stale_to_now: Some(true),
            ..Default::default()
        };
        let d = resolve_dates(&overrides, None::<&RedditAccount>, &config).opts();
        assert!(d.stale_to_now);

        // flag forces it off against a true global
        let mut config = Config::default();
        config.pinboard.cleanup_stale_to_now = true;
        let overrides = DateFlags {
            stale_to_now: Some(false),
            ..Default::default()
        };
        let d = resolve_dates(&overrides, None::<&RedditAccount>, &config).opts();
        assert!(!d.stale_to_now);
    }

    #[test]
    fn date_settings_max_age_days_flag_tops_the_tier() {
        let mut config = Config::default();
        config.pinboard.post_date_max_age_days = Some(10); // global

        // flag wins
        let overrides = DateFlags {
            max_age_days: Some(99),
            ..Default::default()
        };
        let d = resolve_dates(&overrides, None::<&RedditAccount>, &config).opts();
        assert_eq!(d.max_age_days, 99);

        // no flag -> global
        let d = resolve_dates(&DateFlags::default(), None::<&RedditAccount>, &config).opts();
        assert_eq!(d.max_age_days, 10);

        // global unset -> built-in default
        let config = Config::default();
        let d = resolve_dates(&DateFlags::default(), None::<&RedditAccount>, &config).opts();
        assert_eq!(d.max_age_days, config::DEFAULT_MAX_AGE_DAYS);
    }
}
