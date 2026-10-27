//! pinboard-sync: sync saved/favorited items from multiple services to Pinboard.

mod backup;
mod bookmark;
mod cleanup_pass;
mod cli;
mod config;
mod doctor;
mod domains;
mod github;
mod hackernews;
mod hook;
mod htmltext;
mod http;
mod jobs;
mod pinboard;
mod reddit;
mod secrets;
mod source;
mod sync;
mod tags;
#[cfg(test)]
mod test_support;
mod timefmt;
mod urls;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};
use clap::{CommandFactory, Parser};
use clap_complete::Shell;
use log::{debug, error, info, warn};

use bookmark::{AccountState, AccountView, Bookmark, BookmarkStore, CleanupStore};
use cli::{
    BackupCmd, CleanupCmd, CleanupSource, Cli, Command, ConfigAction, GitHubCleanupArgs,
    HackerNewsCleanupArgs, RedditCleanupArgs, SyncCmd, SyncSource,
};
use config::{Config, GitHubAccount, HackerNewsAccount, RedditAccount};
use github::{GitHubClient, GitHubConfig};
use hackernews::cleanup::HackerNewsCleanupOpts;
use hackernews::{HackerNewsClient, HackerNewsConfig};
use hook::{cleanup_hook, handle_source_err, on_auth_failure_from_env};
use jobs::backup_jobs::{build_backup_plan, BackupJob};
use jobs::{
    build_all_jobs, build_jobs, open_pinboard, AllRun, SkippedTarget, SourceAccount, SyncJob,
    SyncOverrides,
};
use pinboard::PinboardClient;
use reddit::cleanup::RedditCleanupOverrides;
use reddit::model::RedditConfig;
use reddit::RedditClient;
use secrets::{
    first_nonempty, missing_github_token, missing_reddit_cookie, resolve_github_token,
    resolve_reddit_cookie,
};
use source::{BookmarkDraft, SourceError};
use timefmt::{resolve_cleanup_dates, DateOpts, DateOverrides};

#[tokio::main]
async fn main() -> ExitCode {
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let result = async {
        match cli.command {
            Command::Sync(cmd) => {
                log_start("sync");
                run_sync(cmd, &load_config(cli.config.clone())?).await
            }
            Command::Cleanup(cmd) => {
                log_start("cleanup");
                run_cleanup(cmd, &load_config(cli.config.clone())?).await
            }
            Command::Doctor => {
                log_start("doctor");
                doctor::run_doctor(&load_config(cli.config.clone())?).await
            }
            Command::Backup(cmd) => {
                log_start("backup");
                run_backup(cmd, &load_config(cli.config.clone())?).await
            }
            Command::Completions { shell } => {
                print_completions(shell);
                Ok(())
            }
            Command::Man => print_man(),
            Command::Config {
                action: ConfigAction::Example,
            } => {
                print!("{}", include_str!("config.example.toml"));
                Ok(())
            }
            Command::Config {
                action: ConfigAction::Check { file },
            } => check_config(&file).map(|summary| println!("{summary}")),
        }
    }
    .await;

    if let Err(e) = result {
        error!("{e:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Initialize logging, written to stderr with a short `LEVEL message` format so stdout
/// stays clean for generated output (dry-run listings, completions, man, config).
/// Verbosity escalates with repeated `-v`: info → debug (`-v`) → trace (`-vv`) → trace
/// for every crate, including dependencies (`-vvv`). `RUST_LOG` overrides the filter.
fn init_logging(verbose: u8) {
    use std::io::Write;
    let default = match verbose {
        0 => concat!(env!("CARGO_CRATE_NAME"), "=info"),
        1 => concat!(env!("CARGO_CRATE_NAME"), "=debug"),
        2 => concat!(env!("CARGO_CRATE_NAME"), "=trace"),
        _ => "trace",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default))
        .format(|buf, record| writeln!(buf, "{:5} {}", record.level(), record.args()))
        .init();
}

/// Log the startup banner (version + subcommand) for an operational run.
fn log_start(command: &str) {
    info!(
        "pinboard-sync {} starting ({command})",
        env!("CARGO_PKG_VERSION")
    );
}

/// Write a shell completion script for `shell` to stdout.
fn print_completions(shell: Shell) {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
}

/// Write a roff man page for the CLI to stdout.
fn print_man() -> Result<()> {
    use std::io::Write;
    let mut buf = Vec::new();
    clap_mangen::Man::new(Cli::command())
        .render(&mut buf)
        .context("rendering man page")?;
    // A closed pipe (e.g. `pinboard-sync man | head`) is not an error.
    match std::io::stdout().write_all(&buf) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        r => r.context("writing man page"),
    }
}

/// Load the config from `--config` / `$PINBOARD_SYNC_CONFIG` (a direct path to the
/// TOML file); absent = defaults. Unlike secrets, the config takes no `_FILE` form —
/// it is already a file path.
fn load_config(flag: Option<String>) -> Result<Config> {
    let path = flag
        .or_else(|| std::env::var("PINBOARD_SYNC_CONFIG").ok())
        .filter(|s| !s.is_empty());
    match path {
        Some(path) => {
            let config = read_config(Path::new(&path))?;
            info!("loaded config {path}: {}", account_counts(&config));
            Ok(config)
        }
        None => {
            debug!("no --config / PINBOARD_SYNC_CONFIG; using built-in defaults");
            Ok(Config::default())
        }
    }
}

fn read_config(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    Config::parse(&text)
}

fn account_counts(config: &Config) -> String {
    format!(
        "{} reddit, {} github, {} hackernews account(s)",
        config.reddit.len(),
        config.github.len(),
        config.hackernews.len()
    )
}

/// The `config check` verdict: `path` read and validated exactly as `--config` is, which
/// resolves no secret or `*_file` path and contacts no service.
fn check_config(path: &Path) -> Result<String> {
    let config = read_config(path)?;
    Ok(format!(
        "{}: OK ({})",
        path.display(),
        account_counts(&config)
    ))
}

// --- sync --------------------------------------------------------------------

async fn run_sync(cmd: SyncCmd, config: &Config) -> Result<()> {
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
async fn run_backup(cmd: BackupCmd, config: &Config) -> Result<()> {
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

async fn run_cleanup(cmd: CleanupCmd, config: &Config) -> Result<()> {
    let overrides = DateOverrides::from(&cmd.dates);
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
    overrides: &DateOverrides,
    config: &'a Config,
) -> Result<Option<CleanupTarget<'a, A>>> {
    let Some(account) = A::configured(config).first() else {
        return Ok(None);
    };
    let pass = format!("`cleanup --all` ({} pass)", A::SOURCE);
    let dates =
        resolve_cleanup_dates(&pass, overrides, Some(account), A::defaults(config), config)?;
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
    top_overrides: &DateOverrides,
    config: &Config,
) -> Result<()> {
    let overrides = DateOverrides::from(&args.dates).with_top_level(top_overrides);
    let account = config::select_account(&config.github, args.account.as_deref())?;
    let dates = resolve_cleanup_dates(
        "`cleanup github`",
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
    top_overrides: &DateOverrides,
    config: &Config,
) -> Result<()> {
    let dry_run = cleanup_dry_run(top_dry_run, args.dry_run);
    // One pass over the Pinboard account's reddit bookmarks, using the selected (or
    // first, or implicit CLI/env) account's cookie + domain/tags.
    let account = config::select_account(&config.reddit, args.account.as_deref())?;
    let dates = resolve_cleanup_dates(
        "`cleanup reddit`",
        &DateOverrides::from(&args.dates).with_top_level(top_overrides),
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
    top_overrides: &DateOverrides,
    config: &Config,
) -> Result<()> {
    // One pass over the Pinboard account's HN bookmarks, using the selected (or
    // first, or implicit) account's tag config.
    let account = config::select_account(&config.hackernews, args.account.as_deref())?;
    let dates = resolve_cleanup_dates(
        "`cleanup hackernews`",
        &DateOverrides::from(&args.dates).with_top_level(top_overrides),
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
        config::check_taggable("--link-tag", &tag)?;
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
mod tests {
    use super::*;
    use crate::cli::{BackupTarget, RedditSyncArgs};
    use crate::jobs::build_job;
    use crate::test_support::lock_on_auth_failure_env;

    #[test]
    fn link_tag_override_rejects_whitespace_and_commas() {
        assert!(resolve_hackernews_cleanup_config(None, Some("find hn".into())).is_err());
        assert!(resolve_hackernews_cleanup_config(None, Some("find,hn".into())).is_err());
    }

    #[test]
    fn link_tag_override_accepts_a_bare_tag() {
        let hn_config =
            resolve_hackernews_cleanup_config(None, Some("find-hn".into())).expect("valid tag");
        assert_eq!(hn_config.link_tag, "find-hn");
    }

    #[test]
    fn config_check_takes_the_file_as_a_positional() {
        match parse("config", &["check", "pinboard-sync.toml"]) {
            Command::Config {
                action: ConfigAction::Check { file },
            } => assert_eq!(file, Path::new("pinboard-sync.toml")),
            _ => panic!("expected `config check`"),
        }
    }

    fn check_config_text(text: &str) -> Result<String> {
        let dir = crate::test_support::scratch_dir("config-check");
        let path = dir.path().join("pinboard-sync.toml");
        std::fs::write(&path, text).unwrap();
        check_config(&path)
    }

    #[test]
    fn config_check_reports_ok_with_the_account_counts() {
        let summary = check_config_text("[[hackernews]]\nusername = \"example\"\n").unwrap();
        assert!(
            summary
                .ends_with("pinboard-sync.toml: OK (0 reddit, 0 github, 1 hackernews account(s))"),
            "{summary}"
        );
    }

    #[test]
    fn config_check_does_not_read_the_secret_files_it_names() {
        let text = "[pinboard]\ntoken_file = \"/nonexistent/pinboard-token\"\n\
                    [[reddit]]\ncookie_file = \"/nonexistent/reddit-cookie\"\n\
                    [[github]]\ntoken_file = \"/nonexistent/github-token\"\n";
        assert!(check_config_text(text).is_ok());
    }

    #[test]
    fn config_check_failure_names_the_field_not_the_value() {
        for (text, field) in [
            (
                "[pinboard]\ntoken = \"user:INLINESECRET\"\ntokn = \"user:TYPOSECRET\"\n",
                "unknown field `tokn`",
            ),
            (
                "[[reddit]]\ntags = [\"reddit_session=SECRET x\"]\n",
                "`reddit[0].tags[0]` must not contain whitespace",
            ),
            (
                "[[reddit]]\nreddit_domain = \"SECRET.example:8080\"\n",
                "`reddit[0].reddit_domain` contains a `:`",
            ),
            (
                "[[github]]\nlimit = \"ghp_SECRET\"\n",
                "line 2, column 9: `limit` must be",
            ),
            (
                "[[github]]\ntoken = \"ghp_SECRET\"\n[[github]]\ntoken = \"ghp_SECRET2\"\n",
                "github[0] and github[1] would both back up as `github-default`",
            ),
            (
                "[[reddit]]\ncookie = \"reddit_session=SECRET\"\n[[reddit]]\n",
                "reddit[0] and reddit[1] would both back up as `reddit-default`",
            ),
        ] {
            let error = format!("{:#}", check_config_text(text).unwrap_err());
            assert!(error.contains(field), "{error}");
            assert!(!error.contains("SECRET"), "{error}");
        }
    }

    #[test]
    fn config_check_of_a_missing_file_names_the_path() {
        let dir = crate::test_support::scratch_dir("config-check-missing");
        let path = dir.path().join("absent.toml");
        let error = format!("{:#}", check_config(&path).unwrap_err());
        assert!(error.contains("reading config file"), "{error}");
        assert!(error.contains("absent.toml"), "{error}");
    }

    /// Parse `pinboard-sync <subcommand> <extra>` and return the command.
    fn parse(subcommand: &str, extra: &[&str]) -> Command {
        use clap::Parser;
        let argv = ["pinboard-sync", subcommand]
            .into_iter()
            .chain(extra.iter().copied());
        Cli::try_parse_from(argv).expect("parses").command
    }

    /// Parse `sync <extra>` and return the top-level command.
    fn parse_sync(extra: &[&str]) -> SyncCmd {
        let Command::Sync(cmd) = parse("sync", extra) else {
            panic!("expected `sync` command");
        };
        cmd
    }

    /// The `sync reddit` args of a parsed `sync`.
    fn reddit_sync_args(cmd: &SyncCmd) -> RedditSyncArgs {
        let Some(SyncSource::Reddit(args)) = &cmd.source else {
            panic!("expected `sync reddit` args");
        };
        args.clone()
    }

    /// Both `--all` forms must carry the accounts they could not build out in
    /// `SyncPlan::skipped`, or a run that synced only the healthy ones exits 0.
    #[test]
    fn sync_all_counts_the_accounts_it_could_not_build() {
        let _env = lock_on_auth_failure_env();
        let config = Config {
            github: vec![
                GitHubAccount {
                    name: Some("main".into()),
                    token: Some("ghp_test".into()),
                    ..Default::default()
                },
                GitHubAccount {
                    name: Some("tokenless".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        for argv in [&["github", "--all"][..], &["--all"]] {
            let plan = build_sync_plan(parse_sync(argv), &config).expect("plans");
            assert_eq!(
                (plan.jobs.len(), plan.skipped.len()),
                (1, 1),
                "sync {argv:?}"
            );
        }
    }

    #[tokio::test]
    async fn run_sync_jobs_fails_the_run_for_accounts_it_could_not_build() {
        let pinboard = PinboardClient::new("user:token".into(), 0).unwrap();
        let error = run_sync_jobs(Vec::new(), AllRun { failed: 1 }, &pinboard, &[], false)
            .await
            .expect_err("an unbuilt account fails the run");
        assert_eq!(error.to_string(), "1 account(s) failed");
    }

    #[test]
    fn top_level_all_threads_on_auth_failure_into_the_hook() {
        let _env = lock_on_auth_failure_env();
        // The `sync --all` path builds `SyncOverrides` itself (not from the source args),
        // so the hook must be carried explicitly from the top-level flag.
        let cmd = parse_sync(&["--all", "--on-auth-failure", "refresh-cookie"]);
        assert_eq!(cmd.on_auth_failure.as_deref(), Some("refresh-cookie"));

        let overrides = SyncOverrides {
            dry_run: cmd.dry_run,
            on_auth_failure: cmd.on_auth_failure,
            reddit_username: Some("alice".into()),
            reddit_cookie: Some("reddit_session=x".into()),
            ..SyncOverrides::default()
        };
        let job = build_job::<RedditAccount>(None, &overrides, &Config::default()).expect("builds");
        assert_eq!(job.hook.as_deref(), Some("refresh-cookie"));
    }

    #[test]
    fn sync_all_falls_back_to_on_auth_failure_env_when_flag_absent() {
        let _env = lock_on_auth_failure_env();
        // The NixOS service runs `sync --all` with the hook supplied only via
        // `PINBOARD_SYNC_ON_AUTH_FAILURE`. The top-level flag doesn't bind that env,
        // so the `--all` arm must read it directly or the hook silently never fires.
        std::env::set_var("PINBOARD_SYNC_ON_AUTH_FAILURE", "env-hook");
        let cmd = parse_sync(&["--all"]);
        assert_eq!(
            cmd.on_auth_failure, None,
            "top-level flag must not bind the env"
        );

        let overrides = SyncOverrides {
            dry_run: cmd.dry_run,
            on_auth_failure: cmd.on_auth_failure.or_else(on_auth_failure_from_env),
            reddit_username: Some("alice".into()),
            reddit_cookie: Some("reddit_session=x".into()),
            ..SyncOverrides::default()
        };
        std::env::remove_var("PINBOARD_SYNC_ON_AUTH_FAILURE");

        let job = build_job::<RedditAccount>(None, &overrides, &Config::default()).expect("builds");
        assert_eq!(job.hook.as_deref(), Some("env-hook"));
    }

    #[test]
    fn top_level_on_auth_failure_reaches_single_source_hook() {
        let _env = lock_on_auth_failure_env();
        // `--on-auth-failure` placed before the source subcommand must reach the
        // job just like the `--all` path; the overrides from the source args alone only see the
        // per-source flag.
        let cmd = parse_sync(&[
            "--on-auth-failure",
            "top-hook",
            "reddit",
            "--reddit-username",
            "alice",
            "--reddit-cookie",
            "reddit_session=x",
        ]);
        let args = reddit_sync_args(&cmd);
        assert_eq!(args.on_auth_failure, None);

        let overrides = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure);
        let job = build_job::<RedditAccount>(None, &overrides, &Config::default()).expect("builds");
        assert_eq!(job.hook.as_deref(), Some("top-hook"));
    }

    #[test]
    fn per_source_on_auth_failure_falls_back_when_no_top_level() {
        let _env = lock_on_auth_failure_env();
        let cmd = parse_sync(&[
            "reddit",
            "--reddit-username",
            "alice",
            "--reddit-cookie",
            "reddit_session=x",
            "--on-auth-failure",
            "source-hook",
        ]);
        assert_eq!(cmd.on_auth_failure, None);
        let args = reddit_sync_args(&cmd);

        let overrides = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure);
        let job = build_job::<RedditAccount>(None, &overrides, &Config::default()).expect("builds");
        assert_eq!(job.hook.as_deref(), Some("source-hook"));
    }

    #[test]
    fn explicit_per_source_on_auth_failure_beats_env() {
        let _env = lock_on_auth_failure_env();
        std::env::set_var("PINBOARD_SYNC_ON_AUTH_FAILURE", "env-hook");
        let parsed = std::panic::catch_unwind(|| {
            parse_sync(&[
                "reddit",
                "--reddit-username",
                "alice",
                "--reddit-cookie",
                "reddit_session=x",
                "--on-auth-failure",
                "explicit-hook",
            ])
        });
        std::env::remove_var("PINBOARD_SYNC_ON_AUTH_FAILURE");
        let cmd = parsed.expect("parses");

        // The env only backs the per-source flag, so the top level stays empty and
        // the explicit per-source flag wins over the env value.
        assert_eq!(cmd.on_auth_failure, None);
        let args = reddit_sync_args(&cmd);
        assert_eq!(args.on_auth_failure.as_deref(), Some("explicit-hook"));

        let overrides = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure);
        let job = build_job::<RedditAccount>(None, &overrides, &Config::default()).expect("builds");
        assert_eq!(job.hook.as_deref(), Some("explicit-hook"));
    }

    #[test]
    fn on_auth_failure_env_fills_per_source_when_flag_absent() {
        let _env = lock_on_auth_failure_env();
        std::env::set_var("PINBOARD_SYNC_ON_AUTH_FAILURE", "env-hook");
        let parsed = std::panic::catch_unwind(|| {
            parse_sync(&[
                "reddit",
                "--reddit-username",
                "alice",
                "--reddit-cookie",
                "reddit_session=x",
            ])
        });
        std::env::remove_var("PINBOARD_SYNC_ON_AUTH_FAILURE");
        let cmd = parsed.expect("parses");

        assert_eq!(cmd.on_auth_failure, None);
        let args = reddit_sync_args(&cmd);

        let overrides = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure);
        let job = build_job::<RedditAccount>(None, &overrides, &Config::default()).expect("builds");
        assert_eq!(job.hook.as_deref(), Some("env-hook"));
    }

    /// Parse `cleanup <extra>` and return the top-level command.
    fn parse_cleanup(extra: &[&str]) -> CleanupCmd {
        let Command::Cleanup(cmd) = parse("cleanup", extra) else {
            panic!("expected `cleanup` command");
        };
        cmd
    }

    /// The `cleanup reddit` args of a parsed `cleanup`.
    fn reddit_cleanup_args(cmd: &CleanupCmd) -> RedditCleanupArgs {
        let Some(CleanupSource::Reddit(args)) = &cmd.source else {
            panic!("expected `cleanup reddit` args");
        };
        args.clone()
    }

    #[test]
    fn top_level_use_post_date_reaches_single_source_cleanup() {
        // `--use-post-date` placed before the source subcommand must reach the
        // cleanup pass; the per-source `dates` alone only sees a flag placed after.
        let cmd = parse_cleanup(&["--use-post-date", "reddit", "alice"]);
        assert_eq!(cmd.dates.use_post_date, Some(true));
        let args = reddit_cleanup_args(&cmd);
        assert_eq!(args.dates.use_post_date, None);

        let overrides =
            DateOverrides::from(&args.dates).with_top_level(&DateOverrides::from(&cmd.dates));
        assert_eq!(overrides.use_post_date, Some(true));
    }

    #[test]
    fn per_source_cleanup_date_flag_falls_back_when_no_top_level() {
        // A flag placed after the source subcommand still applies when the
        // top-level `dates` is empty.
        let cmd = parse_cleanup(&["reddit", "alice", "--use-post-date=false"]);
        assert_eq!(cmd.dates.use_post_date, None);
        let args = reddit_cleanup_args(&cmd);
        assert_eq!(args.dates.use_post_date, Some(false));

        let overrides =
            DateOverrides::from(&args.dates).with_top_level(&DateOverrides::from(&cmd.dates));
        assert_eq!(overrides.use_post_date, Some(false));
    }

    /// The `dry_run` field of a parsed per-source cleanup args struct.
    fn source_dry_run(source: &CleanupSource) -> bool {
        match source {
            CleanupSource::Reddit(args) => args.dry_run,
            CleanupSource::Github(args) => args.dry_run,
            CleanupSource::Hackernews(args) => args.dry_run,
        }
    }

    #[test]
    fn top_level_dry_run_reaches_single_source_cleanup() {
        // `--dry-run` placed before the source subcommand must make the pass dry, even
        // though the per-source args default `dry_run` to false: honoring only
        // `args.dry_run` would make a `--dry-run` before the subcommand write for real.
        for name in ["reddit", "github", "hackernews"] {
            let cmd = parse_cleanup(&["--dry-run", name]);
            assert!(cmd.dry_run, "top-level --dry-run parsed for {name}");
            let source = cmd.source.expect("a source subcommand");
            assert!(
                !source_dry_run(&source),
                "per-source dry_run defaults off for {name}"
            );
            assert!(
                cleanup_dry_run(cmd.dry_run, source_dry_run(&source)),
                "effective dry_run true for `cleanup --dry-run {name}`"
            );
        }
    }

    #[test]
    fn per_source_dry_run_still_applies_without_top_level() {
        // A `--dry-run` placed after the source subcommand still makes the pass dry.
        let cmd = parse_cleanup(&["github", "--dry-run"]);
        assert!(!cmd.dry_run);
        let source = cmd.source.expect("a source subcommand");
        assert!(source_dry_run(&source));
        assert!(cleanup_dry_run(cmd.dry_run, source_dry_run(&source)));
    }

    /// Parse `backup <extra>` and return the parsed command.
    fn parse_backup(extra: &[&str]) -> BackupCmd {
        let Command::Backup(cmd) = parse("backup", extra) else {
            panic!("expected `backup` args");
        };
        cmd
    }

    #[test]
    fn backup_accepts_out_before_or_after_the_target() {
        let cmd = parse_backup(&["--all", "--out", "/snap"]);
        assert!(cmd.all);
        assert_eq!(cmd.out.out.as_deref(), Some("/snap"));

        // `--out` after the target is honored too, as `--dry-run` is on `sync`.
        let cmd = parse_backup(&["reddit", "main", "--out", "/snap"]);
        match cmd.target {
            Some(BackupTarget::Reddit(args)) => {
                assert_eq!(args.account.as_deref(), Some("main"));
                assert_eq!(args.out.out.as_deref(), Some("/snap"));
            }
            _ => panic!("expected the reddit target"),
        }

        // Backing up a source never contacts Pinboard, so it takes no token.
        assert!(Cli::try_parse_from([
            "pinboard-sync",
            "backup",
            "reddit",
            "--pinboard-token",
            "x"
        ])
        .is_err());
        // Pinboard is a target now, not a positional path.
        assert!(matches!(
            parse_backup(&["pinboard"]).target,
            Some(BackupTarget::Pinboard(_))
        ));
    }

    #[test]
    fn backup_all_conflicts_with_a_target_and_a_bare_backup_is_rejected() {
        let config = Config::default();

        let cmd = parse_backup(&["--all", "pinboard"]);
        let err = build_backup_plan(&cmd, &config)
            .err()
            .expect("should be rejected");
        assert!(
            err.to_string().contains("--all cannot be combined"),
            "{err}"
        );

        let cmd = parse_backup(&[]);
        let err = build_backup_plan(&cmd, &config)
            .err()
            .expect("should be rejected");
        assert!(err.to_string().contains("specify a target"), "{err}");
    }

    #[test]
    fn backup_dir_prefers_the_flag_then_the_config_then_errors() {
        let mut config = Config::default();
        assert!(resolve_backup_dir(None, &config).is_err());

        config.backup.directory = Some("/from-config".into());
        assert_eq!(
            resolve_backup_dir(None, &config).unwrap(),
            PathBuf::from("/from-config")
        );
        assert_eq!(
            resolve_backup_dir(Some("/from-flag".into()), &config).unwrap(),
            PathBuf::from("/from-flag")
        );
    }

    /// Each source's name reaches its jobs' labels and backup file stems, which are part of
    /// the snapshot an operator keeps: a renamed source would orphan its earlier files.
    #[test]
    fn backup_all_names_each_source_in_its_labels_and_stems() {
        let config = Config {
            reddit: vec![RedditAccount {
                name: Some("main".into()),
                username: Some("alice".into()),
                cookie: Some("reddit_session=x".into()),
                ..Default::default()
            }],
            github: vec![GitHubAccount {
                name: Some("main".into()),
                token: Some("token".into()),
                ..Default::default()
            }],
            hackernews: vec![HackerNewsAccount {
                name: Some("main".into()),
                username: Some("bob".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let plan = build_backup_plan(&parse_backup(&["--all"]), &config).expect("plans");
        let named: Vec<(&str, &str)> = plan
            .jobs
            .iter()
            .map(|job| (job.label.as_str(), job.stem.as_str()))
            .collect();
        assert_eq!(
            named,
            [
                ("reddit[main]", "reddit-main"),
                ("github[main]", "github-main"),
                ("hackernews[main]", "hackernews-main"),
            ]
        );
        assert_eq!(
            skipped_labels(&plan.skipped),
            ["pinboard"],
            "no Pinboard token in this test"
        );
    }

    fn skipped_labels(skipped: &[SkippedTarget]) -> Vec<&str> {
        skipped.iter().map(|target| target.label.as_str()).collect()
    }

    /// One account that can't be built (a github account with no token) must not cost the
    /// rest of `backup --all` its snapshot: the others are still planned, and the run
    /// records the failure in the manifest and exits non-zero.
    #[tokio::test]
    async fn backup_all_backs_up_the_rest_when_one_account_cannot_be_built() {
        let mut config = Config {
            reddit: vec![RedditAccount {
                username: Some("alice".into()),
                cookie: Some("reddit_session=x".into()),
                ..Default::default()
            }],
            github: vec![GitHubAccount::default()],
            ..Default::default()
        };
        config.pinboard.token = Some("user:tok".into());

        let plan = build_backup_plan(&parse_backup(&["--all"]), &config).expect("plans");
        let labels: Vec<&str> = plan.jobs.iter().map(|job| job.label.as_str()).collect();
        assert_eq!(labels, ["reddit[alice]", "pinboard"]);
        assert_eq!(skipped_labels(&plan.skipped), ["github[default]"]);

        // The planned jobs are dropped so nothing is contacted: what is under test is how
        // the run accounts for the target it could not build.
        let scratch = crate::test_support::scratch_dir("backup-skipped-account");
        let dir = scratch.path().join("snapshot");
        let result = back_up(Vec::new(), plan.skipped, &dir, false).await;

        let err = result.expect_err("a skipped account fails the run");
        assert_eq!(err.to_string(), "1 account(s) failed");
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["complete"], false);
        assert_eq!(manifest["failed"], serde_json::json!(["github[default]"]));
    }

    #[test]
    fn backup_source_all_backs_up_the_rest_when_one_account_cannot_be_built() {
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
        let plan = build_backup_plan(&parse_backup(&["github", "--all"]), &config).expect("plans");
        let labels: Vec<&str> = plan.jobs.iter().map(|job| job.label.as_str()).collect();
        assert_eq!(labels, ["github[main]"]);
        assert_eq!(skipped_labels(&plan.skipped), ["github[tokenless]"]);
    }

    /// `cleanup <argv>`'s error against `config_text`, with no Pinboard token: the date
    /// refusal if it fires, else the missing token `posts/all` would need.
    async fn cleanup_error(config_text: &str, argv: &[&str]) -> String {
        let config = Config::parse(config_text).expect("valid config");
        let error = run_cleanup(parse_cleanup(argv), &config)
            .await
            .expect_err("no Pinboard token");
        format!("{error:#}")
    }

    #[tokio::test]
    async fn cleanup_refuses_a_zero_cap_with_stale_to_now_before_reading_pinboard() {
        let refused = |pass: &str| {
            format!(
                "{pass} with its flags resolves `post_date_max_age_days` to 0 with \
                 `cleanup_stale_to_now` on, so cleanup would re-date its stale bookmarks on \
                 every run; raise the cap or turn `cleanup_stale_to_now` off"
            )
        };
        let both = ["--max-age-days", "0", "--stale-to-now"];
        for source in ["github", "reddit", "hackernews"] {
            let pass = refused(&format!("`cleanup {source}`"));
            assert_eq!(
                cleanup_error("", &[&[source][..], &both].concat()).await,
                pass
            );
            assert_eq!(
                cleanup_error("", &[&both[..], &[source]].concat()).await,
                pass
            );
            assert_eq!(
                cleanup_error(
                    "[pinboard]\ncleanup_stale_to_now = true\n",
                    &[source, "--max-age-days", "0"]
                )
                .await,
                pass
            );
        }

        let every_source = "[[github]]\nname = \"a\"\n[[reddit]]\nusername = \"u\"\n\
                            [[hackernews]]\nusername = \"u\"\n";
        assert_eq!(
            cleanup_error(every_source, &[&["--all"][..], &both].concat()).await,
            refused("`cleanup --all` (github pass)")
        );
        assert_eq!(
            cleanup_error(
                &format!("[defaults.reddit]\npost_date_max_age_days = 0\n{every_source}"),
                &["--all", "--stale-to-now"]
            )
            .await,
            refused("`cleanup --all` (reddit pass)")
        );

        for argv in [
            &["github", "--max-age-days", "0"][..],
            &["--all", "--stale-to-now"],
        ] {
            let error = cleanup_error(every_source, argv).await;
            assert!(
                error.starts_with("missing Pinboard token"),
                "{argv:?}: {error}"
            );
        }
    }

    mod net_tests {
        use super::*;
        use crate::test_support::{stored_at, FakePinboard, NO_DATING};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        /// `cleanup github` runs as one account but must refresh the stars of every
        /// `[[github]]` account, so all of their tag configs have to reach the pass.
        #[tokio::test]
        async fn cleanup_github_refreshes_another_accounts_star_by_its_tags() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/repos/o/r"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "full_name": "o/r",
                    "html_url": "https://github.com/o/r",
                    "language": "Rust"
                })))
                .mount(&server)
                .await;
            let config = Config::parse(
                "[[github]]\nname = \"home\"\n\
                 [[github]]\nname = \"work\"\ntags = [\"work-star\"]\n\
                 tag_lang_prefix = \"language:\"\n",
            )
            .unwrap();
            let pinboard = FakePinboard {
                all: vec![Bookmark {
                    tags: vec!["work-star".into()],
                    ..stored_at("https://github.com/o/r")
                }],
                ..Default::default()
            };
            let home = &config.github[0];
            let client =
                GitHubClient::with_base_url("tok".into(), GitHubConfig::from(home), server.uri());

            cleanup_github_with_client(
                &pinboard.cleanup_store(false),
                &client,
                Some(home),
                NO_DATING,
                &config,
            )
            .await
            .unwrap();

            let updated = pinboard.updated.borrow();
            assert_eq!(updated.len(), 1, "{updated:?}");
            assert_eq!(updated[0].description, "o/r");
            assert_eq!(updated[0].tags, ["work-star", "language:rust"]);
        }
    }
}
