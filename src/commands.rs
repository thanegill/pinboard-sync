//! The `sync`, `backup` and `cleanup` subcommands: building each one's jobs from the
//! parsed command line and the config, and running them.

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
    first_nonempty, missing_reddit_cookie, resolve_github_token, resolve_reddit_cookie, SecretSpec,
};
use crate::settings::resolve_cleanup_dates;
use crate::source::{BookmarkDraft, SourceError};
use crate::timefmt::DateOpts;
use crate::{backup, config, github, hackernews, reddit, sync, timefmt};

// --- sync --------------------------------------------------------------------

pub async fn run_sync(cmd: SyncCmd, config: &Config) -> Result<()> {
    let top_dry_run = cmd.dry_run;
    let mut plan = build_sync_plan(cmd, config)?;
    let mut run = AllRun::default();
    run.record_skipped(plan.skipped, "could not be built");
    let (pinboard, bookmarks) =
        open_pinboard(plan.overrides.flags.pinboard_token.take(), config).await?;
    let mode = WriteMode::from_flags(top_dry_run, plan.overrides.dry_run);
    run_sync_jobs(plan.jobs, run, &pinboard, &bookmarks, mode).await
}

/// What a `sync` invocation resolves to: the jobs, the overrides they were built from,
/// and the accounts that could not be built, for the run to count as failed. Mirrors
/// `backup`'s `BackupPlan`.
struct SyncPlan {
    jobs: Vec<SyncJob>,
    overrides: SyncOverrides,
    skipped: Vec<SkippedTarget>,
}

fn build_sync_plan(cmd: SyncCmd, config: &Config) -> Result<SyncPlan> {
    let (jobs, overrides, skipped) = match (cmd.all, cmd.source) {
        (true, Some(_)) => bail!("--all cannot be combined with a source subcommand"),
        (true, None) => {
            if !config.has_accounts() {
                bail!("--all requires a --config with at least one configured account");
            }
            let overrides = SyncOverrides {
                dry_run: cmd.dry_run,
                on_auth_failure: cmd.on_auth_failure.or_else(on_auth_failure_from_env),
                ..SyncOverrides::default()
            };
            let (jobs, skipped) = build_all_jobs(config, &overrides);
            (jobs, overrides, skipped)
        }
        (false, Some(SyncSource::Reddit(mut args))) => {
            let (account, all) = (args.account.take(), args.all);
            let selection = Selection::new(account.as_deref(), all)?;
            let overrides = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure);
            let (jobs, skipped) = build_jobs::<RedditAccount>(selection, &overrides, config)?;
            (jobs, overrides, skipped)
        }
        (false, Some(SyncSource::Github(mut args))) => {
            let (account, all) = (args.account.take(), args.all);
            let selection = Selection::new(account.as_deref(), all)?;
            let overrides = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure);
            let (jobs, skipped) = build_jobs::<GitHubAccount>(selection, &overrides, config)?;
            (jobs, overrides, skipped)
        }
        (false, Some(SyncSource::Hackernews(mut args))) => {
            let (account, all) = (args.account.take(), args.all);
            let selection = Selection::new(account.as_deref(), all)?;
            let overrides = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure);
            let (jobs, skipped) = build_jobs::<HackerNewsAccount>(selection, &overrides, config)?;
            (jobs, overrides, skipped)
        }
        (false, None) => bail!("specify a source (e.g. `sync reddit`) or pass --all"),
    };
    Ok(SyncPlan {
        jobs,
        overrides,
        skipped,
    })
}

/// Fetch every job's source concurrently (reads only, on one task), then write the
/// merged, de-duplicated drafts to Pinboard sequentially (writes are rate-limited).
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
            // Surface the failure (firing the hook on Halt::Reauth) but keep going.
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
    // Non-zero exit if any source failed to fetch or any bookmark failed to write,
    // but only after attempting every source and every bookmark we could.
    run.finish()?;
    if outcome.failed > 0 {
        bail!("{} bookmark(s) failed to write", outcome.failed);
    }
    Ok(())
}

// --- backup ------------------------------------------------------------------

/// Write a snapshot of every selected service: the verbatim API responses under `raw/`
/// and the same items as domain bookmarks under `normalized/`, plus a `manifest.json`.
/// Each run replaces the previous snapshot in place.
pub async fn run_backup(mut cmd: BackupCmd, config: &Config) -> Result<()> {
    let (top_out, top_dry_run) = (cmd.out.out.take(), cmd.dry_run);
    let plan = build_backup_plan(cmd, config)?;
    let dir = resolve_backup_dir([plan.out, top_out], config)?;
    let mode = WriteMode::from_flags(top_dry_run, plan.dry_run);
    back_up(plan.jobs, plan.skipped, &dir, mode).await
}

/// Run every job of a backup plan into `dir`, then write the manifest, failing the run
/// if any target was `skipped` or failed.
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
    // Sequential, unlike `sync`: backup writes are local, so concurrency would only
    // multiply simultaneous API pressure and interleave the dry-run output.
    for job in &jobs {
        info!("{}: backing up", job.label);
        match backup::run_job(&job.client, &job.stem, dir, mode).await {
            Ok(outcome) => {
                // Recorded whether or not the target is trustworthy: it replaced these
                // files either way, and leaving them out would let the merged manifest
                // keep the previous run's entry describing what is no longer there.
                // The reason is stamped onto each entry as well as counted in `failed`,
                // because a later clean run of a *different* target rewrites `failed` and
                // would otherwise leave these files looking healthy.
                entries.extend(outcome.written.into_iter().map(|mut e| {
                    e.unusable = outcome.unusable.clone();
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
        println!("[dry-run] {} file(s) would be written.", entries.len());
    } else {
        // Written even on a partial run, carrying `complete: false` and the failed
        // targets -- a target that failed left its previous files in place, and the
        // manifest is what stops them passing as part of this run.
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

/// The snapshot directory: the `--out` after the target, else the one before it, else
/// `[backup].directory`. There is no built-in default -- writing a snapshot into an
/// unexpected working directory is worse than an error -- and an empty `--out` in either
/// place is refused, as an empty `[backup].directory` is, rather than skipped.
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
            // Cleanup normalizes the shared Pinboard bookmark set, so it runs once
            // per cleanup-capable service (reddit, github, hackernews), using the
            // first configured account of each for its cookie/domain/tags.
            if !config.has_accounts() {
                bail!("cleanup --all requires a --config with at least one configured account");
            }
            let github = cleanup_all_target::<GitHubAccount>(&overrides, config)?;
            let reddit = cleanup_all_target::<RedditAccount>(&overrides, config)?;
            let hackernews = cleanup_all_target::<HackerNewsAccount>(&overrides, config)?;
            let (pinboard, bookmarks) = open_pinboard(None, config).await?;
            // One view across all three sources: each writes, and the next must plan
            // against what it left rather than the pre-run snapshot.
            let mode = WriteMode::from_flags(cmd.dry_run, false);
            let store = CleanupStore::new(&pinboard, AccountView::new(bookmarks), mode);
            let mut run = AllRun::default();
            if let Some(CleanupTarget { account, dates }) = github {
                run.record(cleanup_github_account(&store, account, None, dates, config).await);
            }
            if let Some(CleanupTarget { account, dates }) = reddit {
                let reddit_overrides = RedditCleanupOverrides {
                    cookie: None,
                    mark_nsfw: true,
                    fix_titles: true,
                };
                run.record(
                    cleanup_reddit_account(account, reddit_overrides, dates, &store, config).await,
                );
            }
            if let Some(CleanupTarget { account, dates }) = hackernews {
                let overrides = HackerNewsCleanupOverrides::default();
                run.record(cleanup_hackernews_account(account, overrides, dates, &store).await);
            }
            run.finish()
        }
        (false, Some(CleanupSource::Reddit(args))) => {
            run_cleanup_reddit(args, cmd.dry_run, &overrides, config).await
        }
        (false, Some(CleanupSource::Github(args))) => {
            run_cleanup_github(args, cmd.dry_run, &overrides, config).await
        }
        (false, Some(CleanupSource::Hackernews(args))) => {
            run_cleanup_hackernews(args, cmd.dry_run, &overrides, config).await
        }
        (false, None) => bail!("specify a source (e.g. `cleanup reddit`) or pass --all"),
    }
}

/// One source's cleanup pass: the account it runs as (`None` for the implicit CLI/env
/// account) and its dates. Resolved before `posts/all` is read, so a refused run does not
/// spend that rate-limited call.
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

/// The [`CleanupTarget`] of `cleanup <source> [account]`: the account named `name`, else
/// the first, else the implicit CLI/env account.
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

/// The [`CleanupTarget`] of `A`'s pass in `cleanup --all`, run as the source's first
/// account, or `None` for a source with no accounts.
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
    top_dry_run: bool,
    top_overrides: &DateFlags,
    config: &Config,
) -> Result<()> {
    let CleanupTarget { account, dates } = source_cleanup_target::<GitHubAccount>(
        args.account.as_deref(),
        &args.dates.with_top_level(top_overrides),
        config,
    )?;
    let client = github_cleanup_client(account, args.github_token, config)?;
    let (pinboard, bookmarks) = open_pinboard(args.pinboard_token, config).await?;
    let mode = WriteMode::from_flags(top_dry_run, args.dry_run);
    let store = CleanupStore::new(&pinboard, AccountView::new(bookmarks), mode);
    cleanup_github_with_client(&store, &client, account, dates, config).await
}

/// The GitHub API client a cleanup pass runs with, from the flag/account/env token.
fn github_cleanup_client(
    account: Option<&GitHubAccount>,
    token_flag: Option<String>,
    config: &Config,
) -> Result<GitHubClient> {
    let token = resolve_github_token(token_flag, account)
        .with_context(|| SecretSpec::GITHUB_TOKEN.missing())?;
    GitHubClient::new(
        token,
        GitHubConfig::resolve(account, &config.defaults.github),
    )
}

/// Run github cleanup: build an API client from the account/env token and refresh
/// renamed repos / titles / language.
async fn cleanup_github_account<S: BookmarkStore + AccountState>(
    store: &S,
    account: Option<&GitHubAccount>,
    token_flag: Option<String>,
    dates: DateOpts,
    config: &Config,
) -> Result<()> {
    let client = github_cleanup_client(account, token_flag, config)?;
    cleanup_github_with_client(store, &client, account, dates, config).await
}

/// Run github cleanup with `client` already built.
async fn cleanup_github_with_client<S: BookmarkStore + AccountState>(
    store: &S,
    client: &GitHubClient,
    account: Option<&GitHubAccount>,
    dates: DateOpts,
    config: &Config,
) -> Result<()> {
    let hook = cleanup_hook(account, config);
    let configured: Vec<GitHubConfig> = config
        .github
        .iter()
        .map(|account| GitHubConfig::resolve(Some(account), &config.defaults.github))
        .collect();
    github::cleanup::run(store, client, dates, &configured)
        .await
        .map_err(|e| handle_source_err(e, hook.as_deref()))
}

async fn run_cleanup_reddit(
    mut args: RedditCleanupArgs,
    top_dry_run: bool,
    top_overrides: &DateFlags,
    config: &Config,
) -> Result<()> {
    let mode = WriteMode::from_flags(top_dry_run, args.dry_run);
    let CleanupTarget { account, dates } = source_cleanup_target::<RedditAccount>(
        args.account.as_deref(),
        &args.dates.with_top_level(top_overrides),
        config,
    )?;
    let pinboard_token = args.pinboard_token.take();
    let reddit = RedditCleanup::build(account, RedditCleanupOverrides::from(args), dates, config)?;
    let (pinboard, bookmarks) = open_pinboard(pinboard_token, config).await?;
    let store = CleanupStore::new(&pinboard, AccountView::new(bookmarks), mode);
    reddit.run(&store, account, config).await
}

async fn cleanup_reddit_account<S: BookmarkStore + AccountState>(
    account: Option<&RedditAccount>,
    overrides: RedditCleanupOverrides,
    dates: DateOpts,
    store: &S,
    config: &Config,
) -> Result<()> {
    RedditCleanup::build(account, overrides, dates, config)?
        .run(store, account, config)
        .await
}

/// A reddit cleanup pass's options, and the `/api/info` client when a step needs one.
struct RedditCleanup {
    opts: reddit::cleanup::RedditCleanupOpts,
    client: Option<RedditInfoClient>,
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
            Some(RedditInfoClient::new(cookie)?)
        } else {
            None
        };
        Ok(Self { opts, client })
    }

    async fn run<S: BookmarkStore + AccountState>(
        &self,
        store: &S,
        account: Option<&RedditAccount>,
        config: &Config,
    ) -> Result<()> {
        let hook = cleanup_hook(account, config);
        reddit::cleanup::run(store, self.client.as_ref(), &self.opts)
            .await
            .map_err(|e| handle_source_err(e, hook.as_deref()))
    }
}

async fn run_cleanup_hackernews(
    mut args: HackerNewsCleanupArgs,
    top_dry_run: bool,
    top_overrides: &DateFlags,
    config: &Config,
) -> Result<()> {
    let CleanupTarget { account, dates } = source_cleanup_target::<HackerNewsAccount>(
        args.account.as_deref(),
        &args.dates.with_top_level(top_overrides),
        config,
    )?;
    let mode = WriteMode::from_flags(top_dry_run, args.dry_run);
    let pinboard_token = args.pinboard_token.take();
    let hackernews =
        HackerNewsCleanup::build(account, HackerNewsCleanupOverrides::from(args), dates)?;
    let (pinboard, bookmarks) = open_pinboard(pinboard_token, config).await?;
    let store = CleanupStore::new(&pinboard, AccountView::new(bookmarks), mode);
    hackernews.run(&store).await
}

/// Build the HackerNews tag config for a cleanup run, letting `--tag-link` override the
/// config `tag_link`. The override is validated for whitespace like the config value so a
/// space-bearing marker (which Pinboard would split, matching nothing) fails loudly.
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

async fn cleanup_hackernews_account<S: BookmarkStore + AccountState>(
    account: Option<&HackerNewsAccount>,
    overrides: HackerNewsCleanupOverrides,
    dates: DateOpts,
    store: &S,
) -> Result<()> {
    HackerNewsCleanup::build(account, overrides, dates)?
        .run(store)
        .await
}

/// A HackerNews cleanup pass's options, on the tag config
/// [`resolve_hackernews_cleanup_config`] gives, and its Algolia client.
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
            // No hook: HackerNews is public, so nothing here can be a re-auth to act on.
            .map_err(SourceError::into_anyhow)
    }
}

#[cfg(test)]
mod tests;

/// Tests that need a socket; the flake skips `net_tests` in the Nix build sandbox.
#[cfg(test)]
mod net_tests;
