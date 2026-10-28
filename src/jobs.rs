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
use crate::bookmark::Bookmark;
use crate::cli::SyncFlags;
use crate::config::{self, Config, GitHubAccount, HackerNewsAccount, RedditAccount, SyncLimit};
use crate::github::{GitHubClient, GitHubConfig};
use crate::hackernews::{HackerNewsClient, HackerNewsConfig};
use crate::pinboard::{PinboardClient, RATE_LIMIT_SECS};
use crate::reddit::model::RedditConfig;
use crate::reddit::RedditClient;
use crate::secrets::{
    self, resolve_github_token, resolve_pinboard_token, resolve_reddit_cookie, SecretSpec,
};
use crate::settings::{resolve_hook, resolve_job_settings};
use crate::source::{BookmarkDraft, Source, SourceError, UrlKey};
use crate::sync::JobSettings;

/// A target an `--all` run could not build, and why. `sync` and `backup` both carry these
/// out of their build phase so the run can count each one as failed, and still run the
/// targets that did build.
pub struct SkippedTarget {
    pub label: String,
    pub reason: anyhow::Error,
}

/// Build one job per configured account across every source for `sync --all`, isolating
/// build failures: an account whose required secret won't resolve is returned as a
/// [`SkippedTarget`] rather than aborting the run, so the healthy accounts still sync.
pub fn build_all_jobs(
    config: &Config,
    overrides: &SyncOverrides,
) -> (Vec<SyncJob>, Vec<SkippedTarget>) {
    let mut jobs = Vec::new();
    let mut skipped = Vec::new();
    build_every_job::<RedditAccount>(config, overrides, &mut jobs, &mut skipped);
    build_every_job::<GitHubAccount>(config, overrides, &mut jobs, &mut skipped);
    build_every_job::<HackerNewsAccount>(config, overrides, &mut jobs, &mut skipped);
    (jobs, skipped)
}

/// [`build_all_jobs`] for one source: a job for each of its configured accounts, with each
/// account that fails to build added to `skipped` instead.
fn build_every_job<A: SourceAccount>(
    config: &Config,
    overrides: &SyncOverrides,
    jobs: &mut Vec<SyncJob>,
    skipped: &mut Vec<SkippedTarget>,
) {
    warn_shared_env_secrets::<A>(overrides, config);
    for account in A::configured(config) {
        match build_job(Some(account), overrides, config) {
            Ok(job) => jobs.push(job),
            Err(reason) => skipped.push(SkippedTarget {
                label: job_label(Some(account)),
                reason,
            }),
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
fn unflagged(flagged: &[(SecretSpec, &Option<String>)]) -> Vec<SecretSpec> {
    flagged
        .iter()
        .filter(|(secret, flag)| !flag.as_deref().is_some_and(|value| secret.accepts(value)))
        .map(|(secret, _)| *secret)
        .collect()
}

/// Build one job per account: every account when `all`, else the named (or first, or
/// implicit CLI/env) account. Under `all`, as in [`build_all_jobs`], an account that fails
/// to build is returned as a [`SkippedTarget`] rather than aborting the rest.
pub fn build_jobs<A: SourceAccount>(
    name: Option<&str>,
    all: bool,
    overrides: &SyncOverrides,
    config: &Config,
) -> Result<(Vec<SyncJob>, Vec<SkippedTarget>)> {
    let accounts = A::configured(config);
    if all {
        if accounts.is_empty() {
            bail!("--all requires a --config with at least one configured account");
        }
        let mut jobs = Vec::new();
        let mut skipped = Vec::new();
        build_every_job::<A>(config, overrides, &mut jobs, &mut skipped);
        Ok((jobs, skipped))
    } else {
        let account = config::select_account(accounts, name)?;
        Ok((vec![build_job(account, overrides, config)?], Vec::new()))
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
    fn env_secrets(overrides: &SyncOverrides) -> Vec<SecretSpec>;

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

    fn env_secrets(overrides: &SyncOverrides) -> Vec<SecretSpec> {
        unflagged(&[
            (SecretSpec::REDDIT_USERNAME, &overrides.reddit_username),
            (SecretSpec::REDDIT_COOKIE, &overrides.reddit_cookie),
        ])
    }

    fn client(
        account: Option<&Self>,
        overrides: &SyncOverrides,
        config: &Config,
    ) -> Result<SourceClient> {
        let username = SecretSpec::REDDIT_USERNAME
            .resolve(
                overrides.reddit_username.clone(),
                account.and_then(|a| a.username.clone()),
                None,
            )
            .map(|username| username.value)
            .with_context(|| SecretSpec::REDDIT_USERNAME.missing())?;
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
        &config.defaults.github.common
    }

    fn on_auth_failure(&self) -> Option<&str> {
        self.on_auth_failure.as_deref()
    }

    fn env_secrets(overrides: &SyncOverrides) -> Vec<SecretSpec> {
        unflagged(&[(SecretSpec::GITHUB_TOKEN, &overrides.github_token)])
    }

    fn client(
        account: Option<&Self>,
        overrides: &SyncOverrides,
        config: &Config,
    ) -> Result<SourceClient> {
        let token = resolve_github_token(overrides.github_token.clone(), account)
            .with_context(|| SecretSpec::GITHUB_TOKEN.missing())?;
        let github_config = GitHubConfig::resolve(account, &config.defaults.github);
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

    fn env_secrets(overrides: &SyncOverrides) -> Vec<SecretSpec> {
        unflagged(&[(SecretSpec::HN_USERNAME, &overrides.hackernews_username)])
    }

    fn client(
        account: Option<&Self>,
        overrides: &SyncOverrides,
        config: &Config,
    ) -> Result<SourceClient> {
        let username = SecretSpec::HN_USERNAME
            .resolve(
                overrides.hackernews_username.clone(),
                account.and_then(|a| a.username.clone()),
                None,
            )
            .map(|username| username.value)
            .with_context(|| SecretSpec::HN_USERNAME.missing())?;
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
        .with_context(|| SecretSpec::PINBOARD_TOKEN.missing())?;
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

    /// Count each skipped target as failed, reporting it as `<label> <what>: <reason>`, and
    /// return their labels.
    pub fn record_skipped(&mut self, skipped: Vec<SkippedTarget>, what: &str) -> Vec<String> {
        skipped
            .into_iter()
            .map(|target| {
                self.record(Err(target
                    .reason
                    .context(format!("{} {what}", target.label))));
                target.label
            })
            .collect()
    }

    pub fn finish(self) -> Result<()> {
        if self.failed > 0 {
            bail!("{} account(s) failed", self.failed);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

/// Tests that need a socket; the flake skips `net_tests` in the Nix build sandbox.
#[cfg(test)]
mod net_tests;
