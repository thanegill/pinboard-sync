//! Resolving a setting through its tiers, highest first: the CLI flag or its env var, the
//! account, `[defaults.<source>]`, then the `[pinboard]` or `[hooks]` global.

use crate::cli::DateFlags;
use crate::config::{self, Config};
use crate::jobs::SyncOverrides;
use crate::sync::JobSettings;
use crate::timefmt::DateOpts;

/// Resolve a setting through its precedence tiers, highest first: the CLI flag, else
/// the account override, else the per-source default, else the resolved `[pinboard]`
/// global. The first present (`Some`) value wins; the global is the guaranteed fallback.
pub fn resolve_setting<T>(flag: Option<T>, account: Option<T>, source: Option<T>, global: T) -> T {
    flag.or(account).or(source).unwrap_or(global)
}

/// The `use_post_date` trio for an account, each resolved through [`resolve_setting`]
/// (flag → account → per-source default → `[pinboard]` global, with `max_age_days`
/// defaulting to [`config::DEFAULT_MAX_AGE_DAYS`]). Shared by every `sync`/`cleanup`
/// builder.
pub fn resolve_dates(
    overrides: &DateFlags,
    account: Option<&impl config::Account>,
    src: &config::SourceDefaults,
    config: &Config,
) -> DateOpts {
    DateOpts {
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

/// [`resolve_dates`] for a cleanup pass, refusing a resolution that
/// [`DateOpts::redates_every_run`]. Config validation rules that out for the config tiers
/// alone; this catches the same pairing when a flag supplies either half. `pass` names
/// the pass in the error, e.g. "`cleanup github`".
pub fn resolve_cleanup_dates(
    pass: &str,
    overrides: &DateFlags,
    account: Option<&impl config::Account>,
    src: &config::SourceDefaults,
    config: &Config,
) -> anyhow::Result<DateOpts> {
    let dates = resolve_dates(overrides, account, src, config);
    if dates.redates_every_run() {
        return Err(redates_every_run_error(&format!("{pass} with its flags")));
    }
    Ok(dates)
}

/// Why `scope`'s dating settings are refused. It names the settings, never a value.
pub fn redates_every_run_error(scope: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{scope} resolves `post_date_max_age_days` to 0 with `cleanup_stale_to_now` on, so \
         cleanup would re-date its stale bookmarks on every run; raise the cap or turn \
         `cleanup_stale_to_now` off"
    )
}

/// The per-account settings [`build_job`](crate::jobs::build_job) resolves: the
/// `limit`/`read_later`/`public` trio through [`resolve_setting`] plus the dates through
/// [`resolve_dates`], each on the flag → account → per-source default → `[pinboard]`
/// global ladder. The client, its secrets and the hook come from
/// [`SourceAccount`](crate::jobs::SourceAccount) instead.
pub fn resolve_job_settings(
    account: Option<&impl config::Account>,
    overrides: &SyncOverrides,
    src: &config::SourceDefaults,
    config: &Config,
) -> JobSettings {
    JobSettings {
        limit: resolve_setting(
            overrides.flags.limit,
            account.and_then(|a| a.limit()),
            src.limit,
            config.pinboard.limit.unwrap_or_default(),
        ),
        read_later: resolve_setting(
            overrides.flags.toread,
            account.and_then(|a| a.toread()),
            src.toread,
            config.pinboard.toread,
        ),
        public: resolve_setting(
            overrides.flags.public,
            account.and_then(|a| a.public()),
            src.public,
            config.pinboard.public,
        ),
        dates: resolve_dates(&DateFlags::from(&overrides.flags), account, src, config),
    }
}

/// The auth-failure hook: resolved flag/env (`overrides.on_auth_failure`) → per-account
/// override → per-source default → `[hooks]` global.
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
        // flag → account → source → global, first `Some` wins.
        assert_eq!(resolve_setting(Some(1), Some(2), Some(3), 5), 1); // flag tops all
        assert_eq!(resolve_setting(None, Some(2), Some(3), 5), 2); // account over source
        assert_eq!(resolve_setting(None, None, Some(3), 5), 3); // source over global
        assert_eq!(resolve_setting::<usize>(None, None, None, 5), 5); // global fallback

        // A `Some(false)` flag is a real value, so it forces a `true` lower tier off —
        // the behavior a bare clap flag could not express (this is why `--public[=BOOL]`).
        assert!(!resolve_setting(Some(false), Some(true), Some(true), true));
        assert!(resolve_setting(Some(true), None, None, false));
        assert!(resolve_setting(None, None, None, true)); // global fallback
    }

    #[test]
    fn date_settings_stale_to_now_flag_tops_the_tier() {
        let config = Config::default(); // global cleanup_stale_to_now = false
        let src = &config.defaults.reddit;

        // flag forces it on against a false global
        let overrides = DateFlags {
            stale_to_now: Some(true),
            ..Default::default()
        };
        let d = resolve_dates(&overrides, None::<&RedditAccount>, src, &config);
        assert!(d.stale_to_now);

        // flag forces it off against a true global
        let mut config = Config::default();
        config.pinboard.cleanup_stale_to_now = true;
        let src = &config.defaults.reddit;
        let overrides = DateFlags {
            stale_to_now: Some(false),
            ..Default::default()
        };
        let d = resolve_dates(&overrides, None::<&RedditAccount>, src, &config);
        assert!(!d.stale_to_now);
    }

    #[test]
    fn date_settings_max_age_days_flag_tops_the_tier() {
        let mut config = Config::default();
        config.pinboard.post_date_max_age_days = Some(10); // global
        let src = &config.defaults.reddit; // per-source default None

        // flag wins
        let overrides = DateFlags {
            max_age_days: Some(99),
            ..Default::default()
        };
        let d = resolve_dates(&overrides, None::<&RedditAccount>, src, &config);
        assert_eq!(d.max_age_days, 99);

        // no flag → global
        let d = resolve_dates(&DateFlags::default(), None::<&RedditAccount>, src, &config);
        assert_eq!(d.max_age_days, 10);

        // global unset → built-in default
        let config = Config::default();
        let src = &config.defaults.reddit;
        let d = resolve_dates(&DateFlags::default(), None::<&RedditAccount>, src, &config);
        assert_eq!(d.max_age_days, config::DEFAULT_MAX_AGE_DAYS);
    }
}
