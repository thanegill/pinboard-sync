//! `sync` jobs, one per account: a source client, its hook and its resolved settings.
//! [`SourceAccount`] gives `sync`, `backup` ([`backup_jobs`]) and `doctor` one path through
//! every source. Also [`open_pinboard`] and [`AllRun`], an `--all` run's tally.

pub mod backup_jobs;

use std::time::Duration;

use anyhow::{bail, Context, Result};
use log::{debug, error, info};
use url::Url;

use crate::backup::{self, BackupSource};
use crate::bookmark::{Bookmark, WriteMode};
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

/// A target an `--all` run couldn't build, and why: counted as failed while the rest run.
pub struct SkippedTarget {
    pub label: String,
    pub reason: anyhow::Error,
}

/// The targets built for some accounts, and the accounts that could not be built.
pub struct Built<T> {
    pub targets: Vec<T>,
    pub skipped: Vec<SkippedTarget>,
}

impl<T> Built<T> {
    const fn none() -> Self {
        Self {
            targets: Vec::new(),
            skipped: Vec::new(),
        }
    }

    fn one(target: T) -> Self {
        Self {
            targets: vec![target],
            skipped: Vec::new(),
        }
    }

    /// `self` followed by `more`.
    #[must_use]
    pub fn and(mut self, more: Self) -> Self {
        self.targets.extend(more.targets);
        self.skipped.extend(more.skipped);
        self
    }
}

/// A target per configured account of `A`, made by `build`; one that fails is skipped, so
/// the rest still run.
pub fn build_every<A: SourceAccount, T>(
    overrides: &SyncOverrides,
    config: &Config,
    build: impl Fn(Option<&A>, &SyncOverrides, &Config) -> Result<T>,
) -> Built<T> {
    warn_shared_env_secrets::<A>(overrides, config);
    let mut every = Built::none();
    for account in A::configured(config) {
        match build(Some(account), overrides, config) {
            Ok(target) => every.targets.push(target),
            Err(reason) => every.skipped.push(SkippedTarget {
                label: job_label(Some(account)),
                reason,
            }),
        }
    }
    every
}

/// A target per account `selection` picks, made by `build`. Under [`Selection::All`] that
/// is [`build_every`], and a source with no accounts is refused.
pub fn build_selected<A: SourceAccount, T>(
    selection: Selection,
    overrides: &SyncOverrides,
    config: &Config,
    build: impl Fn(Option<&A>, &SyncOverrides, &Config) -> Result<T>,
) -> Result<Built<T>> {
    let accounts = A::configured(config);
    match selection {
        Selection::All => {
            if accounts.is_empty() {
                bail!(
                    "--all requires a --config with at least one configured {} account",
                    A::SOURCE
                );
            }
            Ok(build_every(overrides, config, build))
        }
        Selection::One(name) => {
            let account = config::select_account(accounts, name)?;
            Ok(Built::one(build(account, overrides, config)?))
        }
    }
}

/// One job per configured account across every source, for `sync --all`.
pub fn build_all_jobs(config: &Config, overrides: &SyncOverrides) -> Built<SyncJob> {
    build_every::<RedditAccount, _>(overrides, config, build_job)
        .and(build_every::<GitHubAccount, _>(
            overrides, config, build_job,
        ))
        .and(build_every::<HackerNewsAccount, _>(
            overrides, config, build_job,
        ))
}

/// [`secrets::warn_if_env_shared`] for `A`'s accounts.
pub fn warn_shared_env_secrets<A: SourceAccount>(overrides: &SyncOverrides, config: &Config) {
    secrets::warn_if_env_shared(
        A::SOURCE,
        &A::env_secrets(overrides),
        A::configured(config).len(),
    );
}

/// The secrets among `flagged` whose flag holds no value the ladder would take, so the
/// environment is the highest rung that can apply.
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
    /// would ignore the name, even one no account has.
    pub fn new(account: Option<&'a str>, all: bool) -> Result<Self> {
        match (account, all) {
            (Some(_), true) => bail!("--all cannot be combined with an account name"),
            (None, true) => Ok(Selection::All),
            (account, false) => Ok(Selection::One(account)),
        }
    }
}

/// One job per account `selection` picks, through [`build_selected`].
pub fn build_jobs<A: SourceAccount>(
    selection: Selection,
    overrides: &SyncOverrides,
    config: &Config,
) -> Result<Built<SyncJob>> {
    build_selected(selection, overrides, config, build_job::<A>)
}

#[derive(Default)]
pub struct SyncOverrides {
    pub reddit_username: Option<String>,
    pub reddit_cookie: Option<String>,
    pub github_token: Option<String>,
    pub hackernews_username: Option<String>,
    pub on_auth_failure: Option<String>,
    pub flags: SyncFlags,
    /// `--dry-run` after the source subcommand.
    pub mode: WriteMode,
}

impl SyncOverrides {
    /// `--on-auth-failure` from before the source subcommand, else the per-source value:
    /// one given after it, or `PINBOARD_SYNC_ON_AUTH_FAILURE`, which the flag outranks. The
    /// command line refuses both.
    #[must_use]
    pub fn with_top_level_hook(mut self, on_auth_failure: Option<String>) -> Self {
        self.on_auth_failure = on_auth_failure.or(self.on_auth_failure);
        self
    }
}

/// A source client ready to fetch, with its hook and resolved write settings.
pub struct SyncJob {
    pub client: SourceClient,
    /// Human label for logs, e.g. `reddit[alice]`.
    pub label: String,
    pub hook: Option<String>,
    pub settings: JobSettings,
}

impl SyncJob {
    /// The source's drafts, read incrementally against `existing` under
    /// [`SyncLimit::Auto`], else in full: a capped run leaves older drafts behind pages
    /// that already look synced, which an incremental read never gets past.
    pub async fn fetch(&self, existing: &[Bookmark]) -> Result<Vec<BookmarkDraft>, SourceError> {
        match self.settings.limit {
            SyncLimit::Auto => self.client.fetch_incremental(existing).await,
            SyncLimit::All | SyncLimit::Max(_) => self.client.fetch().await,
        }
    }
}

/// `source[account]`, or `source[default]` for the implicit CLI/env account.
fn job_label<A: SourceAccount>(account: Option<&A>) -> String {
    format!(
        "{}[{}]",
        A::SOURCE,
        account.and_then(|a| a.account_name()).unwrap_or("default")
    )
}

/// One of the source clients, behind the `Source` port so `--all` fetches them
/// concurrently, as one type.
pub enum SourceClient {
    Reddit(RedditClient),
    GitHub(GitHubClient),
    HackerNews(HackerNewsClient),
}

impl Source for SourceClient {
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError> {
        match self {
            Self::Reddit(c) => c.fetch().await,
            Self::GitHub(c) => c.fetch().await,
            Self::HackerNews(c) => c.fetch().await,
        }
    }

    async fn fetch_incremental(
        &self,
        existing: &[Bookmark],
    ) -> Result<Vec<BookmarkDraft>, SourceError> {
        match self {
            Self::Reddit(c) => c.fetch_incremental(existing).await,
            Self::GitHub(c) => c.fetch_incremental(existing).await,
            Self::HackerNews(c) => c.fetch_incremental(existing).await,
        }
    }
}

impl BackupSource for SourceClient {
    async fn dump(&self) -> Result<backup::BackupDump, SourceError> {
        match self {
            Self::Reddit(c) => c.dump().await,
            Self::GitHub(c) => c.dump().await,
            Self::HackerNews(c) => c.dump().await,
        }
    }
}

impl UrlKey for SourceClient {
    fn dedup_key(&self, url: &Url) -> Option<DedupKey> {
        match self {
            Self::Reddit(c) => c.dedup_key(url),
            Self::GitHub(c) => c.dedup_key(url),
            Self::HackerNews(c) => c.dedup_key(url),
        }
    }
}

/// What `sync`, `backup` and `doctor` need of a source's account type beyond
/// [`config::Account`]: how its client is built, and which hook its jobs carry.
pub trait SourceAccount: config::Account {
    /// The account's own `on_auth_failure` override.
    fn on_auth_failure(&self) -> Option<&str>;

    /// The secrets [`Self::client`] resolves whose flag in `overrides` holds no usable
    /// value, so the environment outranks each account's own.
    fn env_secrets(overrides: &SyncOverrides) -> Vec<SecretSpec>;

    /// The client for `account`, or the implicit CLI/env account for `None`. `config` is
    /// the whole config: a source keys URLs on other accounts' domains.
    fn client(
        account: Option<&Self>,
        overrides: &SyncOverrides,
        config: &Config,
    ) -> Result<SourceClient>;

    /// The hook for `account`: `top_hook` (the flag or env value), else the account's, the
    /// per-source default's or `[hooks]`. Every command resolves it here, so a source that
    /// never re-authenticates rules it out for every command at once.
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

/// The `sync` job for `account`, or the implicit CLI/env account.
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

/// The client and `posts/all`, read once for the whole run: it is the most rate-limited.
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

/// An `--all` run's failures: every account is attempted, and the run fails at the end if
/// any account or bookmark write did.
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

    /// Count each skipped target as failed, logged as `<label> <what>: <reason>`; returns
    /// the skipped labels.
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

    /// Count bookmarks that failed to write, already logged.
    pub const fn record_write_failures(&mut self, count: usize) {
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
