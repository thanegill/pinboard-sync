//! The `sync`, `backup` and `cleanup` subcommands: building each one's jobs from the
//! parsed command line and the config, and running them.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use log::{info, warn};

use crate::bookmark::{AccountState, AccountView, Bookmark, BookmarkStore, CleanupStore};
use crate::cli::{
    BackupCmd, CleanupCmd, CleanupSource, DateFlags, GitHubCleanupArgs, HackerNewsCleanupArgs,
    RedditCleanupArgs, SyncCmd, SyncSource,
};
use crate::config::{Config, GitHubAccount, HackerNewsAccount, RedditAccount};
use crate::github::{GitHubClient, GitHubConfig};
use crate::hackernews::cleanup::HackerNewsCleanupOpts;
use crate::hackernews::{HackerNewsClient, HackerNewsConfig};
use crate::hook::{cleanup_hook, handle_source_err, on_auth_failure_from_env};
use crate::jobs::backup_jobs::{build_backup_plan, BackupJob};
use crate::jobs::{
    build_all_jobs, build_jobs, open_pinboard, AllRun, SkippedTarget, SourceAccount, SyncJob,
    SyncOverrides,
};
use crate::pinboard::PinboardClient;
use crate::reddit::cleanup::RedditCleanupOverrides;
use crate::reddit::model::RedditConfig;
use crate::reddit::RedditClient;
use crate::secrets::{
    first_nonempty, missing_github_token, missing_reddit_cookie, resolve_github_token,
    resolve_reddit_cookie,
};
use crate::settings::resolve_cleanup_dates;
use crate::source::{BookmarkDraft, SourceError};
use crate::timefmt::DateOpts;
use crate::{backup, config, github, hackernews, reddit, sync, timefmt};

// --- sync --------------------------------------------------------------------

pub async fn run_sync(cmd: SyncCmd, config: &Config) -> Result<()> {
    let top_dry_run = cmd.dry_run;
    let plan = build_sync_plan(cmd, config)?;
    let mut run = AllRun::default();
    run.record_skipped(plan.skipped, "could not be built");
    let (pinboard, bookmarks) =
        open_pinboard(plan.overrides.flags.pinboard_token.clone(), config).await?;
    // `--dry-run` is accepted both before the source subcommand (on `SyncCmd`) and
    // after it (per-source); honor either placement. (`--verbose` is global.)
    run_sync_jobs(
        plan.jobs,
        run,
        &pinboard,
        &bookmarks,
        plan.overrides.dry_run || top_dry_run,
    )
    .await
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
        (false, Some(SyncSource::Reddit(args))) => {
            let (account, all) = (args.account.clone(), args.all);
            let overrides = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure);
            let (jobs, skipped) =
                build_jobs::<RedditAccount>(account.as_deref(), all, &overrides, config)?;
            (jobs, overrides, skipped)
        }
        (false, Some(SyncSource::Github(args))) => {
            let (account, all) = (args.account.clone(), args.all);
            let overrides = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure);
            let (jobs, skipped) =
                build_jobs::<GitHubAccount>(account.as_deref(), all, &overrides, config)?;
            (jobs, overrides, skipped)
        }
        (false, Some(SyncSource::Hackernews(args))) => {
            let (account, all) = (args.account.clone(), args.all);
            let overrides = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure);
            let (jobs, skipped) =
                build_jobs::<HackerNewsAccount>(account.as_deref(), all, &overrides, config)?;
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
    dry_run: bool,
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
            // Surface the failure (firing the hook on ReauthRequired) but keep going.
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
        if dry_run { " (dry run)" } else { "" }
    );
    let outcome = sync::write_drafts(pinboard, &merged, dry_run).await;
    if !dry_run {
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
pub async fn run_backup(cmd: BackupCmd, config: &Config) -> Result<()> {
    let plan = build_backup_plan(&cmd, config)?;
    let dir = resolve_backup_dir(plan.out.or_else(|| cmd.out.out.clone()), config)?;
    back_up(plan.jobs, plan.skipped, &dir, plan.dry_run || cmd.dry_run).await
}

/// Run every job of a backup plan into `dir`, then write the manifest, failing the run
/// if any target was `skipped` or failed.
async fn back_up(
    jobs: Vec<BackupJob>,
    skipped: Vec<SkippedTarget>,
    dir: &Path,
    dry_run: bool,
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
        match backup::run_job(&job.client, &job.stem, dir, dry_run).await {
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

    if dry_run {
        println!("[dry-run] {} file(s) would be written.", entries.len());
    } else {
        // Written even on a partial run, carrying `complete: false` and the failed
        // targets — a target that failed left its previous files in place, and the
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

/// The snapshot directory: the `--out` flag, else `[backup].directory`. There is no
/// built-in default — writing a snapshot into an unexpected working directory is worse
/// than an error.
fn resolve_backup_dir(flag: Option<String>, config: &Config) -> Result<PathBuf> {
    first_nonempty([flag, config.backup.directory.clone()])
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
            let store = CleanupStore::new(&pinboard, AccountView::new(bookmarks), cmd.dry_run);
            let mut run = AllRun::default();
            if let Some(CleanupTarget { account, dates }) = github {
                run.record(
                    cleanup_github_account(&store, Some(account), None, dates, config).await,
                );
            }
            if let Some(CleanupTarget { account, dates }) = reddit {
                let reddit_overrides = RedditCleanupOverrides {
                    cookie: None,
                    mark_nsfw: true,
                    fix_titles: true,
                };
                run.record(
                    cleanup_reddit_account(Some(account), reddit_overrides, dates, &store, config)
                        .await,
                );
            }
            if let Some(CleanupTarget { account, dates }) = hackernews {
                run.record(
                    cleanup_hackernews_account(
                        Some(account),
                        false, // linking is opt-in via `cleanup hackernews --link-discussions`
                        None,
                        dates,
                        &store,
                    )
                    .await,
                );
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

/// One source's pass of a `cleanup --all` run: the account it runs as and its dates.
struct CleanupTarget<'a, A> {
    account: &'a A,
    dates: DateOpts,
}

/// The [`CleanupTarget`] of `A`'s pass in `cleanup --all`, run as the source's first
/// account, or `None` for a source with no accounts. Resolved before `posts/all` is read,
/// so a refused run does not spend that rate-limited call.
fn cleanup_all_target<'a, A: SourceAccount>(
    overrides: &DateFlags,
    config: &'a Config,
) -> Result<Option<CleanupTarget<'a, A>>> {
    let Some(account) = A::configured(config).first() else {
        return Ok(None);
    };
    let pass = format!("`cleanup --all` ({} pass)", A::SOURCE);
    let dates = resolve_cleanup_dates(
        &pass,
        A::SOURCE,
        overrides,
        Some(account),
        A::defaults(config),
        config,
    )?;
    Ok(Some(CleanupTarget { account, dates }))
}

/// A cleanup pass runs dry when `--dry-run` is given either before the source
/// subcommand (top-level `CleanupCmd::dry_run`) or after it (the per-source args),
/// mirroring how `run_sync` honors the flag on both sides of the subcommand.
fn cleanup_dry_run(top_dry_run: bool, source_dry_run: bool) -> bool {
    top_dry_run || source_dry_run
}

async fn run_cleanup_github(
    args: GitHubCleanupArgs,
    top_dry_run: bool,
    top_overrides: &DateFlags,
    config: &Config,
) -> Result<()> {
    let overrides = args.dates.with_top_level(top_overrides);
    let account = config::select_account(&config.github, args.account.as_deref())?;
    let dates = resolve_cleanup_dates(
        "`cleanup github`",
        "github",
        &overrides,
        account,
        &config.defaults.github,
        config,
    )?;
    let (pinboard, bookmarks) = open_pinboard(args.pinboard_token, config).await?;
    let dry_run = cleanup_dry_run(top_dry_run, args.dry_run);
    let store = CleanupStore::new(&pinboard, AccountView::new(bookmarks), dry_run);
    cleanup_github_account(&store, account, args.github_token, dates, config).await
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
    let token = resolve_github_token(token_flag, account).with_context(missing_github_token)?;
    let client = GitHubClient::new(token, account.map(GitHubConfig::from).unwrap_or_default())?;
    cleanup_github_with_client(store, &client, account, dates, config).await
}

/// [`cleanup_github_account`] with its client built, which a test points at a fake API.
async fn cleanup_github_with_client<S: BookmarkStore + AccountState>(
    store: &S,
    client: &GitHubClient,
    account: Option<&GitHubAccount>,
    dates: DateOpts,
    config: &Config,
) -> Result<()> {
    let hook = cleanup_hook(account, config);
    let configured: Vec<GitHubConfig> = config.github.iter().map(GitHubConfig::from).collect();
    github::cleanup::run(store, client, dates, &configured)
        .await
        .map_err(|e| handle_source_err(e, hook.as_deref()))
}

async fn run_cleanup_reddit(
    args: RedditCleanupArgs,
    top_dry_run: bool,
    top_overrides: &DateFlags,
    config: &Config,
) -> Result<()> {
    let dry_run = cleanup_dry_run(top_dry_run, args.dry_run);
    // One pass over the Pinboard account's reddit bookmarks, using the selected (or
    // first, or implicit CLI/env) account's cookie + domain/tags.
    let account = config::select_account(&config.reddit, args.account.as_deref())?;
    let dates = resolve_cleanup_dates(
        "`cleanup reddit`",
        "reddit",
        &args.dates.with_top_level(top_overrides),
        account,
        &config.defaults.reddit,
        config,
    )?;
    let (pinboard, bookmarks) = open_pinboard(args.pinboard_token.clone(), config).await?;
    let store = CleanupStore::new(&pinboard, AccountView::new(bookmarks), dry_run);
    cleanup_reddit_account(
        account,
        RedditCleanupOverrides::from(&args),
        dates,
        &store,
        config,
    )
    .await
}

async fn cleanup_reddit_account<S: BookmarkStore + AccountState>(
    account: Option<&RedditAccount>,
    overrides: RedditCleanupOverrides,
    dates: DateOpts,
    store: &S,
    config: &Config,
) -> Result<()> {
    let opts = reddit::cleanup::RedditCleanupOpts {
        mark_nsfw: overrides.mark_nsfw,
        fix_titles: overrides.fix_titles,
        config: account.map(RedditConfig::from).unwrap_or_default(),
        dates,
        configured: config.reddit.iter().map(RedditConfig::from).collect(),
    };

    // Reddit (for /api/info) is needed when marking NSFW, fixing titles, or dating by
    // the source post (the post's created_utc comes from /api/info too).
    let reddit = if opts.mark_nsfw || opts.fix_titles || opts.dates.use_post_date {
        let cookie =
            resolve_reddit_cookie(overrides.cookie, account).with_context(missing_reddit_cookie)?;
        Some(RedditClient::for_info(Some(cookie))?)
    } else {
        None
    };

    let hook = cleanup_hook(account, config);
    reddit::cleanup::run(store, reddit.as_ref(), &opts)
        .await
        .map_err(|e| handle_source_err(e, hook.as_deref()))
}

async fn run_cleanup_hackernews(
    args: HackerNewsCleanupArgs,
    top_dry_run: bool,
    top_overrides: &DateFlags,
    config: &Config,
) -> Result<()> {
    // One pass over the Pinboard account's HN bookmarks, using the selected (or
    // first, or implicit) account's tag config.
    let account = config::select_account(&config.hackernews, args.account.as_deref())?;
    let dates = resolve_cleanup_dates(
        "`cleanup hackernews`",
        "hackernews",
        &args.dates.with_top_level(top_overrides),
        account,
        &config.defaults.hackernews,
        config,
    )?;
    let (pinboard, bookmarks) = open_pinboard(args.pinboard_token.clone(), config).await?;
    let dry_run = cleanup_dry_run(top_dry_run, args.dry_run);
    let store = CleanupStore::new(&pinboard, AccountView::new(bookmarks), dry_run);
    cleanup_hackernews_account(account, args.link_discussions, args.link_tag, dates, &store).await
}

/// Build the HackerNews tag config for a cleanup run, letting `--link-tag` override the
/// config `tag_link`. The override is validated for whitespace like the config value so a
/// space-bearing marker (which Pinboard would split, matching nothing) fails loudly.
fn resolve_hackernews_cleanup_config(
    account: Option<&HackerNewsAccount>,
    link_tag: Option<String>,
) -> Result<HackerNewsConfig> {
    let mut hn_config = account.map(HackerNewsConfig::from).unwrap_or_default();
    if let Some(tag) = link_tag {
        config::check_taggable("`--link-tag`", &tag)?;
        hn_config.link_tag = tag;
    }
    Ok(hn_config)
}

async fn cleanup_hackernews_account<S: BookmarkStore + AccountState>(
    account: Option<&HackerNewsAccount>,
    link_discussions: bool,
    link_tag: Option<String>,
    dates: DateOpts,
    store: &S,
) -> Result<()> {
    let hn_config = resolve_hackernews_cleanup_config(account, link_tag)?;
    let hn = HackerNewsClient::for_cleanup(hn_config)?;
    hackernews::cleanup::run(
        store,
        &hn,
        &HackerNewsCleanupOpts {
            link_discussions,
            dates,
        },
    )
    .await
    // No hook: HackerNews is public, so nothing here can be a re-auth to act on.
    .map_err(SourceError::into_anyhow)
}

#[cfg(test)]
mod tests;
