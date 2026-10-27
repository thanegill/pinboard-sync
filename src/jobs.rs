//! `sync` jobs: one per account, each a configured source client plus its auth-failure
//! hook and settings resolved through the flag → account → `[defaults.<source>]` →
//! `[pinboard]` ladder. [`SourceAccount`] is what lets `sync`, `backup` and `doctor` treat
//! the three sources through one generic path. The Pinboard client is built here too
//! ([`open_pinboard`]), as is [`AllRun`], which tallies an `--all` run's failures, and
//! [`backup_jobs`] builds `backup`'s targets on the same path.

pub mod backup_jobs;

use anyhow::{bail, Context, Result};
use log::{debug, info};
use url::Url;

use crate::backup::{self, BackupSource};
use crate::bookmark::{Bookmark, BookmarkStore};
use crate::cli::SyncFlags;
use crate::config::{
    self, resolve_setting, Config, GitHubAccount, HackerNewsAccount, RedditAccount, SyncLimit,
};
use crate::github::{GitHubClient, GitHubConfig};
use crate::hackernews::{HackerNewsClient, HackerNewsConfig};
use crate::hook::resolve_hook;
use crate::pinboard::{PinboardClient, RATE_LIMIT_SECS};
use crate::reddit::model::RedditConfig;
use crate::reddit::RedditClient;
use crate::secrets::{
    self, missing_github_token, resolve_github_token, resolve_pinboard_token,
    resolve_reddit_cookie, resolve_secret, EnvSecret,
};
use crate::source::{BookmarkDraft, Source, SourceError, UrlKey};
use crate::sync::JobSettings;
use crate::timefmt::{resolve_dates, DateOverrides};

/// Build one job per configured account across every source for `sync --all`,
/// isolating build failures: an account whose required secret won't resolve is
/// reported and counted rather than aborting the whole run, so the healthy
/// accounts still sync. Returns the buildable jobs and the count of accounts
/// that failed to build, which the caller threads into the run's exit code.
pub fn build_all_jobs(config: &Config, overrides: &SyncOverrides) -> (Vec<SyncJob>, usize) {
    let mut jobs = Vec::new();
    let mut run = AllRun::default();
    build_every_job::<RedditAccount>(config, overrides, &mut jobs, &mut run);
    build_every_job::<GitHubAccount>(config, overrides, &mut jobs, &mut run);
    build_every_job::<HackerNewsAccount>(config, overrides, &mut jobs, &mut run);
    (jobs, run.failed)
}

/// [`build_all_jobs`] for one source: a job for each of its configured accounts, with each
/// account that fails to build recorded in `run` instead.
fn build_every_job<A: SourceAccount>(
    config: &Config,
    overrides: &SyncOverrides,
    jobs: &mut Vec<SyncJob>,
    run: &mut AllRun,
) {
    warn_shared_env_secrets::<A>(overrides, config);
    for account in A::configured(config) {
        match build_job(Some(account), overrides, config) {
            Ok(job) => jobs.push(job),
            Err(e) => run.record(Err(
                e.context(format!("{} could not be built", job_label(Some(account))))
            )),
        }
    }
}

/// Warn when an environment secret would stand in for the own value of every configured
/// account of `A`, through [`secrets::warn_if_env_shared`].
pub fn warn_shared_env_secrets<A: SourceAccount>(overrides: &SyncOverrides, config: &Config) {
    secrets::warn_if_env_shared(
        A::SOURCE,
        &A::env_secrets(overrides),
        A::configured(config).len(),
    );
}

/// The secrets among `flagged` whose CLI flag holds no value the ladder would take, so
/// that the environment rung is the highest one that can apply.
fn unflagged(flagged: &[(EnvSecret, &Option<String>)]) -> Vec<EnvSecret> {
    flagged
        .iter()
        .filter(|(secret, flag)| !flag.as_deref().is_some_and(|value| secret.accepts(value)))
        .map(|(secret, _)| *secret)
        .collect()
}

/// Build one job per account: every account when `all`, else the named (or first,
/// or implicit CLI/env) account. Under `all`, as in [`build_all_jobs`], an account that
/// fails to build is reported and counted rather than aborting the rest; the count is
/// returned alongside the jobs.
pub fn build_jobs<A: SourceAccount>(
    name: Option<&str>,
    all: bool,
    overrides: &SyncOverrides,
    config: &Config,
) -> Result<(Vec<SyncJob>, usize)> {
    let accounts = A::configured(config);
    if all {
        if accounts.is_empty() {
            bail!("--all requires a --config with at least one configured account");
        }
        let mut jobs = Vec::new();
        let mut run = AllRun::default();
        build_every_job::<A>(config, overrides, &mut jobs, &mut run);
        Ok((jobs, run.failed))
    } else {
        let account = config::select_account(accounts, name)?;
        Ok((vec![build_job(account, overrides, config)?], 0))
    }
}

#[derive(Default)]
pub struct SyncOverrides {
    pub reddit_username: Option<String>,
    pub reddit_cookie: Option<String>,
    pub github_token: Option<String>,
    pub hackernews_username: Option<String>,
    pub on_auth_failure: Option<String>,
    pub flags: SyncFlags,
    pub dry_run: bool,
}

impl SyncOverrides {
    /// Let a `--on-auth-failure` placed before the source subcommand (on
    /// `SyncCmd`) take effect, falling back to the per-source value. Mirrors the
    /// `--dry-run` merge in `run_sync`, which honors either placement too.
    pub fn with_top_level_hook(mut self, on_auth_failure: Option<String>) -> Self {
        self.on_auth_failure = on_auth_failure.or(self.on_auth_failure);
        self
    }
}

/// A configured source client ready to fetch, plus its auth-failure hook and the
/// account's resolved write settings.
pub struct SyncJob {
    pub client: SourceClient,
    /// Human label for logs, e.g. `reddit[alice]`.
    pub label: String,
    pub hook: Option<String>,
    pub settings: JobSettings,
}

impl SyncJob {
    /// The source's drafts: read incrementally against `existing` under
    /// [`SyncLimit::Auto`], else in full.
    pub async fn fetch(&self, existing: &[Bookmark]) -> Result<Vec<BookmarkDraft>, SourceError> {
        match self.settings.limit {
            SyncLimit::Auto => self.client.fetch_incremental(existing).await,
            SyncLimit::All | SyncLimit::Max(_) => self.client.fetch().await,
        }
    }
}

/// A log/display label for a job: `source[account]`, or `source[default]` when no
/// named account (an implicit CLI/env invocation).
fn job_label<A: SourceAccount>(account: Option<&A>) -> String {
    format!(
        "{}[{}]",
        A::SOURCE,
        account.and_then(|a| a.account_name()).unwrap_or("default")
    )
}

/// One of the concrete source clients, unified behind the `Source` port so `--all`
/// can fetch them concurrently and the dispatch can treat them uniformly.
pub enum SourceClient {
    Reddit(RedditClient),
    GitHub(GitHubClient),
    HackerNews(HackerNewsClient),
}

impl Source for SourceClient {
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError> {
        match self {
            SourceClient::Reddit(c) => c.fetch().await,
            SourceClient::GitHub(c) => c.fetch().await,
            SourceClient::HackerNews(c) => c.fetch().await,
        }
    }

    async fn fetch_incremental(
        &self,
        existing: &[Bookmark],
    ) -> Result<Vec<BookmarkDraft>, SourceError> {
        match self {
            SourceClient::Reddit(c) => c.fetch_incremental(existing).await,
            SourceClient::GitHub(c) => c.fetch_incremental(existing).await,
            SourceClient::HackerNews(c) => c.fetch_incremental(existing).await,
        }
    }
}

impl BackupSource for SourceClient {
    async fn dump(&self) -> Result<backup::BackupDump, SourceError> {
        match self {
            SourceClient::Reddit(c) => c.dump().await,
            SourceClient::GitHub(c) => c.dump().await,
            SourceClient::HackerNews(c) => c.dump().await,
        }
    }
}

impl UrlKey for SourceClient {
    fn dedup_key(&self, url: &Url) -> Option<String> {
        match self {
            SourceClient::Reddit(c) => c.dedup_key(url),
            SourceClient::GitHub(c) => c.dedup_key(url),
            SourceClient::HackerNews(c) => c.dedup_key(url),
        }
    }
}

/// The per-account settings every `build_*_job` resolves the same way: the
/// `limit`/`toread`/`shared` trio through [`resolve_setting`] plus the dates through
/// [`resolve_dates`], each on the flag → account → per-source default → `[pinboard]`
/// global ladder. Leaves client/secret/hook construction to each source builder.
fn resolve_job_settings(
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
        toread: resolve_setting(
            overrides.flags.toread,
            account.and_then(|a| a.toread()),
            src.toread,
            config.pinboard.toread,
        ),
        shared: resolve_setting(
            overrides.flags.public,
            account.and_then(|a| a.public()),
            src.public,
            config.pinboard.public,
        ),
        dates: resolve_dates(&DateOverrides::from(&overrides.flags), account, src, config),
    }
}

/// What `sync`, `backup` and `doctor` need from one source's account type, so each
/// handles the three sources through one generic path: where its accounts and per-source
/// defaults live in the config, how its client is built from an account plus the CLI
/// overrides, and which auth-failure hook its jobs carry.
pub trait SourceAccount: config::Account + Sized {
    /// The source's name in job labels, backup file stems and `doctor` lines.
    const SOURCE: &'static str;

    /// The source's configured accounts (`[[reddit]]` and so on).
    fn configured(config: &Config) -> &[Self];

    /// The source's `[defaults.<source>]` tier.
    fn defaults(config: &Config) -> &config::SourceDefaults;

    /// The account's own `on_auth_failure` override.
    fn on_auth_failure(&self) -> Option<&str>;

    /// The secrets [`Self::client`] resolves whose CLI flag in `overrides` holds no usable
    /// value, so that the environment outranks each account's own value.
    fn env_secrets(overrides: &SyncOverrides) -> Vec<EnvSecret>;

    /// The client for `account`, or for the implicit CLI/env account when `None`, with
    /// its credentials resolved through the secret ladder. `config` is the whole config,
    /// for what one source needs to know of another's accounts.
    fn client(
        account: Option<&Self>,
        overrides: &SyncOverrides,
        config: &Config,
    ) -> Result<SourceClient>;

    /// The auth-failure hook for `account`: `top_hook` (the flag or env value the command
    /// resolved), else the account's, else the per-source default, else `[hooks]`. Both
    /// `sync` jobs and `cleanup` resolve it here, so a source that never re-authenticates
    /// can rule a hook out for every command at once.
    fn hook(account: Option<&Self>, top_hook: Option<String>, config: &Config) -> Option<String> {
        resolve_hook(
            top_hook,
            account.and_then(Self::on_auth_failure),
            Self::defaults(config).on_auth_failure.as_deref(),
            config,
        )
    }
}

impl SourceAccount for RedditAccount {
    const SOURCE: &'static str = "reddit";

    fn configured(config: &Config) -> &[Self] {
        &config.reddit
    }

    fn defaults(config: &Config) -> &config::SourceDefaults {
        &config.defaults.reddit
    }

    fn on_auth_failure(&self) -> Option<&str> {
        self.on_auth_failure.as_deref()
    }

    fn env_secrets(overrides: &SyncOverrides) -> Vec<EnvSecret> {
        unflagged(&[
            (EnvSecret::REDDIT_USERNAME, &overrides.reddit_username),
            (EnvSecret::REDDIT_COOKIE, &overrides.reddit_cookie),
        ])
    }

    fn client(
        account: Option<&Self>,
        overrides: &SyncOverrides,
        config: &Config,
    ) -> Result<SourceClient> {
        let username = resolve_secret(
            overrides.reddit_username.clone(),
            "REDDIT_USERNAME",
            account.and_then(|a| a.username.clone()),
            None,
        )
        .context("missing Reddit username (set --reddit-username, REDDIT_USERNAME, or `username` in the config)")?;
        let cookie = resolve_reddit_cookie(overrides.reddit_cookie.clone(), account);
        let reddit_config = account.map(RedditConfig::from).unwrap_or_default();
        Ok(SourceClient::Reddit(
            RedditClient::for_user(username, cookie, reddit_config)?
                .with_reddit_domains(config.reddit_domains()),
        ))
    }
}

impl SourceAccount for GitHubAccount {
    const SOURCE: &'static str = "github";

    fn configured(config: &Config) -> &[Self] {
        &config.github
    }

    fn defaults(config: &Config) -> &config::SourceDefaults {
        &config.defaults.github
    }

    fn on_auth_failure(&self) -> Option<&str> {
        self.on_auth_failure.as_deref()
    }

    fn env_secrets(overrides: &SyncOverrides) -> Vec<EnvSecret> {
        unflagged(&[(EnvSecret::GITHUB_TOKEN, &overrides.github_token)])
    }

    fn client(
        account: Option<&Self>,
        overrides: &SyncOverrides,
        _: &Config,
    ) -> Result<SourceClient> {
        let token = resolve_github_token(overrides.github_token.clone(), account)
            .with_context(missing_github_token)?;
        let github_config = account.map(GitHubConfig::from).unwrap_or_default();
        Ok(SourceClient::GitHub(GitHubClient::new(
            token,
            github_config,
        )?))
    }
}

impl SourceAccount for HackerNewsAccount {
    const SOURCE: &'static str = "hackernews";

    fn configured(config: &Config) -> &[Self] {
        &config.hackernews
    }

    fn defaults(config: &Config) -> &config::SourceDefaults {
        &config.defaults.hackernews
    }

    fn on_auth_failure(&self) -> Option<&str> {
        None
    }

    fn env_secrets(overrides: &SyncOverrides) -> Vec<EnvSecret> {
        unflagged(&[(EnvSecret::HN_USERNAME, &overrides.hackernews_username)])
    }

    fn client(
        account: Option<&Self>,
        overrides: &SyncOverrides,
        config: &Config,
    ) -> Result<SourceClient> {
        let username = resolve_secret(
            overrides.hackernews_username.clone(),
            "HN_USERNAME",
            account.and_then(|a| a.username.clone()),
            None,
        )
        .context(
            "missing HackerNews username (set --username, HN_USERNAME, or `username` in the config)",
        )?;
        let hn_config = account.map(HackerNewsConfig::from).unwrap_or_default();
        Ok(SourceClient::HackerNews(
            HackerNewsClient::new(username, hn_config)?
                .with_reddit_domains(config.reddit_domains()),
        ))
    }

    /// None: HackerNews favorites are public, so nothing can need re-authenticating.
    fn hook(_: Option<&Self>, _: Option<String>, _: &Config) -> Option<String> {
        None
    }
}

/// The `sync` job for `account` (or the implicit CLI/env account): its client, label and
/// hook, and its settings resolved through [`resolve_job_settings`].
pub fn build_job<A: SourceAccount>(
    account: Option<&A>,
    overrides: &SyncOverrides,
    config: &Config,
) -> Result<SyncJob> {
    Ok(SyncJob {
        client: A::client(account, overrides, config)?,
        label: job_label(account),
        hook: A::hook(account, overrides.on_auth_failure.clone(), config),
        settings: resolve_job_settings(account, overrides, A::defaults(config), config),
    })
}

/// Resolve the Pinboard token and build the client (no fetch).
fn build_pinboard_client(token_flag: Option<String>, config: &Config) -> Result<PinboardClient> {
    let token = resolve_pinboard_token(token_flag, &config.pinboard)
        .context("missing Pinboard token (set --pinboard-token, PINBOARD_TOKEN/_FILE, or [pinboard] in the config)")?;
    pinboard_client_for_token(token, config)
}

/// The client for an already-resolved token, at the configured write pacing.
fn pinboard_client_for_token(token: String, config: &Config) -> Result<PinboardClient> {
    let rate_limit = config.pinboard.rate_limit_secs.unwrap_or(RATE_LIMIT_SECS);
    PinboardClient::new(token, rate_limit)
}

/// Build the client and fetch `posts/all` once. Returns the client and the bookmark
/// set to share across every account in a run (so `posts/all` — the most rate-limited
/// endpoint — is hit once, not once per account).
pub async fn open_pinboard(
    token_flag: Option<String>,
    config: &Config,
) -> Result<(PinboardClient, Vec<Bookmark>)> {
    let pinboard = build_pinboard_client(token_flag, config)?;
    debug!("fetching existing bookmarks from Pinboard (posts/all)");
    let bookmarks = pinboard.all().await.context("listing Pinboard bookmarks")?;
    info!("pinboard: {} existing bookmark(s)", bookmarks.len());
    Ok((pinboard, bookmarks))
}

/// Accumulates failures across an `--all` run: each account is attempted, errors
/// are reported and counted, and the run errors at the end if any account failed.
#[derive(Default)]
pub struct AllRun {
    pub failed: usize,
}

impl AllRun {
    pub fn record(&mut self, result: Result<()>) {
        if let Err(e) = result {
            eprintln!("error: {e:#}");
            self.failed += 1;
        }
    }

    pub fn finish(self) -> Result<()> {
        if self.failed > 0 {
            bail!("{} account(s) failed", self.failed);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::warnings;
    use std::num::NonZeroUsize;

    /// Override values chosen so each setting is distinct from the others and from the
    /// `Config::default()` globals — so a builder that wires a flag into the *wrong*
    /// `SyncJob` field (e.g. `public` into the `toread` slot) fails the assertions.
    fn wiring_overrides() -> SyncOverrides {
        SyncOverrides {
            flags: SyncFlags {
                limit: Some(SyncLimit::Max(NonZeroUsize::new(7).unwrap())),
                toread: Some(true),        // global default is false
                public: Some(false),       // distinct from toread, so a swap is caught
                use_post_date: Some(true), // global default is false
                max_age_days: Some(99),    // global default is 30
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn assert_wired(job: &SyncJob) {
        let settings = &job.settings;
        assert_eq!(
            settings.limit,
            SyncLimit::Max(NonZeroUsize::new(7).unwrap()),
            "limit"
        );
        assert!(settings.toread, "toread");
        assert!(!settings.shared, "shared (from --public=false)");
        assert!(settings.dates.use_post_date, "use_post_date");
        assert_eq!(settings.dates.max_age_days, 99, "max_age_days");
    }

    /// The write settings of a job a test only fetches through: everything off but `limit`.
    pub(super) fn fetch_only(limit: SyncLimit) -> JobSettings {
        JobSettings {
            limit,
            toread: false,
            shared: false,
            dates: crate::test_support::NO_DATING,
        }
    }

    #[test]
    fn build_job_wires_reddit_overrides_to_job_fields() {
        let overrides = SyncOverrides {
            reddit_username: Some("alice".into()),
            reddit_cookie: Some("reddit_session=x".into()),
            ..wiring_overrides()
        };
        let job = build_job::<RedditAccount>(None, &overrides, &Config::default()).expect("builds");
        assert_wired(&job);
    }

    #[test]
    fn build_job_wires_github_overrides_to_job_fields() {
        let overrides = SyncOverrides {
            github_token: Some("ghp_test".into()),
            ..wiring_overrides()
        };
        let job = build_job::<GitHubAccount>(None, &overrides, &Config::default()).expect("builds");
        assert_wired(&job);
    }

    #[test]
    fn build_job_wires_hackernews_overrides_to_job_fields() {
        let overrides = SyncOverrides {
            hackernews_username: Some("alice".into()),
            ..wiring_overrides()
        };
        let job =
            build_job::<HackerNewsAccount>(None, &overrides, &Config::default()).expect("builds");
        assert_wired(&job);
    }

    #[test]
    fn a_hackernews_job_keys_articles_on_a_configured_reddit_domain_as_reddit() {
        let config = Config {
            reddit: vec![RedditAccount {
                reddit_domain: Some("reddit.mirror.example".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let overrides = SyncOverrides {
            hackernews_username: Some("alice".into()),
            ..Default::default()
        };
        let job = build_job::<HackerNewsAccount>(None, &overrides, &config).expect("builds");
        assert_eq!(
            job.client
                .dedup_key(&crate::test_support::url(
                    "https://reddit.mirror.example/r/rust/comments/abc/title/"
                ))
                .as_deref(),
            Some("/r/rust/comments/abc/title")
        );
    }

    /// Another account's sync, or a `cleanup reddit` run as it, leaves a save on that
    /// account's mirror; this account's sync must still see it as the same save.
    #[test]
    fn a_save_on_another_reddit_accounts_mirror_is_not_re_added() {
        use crate::test_support::stored_at;

        let account = |name: &str, domain: &str| RedditAccount {
            username: Some(name.into()),
            cookie: Some("reddit_session=x".into()),
            reddit_domain: Some(domain.into()),
            ..Default::default()
        };
        let config = Config {
            reddit: vec![
                account("alice", "www.reddit.com"),
                account("bob", "reddit.mirror.example"),
            ],
            ..Default::default()
        };
        let alices_save = BookmarkDraft {
            bookmark: stored_at("https://www.reddit.com/r/rust/comments/abc/title/"),
            dedup_key: "/r/rust/comments/abc/title".into(),
        };
        let stored = [stored_at(
            "https://reddit.mirror.example/r/rust/comments/abc/title/",
        )];

        let alices =
            build_job(Some(&config.reddit[0]), &SyncOverrides::default(), &config).expect("builds");
        let new = crate::sync::prepare_new_drafts(
            &alices.client,
            vec![alices_save],
            &stored,
            &fetch_only(SyncLimit::All),
            0,
        );
        assert!(new.is_empty(), "re-added: {:?}", new[0].bookmark.url);
    }

    /// `sync --all`'s build phase must isolate a per-account build failure so the
    /// healthy accounts still yield jobs. Here a GitHub account with no resolvable
    /// token (no inline token, no `token_file`, and this test sets no fake `$GITHUB_TOKEN`)
    /// fails to build; it must be counted, not abort the whole run.
    #[test]
    fn build_all_jobs_isolates_a_failing_account() {
        let config = Config {
            reddit: vec![RedditAccount {
                username: Some("alice".into()),
                cookie: Some("reddit_session=x".into()),
                ..Default::default()
            }],
            github: vec![GitHubAccount::default()],
            ..Default::default()
        };
        let (jobs, prebuilt_failures) = build_all_jobs(&config, &SyncOverrides::default());
        assert_eq!(jobs.len(), 1, "healthy reddit account still builds");
        assert_eq!(jobs[0].label, "reddit[alice]");
        assert!(
            prebuilt_failures >= 1,
            "the tokenless github account is counted, not aborted"
        );
    }

    #[test]
    fn build_jobs_for_every_account_of_a_source_isolates_a_failing_account() {
        let config = Config {
            github: vec![
                GitHubAccount {
                    name: Some("tokenless".into()),
                    ..Default::default()
                },
                GitHubAccount {
                    name: Some("main".into()),
                    token: Some("token".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let (jobs, prebuilt_failures) =
            build_jobs::<GitHubAccount>(None, true, &SyncOverrides::default(), &config)
                .expect("a failing account does not abort the source");
        let labels: Vec<&str> = jobs.iter().map(|job| job.label.as_str()).collect();
        assert_eq!(labels, ["github[main]"]);
        assert_eq!(prebuilt_failures, 1);
    }

    fn reddit_account(name: &str) -> RedditAccount {
        RedditAccount {
            name: Some(name.into()),
            username: Some(name.into()),
            cookie: Some(format!("reddit_session={name}")),
            ..Default::default()
        }
    }

    /// `$REDDIT_COOKIE_FILE` outranks each account's own cookie, so with two accounts both
    /// silently use it: worth one warning for the source, naming the setting, not its value.
    #[tokio::test]
    async fn an_env_secret_shared_by_several_accounts_is_warned_about_once() {
        let scratch = crate::test_support::scratch_dir("shared-env-secret");
        let cookie_file = scratch.path().join("cookie");
        std::fs::write(&cookie_file, "reddit_session=SHAREDVALUE\n").unwrap();
        let _env = crate::secrets::FakeSecretEnv::set(&[(
            "REDDIT_COOKIE_FILE",
            cookie_file.to_str().unwrap(),
        )]);
        let config = Config {
            reddit: vec![reddit_account("alice"), reddit_account("bob")],
            ..Default::default()
        };

        let ((jobs, _), logs) = crate::test_support::capturing_logs(async {
            build_all_jobs(&config, &SyncOverrides::default())
        })
        .await;

        assert_eq!(jobs.len(), 2);
        assert_eq!(
            warnings(&logs),
            [
                "WARN $REDDIT_COOKIE_FILE is set in the environment, so all 2 configured \
                 reddit accounts use it, whatever their own config sets"
            ]
        );
    }

    #[tokio::test]
    async fn an_env_secret_is_not_warned_about_for_one_account_or_under_its_flag() {
        let _env = crate::secrets::FakeSecretEnv::set(&[("REDDIT_COOKIE", "reddit_session=x")]);
        let one = Config {
            reddit: vec![reddit_account("alice")],
            ..Default::default()
        };
        let two = Config {
            reddit: vec![reddit_account("alice"), reddit_account("bob")],
            ..Default::default()
        };
        let flagged = SyncOverrides {
            reddit_cookie: Some("reddit_session=flag".into()),
            ..Default::default()
        };

        let (_, logs) = crate::test_support::capturing_logs(async {
            build_all_jobs(&one, &SyncOverrides::default());
            build_all_jobs(&two, &flagged);
        })
        .await;

        assert_eq!(warnings(&logs), Vec::<&str>::new());
    }

    /// The ladder skips an env value it would not take (an empty `_FILE`, a cookie naming no
    /// session), so each account keeps its own and there is nothing to warn about.
    #[tokio::test]
    async fn an_env_secret_the_ladder_skips_is_not_warned_about() {
        let scratch = crate::test_support::scratch_dir("skipped-env-secret");
        let empty_file = scratch.path().join("empty");
        std::fs::write(&empty_file, "\n").unwrap();
        let _env = crate::secrets::FakeSecretEnv::set(&[
            ("REDDIT_COOKIE", "reddit_session="),
            ("REDDIT_USERNAME_FILE", empty_file.to_str().unwrap()),
        ]);
        let config = Config {
            reddit: vec![reddit_account("alice"), reddit_account("bob")],
            ..Default::default()
        };

        let (_, logs) = crate::test_support::capturing_logs(async {
            build_all_jobs(&config, &SyncOverrides::default())
        })
        .await;

        assert_eq!(warnings(&logs), Vec::<&str>::new());
    }

    /// The warning names the rung the ladder takes: `$REDDIT_COOKIE` names no session, so
    /// the cookie comes from `$REDDIT_COOKIE_FILE`. A blank `--github-token` falls through
    /// to `$GITHUB_TOKEN`, so that is warned about too.
    #[tokio::test]
    async fn an_env_secret_warning_names_the_rung_the_ladder_takes() {
        let scratch = crate::test_support::scratch_dir("env-secret-rung");
        let cookie_file = scratch.path().join("cookie");
        std::fs::write(&cookie_file, "reddit_session=SHAREDVALUE\n").unwrap();
        let _env = crate::secrets::FakeSecretEnv::set(&[
            ("REDDIT_COOKIE", "reddit_session="),
            ("REDDIT_COOKIE_FILE", cookie_file.to_str().unwrap()),
            ("GITHUB_TOKEN", "SHAREDVALUE"),
        ]);
        let github_account = |name: &str| GitHubAccount {
            name: Some(name.into()),
            ..Default::default()
        };
        let config = Config {
            reddit: vec![reddit_account("alice"), reddit_account("bob")],
            github: vec![github_account("a"), github_account("b")],
            ..Default::default()
        };
        let blank_token_flag = SyncOverrides {
            github_token: Some(" ".into()),
            ..Default::default()
        };

        let (_, logs) = crate::test_support::capturing_logs(async {
            build_all_jobs(&config, &blank_token_flag)
        })
        .await;

        assert_eq!(
            warnings(&logs),
            [
                "WARN $REDDIT_COOKIE_FILE is set in the environment, so all 2 configured \
                 reddit accounts use it, whatever their own config sets",
                "WARN $GITHUB_TOKEN is set in the environment, so all 2 configured github \
                 accounts use it, whatever their own config sets",
            ]
        );
    }
}

/// Tests that need a socket; the flake skips `net_tests` in the Nix build sandbox.
#[cfg(test)]
mod net_tests {
    use super::tests::fetch_only;
    use super::*;
    use serde_json::json;
    use std::num::NonZeroUsize;
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Fetches through a HackerNews `SyncJob` capped at `limit`, against favorites whose
    /// first page (both streams) is already synced and links to a second page that is
    /// expected `second_page_hits` times.
    async fn fetch_hackernews_job(limit: SyncLimit, second_page_hits: u64) {
        let hn = MockServer::start().await;
        let page = |rows: &str| {
            ResponseTemplate::new(200).set_body_string(format!(
                r#"<html op="favorites"><body><table>{rows}</table></body></html>"#
            ))
        };
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .and(query_param_is_missing("p"))
            .respond_with(page(
                r#"<tr class="athing" id="1"><td>x</td></tr>
                   <tr><td><a class="morelink" href="favorites?id=u&amp;p=2">More</a></td></tr>"#,
            ))
            .expect(2)
            .mount(&hn)
            .await;
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .and(query_param("p", "2"))
            .respond_with(page(r#"<tr class="athing" id="2"><td>x</td></tr>"#))
            .expect(second_page_hits)
            .mount(&hn)
            .await;
        let algolia = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "hits": [
                { "objectID": "1", "title": "Ask HN: one", "author": "a", "_tags": ["story"] },
                { "objectID": "2", "title": "Ask HN: two", "author": "a", "_tags": ["story"] },
            ] })))
            .mount(&algolia)
            .await;
        let job = SyncJob {
            client: SourceClient::HackerNews(HackerNewsClient::with_base_urls(
                "u".into(),
                HackerNewsConfig::default(),
                hn.uri(),
                algolia.uri(),
            )),
            label: "hackernews[u]".into(),
            hook: None,
            settings: fetch_only(limit),
        };
        let existing = [crate::test_support::stored_at(
            "https://news.ycombinator.com/item?id=1",
        )];

        crate::http::recording_waits(job.fetch(&existing))
            .await
            .0
            .unwrap();
    }

    #[tokio::test]
    async fn an_auto_sync_job_fetches_hackernews_incrementally() {
        fetch_hackernews_job(SyncLimit::Auto, 0).await;
    }

    #[tokio::test]
    async fn an_all_sync_job_reads_every_favorites_page() {
        fetch_hackernews_job(SyncLimit::All, 2).await;
    }

    #[tokio::test]
    async fn a_capped_sync_job_reads_every_favorites_page() {
        fetch_hackernews_job(SyncLimit::Max(NonZeroUsize::new(5).unwrap()), 2).await;
    }

    /// Under each limit, fetches through a job built by `client` against two mocked
    /// pages whose first is already on Pinboard (`existing`), and asserts how often the
    /// second page was read: never under `auto`, once under `all` and a count.
    async fn assert_second_page_reads(
        mount: impl AsyncFn(&MockServer),
        client: impl Fn(&MockServer) -> SourceClient,
        is_second_page: impl Fn(&wiremock::Request) -> bool,
        existing: &[Bookmark],
    ) {
        let limits = [
            (SyncLimit::Auto, 0),
            (SyncLimit::All, 1),
            (SyncLimit::Max(NonZeroUsize::new(5).unwrap()), 1),
        ];
        for (limit, expected_reads) in limits {
            let server = MockServer::start().await;
            mount(&server).await;
            let job = SyncJob {
                client: client(&server),
                label: "test".into(),
                hook: None,
                settings: fetch_only(limit),
            };

            crate::http::recording_waits(job.fetch(existing))
                .await
                .0
                .unwrap();
            let requests = server.received_requests().await.unwrap();
            let second_page_reads = requests.iter().filter(|r| is_second_page(r)).count();
            assert_eq!(second_page_reads, expected_reads, "under {limit:?}");
        }
    }

    #[tokio::test]
    async fn only_an_auto_sync_job_stops_reddit_at_a_synced_page() {
        let mount = async |server: &MockServer| {
            for (after, id, next) in [(None, "n", Some("t3_n")), (Some("t3_n"), "o", None)] {
                let saved = Mock::given(method("GET")).and(path("/user/u/saved.json"));
                let saved = match after {
                    None => saved.and(query_param_is_missing("after")),
                    Some(after) => saved.and(query_param("after", after)),
                };
                saved
                    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                        "kind": "Listing",
                        "data": { "after": next, "children": [
                            { "kind": "t3", "data": { "name": format!("t3_{id}"),
                              "subreddit": "rust", "title": id,
                              "permalink": format!("/r/rust/comments/{id}/x/") } }
                        ] }
                    })))
                    .mount(server)
                    .await;
            }
        };
        assert_second_page_reads(
            mount,
            |server| {
                SourceClient::Reddit(RedditClient::for_test(None, Some("u".into()), server.uri()))
            },
            |request| request.url.query_pairs().any(|(key, _)| key == "after"),
            &[crate::test_support::stored_at(
                "https://old.reddit.com/r/rust/comments/n/x/",
            )],
        )
        .await;
    }

    #[tokio::test]
    async fn only_an_auto_sync_job_stops_github_at_a_synced_page() {
        let mount = async |server: &MockServer| {
            for (page, name) in [(1, "s/n"), (2, "s/o")] {
                let mut response = ResponseTemplate::new(200).set_body_json(json!([
                    { "starred_at": "2023-01-02T03:04:05Z",
                      "repo": { "full_name": name,
                                "html_url": format!("https://github.com/{name}") } }
                ]));
                if page == 1 {
                    let next = format!("<{}/user/starred?page=2>; rel=\"next\"", server.uri());
                    response = response.insert_header("link", next.as_str());
                }
                Mock::given(method("GET"))
                    .and(path("/user/starred"))
                    .and(query_param("page", page.to_string().as_str()))
                    .respond_with(response)
                    .mount(server)
                    .await;
            }
        };
        assert_second_page_reads(
            mount,
            |server| {
                SourceClient::GitHub(GitHubClient::with_base_url(
                    "tok".into(),
                    GitHubConfig::default(),
                    server.uri(),
                ))
            },
            |request| {
                request
                    .url
                    .query_pairs()
                    .any(|(key, value)| key == "page" && value == "2")
            },
            &[crate::test_support::stored_at("https://github.com/s/n")],
        )
        .await;
    }
}
