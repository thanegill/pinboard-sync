//! `sync` jobs: one per account, each a configured source client plus its auth-failure
//! hook and settings resolved through the flag -> account -> `[defaults.<source>]` ->
//! `[pinboard]` ladder. [`SourceAccount`] is what lets `sync`, `backup` and `doctor` treat
//! the three sources through one generic path. The Pinboard client is built here too
//! ([`open_pinboard`]), as is [`AllRun`], which tallies an `--all` run's failures, and
//! [`backup_jobs`] builds `backup`'s targets on the same path.

pub mod backup_jobs;

use std::time::Duration;

use anyhow::{bail, Context, Result};
use log::{debug, error, info};
use url::Url;

use crate::backup::{self, BackupSource};
use crate::bookmark::Bookmark;
use crate::cli::SyncFlags;
use crate::config::{self, Config, GitHubAccount, HackerNewsAccount, RedditAccount, SyncLimit};
use crate::github::{GitHubClient, GitHubConfig};
use crate::hackernews::{HackerNewsClient, HackerNewsConfig};
use crate::pinboard::{PinboardClient, RATE_LIMIT};
use crate::reddit::model::RedditConfig;
use crate::reddit::RedditClient;
use crate::secrets::{self, require_github_token, resolve_reddit_cookie, SecretSpec};
use crate::settings::{resolve_hook, resolve_job_settings};
use crate::source::{BookmarkDraft, DedupKey, Source, SourceError, UrlKey};
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
fn unflagged(flagged: &[(SecretSpec, Option<&str>)]) -> Vec<SecretSpec> {
    flagged
        .iter()
        .filter(|(secret, flag)| !flag.is_some_and(|value| secret.accepts(value)))
        .map(|(secret, _)| *secret)
        .collect()
}

/// Which of one source's accounts a `sync` or `backup` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection<'a> {
    /// Every configured account.
    All,
    /// The named account, else the first, else an implicit CLI/env account.
    One(Option<&'a str>),
}

impl<'a> Selection<'a> {
    /// The selection a source's `[account]` and `--all` make. Both at once is refused: it
    /// would have to ignore the name, even one no account has.
    pub fn new(account: Option<&'a str>, all: bool) -> Result<Self> {
        match (account, all) {
            (Some(_), true) => bail!("--all cannot be combined with an account name"),
            (None, true) => Ok(Selection::All),
            (account, false) => Ok(Selection::One(account)),
        }
    }
}

/// Build one job per selected account. Under [`Selection::All`], as in [`build_all_jobs`],
/// an account that fails to build is returned as a [`SkippedTarget`] rather than aborting
/// the rest.
pub fn build_jobs<A: SourceAccount>(
    selection: Selection,
    overrides: &SyncOverrides,
    config: &Config,
) -> Result<(Vec<SyncJob>, Vec<SkippedTarget>)> {
    let accounts = A::configured(config);
    match selection {
        Selection::All => {
            if accounts.is_empty() {
                bail!(
                    "--all requires a --config with at least one configured {} account",
                    A::SOURCE
                );
            }
            let mut jobs = Vec::new();
            let mut skipped = Vec::new();
            build_every_job::<A>(config, overrides, &mut jobs, &mut skipped);
            Ok((jobs, skipped))
        }
        Selection::One(name) => {
            let account = config::select_account(accounts, name)?;
            Ok((vec![build_job(account, overrides, config)?], Vec::new()))
        }
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
    /// Take `--on-auth-failure` from before the source subcommand (on `SyncCmd`), else
    /// the per-source value. The command line refuses it on both sides, so the per-source
    /// value here is one from after the subcommand or from `PINBOARD_SYNC_ON_AUTH_FAILURE`,
    /// which the flag outranks.
    #[must_use]
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
    fn dedup_key(&self, url: &Url) -> Option<DedupKey> {
        match self {
            SourceClient::Reddit(c) => c.dedup_key(url),
            SourceClient::GitHub(c) => c.dedup_key(url),
            SourceClient::HackerNews(c) => c.dedup_key(url),
        }
    }
}

/// What `sync`, `backup` and `doctor` need from one source's account type beyond
/// [`config::Account`], so each handles the three sources through one generic path: how
/// its client is built from an account plus the CLI overrides, and which auth-failure hook
/// its jobs carry.
pub trait SourceAccount: config::Account {
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
    fn on_auth_failure(&self) -> Option<&str> {
        self.on_auth_failure.as_deref()
    }

    fn env_secrets(overrides: &SyncOverrides) -> Vec<SecretSpec> {
        unflagged(&[
            (
                SecretSpec::REDDIT_USERNAME,
                overrides.reddit_username.as_deref(),
            ),
            (
                SecretSpec::REDDIT_COOKIE,
                overrides.reddit_cookie.as_deref(),
            ),
        ])
    }

    fn client(
        account: Option<&Self>,
        overrides: &SyncOverrides,
        config: &Config,
    ) -> Result<SourceClient> {
        let username = SecretSpec::REDDIT_USERNAME
            .require(
                overrides.reddit_username.clone(),
                account.and_then(|a| a.username.clone()),
                None,
            )?
            .value;
        let cookie = resolve_reddit_cookie(overrides.reddit_cookie.clone(), account);
        let reddit_config = account.map(RedditConfig::from).unwrap_or_default();
        Ok(SourceClient::Reddit(
            RedditClient::new(username, cookie, reddit_config)?
                .with_reddit_domains(config.reddit_domains()),
        ))
    }
}

impl SourceAccount for GitHubAccount {
    fn on_auth_failure(&self) -> Option<&str> {
        self.on_auth_failure.as_deref()
    }

    fn env_secrets(overrides: &SyncOverrides) -> Vec<SecretSpec> {
        unflagged(&[(SecretSpec::GITHUB_TOKEN, overrides.github_token.as_deref())])
    }

    fn client(
        account: Option<&Self>,
        overrides: &SyncOverrides,
        config: &Config,
    ) -> Result<SourceClient> {
        let token = require_github_token(overrides.github_token.clone(), account)?;
        let github_config = GitHubConfig::resolve(account, &config.defaults.github);
        Ok(SourceClient::GitHub(GitHubClient::new(
            token,
            github_config,
        )?))
    }
}

impl SourceAccount for HackerNewsAccount {
    fn on_auth_failure(&self) -> Option<&str> {
        None
    }

    fn env_secrets(overrides: &SyncOverrides) -> Vec<SecretSpec> {
        unflagged(&[(
            SecretSpec::HN_USERNAME,
            overrides.hackernews_username.as_deref(),
        )])
    }

    fn client(
        account: Option<&Self>,
        overrides: &SyncOverrides,
        config: &Config,
    ) -> Result<SourceClient> {
        let username = SecretSpec::HN_USERNAME
            .require(
                overrides.hackernews_username.clone(),
                account.and_then(|a| a.username.clone()),
                None,
            )?
            .value;
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
        settings: resolve_job_settings(account, overrides, config),
    })
}

/// Resolve the Pinboard token and build the client (no fetch).
fn build_pinboard_client(token_flag: Option<String>, config: &Config) -> Result<PinboardClient> {
    let token = SecretSpec::PINBOARD_TOKEN.require(
        token_flag,
        config.pinboard.token.clone(),
        config.pinboard.token_file.as_deref(),
    )?;
    pinboard_client_for_token(token.value, config)
}

/// The client for an already-resolved token, at the configured write pacing.
fn pinboard_client_for_token(token: String, config: &Config) -> Result<PinboardClient> {
    let rate_limit = config
        .pinboard
        .rate_limit_secs
        .map_or(RATE_LIMIT, Duration::from_secs);
    PinboardClient::new(token, rate_limit)
}

/// Build the client and fetch `posts/all` once. Returns the client and the bookmark
/// set to share across every account in a run (so `posts/all` -- the most rate-limited
/// endpoint -- is hit once, not once per account).
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
/// are reported and counted, and the run errors at the end if any account failed or any
/// bookmark failed to write.
#[derive(Default)]
pub struct AllRun {
    pub failed: usize,
    pub write_failed: usize,
}

impl AllRun {
    pub fn record(&mut self, result: Result<()>) {
        if let Err(e) = result {
            error!("{e:#}");
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

    /// Count `count` bookmarks that failed to write, already logged one by one.
    pub fn record_write_failures(&mut self, count: usize) {
        self.write_failed += count;
    }

    /// The run's verdict, naming every kind of failure it had.
    pub fn finish(self) -> Result<()> {
        let mut failures = Vec::new();
        if self.failed > 0 {
            failures.push(format!("{} account(s) failed", self.failed));
        }
        if self.write_failed > 0 {
            failures.push(format!("{} bookmark(s) failed to write", self.write_failed));
        }
        if failures.is_empty() {
            return Ok(());
        }
        bail!("{}", failures.join("; "))
    }
}

#[cfg(test)]
mod tests;

/// Tests that need a socket; the flake skips `net_tests` in the Nix build sandbox.
#[cfg(test)]
mod net_tests;
