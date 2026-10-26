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
#[cfg(test)]
mod test_support;
mod timefmt;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};
use clap::{CommandFactory, Parser};
use clap_complete::Shell;
use log::{debug, error, info, warn};

use backup::BackupSource;
use bookmark::{AccountState, AccountView, Bookmark, BookmarkStore, CleanupStore};
use cli::{
    BackupCmd, BackupTarget, CleanupCmd, CleanupSource, Cli, Command, ConfigAction,
    GitHubCleanupArgs, HackerNewsCleanupArgs, RedditCleanupArgs, SyncCmd, SyncSource,
};
use config::{Config, GitHubAccount, HackerNewsAccount, RedditAccount};
use github::{GitHubClient, GitHubConfig};
use hackernews::cleanup::HackerNewsCleanupOpts;
use hackernews::{HackerNewsClient, HackerNewsConfig};
use hook::{cleanup_hook, handle_source_err, on_auth_failure_from_env};
use jobs::{
    build_all_jobs, build_job, build_jobs, resolve_dates, DateOverrides, SourceAccount,
    SourceClient, SyncJob, SyncOverrides,
};
use pinboard::{PinboardClient, RATE_LIMIT_SECS};
use reddit::model::RedditConfig;
use reddit::RedditClient;
use secrets::{
    first_nonempty, missing_github_token, missing_reddit_cookie, resolve_github_token,
    resolve_pinboard_token, resolve_reddit_cookie,
};
use source::{BookmarkDraft, SourceError};
use timefmt::DateOpts;

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
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading config file {path}"))?;
            let config = Config::parse(&text)?;
            info!(
                "loaded config {path}: {} reddit, {} github, {} hackernews account(s)",
                config.reddit.len(),
                config.github.len(),
                config.hackernews.len()
            );
            Ok(config)
        }
        None => {
            debug!("no --config / PINBOARD_SYNC_CONFIG; using built-in defaults");
            Ok(Config::default())
        }
    }
}

// --- sync --------------------------------------------------------------------

async fn run_sync(cmd: SyncCmd, config: &Config) -> Result<()> {
    let (jobs, ovr, prebuilt_failures) = match (cmd.all, cmd.source) {
        (true, Some(_)) => bail!("--all cannot be combined with a source subcommand"),
        (true, None) => {
            if !config.has_accounts() {
                bail!("--all requires a --config with at least one configured account");
            }
            let ovr = SyncOverrides {
                dry_run: cmd.dry_run,
                on_auth_failure: cmd
                    .on_auth_failure
                    .clone()
                    .or_else(on_auth_failure_from_env),
                ..SyncOverrides::default()
            };
            let (jobs, prebuilt_failures) = build_all_jobs(config, &ovr);
            (jobs, ovr, prebuilt_failures)
        }
        (false, Some(SyncSource::Reddit(args))) => {
            let (account, all) = (args.account.clone(), args.all);
            let ovr = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure.clone());
            let jobs = build_jobs::<RedditAccount>(account.as_deref(), all, &ovr, config)?;
            (jobs, ovr, 0)
        }
        (false, Some(SyncSource::Github(args))) => {
            let (account, all) = (args.account.clone(), args.all);
            let ovr = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure.clone());
            let jobs = build_jobs::<GitHubAccount>(account.as_deref(), all, &ovr, config)?;
            (jobs, ovr, 0)
        }
        (false, Some(SyncSource::Hackernews(args))) => {
            let (account, all) = (args.account.clone(), args.all);
            let ovr = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure.clone());
            let jobs = build_jobs::<HackerNewsAccount>(account.as_deref(), all, &ovr, config)?;
            (jobs, ovr, 0)
        }
        (false, None) => bail!("specify a source (e.g. `sync reddit`) or pass --all"),
    };

    let (pinboard, bookmarks) = open_pinboard(ovr.flags.pinboard_token.clone(), config).await?;
    // `--dry-run` is accepted both before the source subcommand (on `SyncCmd`) and
    // after it (per-source); honor either placement. (`--verbose` is global.)
    run_sync_jobs(
        jobs,
        prebuilt_failures,
        &pinboard,
        &bookmarks,
        ovr.dry_run || cmd.dry_run,
    )
    .await
}

/// Fetch every job's source concurrently (reads only, on one task), then write the
/// merged, de-duplicated drafts to Pinboard sequentially (writes are rate-limited).
async fn run_sync_jobs(
    jobs: Vec<SyncJob>,
    prebuilt_failures: usize,
    pinboard: &PinboardClient,
    bookmarks: &[Bookmark],
    dry_run: bool,
) -> Result<()> {
    info!(
        "syncing {} account(s) against {} existing bookmark(s)",
        jobs.len(),
        bookmarks.len()
    );
    let now = timefmt::now_unix();
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

    let mut run = AllRun {
        failed: prebuilt_failures,
    };
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

/// Resolve the Pinboard token and build the client (no fetch).
fn build_pinboard(token_flag: Option<String>, config: &Config) -> Result<PinboardClient> {
    let token = resolve_pinboard_token(token_flag, &config.pinboard)
        .context("missing Pinboard token (set --pinboard-token, PINBOARD_TOKEN/_FILE, or [pinboard] in the config)")?;
    pinboard_client(token, config)
}

/// The client for an already-resolved token, at the configured write pacing.
fn pinboard_client(token: String, config: &Config) -> Result<PinboardClient> {
    let rate_limit = config.pinboard.rate_limit_secs.unwrap_or(RATE_LIMIT_SECS);
    PinboardClient::new(token, rate_limit)
}

/// Build the client and fetch `posts/all` once. Returns the client and the bookmark
/// set to share across every account in a run (so `posts/all` — the most rate-limited
/// endpoint — is hit once, not once per account).
async fn open_pinboard(
    token_flag: Option<String>,
    config: &Config,
) -> Result<(PinboardClient, Vec<Bookmark>)> {
    let pinboard = build_pinboard(token_flag, config)?;
    debug!("fetching existing bookmarks from Pinboard (posts/all)");
    let bookmarks = pinboard.all().await.context("listing Pinboard bookmarks")?;
    info!("pinboard: {} existing bookmark(s)", bookmarks.len());
    Ok((pinboard, bookmarks))
}

// --- backup ------------------------------------------------------------------

/// One backup target: which client to dump and what to name its files.
struct BackupJob {
    client: BackupClient,
    /// Filename stem, e.g. `reddit-main` or `pinboard`.
    stem: String,
    label: String,
    hook: Option<String>,
}

/// A backup target's client. Pinboard is a `BookmarkStore`, not a `Source`, so it can't
/// join `SourceClient` — but it implements the same `BackupSource` port, so the driver
/// treats all four identically.
enum BackupClient {
    /// Boxed: a `SourceClient` is several times the size of a `PinboardClient`, so an
    /// unboxed variant would make every job pay the larger footprint.
    Source(Box<SourceClient>),
    Pinboard(PinboardClient),
}

impl BackupSource for BackupClient {
    async fn dump(&self) -> Result<backup::BackupDump, SourceError> {
        match self {
            BackupClient::Source(c) => c.dump().await,
            BackupClient::Pinboard(c) => c.dump().await,
        }
    }
}

/// Write a snapshot of every selected service: the verbatim API responses under `raw/`
/// and the same items as domain bookmarks under `normalized/`, plus a `manifest.json`.
/// Each run replaces the previous snapshot in place.
async fn run_backup(cmd: BackupCmd, config: &Config) -> Result<()> {
    let plan = backup_jobs(&cmd, config)?;
    let dir = resolve_backup_dir(plan.out.or_else(|| cmd.out.out.clone()), config)?;
    let jobs = plan.jobs;
    backup::check_stem_collisions(&jobs.iter().map(|j| j.stem.clone()).collect::<Vec<_>>())?;
    backup::check_backup_dir(&dir)?;

    let dry_run = plan.dry_run || cmd.dry_run;
    let mut run = AllRun::default();
    let mut entries = Vec::new();
    let mut removed = Vec::new();
    let mut failed = plan.skipped;
    for target in &failed {
        run.record(Err(anyhow!("{target} was skipped: no credential resolved")));
    }
    // Sequential, unlike `sync`: backup writes are local, so concurrency would only
    // multiply simultaneous API pressure and interleave the dry-run output.
    for job in &jobs {
        info!("{}: backing up", job.label);
        match backup::run_job(&job.client, &job.stem, &dir, dry_run).await {
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
        backup::write_manifest(&dir, &entries, &removed, &failed, &now)?;
        info!(
            "backed up {} file(s) to {}",
            entries.len() + 1,
            dir.display()
        );
    }
    run.finish()
}

/// What a `backup` invocation resolves to: the jobs, anything it could not cover, and the
/// target-level `--out`/`--dry-run`. Mirrors `run_sync`'s `(all, source)` match.
struct BackupPlan {
    jobs: Vec<BackupJob>,
    /// Targets that couldn't be built at all (no credential resolved). Counted as
    /// failures, so a run that quietly missed one can't report itself complete.
    skipped: Vec<String>,
    out: Option<String>,
    dry_run: bool,
}

fn backup_jobs(cmd: &BackupCmd, config: &Config) -> Result<BackupPlan> {
    let ovr = SyncOverrides::default();
    match (cmd.all, &cmd.target) {
        (true, Some(_)) => bail!("--all cannot be combined with a target subcommand"),
        (true, None) => {
            let mut jobs = Vec::new();
            let mut skipped = Vec::new();
            jobs.extend(every_backup_job::<RedditAccount>(&ovr, config)?);
            jobs.extend(every_backup_job::<GitHubAccount>(&ovr, config)?);
            jobs.extend(every_backup_job::<HackerNewsAccount>(&ovr, config)?);
            match pinboard_backup_job(None, config)? {
                Some(job) => jobs.push(job),
                // Silence here would be the worst outcome: a nightly `--all` that quietly
                // stops covering Pinboard (an unprovisioned sops secret makes
                // `resolve_secret` warn and yield `None`) while still exiting 0. Recorded
                // as a failed target so the manifest says `complete: false`.
                None if !jobs.is_empty() => {
                    warn!(
                        "backup --all: no Pinboard token resolved — the Pinboard account \
                         is NOT in this snapshot"
                    );
                    skipped.push("pinboard".to_string());
                }
                None => {}
            }
            if jobs.is_empty() {
                bail!(
                    "backup --all found nothing to back up: configure an account with \
                     --config, or set a Pinboard token"
                );
            }
            Ok(BackupPlan {
                jobs,
                skipped,
                out: None,
                dry_run: false,
            })
        }
        (false, Some(BackupTarget::Reddit(args))) => Ok(BackupPlan {
            jobs: source_backup_jobs::<RedditAccount>(
                args.account.as_deref(),
                args.all,
                &SyncOverrides::from(args.clone()),
                config,
            )?,
            skipped: Vec::new(),
            out: args.out.out.clone(),
            dry_run: args.dry_run,
        }),
        (false, Some(BackupTarget::Github(args))) => Ok(BackupPlan {
            jobs: source_backup_jobs::<GitHubAccount>(
                args.account.as_deref(),
                args.all,
                &SyncOverrides::from(args.clone()),
                config,
            )?,
            skipped: Vec::new(),
            out: args.out.out.clone(),
            dry_run: args.dry_run,
        }),
        (false, Some(BackupTarget::Hackernews(args))) => Ok(BackupPlan {
            jobs: source_backup_jobs::<HackerNewsAccount>(
                args.account.as_deref(),
                args.all,
                &SyncOverrides::from(args.clone()),
                config,
            )?,
            skipped: Vec::new(),
            out: args.out.out.clone(),
            dry_run: args.dry_run,
        }),
        (false, Some(BackupTarget::Pinboard(args))) => {
            let job = pinboard_backup_job(args.pinboard_token.clone(), config)?.context(
                "missing Pinboard token (set --pinboard-token, PINBOARD_TOKEN/_FILE, or \
                 [pinboard] in the config)",
            )?;
            Ok(BackupPlan {
                jobs: vec![job],
                skipped: Vec::new(),
                out: args.out.out.clone(),
                dry_run: args.dry_run,
            })
        }
        (false, None) => bail!("specify a target (e.g. `backup pinboard`) or pass --all"),
    }
}

/// The backup jobs for one source, selecting by name / first / every account. Reuses
/// `build_job` so account selection and the whole secret ladder behave as in `sync`.
fn source_backup_jobs<A: SourceAccount>(
    name: Option<&str>,
    all: bool,
    ovr: &SyncOverrides,
    config: &Config,
) -> Result<Vec<BackupJob>> {
    if all {
        if A::configured(config).is_empty() {
            bail!(
                "--all requires a --config with at least one configured {} account",
                A::SOURCE
            );
        }
        every_backup_job::<A>(ovr, config)
    } else {
        let account = config::select_account(A::configured(config), name)?;
        Ok(vec![source_backup_job(account, ovr, config)?])
    }
}

/// A backup job for every configured account of one source, stopping at the first that
/// fails to build.
fn every_backup_job<A: SourceAccount>(
    ovr: &SyncOverrides,
    config: &Config,
) -> Result<Vec<BackupJob>> {
    A::configured(config)
        .iter()
        .map(|a| source_backup_job(Some(a), ovr, config))
        .collect()
}

/// Wrap one account's `SyncJob` as a backup job, keeping its client and auth hook and
/// discarding the write-only settings (`limit`, `toread`, dates) backup has no use for.
fn source_backup_job<A: SourceAccount>(
    account: Option<&A>,
    ovr: &SyncOverrides,
    config: &Config,
) -> Result<BackupJob> {
    let job = build_job(account, ovr, config)?;
    Ok(BackupJob {
        stem: format!(
            "{}-{}",
            A::SOURCE,
            backup::slug(account.and_then(config::Named::account_name))
        ),
        label: job.label.clone(),
        hook: job.hook.clone(),
        client: BackupClient::Source(Box::new(job.client)),
    })
}

/// The Pinboard backup job, or `None` when no token is configured — so `backup --all`
/// still backs up the sources on a machine that only holds source credentials.
fn pinboard_backup_job(flag: Option<String>, config: &Config) -> Result<Option<BackupJob>> {
    let Some(token) = resolve_pinboard_token(flag, &config.pinboard) else {
        return Ok(None);
    };
    Ok(Some(BackupJob {
        client: BackupClient::Pinboard(pinboard_client(token, config)?),
        stem: "pinboard".to_string(),
        label: "pinboard".to_string(),
        hook: None,
    }))
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
    let over = DateOverrides::from(&cmd.dates);
    match (cmd.all, cmd.source) {
        (true, Some(_)) => bail!("--all cannot be combined with a source subcommand"),
        (true, None) => {
            // Cleanup normalizes the shared Pinboard bookmark set, so it runs once
            // per cleanup-capable service (reddit, github, hackernews), using the
            // first configured account of each for its cookie/domain/tags.
            if !config.has_accounts() {
                bail!("cleanup --all requires a --config with at least one configured account");
            }
            let (pinboard, bookmarks) = open_pinboard(None, config).await?;
            // One view across all three sources: each writes, and the next must plan
            // against what it left rather than the pre-run snapshot.
            let store = CleanupStore::new(&pinboard, AccountView::new(bookmarks), cmd.dry_run);
            let mut run = AllRun::default();
            if let Some(acct) = config.github.first() {
                let dates = resolve_dates(&over, Some(acct), &config.defaults.github, config);
                run.record(cleanup_github_for(&store, Some(acct), None, dates, config).await);
            }
            if let Some(acct) = config.reddit.first() {
                let flags = RedditCleanupFlags {
                    cookie: None,
                    mark_nsfw: true,
                    fix_titles: true,
                };
                run.record(cleanup_one_reddit(Some(acct), flags, &over, &store, config).await);
            }
            if let Some(acct) = config.hackernews.first() {
                run.record(
                    cleanup_one_hackernews(
                        Some(acct),
                        false, // linking is opt-in via `cleanup hackernews --link-discussions`
                        None,
                        &over,
                        &store,
                        config,
                    )
                    .await,
                );
            }
            run.finish()
        }
        (false, Some(CleanupSource::Reddit(args))) => {
            run_cleanup_reddit(args, cmd.dry_run, &over, config).await
        }
        (false, Some(CleanupSource::Github(args))) => {
            run_cleanup_github(args, cmd.dry_run, &over, config).await
        }
        (false, Some(CleanupSource::Hackernews(args))) => {
            run_cleanup_hackernews(args, cmd.dry_run, &over, config).await
        }
        (false, None) => bail!("specify a source (e.g. `cleanup reddit`) or pass --all"),
    }
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
    top: &DateOverrides,
    config: &Config,
) -> Result<()> {
    let over = DateOverrides::from(&args.dates).with_top_level(top);
    let (pinboard, bookmarks) = open_pinboard(args.pinboard_token, config).await?;
    let account = config::select_account(&config.github, args.account.as_deref())?;
    let dates = resolve_dates(&over, account, &config.defaults.github, config);
    let dry_run = cleanup_dry_run(top_dry_run, args.dry_run);
    let store = CleanupStore::new(&pinboard, AccountView::new(bookmarks), dry_run);
    cleanup_github_for(&store, account, args.github_token, dates, config).await
}

/// Run github cleanup: build an API client from the account/env token and refresh
/// renamed repos / titles / language.
async fn cleanup_github_for<S: BookmarkStore + AccountState>(
    store: &S,
    account: Option<&GitHubAccount>,
    token_flag: Option<String>,
    dates: DateOpts,
    config: &Config,
) -> Result<()> {
    let token = resolve_github_token(token_flag, account).with_context(missing_github_token)?;
    let hook = cleanup_hook(account, config);
    let client = GitHubClient::new(token, account.map(GitHubConfig::from).unwrap_or_default())?;
    github::cleanup::run(store, &client, dates)
        .await
        .map_err(|e| handle_source_err(e, hook.as_deref()))
}

async fn run_cleanup_reddit(
    args: RedditCleanupArgs,
    top_dry_run: bool,
    top: &DateOverrides,
    config: &Config,
) -> Result<()> {
    let dry_run = cleanup_dry_run(top_dry_run, args.dry_run);
    // One pass over the Pinboard account's reddit bookmarks, using the selected (or
    // first, or implicit CLI/env) account's cookie + domain/tags.
    let (pinboard, bookmarks) = open_pinboard(args.pinboard_token.clone(), config).await?;
    let account = config::select_account(&config.reddit, args.account.as_deref())?;
    let store = CleanupStore::new(&pinboard, AccountView::new(bookmarks), dry_run);
    cleanup_one_reddit(
        account,
        RedditCleanupFlags::from(&args),
        &DateOverrides::from(&args.dates).with_top_level(top),
        &store,
        config,
    )
    .await
}

/// The `cleanup reddit` choices that are not account settings: the cookie flag, and which
/// `/api/info`-backed fixes to make.
struct RedditCleanupFlags {
    cookie: Option<String>,
    mark_nsfw: bool,
    fix_titles: bool,
}

async fn cleanup_one_reddit<S: BookmarkStore + AccountState>(
    account: Option<&RedditAccount>,
    flags: RedditCleanupFlags,
    over: &DateOverrides,
    store: &S,
    config: &Config,
) -> Result<()> {
    let opts = reddit::cleanup::RedditCleanupOpts {
        mark_nsfw: flags.mark_nsfw,
        fix_titles: flags.fix_titles,
        config: account.map(RedditConfig::from).unwrap_or_default(),
        dates: resolve_dates(over, account, &config.defaults.reddit, config),
    };

    // Reddit (for /api/info) is needed when marking NSFW, fixing titles, or dating by
    // the source post (the post's created_utc comes from /api/info too).
    let reddit = if opts.mark_nsfw || opts.fix_titles || opts.dates.use_post_date {
        let cookie =
            resolve_reddit_cookie(flags.cookie, account).with_context(missing_reddit_cookie)?;
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
    top: &DateOverrides,
    config: &Config,
) -> Result<()> {
    // One pass over the Pinboard account's HN bookmarks, using the selected (or
    // first, or implicit) account's tag config.
    let (pinboard, bookmarks) = open_pinboard(args.pinboard_token.clone(), config).await?;
    let account = config::select_account(&config.hackernews, args.account.as_deref())?;
    let over = DateOverrides::from(&args.dates).with_top_level(top);
    let dry_run = cleanup_dry_run(top_dry_run, args.dry_run);
    let store = CleanupStore::new(&pinboard, AccountView::new(bookmarks), dry_run);
    cleanup_one_hackernews(
        account,
        args.link_discussions,
        args.link_tag,
        &over,
        &store,
        config,
    )
    .await
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
        config::reject_whitespace("--link-tag", &tag)?;
        hn_config.link_tag = tag;
    }
    Ok(hn_config)
}

async fn cleanup_one_hackernews<S: BookmarkStore + AccountState>(
    account: Option<&HackerNewsAccount>,
    link_discussions: bool,
    link_tag: Option<String>,
    over: &DateOverrides,
    store: &S,
    config: &Config,
) -> Result<()> {
    let hn_config = resolve_hackernews_cleanup_config(account, link_tag)?;
    let hn = HackerNewsClient::for_cleanup(hn_config)?;
    hn.cleanup(
        store,
        &HackerNewsCleanupOpts {
            link_discussions,
            dates: resolve_dates(over, account, &config.defaults.hackernews, config),
        },
    )
    .await
    // No hook: HackerNews is public, so nothing here can be a re-auth to act on.
    .map_err(SourceError::into_anyhow)
}

// --- shared dispatch helpers -------------------------------------------------

/// Accumulates failures across an `--all` run: each account is attempted, errors
/// are reported and counted, and the run errors at the end if any account failed.
#[derive(Default)]
struct AllRun {
    failed: usize,
}

impl AllRun {
    fn record(&mut self, result: Result<()>) {
        if let Err(e) = result {
            eprintln!("error: {e:#}");
            self.failed += 1;
        }
    }

    fn finish(self) -> Result<()> {
        if self.failed > 0 {
            bail!("{} account(s) failed", self.failed);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lock_on_auth_failure_env;

    #[test]
    fn link_tag_override_rejects_whitespace() {
        assert!(resolve_hackernews_cleanup_config(None, Some("find hn".into())).is_err());
    }

    #[test]
    fn link_tag_override_accepts_a_bare_tag() {
        let hn_config =
            resolve_hackernews_cleanup_config(None, Some("find-hn".into())).expect("valid tag");
        assert_eq!(hn_config.link_tag, "find-hn");
    }

    /// Parse `sync <extra>` (no source subcommand) and return the top-level command.
    fn parse_sync(extra: &[&str]) -> SyncCmd {
        use clap::Parser;
        let mut argv = vec!["pinboard-sync", "sync"];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(argv).expect("parses").command {
            Command::Sync(cmd) => cmd,
            _ => panic!("expected `sync` command"),
        }
    }

    #[test]
    fn top_level_all_threads_on_auth_failure_into_the_hook() {
        let _env = lock_on_auth_failure_env();
        // The `sync --all` path builds `SyncOverrides` itself (not from the source args),
        // so the hook must be carried explicitly from the top-level flag.
        let cmd = parse_sync(&["--all", "--on-auth-failure", "refresh-cookie"]);
        assert_eq!(cmd.on_auth_failure.as_deref(), Some("refresh-cookie"));

        let ovr = SyncOverrides {
            dry_run: cmd.dry_run,
            on_auth_failure: cmd.on_auth_failure.clone(),
            reddit_username: Some("alice".into()),
            reddit_cookie: Some("reddit_session=x".into()),
            ..SyncOverrides::default()
        };
        let job = build_job::<RedditAccount>(None, &ovr, &Config::default()).expect("builds");
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

        let ovr = SyncOverrides {
            dry_run: cmd.dry_run,
            on_auth_failure: cmd
                .on_auth_failure
                .clone()
                .or_else(on_auth_failure_from_env),
            reddit_username: Some("alice".into()),
            reddit_cookie: Some("reddit_session=x".into()),
            ..SyncOverrides::default()
        };
        std::env::remove_var("PINBOARD_SYNC_ON_AUTH_FAILURE");

        let job = build_job::<RedditAccount>(None, &ovr, &Config::default()).expect("builds");
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
        let args = match &cmd.source {
            Some(SyncSource::Reddit(args)) => args.clone(),
            _ => panic!("expected `sync reddit` args"),
        };
        assert_eq!(args.on_auth_failure, None);

        let ovr = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure.clone());
        let job = build_job::<RedditAccount>(None, &ovr, &Config::default()).expect("builds");
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
        let args = match &cmd.source {
            Some(SyncSource::Reddit(args)) => args.clone(),
            _ => panic!("expected `sync reddit` args"),
        };

        let ovr = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure.clone());
        let job = build_job::<RedditAccount>(None, &ovr, &Config::default()).expect("builds");
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
        let args = match &cmd.source {
            Some(SyncSource::Reddit(args)) => args.clone(),
            _ => panic!("expected `sync reddit` args"),
        };
        assert_eq!(args.on_auth_failure.as_deref(), Some("explicit-hook"));

        let ovr = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure.clone());
        let job = build_job::<RedditAccount>(None, &ovr, &Config::default()).expect("builds");
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
        let args = match &cmd.source {
            Some(SyncSource::Reddit(args)) => args.clone(),
            _ => panic!("expected `sync reddit` args"),
        };

        let ovr = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure.clone());
        let job = build_job::<RedditAccount>(None, &ovr, &Config::default()).expect("builds");
        assert_eq!(job.hook.as_deref(), Some("env-hook"));
    }

    /// Parse `cleanup <extra>` and return the top-level command.
    fn parse_cleanup(extra: &[&str]) -> CleanupCmd {
        use clap::Parser;
        let mut argv = vec!["pinboard-sync", "cleanup"];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(argv).expect("parses").command {
            Command::Cleanup(cmd) => cmd,
            _ => panic!("expected `cleanup` command"),
        }
    }

    #[test]
    fn top_level_use_post_date_reaches_single_source_cleanup() {
        // `--use-post-date` placed before the source subcommand must reach the
        // cleanup pass; the per-source `dates` alone only sees a flag placed after.
        let cmd = parse_cleanup(&["--use-post-date", "reddit", "alice"]);
        assert_eq!(cmd.dates.use_post_date, Some(true));
        let args = match &cmd.source {
            Some(CleanupSource::Reddit(args)) => args.clone(),
            _ => panic!("expected `cleanup reddit` args"),
        };
        assert_eq!(args.dates.use_post_date, None);

        let over =
            DateOverrides::from(&args.dates).with_top_level(&DateOverrides::from(&cmd.dates));
        assert_eq!(over.use_post_date, Some(true));
    }

    #[test]
    fn per_source_cleanup_date_flag_falls_back_when_no_top_level() {
        // A flag placed after the source subcommand still applies when the
        // top-level `dates` is empty.
        let cmd = parse_cleanup(&["reddit", "alice", "--use-post-date=false"]);
        assert_eq!(cmd.dates.use_post_date, None);
        let args = match &cmd.source {
            Some(CleanupSource::Reddit(args)) => args.clone(),
            _ => panic!("expected `cleanup reddit` args"),
        };
        assert_eq!(args.dates.use_post_date, Some(false));

        let over =
            DateOverrides::from(&args.dates).with_top_level(&DateOverrides::from(&cmd.dates));
        assert_eq!(over.use_post_date, Some(false));
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
        // `--dry-run` placed before the source subcommand must make the pass dry,
        // even though the per-source args default `dry_run` to false. Regression:
        // the single-source arms once passed only `args.dry_run`, so a `--dry-run`
        // before the subcommand silently performed live writes.
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
        use clap::Parser;
        let mut argv = vec!["pinboard-sync", "backup"];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(argv).expect("parses").command {
            Command::Backup(cmd) => cmd,
            _ => panic!("expected `backup` args"),
        }
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
        let err = backup_jobs(&cmd, &config)
            .err()
            .expect("should be rejected");
        assert!(
            err.to_string().contains("--all cannot be combined"),
            "{err}"
        );

        let cmd = parse_backup(&[]);
        let err = backup_jobs(&cmd, &config)
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
        let plan = backup_jobs(&parse_backup(&["--all"]), &config).expect("plans");
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
        assert_eq!(plan.skipped, ["pinboard"], "no Pinboard token in this test");
    }
}
