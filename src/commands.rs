//! The `sync`, `backup` and `cleanup` drivers: their jobs from the command line and the
//! config, and running them.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use log::{info, warn};

use crate::bookmark::{
    AccountState, AccountView, Bookmark, BookmarkStore, CleanupStore, WriteMode,
};
use crate::cli::{
    BackupCmd, CleanupCmd, CleanupSource, DateFlags, GitHubCleanupArgs, HackerNewsCleanupArgs,
    RedditCleanupArgs, SyncCmd, SyncSource,
};
use crate::config::{Config, GitHubAccount, HackerNewsAccount, RedditAccount};
use crate::github::{GitHubClient, GitHubConfig};
use crate::hackernews::cleanup::{HackerNewsCleanupOpts, HackerNewsCleanupOverrides};
use crate::hackernews::{AlgoliaClient, HackerNewsConfig};
use crate::hook::{cleanup_hook, handle_source_err, on_auth_failure_from_env};
use crate::jobs::backup_jobs::{build_backup_plan, BackupJob};
use crate::jobs::{
    build_all_jobs, build_jobs, open_pinboard, AllRun, Selection, SkippedTarget, SourceAccount,
    SyncJob, SyncOverrides,
};
use crate::pinboard::PinboardClient;
use crate::reddit::cleanup::RedditCleanupOverrides;
use crate::reddit::model::RedditConfig;
use crate::reddit::RedditInfoClient;
use crate::secrets::{
    first_nonempty, missing_reddit_cookie, require_github_token, resolve_reddit_cookie, SecretSpec,
};
use crate::settings::resolve_cleanup_dates;
use crate::source::{BookmarkDraft, SourceError};
use crate::stdout::outln;
use crate::timefmt::DateOpts;
use crate::{backup, config, github, hackernews, reddit, sync, timefmt};

// --- sync --------------------------------------------------------------------

pub async fn run_sync(cmd: SyncCmd, config: &Config) -> Result<()> {
    let top_mode = WriteMode::from(cmd.dry_run);
    let mut plan = build_sync_plan(cmd, config)?;
    let mut run = AllRun::default();
    run.record_skipped(plan.skipped, "could not be built");
    let (pinboard, bookmarks) =
        open_pinboard(plan.overrides.flags.pinboard_token.take(), config).await?;
    let mode = top_mode.or(plan.overrides.mode);
    run_sync_jobs(plan.jobs, run, &pinboard, &bookmarks, mode).await
}

/// What a `sync` invocation resolves to: the jobs, their overrides, and the accounts that
/// couldn't be built, which the run counts as failed.
struct SyncPlan {
    jobs: Vec<SyncJob>,
    overrides: SyncOverrides,
    skipped: Vec<SkippedTarget>,
}

fn build_sync_plan(cmd: SyncCmd, config: &Config) -> Result<SyncPlan> {
    let (built, overrides) = match (cmd.all, cmd.source) {
        (true, Some(_)) => bail!("--all cannot be combined with a source subcommand"),
        (true, None) => {
            if !config.has_accounts() {
                bail!("--all requires a --config with at least one configured account");
            }
            let overrides = SyncOverrides {
                on_auth_failure: cmd.on_auth_failure.or_else(on_auth_failure_from_env),
                ..SyncOverrides::default()
            };
            (build_all_jobs(config, &overrides), overrides)
        }
        (false, Some(SyncSource::Reddit(mut args))) => {
            let (account, all) = (args.account.take(), args.all);
            let selection = Selection::new(account.as_deref(), all)?;
            let overrides = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure);
            (
                build_jobs::<RedditAccount>(selection, &overrides, config)?,
                overrides,
            )
        }
        (false, Some(SyncSource::Github(mut args))) => {
            let (account, all) = (args.account.take(), args.all);
            let selection = Selection::new(account.as_deref(), all)?;
            let overrides = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure);
            (
                build_jobs::<GitHubAccount>(selection, &overrides, config)?,
                overrides,
            )
        }
        (false, Some(SyncSource::Hackernews(mut args))) => {
            let (account, all) = (args.account.take(), args.all);
            let selection = Selection::new(account.as_deref(), all)?;
            let overrides = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure);
            (
                build_jobs::<HackerNewsAccount>(selection, &overrides, config)?,
                overrides,
            )
        }
        (false, None) => bail!("specify a source (e.g. `sync reddit`) or pass --all"),
    };
    Ok(SyncPlan {
        jobs: built.targets,
        overrides,
        skipped: built.skipped,
    })
}

/// Fetch every source concurrently, on one task, then write the merged, deduplicated drafts
/// in turn: writes are rate-limited.
async fn run_sync_jobs(
    jobs: Vec<SyncJob>,
    mut run: AllRun,
    pinboard: &PinboardClient,
    bookmarks: &[Bookmark],
    mode: WriteMode,
) -> Result<()> {
    info!(
        "syncing {} account(s) against {} existing bookmark(s)",
        jobs.len(),
        bookmarks.len()
    );
    let now = time::OffsetDateTime::now_utc();
    let fetched = futures::future::join_all(jobs.iter().map(|job| async move {
        let drafts = job.fetch(bookmarks).await?;
        let fetched = drafts.len();
        let new = sync::prepare_new_drafts(&job.client, drafts, bookmarks, &job.settings, now);
        info!(
            "{}: fetched {fetched}, {} new after dedup",
            job.label,
            new.len()
        );
        Ok::<_, SourceError>(new)
    }))
    .await;

    let mut per_job: Vec<Vec<BookmarkDraft>> = Vec::new();
    for (job, result) in jobs.iter().zip(fetched) {
        match result {
            Ok(drafts) => per_job.push(drafts),
            Err(e) => {
                warn!("{}: fetch failed", job.label);
                run.record(Err(handle_source_err(e, job.hook.as_deref())));
            }
        }
    }
    let merged = sync::merge_deduped(per_job);

    info!(
        "{} new bookmark(s) to write{}",
        merged.len(),
        if mode.is_dry_run() { " (dry run)" } else { "" }
    );
    let outcome = sync::write_drafts(pinboard, &merged, mode).await;
    if !mode.is_dry_run() {
        info!("done: wrote {}, failed {}", outcome.written, outcome.failed);
    }
    run.record_write_failures(outcome.failed);
    run.finish()
}

// --- backup ------------------------------------------------------------------

/// Snapshot every selected service, replacing the previous snapshot in place.
pub async fn run_backup(mut cmd: BackupCmd, config: &Config) -> Result<()> {
    let (top_out, top_mode) = (cmd.out.out.take(), WriteMode::from(cmd.dry_run));
    let plan = build_backup_plan(cmd, config)?;
    let dir = resolve_backup_dir([plan.out, top_out], config)?;
    let mode = top_mode.or(plan.mode);
    back_up(plan.jobs, plan.skipped, &dir, mode).await
}

/// Run every job into `dir`, then the manifest; any skipped or failed target fails it.
async fn back_up(
    jobs: Vec<BackupJob>,
    skipped: Vec<SkippedTarget>,
    dir: &Path,
    mode: WriteMode,
) -> Result<()> {
    backup::check_stem_collisions(jobs.iter().map(|job| job.stem.as_str()))?;
    backup::check_backup_dir(dir)?;

    let mut run = AllRun::default();
    let mut entries = Vec::new();
    let mut removed = Vec::new();
    let mut failed = run.record_skipped(skipped, "was skipped");
    // Sequential, unlike `sync`: writes are local, so concurrency only adds API pressure
    // and interleaves the dry-run output.
    for job in &jobs {
        info!("{}: backing up", job.label);
        match backup::run_job(&job.client, &job.stem, dir, mode).await {
            Ok(outcome) => {
                // Recorded even when untrustworthy: it replaced these files either way, and
                // the merged manifest would keep the stale entries. The reason goes on each
                // entry too, since a later run of another target rewrites `failed`.
                entries.extend(outcome.written.into_iter().map(|mut e| {
                    e.unusable.clone_from(&outcome.unusable);
                    e
                }));
                removed.extend(outcome.removed);
                if let Some(reason) = outcome.unusable {
                    failed.push(job.label.clone());
                    run.record(Err(anyhow!("backing up {}: {reason}", job.label)));
                }
            }
            Err(e) => {
                failed.push(job.label.clone());
                run.record(Err(handle_source_err(e, job.hook.as_deref())
                    .context(format!("backing up {}", job.label))));
            }
        }
    }

    if mode.is_dry_run() {
        outln!("[dry-run] {} file(s) would be written.", entries.len());
    } else {
        // Even on a partial run: a failed target's previous files are still there, and the
        // manifest is what stops them passing as this run's.
        let now = timefmt::to_rfc3339(time::OffsetDateTime::now_utc()).unwrap_or_default();
        backup::write_manifest(dir, &entries, &removed, &failed, &now)?;
        info!(
            "backed up {} file(s) to {}",
            entries.len() + 1,
            dir.display()
        );
    }
    run.finish()
}

/// The snapshot directory: `--out`, given after or before the target, else
/// `[backup].directory`. No built-in default: a snapshot in an unexpected directory is
/// worse than an error. An empty `--out` is refused, not skipped.
fn resolve_backup_dir(flags: [Option<String>; 2], config: &Config) -> Result<PathBuf> {
    if flags.iter().flatten().any(|dir| dir.trim().is_empty()) {
        bail!("`--out` must not be empty");
    }
    first_nonempty(flags.into_iter().chain([config.backup.directory.clone()]))
        .map(PathBuf::from)
        .context("no backup directory (pass --out DIR or set [backup].directory in the config)")
}

// --- cleanup -----------------------------------------------------------------

pub async fn run_cleanup(cmd: CleanupCmd, config: &Config) -> Result<()> {
    let overrides = cmd.dates;
    match (cmd.all, cmd.source) {
        (true, Some(_)) => bail!("--all cannot be combined with a source subcommand"),
        (true, None) => {
            // Once per source, as its first account: the bookmarks are shared.
            if !config.has_accounts() {
                bail!("cleanup --all requires a --config with at least one configured account");
            }
            let github = cleanup_all_target::<GitHubAccount>(&overrides, config)?;
            let reddit = cleanup_all_target::<RedditAccount>(&overrides, config)?;
            let hackernews = cleanup_all_target::<HackerNewsAccount>(&overrides, config)?;
            let (pinboard, bookmarks) = open_pinboard(None, config).await?;
            // One view for all three: each plans against what the one before left.
            let mode = WriteMode::from(cmd.dry_run);
            let store = CleanupStore::new(&pinboard, AccountView::new(bookmarks), mode);
            let mut run = AllRun::default();
            if let Some(CleanupTarget { account, dates }) = github {
                let pass = async {
                    GitHubCleanup::build(account, None, dates, config)?
                        .run(&store)
                        .await
                };
                run.record(pass.await);
            }
            if let Some(CleanupTarget { account, dates }) = reddit {
                let overrides = RedditCleanupOverrides::default();
                let pass = async {
                    RedditCleanup::build(account, overrides, dates, config)?
                        .run(&store)
                        .await
                };
                run.record(pass.await);
            }
            if let Some(CleanupTarget { account, dates }) = hackernews {
                let overrides = HackerNewsCleanupOverrides::default();
                let pass = async {
                    HackerNewsCleanup::build(account, overrides, dates)?
                        .run(&store)
                        .await
                };
                run.record(pass.await);
            }
            run.finish()
        }
        (false, Some(CleanupSource::Reddit(args))) => {
            run_cleanup_reddit(args, cmd.dry_run.into(), &overrides, config).await
        }
        (false, Some(CleanupSource::Github(args))) => {
            run_cleanup_github(args, cmd.dry_run.into(), &overrides, config).await
        }
        (false, Some(CleanupSource::Hackernews(args))) => {
            run_cleanup_hackernews(args, cmd.dry_run.into(), &overrides, config).await
        }
        (false, None) => bail!("specify a source (e.g. `cleanup reddit`) or pass --all"),
    }
}

/// One source's cleanup pass: its account (`None` for the implicit CLI/env one) and dates,
/// resolved before `posts/all` so a refused run doesn't spend that call.
struct CleanupTarget<'a, A> {
    account: Option<&'a A>,
    dates: DateOpts,
}

/// `A`'s [`CleanupTarget`] run as `account`. `pass` names the run in a refusal.
fn cleanup_target<'a, A: SourceAccount>(
    pass: &str,
    account: Option<&'a A>,
    overrides: &DateFlags,
    config: &'a Config,
) -> Result<CleanupTarget<'a, A>> {
    let dates = resolve_cleanup_dates(pass, overrides, account, config)?;
    Ok(CleanupTarget { account, dates })
}

/// The [`CleanupTarget`] of `cleanup <source> [account]`: the account `name`, else the
/// first, else the implicit CLI/env one.
fn source_cleanup_target<'a, A: SourceAccount>(
    name: Option<&str>,
    overrides: &DateFlags,
    config: &'a Config,
) -> Result<CleanupTarget<'a, A>> {
    let account = config::select_account(A::configured(config), name)?;
    cleanup_target(
        &format!("`cleanup {}`", A::SOURCE),
        account,
        overrides,
        config,
    )
}

/// The [`CleanupTarget`] of `A`'s `cleanup --all` pass, as its first account, or `None`.
fn cleanup_all_target<'a, A: SourceAccount>(
    overrides: &DateFlags,
    config: &'a Config,
) -> Result<Option<CleanupTarget<'a, A>>> {
    let Some(account) = A::configured(config).first() else {
        return Ok(None);
    };
    let pass = format!("`cleanup --all` ({} pass)", A::SOURCE);
    cleanup_target(&pass, Some(account), overrides, config).map(Some)
}

async fn run_cleanup_github(
    args: GitHubCleanupArgs,
    top_mode: WriteMode,
    top_overrides: &DateFlags,
    config: &Config,
) -> Result<()> {
    let CleanupTarget { account, dates } = source_cleanup_target::<GitHubAccount>(
        args.account.as_deref(),
        &args.dates.with_top_level(top_overrides),
        config,
    )?;
    let github = GitHubCleanup::build(account, args.github_token, dates, config)?;
    let (pinboard, bookmarks) = open_pinboard(args.pinboard_token, config).await?;
    let mode = top_mode.or(args.dry_run.into());
    let store = CleanupStore::new(&pinboard, AccountView::new(bookmarks), mode);
    github.run(&store).await
}

/// A GitHub cleanup pass: its client, every account's tag config, its dates and its hook.
struct GitHubCleanup {
    client: GitHubClient,
    configured: Vec<GitHubConfig>,
    dates: DateOpts,
    hook: Option<String>,
}

impl GitHubCleanup {
    fn build(
        account: Option<&GitHubAccount>,
        token_flag: Option<String>,
        dates: DateOpts,
        config: &Config,
    ) -> Result<Self> {
        let token = require_github_token(token_flag, account)?;
        let client = GitHubClient::new(
            token,
            GitHubConfig::resolve(account, &config.defaults.github),
        )?;
        Ok(Self::with_client(client, account, dates, config))
    }

    /// The pass for `account`, with `client` already built.
    fn with_client(
        client: GitHubClient,
        account: Option<&GitHubAccount>,
        dates: DateOpts,
        config: &Config,
    ) -> Self {
        Self {
            client,
            configured: config
                .github
                .iter()
                .map(|account| GitHubConfig::resolve(Some(account), &config.defaults.github))
                .collect(),
            dates,
            hook: cleanup_hook(account, config),
        }
    }

    async fn run<S: BookmarkStore + AccountState>(&self, store: &S) -> Result<()> {
        github::cleanup::run(store, &self.client, self.dates, &self.configured)
            .await
            .map_err(|e| handle_source_err(e, self.hook.as_deref()))
    }
}

async fn run_cleanup_reddit(
    mut args: RedditCleanupArgs,
    top_mode: WriteMode,
    top_overrides: &DateFlags,
    config: &Config,
) -> Result<()> {
    let mode = top_mode.or(args.dry_run.into());
    let CleanupTarget { account, dates } = source_cleanup_target::<RedditAccount>(
        args.account.as_deref(),
        &args.dates.with_top_level(top_overrides),
        config,
    )?;
    let pinboard_token = args.pinboard_token.take();
    let reddit = RedditCleanup::build(account, RedditCleanupOverrides::from(args), dates, config)?;
    let (pinboard, bookmarks) = open_pinboard(pinboard_token, config).await?;
    let store = CleanupStore::new(&pinboard, AccountView::new(bookmarks), mode);
    reddit.run(&store).await
}

/// A Reddit cleanup pass: options, an `/api/info` client when a step needs one, and hook.
struct RedditCleanup {
    opts: reddit::cleanup::RedditCleanupOpts,
    client: Option<RedditInfoClient>,
    hook: Option<String>,
}

impl RedditCleanup {
    fn build(
        account: Option<&RedditAccount>,
        overrides: RedditCleanupOverrides,
        dates: DateOpts,
        config: &Config,
    ) -> Result<Self> {
        let opts = reddit::cleanup::RedditCleanupOpts {
            mark_nsfw: overrides.mark_nsfw,
            fix_titles: overrides.fix_titles,
            config: account.map(RedditConfig::from).unwrap_or_default(),
            dates,
            configured: config.reddit.iter().map(RedditConfig::from).collect(),
        };

        let client = if opts.needs_post_info() {
            let cookie = resolve_reddit_cookie(overrides.cookie, account)
                .with_context(missing_reddit_cookie)?;
            let username = SecretSpec::REDDIT_USERNAME
                .resolve(None, account.and_then(|a| a.username.clone()), None)
                .map(|username| username.value);
            Some(RedditInfoClient::new(cookie, username)?)
        } else {
            None
        };
        Ok(Self {
            opts,
            client,
            hook: cleanup_hook(account, config),
        })
    }

    async fn run<S: BookmarkStore + AccountState>(&self, store: &S) -> Result<()> {
        reddit::cleanup::run(store, self.client.as_ref(), &self.opts)
            .await
            .map_err(|e| handle_source_err(e, self.hook.as_deref()))
    }
}

async fn run_cleanup_hackernews(
    mut args: HackerNewsCleanupArgs,
    top_mode: WriteMode,
    top_overrides: &DateFlags,
    config: &Config,
) -> Result<()> {
    let CleanupTarget { account, dates } = source_cleanup_target::<HackerNewsAccount>(
        args.account.as_deref(),
        &args.dates.with_top_level(top_overrides),
        config,
    )?;
    let mode = top_mode.or(args.dry_run.into());
    let pinboard_token = args.pinboard_token.take();
    let hackernews =
        HackerNewsCleanup::build(account, HackerNewsCleanupOverrides::from(args), dates)?;
    let (pinboard, bookmarks) = open_pinboard(pinboard_token, config).await?;
    let store = CleanupStore::new(&pinboard, AccountView::new(bookmarks), mode);
    hackernews.run(&store).await
}

/// The HackerNews tag config, with `--tag-link` over `tag_link`, checked as the config
/// value is: Pinboard would split a marker with a space, and it would match nothing.
fn resolve_hackernews_cleanup_config(
    account: Option<&HackerNewsAccount>,
    link_tag: Option<String>,
) -> Result<HackerNewsConfig> {
    let mut hn_config = account.map(HackerNewsConfig::from).unwrap_or_default();
    if let Some(tag) = link_tag {
        config::check_taggable("`--tag-link`", &tag)?;
        hn_config.link_tag = tag;
    }
    Ok(hn_config)
}

/// A HackerNews cleanup pass: its options and Algolia client. No hook: HackerNews is
/// public, so nothing can need re-authenticating.
struct HackerNewsCleanup {
    opts: HackerNewsCleanupOpts,
    client: AlgoliaClient,
}

impl HackerNewsCleanup {
    fn build(
        account: Option<&HackerNewsAccount>,
        overrides: HackerNewsCleanupOverrides,
        dates: DateOpts,
    ) -> Result<Self> {
        Ok(Self {
            opts: HackerNewsCleanupOpts {
                link_discussions: overrides.link_discussions,
                dates,
                config: resolve_hackernews_cleanup_config(account, overrides.link_tag)?,
            },
            client: AlgoliaClient::new()?,
        })
    }

    async fn run<S: BookmarkStore + AccountState>(&self, store: &S) -> Result<()> {
        hackernews::cleanup::run(store, &self.client, &self.opts)
            .await
            .map_err(SourceError::into_anyhow)
    }
}

#[cfg(test)]
mod tests;

/// Tests that need a socket; the flake skips `net_tests` in the Nix build sandbox.
#[cfg(test)]
mod net_tests;
