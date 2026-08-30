//! pinboard-sync: sync saved/favorited items from multiple services to Pinboard.

mod backup;
mod bookmark;
mod cleanup;
mod cleanup_pass;
mod config;
mod domains;
mod github;
mod hackernews;
mod htmltext;
mod http;
mod model;
mod pinboard;
mod reddit;
mod source;
mod sync;
#[cfg(test)]
mod test_support;
mod timefmt;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use log::{debug, error, info, warn};

use backup::BackupSource;
use bookmark::{AccountState, AccountView, Bookmark, BookmarkStore, CleanupStore};
use config::{Config, GitHubAccount, HackernewsAccount, RedditAccount};
use github::GitHubClient;
use hackernews::{HackerNewsCleanupOpts, HackerNewsClient, HackernewsConfig};
use pinboard::{PinboardClient, RATE_LIMIT_SECS};
use reddit::RedditClient;
use source::{BookmarkDraft, Source, SourceError, UrlKey};
use url::Url;

#[derive(Parser)]
#[command(name = "pinboard-sync", version, about, arg_required_else_help = true)]
struct Cli {
    /// Path to the TOML config file (env PINBOARD_SYNC_CONFIG).
    #[arg(long, global = true)]
    config: Option<String>,
    /// Increase log verbosity: `-v` for debug, `-vv` for trace, `-vvv` to also include
    /// dependency logs. Overridden by `RUST_LOG`.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sync a source's saved/favorited items to Pinboard.
    Sync(SyncCmd),
    /// Normalize existing bookmarks for a source.
    Cleanup(CleanupCmd),
    /// Check the Pinboard token and every configured account's credentials.
    Doctor,
    /// Snapshot every service to a directory: verbatim API responses plus normalized items.
    Backup(BackupCmd),
    /// Print a shell completion script (bash, zsh, fish, …) to stdout.
    Completions { shell: Shell },
    /// Print a man page (roff) to stdout.
    Man,
    /// Config helpers.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print a fully-commented example config (every key with its default).
    Example,
}

#[derive(Args)]
struct BackupCmd {
    /// Back up the Pinboard account and every configured account across every source.
    #[arg(long)]
    all: bool,
    /// Show what would be written without touching the filesystem.
    #[arg(long)]
    dry_run: bool,
    #[command(flatten)]
    out: OutFlag,
    #[command(subcommand)]
    target: Option<BackupTarget>,
}

/// The snapshot directory, flattened into `backup` *and* every target so it is accepted
/// on either side of the subcommand (as `--dry-run` already is on `sync`).
#[derive(Args, Clone, Default)]
struct OutFlag {
    /// Directory to write the snapshot into (overrides `[backup].directory`).
    #[arg(long, value_name = "DIR")]
    out: Option<String>,
}

#[derive(Subcommand)]
enum BackupTarget {
    /// Back up saved Reddit posts and comments.
    Reddit(RedditBackupArgs),
    /// Back up starred GitHub repositories.
    Github(GitHubBackupArgs),
    /// Back up favorited HackerNews stories and comments.
    Hackernews(HackernewsBackupArgs),
    /// Back up the Pinboard account itself (raw `posts/all`, verbatim).
    Pinboard(PinboardBackupArgs),
}

// Backing up a source never contacts Pinboard, so none of the source targets take a
// Pinboard token — the one place `backup` deliberately diverges from `sync`/`cleanup`.

#[derive(Args, Clone)]
struct RedditBackupArgs {
    /// Account name to select from the config (default: the first reddit account).
    account: Option<String>,
    /// Back up every reddit account in the config.
    #[arg(long)]
    all: bool,
    /// Reddit username whose saved items to back up (env REDDIT_USERNAME, or *_FILE).
    #[arg(long)]
    reddit_username: Option<String>,
    /// Reddit session cookie, e.g. `reddit_session=…` (env REDDIT_COOKIE, or *_FILE).
    #[arg(long)]
    reddit_cookie: Option<String>,
    /// Shell command run when the Reddit cookie needs refreshing (a 401/403).
    #[arg(long, env = "PINBOARD_SYNC_ON_AUTH_FAILURE")]
    on_auth_failure: Option<String>,
    #[command(flatten)]
    out: OutFlag,
    /// Show what would be written without touching the filesystem.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Clone)]
struct GitHubBackupArgs {
    /// Account name to select from the config (default: the first github account).
    account: Option<String>,
    /// Back up every github account in the config.
    #[arg(long)]
    all: bool,
    /// GitHub personal access token (env GITHUB_TOKEN, or *_FILE).
    #[arg(long)]
    github_token: Option<String>,
    /// Shell command run when the GitHub token needs refreshing (a 401).
    #[arg(long, env = "PINBOARD_SYNC_ON_AUTH_FAILURE")]
    on_auth_failure: Option<String>,
    #[command(flatten)]
    out: OutFlag,
    /// Show what would be written without touching the filesystem.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Clone)]
struct HackernewsBackupArgs {
    /// Account name to select from the config (default: the first hackernews account).
    account: Option<String>,
    /// Back up every hackernews account in the config.
    #[arg(long)]
    all: bool,
    /// HackerNews username whose favorites to back up (env HN_USERNAME, or *_FILE).
    #[arg(long)]
    username: Option<String>,
    #[command(flatten)]
    out: OutFlag,
    /// Show what would be written without touching the filesystem.
    #[arg(long)]
    dry_run: bool,
}

/// Pinboard has no `account`/`--all`: `[pinboard]` is a single destination table, not an
/// array of accounts like the sources.
#[derive(Args, Clone)]
struct PinboardBackupArgs {
    /// Pinboard API token, "user:TOKEN" (env PINBOARD_TOKEN, or *_FILE).
    #[arg(long)]
    pinboard_token: Option<String>,
    #[command(flatten)]
    out: OutFlag,
    /// Show what would be written without touching the filesystem.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args)]
struct SyncCmd {
    /// Run every configured account across every source (requires --config).
    #[arg(long)]
    all: bool,
    /// Show what would be written without touching Pinboard (with --all).
    #[arg(long)]
    dry_run: bool,
    /// Shell command run when a source's credential needs refreshing (a 401/403).
    ///
    /// The `PINBOARD_SYNC_ON_AUTH_FAILURE` env var backs the per-source flag, not
    /// this one, so an explicit per-source `--on-auth-failure` outranks the env
    /// var (`with_top_level_hook` prefers a present top-level value). The `--all`
    /// path, which has no per-source flag, reads that env var itself via
    /// `on_auth_failure_from_env`.
    #[arg(long)]
    on_auth_failure: Option<String>,
    #[command(subcommand)]
    source: Option<SyncSource>,
}

#[derive(Subcommand)]
enum SyncSource {
    /// Sync saved Reddit posts and comments.
    Reddit(RedditSyncArgs),
    /// Sync starred GitHub repositories.
    Github(GitHubSyncArgs),
    /// Sync favorited HackerNews stories and comments.
    Hackernews(HackernewsSyncArgs),
}

#[derive(Args, Clone)]
struct RedditSyncArgs {
    /// Account name to select from the config (default: the first reddit account).
    account: Option<String>,
    /// Run every reddit account in the config.
    #[arg(long)]
    all: bool,
    /// Reddit username whose saved items to sync (env REDDIT_USERNAME, or *_FILE).
    #[arg(long)]
    reddit_username: Option<String>,
    /// Reddit session cookie, e.g. `reddit_session=…` (env REDDIT_COOKIE, or *_FILE).
    #[arg(long)]
    reddit_cookie: Option<String>,
    /// Pinboard API token, "user:TOKEN" (env PINBOARD_TOKEN, or *_FILE).
    #[arg(long)]
    pinboard_token: Option<String>,
    /// Cap on new bookmarks written this run (overrides config; unset = config / no cap).
    #[arg(long)]
    limit: Option<usize>,
    /// Only backdate posts newer than N days; older use "now" (overrides config).
    #[arg(long)]
    max_age_days: Option<u64>,
    /// Mark new bookmarks to-read: `--toread` or `--toread=false` (overrides config).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    toread: Option<bool>,
    /// Create bookmarks public: `--public` or `--public=false` (overrides config).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    public: Option<bool>,
    /// Date bookmarks by the source post date: `--use-post-date[=BOOL]` (overrides config).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    use_post_date: Option<bool>,
    /// Shell command run when the Reddit cookie needs refreshing (a 401/403).
    #[arg(long, env = "PINBOARD_SYNC_ON_AUTH_FAILURE")]
    on_auth_failure: Option<String>,
    /// Fetch and print what would be posted, without writing to Pinboard.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Clone)]
struct GitHubSyncArgs {
    /// Account name to select from the config (default: the first github account).
    account: Option<String>,
    /// Run every github account in the config.
    #[arg(long)]
    all: bool,
    /// GitHub personal access token (env GITHUB_TOKEN, or *_FILE).
    #[arg(long)]
    github_token: Option<String>,
    /// Pinboard API token, "user:TOKEN" (env PINBOARD_TOKEN, or *_FILE).
    #[arg(long)]
    pinboard_token: Option<String>,
    /// Cap on new bookmarks written this run (overrides config; unset = config / no cap).
    #[arg(long)]
    limit: Option<usize>,
    /// Only backdate posts newer than N days; older use "now" (overrides config).
    #[arg(long)]
    max_age_days: Option<u64>,
    /// Mark new bookmarks to-read: `--toread` or `--toread=false` (overrides config).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    toread: Option<bool>,
    /// Create bookmarks public: `--public` or `--public=false` (overrides config).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    public: Option<bool>,
    /// Date bookmarks by the source post date: `--use-post-date[=BOOL]` (overrides config).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    use_post_date: Option<bool>,
    /// Shell command run when the GitHub token needs refreshing (a 401).
    #[arg(long, env = "PINBOARD_SYNC_ON_AUTH_FAILURE")]
    on_auth_failure: Option<String>,
    /// Fetch and print what would be posted, without writing to Pinboard.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Clone)]
struct HackernewsSyncArgs {
    /// Account name to select from the config (default: the first hackernews account).
    account: Option<String>,
    /// Run every hackernews account in the config.
    #[arg(long)]
    all: bool,
    /// HackerNews username whose favorites to sync (env HN_USERNAME, or *_FILE).
    #[arg(long)]
    username: Option<String>,
    /// Pinboard API token, "user:TOKEN" (env PINBOARD_TOKEN, or *_FILE).
    #[arg(long)]
    pinboard_token: Option<String>,
    /// Cap on new bookmarks written this run (overrides config; unset = config / no cap).
    #[arg(long)]
    limit: Option<usize>,
    /// Only backdate posts newer than N days; older use "now" (overrides config).
    #[arg(long)]
    max_age_days: Option<u64>,
    /// Mark new bookmarks to-read: `--toread` or `--toread=false` (overrides config).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    toread: Option<bool>,
    /// Create bookmarks public: `--public` or `--public=false` (overrides config).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    public: Option<bool>,
    /// Date bookmarks by the source post date: `--use-post-date[=BOOL]` (overrides config).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    use_post_date: Option<bool>,
    /// Fetch and print what would be posted, without writing to Pinboard.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args)]
struct CleanupCmd {
    /// Run cleanup for every configured account across every cleanup-capable source.
    #[arg(long)]
    all: bool,
    /// Show what would change without writing to Pinboard (with --all).
    #[arg(long)]
    dry_run: bool,
    #[command(flatten)]
    dates: DateFlags,
    #[command(subcommand)]
    source: Option<CleanupSource>,
}

#[derive(Subcommand)]
enum CleanupSource {
    /// Normalize existing reddit bookmarks (URLs, tags, NSFW, titles).
    Reddit(RedditCleanupArgs),
    /// Canonicalize existing GitHub repo bookmark URLs.
    Github(GitHubCleanupArgs),
    /// Normalize existing HackerNews bookmarks (rewrite item URLs to articles).
    Hackernews(HackernewsCleanupArgs),
}

/// The date-setting flags shared by every cleanup command (and `cleanup --all`),
/// flattened into each so the declarations + their override mapping live in one place.
#[derive(Args, Clone, Default)]
struct DateFlags {
    /// Only backdate posts newer than N days; older use "now" (overrides config).
    #[arg(long)]
    max_age_days: Option<u64>,
    /// Date bookmarks by the source post date: `--use-post-date[=BOOL]` (overrides config).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    use_post_date: Option<bool>,
    /// Re-date too-old posts to now: `--stale-to-now[=BOOL]` (overrides config).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    stale_to_now: Option<bool>,
}

impl DateFlags {
    /// The date-setting overrides these flags supply, sitting at the top of the tier.
    fn overrides(&self) -> DateOverrides {
        DateOverrides {
            use_post_date: self.use_post_date,
            max_age_days: self.max_age_days,
            stale_to_now: self.stale_to_now,
        }
    }
}

#[derive(Args, Clone)]
struct GitHubCleanupArgs {
    /// Account name whose token/tags to use (default: the first github account).
    account: Option<String>,
    /// GitHub personal access token (env GITHUB_TOKEN, or *_FILE).
    #[arg(long)]
    github_token: Option<String>,
    /// Pinboard API token (env PINBOARD_TOKEN, or *_FILE).
    #[arg(long)]
    pinboard_token: Option<String>,
    #[command(flatten)]
    dates: DateFlags,
    /// Show what would change without writing to Pinboard.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Clone)]
struct HackernewsCleanupArgs {
    /// Account name whose tag config to use (default: the first hackernews account).
    account: Option<String>,
    /// Pinboard API token (env PINBOARD_TOKEN, or *_FILE).
    #[arg(long)]
    pinboard_token: Option<String>,
    /// Also link article bookmarks tagged with the link tag (default `find-hn`) to
    /// their HN discussion, via an Algolia URL lookup per tagged bookmark.
    #[arg(long)]
    link_discussions: bool,
    /// Override the marker tag used by --link-discussions (config: `tag_link`).
    #[arg(long)]
    link_tag: Option<String>,
    #[command(flatten)]
    dates: DateFlags,
    /// Show what would change without writing to Pinboard.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Clone)]
struct RedditCleanupArgs {
    /// Account name whose cookie + domain/tags to use (default: the first reddit account).
    account: Option<String>,
    /// Reddit session cookie (env REDDIT_COOKIE, or *_FILE). Needed for the
    /// `/api/info` lookups; not required with --no-nsfw --no-titles.
    #[arg(long)]
    reddit_cookie: Option<String>,
    /// Pinboard API token (env PINBOARD_TOKEN, or *_FILE).
    #[arg(long)]
    pinboard_token: Option<String>,
    /// Skip NSFW tagging (no Reddit /api/info call for over_18).
    #[arg(long)]
    no_nsfw: bool,
    /// Skip replacing generic placeholder titles.
    #[arg(long)]
    no_titles: bool,
    #[command(flatten)]
    dates: DateFlags,
    /// Show what would change without writing to Pinboard.
    #[arg(long)]
    dry_run: bool,
}

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
                run_doctor(&load_config(cli.config.clone())?).await
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

/// The `--all` path has no per-source `--on-auth-failure` flag to carry the hook,
/// and the top-level flag deliberately doesn't bind `PINBOARD_SYNC_ON_AUTH_FAILURE`
/// (so an explicit per-source flag can outrank the env var). So `--all` reads the
/// env var here directly -- without it, the NixOS service (`sync --all`, hook via
/// that env var only) would silently never fire the hook.
fn on_auth_failure_from_env() -> Option<String> {
    std::env::var("PINBOARD_SYNC_ON_AUTH_FAILURE")
        .ok()
        .filter(|v| !v.is_empty())
}

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
            let ovr = args
                .into_overrides()
                .with_top_level_hook(cmd.on_auth_failure.clone());
            let jobs = build_jobs(&config.reddit, account.as_deref(), all, |a| {
                build_reddit_job(a, &ovr, config)
            })?;
            (jobs, ovr, 0)
        }
        (false, Some(SyncSource::Github(args))) => {
            let (account, all) = (args.account.clone(), args.all);
            let ovr = args
                .into_overrides()
                .with_top_level_hook(cmd.on_auth_failure.clone());
            let jobs = build_jobs(&config.github, account.as_deref(), all, |a| {
                build_github_job(a, &ovr, config)
            })?;
            (jobs, ovr, 0)
        }
        (false, Some(SyncSource::Hackernews(args))) => {
            let (account, all) = (args.account.clone(), args.all);
            let ovr = args
                .into_overrides()
                .with_top_level_hook(cmd.on_auth_failure.clone());
            let jobs = build_jobs(&config.hackernews, account.as_deref(), all, |a| {
                build_hackernews_job(a, &ovr, config)
            })?;
            (jobs, ovr, 0)
        }
        (false, None) => bail!("specify a source (e.g. `sync reddit`) or pass --all"),
    };

    let (pinboard, bookmarks) = open_pinboard(ovr.pinboard_token.clone(), config).await?;
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

/// Build one job per configured account across every source for `sync --all`,
/// isolating build failures: an account whose required secret won't resolve is
/// reported and counted rather than aborting the whole run, so the healthy
/// accounts still sync. Returns the buildable jobs and the count of accounts
/// that failed to build, which the caller threads into the run's exit code.
fn build_all_jobs(config: &Config, ovr: &SyncOverrides) -> (Vec<SyncJob>, usize) {
    let mut jobs = Vec::new();
    let mut run = AllRun::default();
    for acct in &config.reddit {
        match build_reddit_job(Some(acct), ovr, config) {
            Ok(job) => jobs.push(job),
            Err(e) => run.record(Err(e)),
        }
    }
    for acct in &config.github {
        match build_github_job(Some(acct), ovr, config) {
            Ok(job) => jobs.push(job),
            Err(e) => run.record(Err(e)),
        }
    }
    for acct in &config.hackernews {
        match build_hackernews_job(Some(acct), ovr, config) {
            Ok(job) => jobs.push(job),
            Err(e) => run.record(Err(e)),
        }
    }
    (jobs, run.failed)
}

/// Build one job per account: every account when `all`, else the named (or first,
/// or implicit CLI/env) account.
fn build_jobs<T: config::Named>(
    accounts: &[T],
    name: Option<&str>,
    all: bool,
    build: impl Fn(Option<&T>) -> Result<SyncJob>,
) -> Result<Vec<SyncJob>> {
    if all {
        if accounts.is_empty() {
            bail!("--all requires a --config with at least one configured account");
        }
        accounts.iter().map(|a| build(Some(a))).collect()
    } else {
        let account = config::select_account(accounts, name)?;
        Ok(vec![build(account)?])
    }
}

#[derive(Default)]
struct SyncOverrides {
    reddit_username: Option<String>,
    reddit_cookie: Option<String>,
    github_token: Option<String>,
    hackernews_username: Option<String>,
    pinboard_token: Option<String>,
    on_auth_failure: Option<String>,
    limit: Option<usize>,
    toread: Option<bool>,
    public: Option<bool>,
    use_post_date: Option<bool>,
    max_age_days: Option<u64>,
    dry_run: bool,
}

impl SyncOverrides {
    /// Let a `--on-auth-failure` placed before the source subcommand (on
    /// `SyncCmd`) take effect, falling back to the per-source value. Mirrors the
    /// `--dry-run` merge in `run_sync`, which honors either placement too.
    fn with_top_level_hook(mut self, on_auth_failure: Option<String>) -> Self {
        self.on_auth_failure = on_auth_failure.or(self.on_auth_failure);
        self
    }

    /// The date-setting overrides this invocation supplies. `stale_to_now` is
    /// cleanup-only, so it is always `None` here.
    fn date_overrides(&self) -> DateOverrides {
        DateOverrides {
            use_post_date: self.use_post_date,
            max_age_days: self.max_age_days,
            stale_to_now: None,
        }
    }
}

// `backup` reuses the sync job builders (and so the whole secret ladder), but supplies
// only the credentials and the auth hook: every other override affects a *write* to
// Pinboard, which backup never performs.

impl RedditBackupArgs {
    fn into_overrides(self) -> SyncOverrides {
        SyncOverrides {
            reddit_username: self.reddit_username,
            reddit_cookie: self.reddit_cookie,
            on_auth_failure: self.on_auth_failure,
            ..SyncOverrides::default()
        }
    }
}

impl GitHubBackupArgs {
    fn into_overrides(self) -> SyncOverrides {
        SyncOverrides {
            github_token: self.github_token,
            on_auth_failure: self.on_auth_failure,
            ..SyncOverrides::default()
        }
    }
}

impl HackernewsBackupArgs {
    fn into_overrides(self) -> SyncOverrides {
        SyncOverrides {
            hackernews_username: self.username,
            ..SyncOverrides::default()
        }
    }
}

impl RedditSyncArgs {
    /// The secret/operational overrides this single-source invocation supplies.
    fn into_overrides(self) -> SyncOverrides {
        SyncOverrides {
            reddit_username: self.reddit_username,
            reddit_cookie: self.reddit_cookie,
            pinboard_token: self.pinboard_token,
            on_auth_failure: self.on_auth_failure,
            limit: self.limit,
            toread: self.toread,
            public: self.public,
            use_post_date: self.use_post_date,
            max_age_days: self.max_age_days,
            dry_run: self.dry_run,
            ..SyncOverrides::default()
        }
    }
}

impl GitHubSyncArgs {
    /// The secret/operational overrides this single-source invocation supplies.
    fn into_overrides(self) -> SyncOverrides {
        SyncOverrides {
            github_token: self.github_token,
            pinboard_token: self.pinboard_token,
            on_auth_failure: self.on_auth_failure,
            limit: self.limit,
            toread: self.toread,
            public: self.public,
            use_post_date: self.use_post_date,
            max_age_days: self.max_age_days,
            dry_run: self.dry_run,
            ..SyncOverrides::default()
        }
    }
}

impl HackernewsSyncArgs {
    /// The secret/operational overrides this single-source invocation supplies.
    fn into_overrides(self) -> SyncOverrides {
        SyncOverrides {
            hackernews_username: self.username,
            pinboard_token: self.pinboard_token,
            limit: self.limit,
            toread: self.toread,
            public: self.public,
            use_post_date: self.use_post_date,
            max_age_days: self.max_age_days,
            dry_run: self.dry_run,
            ..SyncOverrides::default()
        }
    }
}

/// A configured source client ready to fetch, plus its auth-failure hook and the
/// per-run write cap.
struct SyncJob {
    client: SourceClient,
    /// Human label for logs, e.g. `reddit[alice]`.
    label: String,
    hook: Option<String>,
    limit: usize,
    /// Resolved to-read flag for this account's new bookmarks.
    toread: bool,
    /// Resolved public/shared flag for this account's new bookmarks.
    shared: bool,
    /// Resolved: date new bookmarks by the source post date.
    use_post_date: bool,
    /// Resolved backdate age cap, in days (posts older than this use "now").
    max_age_days: u64,
}

impl SyncJob {
    fn settings(&self) -> sync::JobSettings {
        sync::JobSettings {
            toread: self.toread,
            shared: self.shared,
            use_post_date: self.use_post_date,
            max_age_days: self.max_age_days,
            limit: self.limit,
        }
    }
}

/// A log/display label for a job: `source[account]`, or `source[default]` when no
/// named account (an implicit CLI/env invocation).
fn job_label<T: config::Named>(source: &str, account: Option<&T>) -> String {
    format!(
        "{source}[{}]",
        account.and_then(|a| a.account_name()).unwrap_or("default")
    )
}

/// Resolve a setting through its precedence tiers, highest first: the CLI flag, else
/// the account override, else the per-source default, else the resolved `[pinboard]`
/// global. The first present (`Some`) value wins; the global is the guaranteed fallback.
fn resolve_setting<T>(flag: Option<T>, account: Option<T>, source: Option<T>, global: T) -> T {
    flag.or(account).or(source).unwrap_or(global)
}

/// The resolved `use_post_date` trio for an account, each resolved through
/// [`resolve_setting`] (flag → account → per-source default → `[pinboard]` global, with
/// `max_age_days` defaulting to [`config::DEFAULT_MAX_AGE_DAYS`]). Shared by every
/// `sync`/`cleanup` builder.
struct DateSettings {
    use_post_date: bool,
    max_age_days: u64,
    stale_to_now: bool,
}

/// CLI-flag overrides for the date settings, sitting at the top of the tier
/// (flag → account → per-source default → `[pinboard]` global). All-`None` means
/// "no flag given", so resolution falls through to the existing tiers unchanged.
#[derive(Default, Clone)]
struct DateOverrides {
    use_post_date: Option<bool>,
    max_age_days: Option<u64>,
    stale_to_now: Option<bool>,
}

impl DateOverrides {
    /// Let a date flag placed before the source subcommand (on `CleanupCmd`) take
    /// effect, falling back to this per-source value. Mirrors sync's
    /// `SyncOverrides::with_top_level_hook`.
    fn with_top_level(self, top: &DateOverrides) -> DateOverrides {
        DateOverrides {
            use_post_date: top.use_post_date.or(self.use_post_date),
            max_age_days: top.max_age_days.or(self.max_age_days),
            stale_to_now: top.stale_to_now.or(self.stale_to_now),
        }
    }
}

impl DateSettings {
    fn resolve(
        over: &DateOverrides,
        account: Option<&impl config::Account>,
        src: &config::SourceDefaults,
        config: &Config,
    ) -> Self {
        Self {
            use_post_date: resolve_setting(
                over.use_post_date,
                account.and_then(|a| a.use_post_date()),
                src.use_post_date,
                config.pinboard.use_post_date,
            ),
            max_age_days: resolve_setting(
                over.max_age_days,
                account.and_then(|a| a.max_age_days()),
                src.post_date_max_age_days,
                config
                    .pinboard
                    .post_date_max_age_days
                    .unwrap_or(config::DEFAULT_MAX_AGE_DAYS),
            ),
            stale_to_now: resolve_setting(
                over.stale_to_now,
                account.and_then(|a| a.stale_to_now()),
                src.cleanup_stale_to_now,
                config.pinboard.cleanup_stale_to_now,
            ),
        }
    }
}

/// One of the concrete source clients, unified behind the `Source` port so `--all`
/// can fetch them concurrently and the dispatch can treat them uniformly.
enum SourceClient {
    Reddit(RedditClient),
    Github(GitHubClient),
    Hackernews(HackerNewsClient),
}

impl Source for SourceClient {
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError> {
        match self {
            SourceClient::Reddit(c) => c.fetch().await,
            SourceClient::Github(c) => c.fetch().await,
            SourceClient::Hackernews(c) => c.fetch().await,
        }
    }
}

impl BackupSource for SourceClient {
    async fn dump(&self) -> Result<backup::BackupDump, SourceError> {
        match self {
            SourceClient::Reddit(c) => c.dump().await,
            SourceClient::Github(c) => c.dump().await,
            SourceClient::Hackernews(c) => c.dump().await,
        }
    }
}

impl UrlKey for SourceClient {
    fn dedup_key(&self, url: &Url) -> Option<String> {
        match self {
            SourceClient::Reddit(c) => c.dedup_key(url),
            SourceClient::Github(c) => c.dedup_key(url),
            SourceClient::Hackernews(c) => c.dedup_key(url),
        }
    }
}

/// The per-account settings every `build_*_job` resolves the same way: the
/// `limit`/`toread`/`shared` trio through [`resolve_setting`] plus the [`DateSettings`],
/// each on the flag → account → per-source default → `[pinboard]` global ladder. Leaves
/// client/secret/hook construction to each source builder.
struct JobCommon {
    limit: usize,
    toread: bool,
    shared: bool,
    dates: DateSettings,
}

fn resolve_job_common(
    account: Option<&impl config::Account>,
    ovr: &SyncOverrides,
    src: &config::SourceDefaults,
    config: &Config,
) -> JobCommon {
    JobCommon {
        limit: resolve_setting(
            ovr.limit,
            account.and_then(|a| a.limit()),
            src.limit,
            config.pinboard.limit.unwrap_or(0),
        ),
        toread: resolve_setting(
            ovr.toread,
            account.and_then(|a| a.toread()),
            src.toread,
            config.pinboard.toread,
        ),
        shared: resolve_setting(
            ovr.public,
            account.and_then(|a| a.public()),
            src.public,
            config.pinboard.public,
        ),
        dates: DateSettings::resolve(&ovr.date_overrides(), account, src, config),
    }
}

fn build_reddit_job(
    account: Option<&RedditAccount>,
    ovr: &SyncOverrides,
    config: &Config,
) -> Result<SyncJob> {
    let username = resolve_secret(
        ovr.reddit_username.clone(),
        "REDDIT_USERNAME",
        account.and_then(|a| a.username.clone()),
        None,
    )
    .context("missing Reddit username (set --reddit-username, REDDIT_USERNAME, or `username` in the config)")?;
    let cookie = resolve_secret(
        ovr.reddit_cookie.clone(),
        "REDDIT_COOKIE",
        account.and_then(|a| a.cookie.clone()),
        account.and_then(|a| a.cookie_file.as_deref()),
    );
    let reddit_config = account
        .map(RedditAccount::reddit_config)
        .unwrap_or_default();
    let src = &config.defaults.reddit;
    let hook = resolve_hook(
        ovr.on_auth_failure.clone(),
        account.and_then(|a| a.on_auth_failure.as_deref()),
        src.on_auth_failure.as_deref(),
        config,
    );
    let common = resolve_job_common(account, ovr, src, config);
    Ok(SyncJob {
        client: SourceClient::Reddit(RedditClient::for_user(username, cookie, reddit_config)?),
        label: job_label("reddit", account),
        hook,
        limit: common.limit,
        toread: common.toread,
        shared: common.shared,
        use_post_date: common.dates.use_post_date,
        max_age_days: common.dates.max_age_days,
    })
}

fn build_github_job(
    account: Option<&GitHubAccount>,
    ovr: &SyncOverrides,
    config: &Config,
) -> Result<SyncJob> {
    let token = resolve_secret(
        ovr.github_token.clone(),
        "GITHUB_TOKEN",
        account.and_then(|a| a.token.clone()),
        account.and_then(|a| a.token_file.as_deref()),
    )
    .context("missing GitHub token (set --github-token, GITHUB_TOKEN, or `token`/`token_file` in the config)")?;
    let github_config = account
        .map(GitHubAccount::github_config)
        .unwrap_or_default();
    let src = &config.defaults.github;
    let hook = resolve_hook(
        ovr.on_auth_failure.clone(),
        account.and_then(|a| a.on_auth_failure.as_deref()),
        src.on_auth_failure.as_deref(),
        config,
    );
    let common = resolve_job_common(account, ovr, src, config);
    Ok(SyncJob {
        client: SourceClient::Github(GitHubClient::new(token, github_config)?),
        label: job_label("github", account),
        hook,
        limit: common.limit,
        toread: common.toread,
        shared: common.shared,
        use_post_date: common.dates.use_post_date,
        max_age_days: common.dates.max_age_days,
    })
}

fn build_hackernews_job(
    account: Option<&HackernewsAccount>,
    ovr: &SyncOverrides,
    config: &Config,
) -> Result<SyncJob> {
    let username = resolve_secret(
        ovr.hackernews_username.clone(),
        "HN_USERNAME",
        account.and_then(|a| a.username.clone()),
        None,
    )
    .context(
        "missing HackerNews username (set --username, HN_USERNAME, or `username` in the config)",
    )?;
    let hn_config = account
        .map(HackernewsAccount::hackernews_config)
        .unwrap_or_default();
    let src = &config.defaults.hackernews;
    let common = resolve_job_common(account, ovr, src, config);
    Ok(SyncJob {
        // HackerNews favorites are public, so there is no auth-failure hook.
        client: SourceClient::Hackernews(HackerNewsClient::new(username, hn_config)?),
        label: job_label("hackernews", account),
        hook: None,
        limit: common.limit,
        toread: common.toread,
        shared: common.shared,
        use_post_date: common.dates.use_post_date,
        max_age_days: common.dates.max_age_days,
    })
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
        let drafts = job.client.fetch().await?;
        let fetched = drafts.len();
        let new = sync::prepare_new_drafts(&job.client, drafts, bookmarks, &job.settings(), now);
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
            for a in &config.reddit {
                jobs.push(source_backup_job("reddit", Some(a), |x| {
                    build_reddit_job(x, &ovr, config)
                })?);
            }
            for a in &config.github {
                jobs.push(source_backup_job("github", Some(a), |x| {
                    build_github_job(x, &ovr, config)
                })?);
            }
            for a in &config.hackernews {
                jobs.push(source_backup_job("hackernews", Some(a), |x| {
                    build_hackernews_job(x, &ovr, config)
                })?);
            }
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
            jobs: source_backup_jobs(
                "reddit",
                &config.reddit,
                args.account.as_deref(),
                args.all,
                {
                    let ovr = args.clone().into_overrides();
                    move |a| build_reddit_job(a, &ovr, config)
                },
            )?,
            skipped: Vec::new(),
            out: args.out.out.clone(),
            dry_run: args.dry_run,
        }),
        (false, Some(BackupTarget::Github(args))) => Ok(BackupPlan {
            jobs: source_backup_jobs(
                "github",
                &config.github,
                args.account.as_deref(),
                args.all,
                {
                    let ovr = args.clone().into_overrides();
                    move |a| build_github_job(a, &ovr, config)
                },
            )?,
            skipped: Vec::new(),
            out: args.out.out.clone(),
            dry_run: args.dry_run,
        }),
        (false, Some(BackupTarget::Hackernews(args))) => Ok(BackupPlan {
            jobs: source_backup_jobs(
                "hackernews",
                &config.hackernews,
                args.account.as_deref(),
                args.all,
                {
                    let ovr = args.clone().into_overrides();
                    move |a| build_hackernews_job(a, &ovr, config)
                },
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
/// `build_jobs` so account selection and the whole secret ladder behave as in `sync`.
fn source_backup_jobs<T: config::Account>(
    source: &'static str,
    accounts: &[T],
    name: Option<&str>,
    all: bool,
    build: impl Fn(Option<&T>) -> Result<SyncJob>,
) -> Result<Vec<BackupJob>> {
    if all {
        if accounts.is_empty() {
            bail!("--all requires a --config with at least one configured {source} account");
        }
        accounts
            .iter()
            .map(|a| source_backup_job(source, Some(a), &build))
            .collect()
    } else {
        let account = config::select_account(accounts, name)?;
        Ok(vec![source_backup_job(source, account, &build)?])
    }
}

/// Wrap one account's `SyncJob` as a backup job, keeping its client and auth hook and
/// discarding the write-only settings (`limit`, `toread`, dates) backup has no use for.
fn source_backup_job<T: config::Named>(
    source: &'static str,
    account: Option<&T>,
    build: impl Fn(Option<&T>) -> Result<SyncJob>,
) -> Result<BackupJob> {
    let job = build(account)?;
    Ok(BackupJob {
        stem: format!(
            "{source}-{}",
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

// --- doctor ------------------------------------------------------------------

/// Validate the Pinboard token and every configured account's credentials by
/// fetching each source. Prints a ✓/✗ line per check; exits non-zero if any failed.
async fn run_doctor(config: &Config) -> Result<()> {
    let mut failed = 0usize;

    match open_pinboard(None, config).await {
        Ok((_, bms)) => println!("✓ pinboard — {} bookmark(s)", bms.len()),
        Err(e) => {
            println!("✗ pinboard — {e:#}");
            failed += 1;
        }
    }

    // Only when a directory is configured: a machine that never runs `backup` shouldn't
    // fail `doctor` over it. Checked here so a misconfigured StateDirectory surfaces now
    // rather than as a quiet failure in the journal at the next timer firing.
    if let Some(dir) = &config.backup.directory {
        match backup::probe_writable(Path::new(dir)) {
            Ok(backup::DirProbe::Writable) => println!("✓ backup — {dir} is writable"),
            // Not yet created is healthy: `backup` makes it on its first run, and only a
            // typo'd or unwritable *parent* is a real problem. Failing here would red-flag
            // a correct config that simply hasn't run yet.
            Ok(backup::DirProbe::WillBeCreated) => {
                println!("✓ backup — {dir} will be created on the first run");
            }
            Err(e) => {
                println!("✗ backup — {dir}: {e:#}");
                failed += 1;
            }
        }
    }

    let ovr = SyncOverrides::default();
    failed += check_accounts("reddit", &config.reddit, |a| {
        build_reddit_job(Some(a), &ovr, config)
    })
    .await;
    failed += check_accounts("github", &config.github, |a| {
        build_github_job(Some(a), &ovr, config)
    })
    .await;
    failed += check_accounts("hackernews", &config.hackernews, |a| {
        build_hackernews_job(Some(a), &ovr, config)
    })
    .await;

    if failed > 0 {
        bail!("{failed} check(s) failed");
    }
    println!("All checks passed.");
    Ok(())
}

/// Probe each configured account by fetching its source; returns the count that
/// failed. Reuses the normal job builders, so it exercises the real auth path.
async fn check_accounts<T: config::Named>(
    source: &str,
    accounts: &[T],
    build: impl Fn(&T) -> Result<SyncJob>,
) -> usize {
    if accounts.is_empty() {
        println!("- {source} — no accounts configured");
        return 0;
    }
    let mut failed = 0;
    for account in accounts {
        let name = account.account_name().unwrap_or("(unnamed)");
        let result = match build(account) {
            Ok(job) => job.client.fetch().await.map_err(SourceError::into_anyhow),
            Err(e) => Err(e),
        };
        match result {
            Ok(items) => println!("✓ {source} [{name}] — {} item(s)", items.len()),
            Err(e) => {
                println!("✗ {source} [{name}] — {e:#}");
                failed += 1;
            }
        }
    }
    failed
}

// --- cleanup -----------------------------------------------------------------

async fn run_cleanup(cmd: CleanupCmd, config: &Config) -> Result<()> {
    let over = cmd.dates.overrides();
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
                let opts = gh_cleanup_opts(&over, Some(acct), config);
                run.record(cleanup_github_for(&store, Some(acct), None, &opts, config).await);
            }
            if let Some(acct) = config.reddit.first() {
                let args = RedditCleanupArgs {
                    account: None,
                    reddit_cookie: None,
                    pinboard_token: None,
                    no_nsfw: false,
                    no_titles: false,
                    dates: DateFlags::default(),
                    dry_run: cmd.dry_run,
                };
                run.record(cleanup_one_reddit(Some(acct), &args, &over, &store, config).await);
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

/// Resolve the tiered date settings for a github cleanup pass.
fn gh_cleanup_opts(
    over: &DateOverrides,
    account: Option<&GitHubAccount>,
    config: &Config,
) -> github::GitHubCleanupOpts {
    let dates = DateSettings::resolve(over, account, &config.defaults.github, config);
    github::GitHubCleanupOpts {
        use_post_date: dates.use_post_date,
        max_age_days: dates.max_age_days,
        cleanup_stale_to_now: dates.stale_to_now,
    }
}

async fn run_cleanup_github(
    args: GitHubCleanupArgs,
    top_dry_run: bool,
    top: &DateOverrides,
    config: &Config,
) -> Result<()> {
    let over = args.dates.overrides().with_top_level(top);
    let (pinboard, bookmarks) = open_pinboard(args.pinboard_token, config).await?;
    let account = config::select_account(&config.github, args.account.as_deref())?;
    let opts = gh_cleanup_opts(&over, account, config);
    let dry_run = cleanup_dry_run(top_dry_run, args.dry_run);
    let store = CleanupStore::new(&pinboard, AccountView::new(bookmarks), dry_run);
    cleanup_github_for(&store, account, args.github_token, &opts, config).await
}

/// Run github cleanup: build an API client from the account/env token and refresh
/// renamed repos / titles / language.
async fn cleanup_github_for<S: BookmarkStore + AccountState>(
    store: &S,
    account: Option<&GitHubAccount>,
    token_flag: Option<String>,
    opts: &github::GitHubCleanupOpts,
    config: &Config,
) -> Result<()> {
    let github_config = account
        .map(GitHubAccount::github_config)
        .unwrap_or_default();
    let token = resolve_secret(
        token_flag,
        "GITHUB_TOKEN",
        account.and_then(|a| a.token.clone()),
        account.and_then(|a| a.token_file.as_deref()),
    )
    .context("missing GitHub token (set --github-token, GITHUB_TOKEN, or `token`/`token_file` in the config)")?;
    let hook = cleanup_hook(
        account.and_then(|a| a.on_auth_failure.as_deref()),
        config.defaults.github.on_auth_failure.as_deref(),
        config,
    );
    let client = GitHubClient::new(token, github_config.clone())?;
    github::cleanup(store, &client, &github_config, opts)
        .await
        .map_err(|e| handle_source_err(e, hook.as_deref()))
}

async fn run_cleanup_reddit(
    mut args: RedditCleanupArgs,
    top_dry_run: bool,
    top: &DateOverrides,
    config: &Config,
) -> Result<()> {
    args.dry_run = cleanup_dry_run(top_dry_run, args.dry_run);
    // One pass over the Pinboard account's reddit bookmarks, using the selected (or
    // first, or implicit CLI/env) account's cookie + domain/tags.
    let (pinboard, bookmarks) = open_pinboard(args.pinboard_token.clone(), config).await?;
    let account = config::select_account(&config.reddit, args.account.as_deref())?;
    let store = CleanupStore::new(&pinboard, AccountView::new(bookmarks), args.dry_run);
    cleanup_one_reddit(
        account,
        &args,
        &args.dates.overrides().with_top_level(top),
        &store,
        config,
    )
    .await
}

async fn cleanup_one_reddit<S: BookmarkStore + AccountState>(
    account: Option<&RedditAccount>,
    args: &RedditCleanupArgs,
    over: &DateOverrides,
    store: &S,
    config: &Config,
) -> Result<()> {
    let reddit_config = account
        .map(RedditAccount::reddit_config)
        .unwrap_or_default();
    let dates = DateSettings::resolve(over, account, &config.defaults.reddit, config);
    let opts = cleanup::RedditCleanupOpts {
        mark_nsfw: !args.no_nsfw,
        fix_titles: !args.no_titles,
        base_tag: reddit_config.tags.first().cloned().unwrap_or_default(),
        subreddit_tag_prefix: reddit_config.subreddit_prefix.clone(),
        nsfw_tag: reddit_config.nsfw.clone(),
        domain: reddit_config.domain.clone(),
        use_post_date: dates.use_post_date,
        max_age_days: dates.max_age_days,
        cleanup_stale_to_now: dates.stale_to_now,
    };

    // Reddit (for /api/info) is needed when marking NSFW, fixing titles, or dating by
    // the source post (the post's created_utc comes from /api/info too).
    let reddit = if opts.mark_nsfw || opts.fix_titles || opts.use_post_date {
        let cookie = resolve_secret(
            args.reddit_cookie.clone(),
            "REDDIT_COOKIE",
            account.and_then(|a| a.cookie.clone()),
            account.and_then(|a| a.cookie_file.as_deref()),
        )
        .context("missing Reddit cookie (set --reddit-cookie, REDDIT_COOKIE, or pass --no-nsfw --no-titles and disable use_post_date)")?;
        Some(RedditClient::for_info(Some(cookie))?)
    } else {
        None
    };

    let hook = cleanup_hook(
        account.and_then(|a| a.on_auth_failure.as_deref()),
        config.defaults.reddit.on_auth_failure.as_deref(),
        config,
    );
    cleanup::run(store, reddit.as_ref(), &opts)
        .await
        .map_err(|e| handle_source_err(e, hook.as_deref()))
}

async fn run_cleanup_hackernews(
    args: HackernewsCleanupArgs,
    top_dry_run: bool,
    top: &DateOverrides,
    config: &Config,
) -> Result<()> {
    // One pass over the Pinboard account's HN bookmarks, using the selected (or
    // first, or implicit) account's tag config.
    let (pinboard, bookmarks) = open_pinboard(args.pinboard_token.clone(), config).await?;
    let account = config::select_account(&config.hackernews, args.account.as_deref())?;
    let over = args.dates.overrides().with_top_level(top);
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
    account: Option<&HackernewsAccount>,
    link_tag: Option<String>,
) -> Result<HackernewsConfig> {
    let mut hn_config = account
        .map(HackernewsAccount::hackernews_config)
        .unwrap_or_default();
    if let Some(tag) = link_tag {
        config::reject_whitespace("--link-tag", &tag)?;
        hn_config.link_tag = tag;
    }
    Ok(hn_config)
}

#[allow(clippy::too_many_arguments)]
async fn cleanup_one_hackernews<S: BookmarkStore + AccountState>(
    account: Option<&HackernewsAccount>,
    link_discussions: bool,
    link_tag: Option<String>,
    over: &DateOverrides,
    store: &S,
    config: &Config,
) -> Result<()> {
    let hn_config = resolve_hackernews_cleanup_config(account, link_tag)?;
    let dates = DateSettings::resolve(over, account, &config.defaults.hackernews, config);
    let hn = HackerNewsClient::for_cleanup(hn_config)?;
    hn.cleanup(
        store,
        &HackerNewsCleanupOpts {
            link_discussions,
            use_post_date: dates.use_post_date,
            max_age_days: dates.max_age_days,
            cleanup_stale_to_now: dates.stale_to_now,
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

/// Map a `SourceError` to an `anyhow::Error`, firing the auth-failure hook when
/// re-authentication is required.
fn handle_source_err(e: SourceError, hook: Option<&str>) -> anyhow::Error {
    match e {
        SourceError::ReauthRequired(msg) => {
            run_auth_failure_hook(hook, &msg);
            anyhow!("{msg}")
        }
        // Deliberately no hook: no credential change clears a rate limit, so waking the
        // operator to rotate a token would send them after the wrong problem.
        SourceError::RateLimited(msg) => anyhow!("{msg}"),
        SourceError::Other(e) => e,
    }
}

fn run_auth_failure_hook(cmd: Option<&str>, err: &str) {
    let Some(cmd) = cmd else {
        return;
    };
    eprintln!("Running --on-auth-failure hook...");
    match std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .env("PINBOARD_SYNC_AUTH_ERROR", err)
        .env("PINBOARD_SYNC_EVENT", "reauth_required")
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("auth-failure hook exited with {s}"),
        Err(e) => eprintln!("failed to run auth-failure hook: {e}"),
    }
}

// --- secret / value resolution ----------------------------------------------

/// Resolve a value through the ladder: CLI flag → `$VAR` → `$VAR_FILE` → config
/// inline value → config file path. File-sourced values are read and trimmed; the
/// first non-empty candidate wins.
fn resolve_secret(
    flag: Option<String>,
    var: &str,
    cfg_inline: Option<String>,
    cfg_file: Option<&str>,
) -> Option<String> {
    first_nonempty([
        flag,
        std::env::var(var).ok(),
        std::env::var(format!("{var}_FILE"))
            .ok()
            .and_then(|p| read_file_secret(&p)),
        cfg_inline,
        cfg_file.and_then(read_file_secret),
    ])
}

/// The first non-empty value among the candidates.
fn first_nonempty(candidates: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    candidates.into_iter().flatten().find(|s| !s.is_empty())
}

/// Read a file's trimmed contents, or `None` if missing or empty.
fn read_file_secret(path: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let s = s.trim().to_string();
            (!s.is_empty()).then_some(s)
        }
        // A `*_FILE` path was given but can't be read — surface it rather than
        // silently falling through to a confusing "missing X" downstream.
        Err(e) => {
            eprintln!("warning: could not read secret file {path}: {e}");
            None
        }
    }
}

/// The auth-failure hook: resolved flag/env (`ovr.on_auth_failure`) → per-account
/// override → per-source default → `[hooks]` global.
fn resolve_hook(
    flag: Option<String>,
    account_override: Option<&str>,
    source_override: Option<&str>,
    config: &Config,
) -> Option<String> {
    flag.or_else(|| account_override.map(str::to_string))
        .or_else(|| source_override.map(str::to_string))
        .or_else(|| config.hooks.on_auth_failure.clone())
}

/// The auth-failure hook for a `cleanup` run: `PINBOARD_SYNC_ON_AUTH_FAILURE` → account →
/// `[defaults.<source>]` → `[hooks]`. The `cleanup` subcommands take no
/// `--on-auth-failure` flag, so the env var is the top rung.
///
/// Reading the env var here is load-bearing, not a convenience: the NixOS module exports
/// `onAuthFailure` *only* into the unit environment — it never reaches the generated
/// TOML — and runs the `cleanup --all` timer with it. Resolving from config alone would
/// leave the hook dead for the exact deployment it is meant to serve.
fn cleanup_hook(
    account_override: Option<&str>,
    source_override: Option<&str>,
    config: &Config,
) -> Option<String> {
    resolve_hook(
        on_auth_failure_from_env(),
        account_override,
        source_override,
        config,
    )
}

fn resolve_pinboard_token(flag: Option<String>, pb: &config::PinboardConfig) -> Option<String> {
    resolve_secret(
        flag,
        "PINBOARD_TOKEN",
        pb.token.clone(),
        pb.token_file.as_deref(),
    )
}

/// A single-line, length-bounded preview of (possibly multi-line) text, for
/// dry-run output only. The full text is still sent to Pinboard's `extended`.
pub(crate) fn preview(text: &str) -> String {
    const MAX: usize = 160;
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > MAX {
        let head: String = one_line.chars().take(MAX).collect();
        format!("{head}…")
    } else {
        one_line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that read or write `PINBOARD_SYNC_ON_AUTH_FAILURE`, since
    /// the env var is process-global and clap reads it at parse time.
    static ON_AUTH_FAILURE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_on_auth_failure_env() -> std::sync::MutexGuard<'static, ()> {
        ON_AUTH_FAILURE_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn first_nonempty_prefers_earlier_nonempty_candidates() {
        assert_eq!(
            first_nonempty([Some("a".into()), Some("b".into())]),
            Some("a".into())
        );
        assert_eq!(
            first_nonempty([None, Some(String::new()), Some("b".into())]),
            Some("b".into())
        );
        assert_eq!(first_nonempty([None, Some(String::new())]), None);
    }

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

    #[test]
    fn preview_collapses_whitespace() {
        assert_eq!(preview("line1\nline2   line3"), "line1 line2 line3");
    }

    #[test]
    fn preview_truncates_long_text_with_ellipsis() {
        let long = "word ".repeat(100);
        let p = preview(&long);
        assert!(p.ends_with('…'));
        assert_eq!(p.chars().count(), 161);
    }

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
        let over = DateOverrides {
            stale_to_now: Some(true),
            ..Default::default()
        };
        let d = DateSettings::resolve(&over, None::<&RedditAccount>, src, &config);
        assert!(d.stale_to_now);

        // flag forces it off against a true global
        let mut config = Config::default();
        config.pinboard.cleanup_stale_to_now = true;
        let src = &config.defaults.reddit;
        let over = DateOverrides {
            stale_to_now: Some(false),
            ..Default::default()
        };
        let d = DateSettings::resolve(&over, None::<&RedditAccount>, src, &config);
        assert!(!d.stale_to_now);
    }

    #[test]
    fn date_settings_max_age_days_flag_tops_the_tier() {
        let mut config = Config::default();
        config.pinboard.post_date_max_age_days = Some(10); // global
        let src = &config.defaults.reddit; // per-source default None

        // flag wins
        let over = DateOverrides {
            max_age_days: Some(99),
            ..Default::default()
        };
        let d = DateSettings::resolve(&over, None::<&RedditAccount>, src, &config);
        assert_eq!(d.max_age_days, 99);

        // no flag → global
        let d = DateSettings::resolve(
            &DateOverrides::default(),
            None::<&RedditAccount>,
            src,
            &config,
        );
        assert_eq!(d.max_age_days, 10);

        // global unset → built-in default
        let config = Config::default();
        let src = &config.defaults.reddit;
        let d = DateSettings::resolve(
            &DateOverrides::default(),
            None::<&RedditAccount>,
            src,
            &config,
        );
        assert_eq!(d.max_age_days, config::DEFAULT_MAX_AGE_DAYS);
    }

    /// Override values chosen so each setting is distinct from the others and from the
    /// `Config::default()` globals — so a builder that wires a flag into the *wrong*
    /// `SyncJob` field (e.g. `public` into the `toread` slot) fails the assertions.
    fn wiring_overrides() -> SyncOverrides {
        SyncOverrides {
            limit: Some(7),
            toread: Some(true),        // global default is false
            public: Some(false),       // distinct from toread, so a swap is caught
            use_post_date: Some(true), // global default is false
            max_age_days: Some(99),    // global default is 30
            ..Default::default()
        }
    }

    fn assert_wired(job: &SyncJob) {
        assert_eq!(job.limit, 7, "limit");
        assert!(job.toread, "toread");
        assert!(!job.shared, "shared (from --public=false)");
        assert!(job.use_post_date, "use_post_date");
        assert_eq!(job.max_age_days, 99, "max_age_days");
    }

    #[test]
    fn build_reddit_job_wires_overrides_to_job_fields() {
        let ovr = SyncOverrides {
            reddit_username: Some("alice".into()),
            reddit_cookie: Some("reddit_session=x".into()),
            ..wiring_overrides()
        };
        let job = build_reddit_job(None, &ovr, &Config::default()).expect("builds");
        assert_wired(&job);
    }

    #[test]
    fn build_github_job_wires_overrides_to_job_fields() {
        let ovr = SyncOverrides {
            github_token: Some("ghp_test".into()),
            ..wiring_overrides()
        };
        let job = build_github_job(None, &ovr, &Config::default()).expect("builds");
        assert_wired(&job);
    }

    #[test]
    fn build_hackernews_job_wires_overrides_to_job_fields() {
        let ovr = SyncOverrides {
            hackernews_username: Some("alice".into()),
            ..wiring_overrides()
        };
        let job = build_hackernews_job(None, &ovr, &Config::default()).expect("builds");
        assert_wired(&job);
    }

    /// `sync --all`'s build phase must isolate a per-account build failure so the
    /// healthy accounts still yield jobs. Here a GitHub account with no resolvable
    /// token (no inline token, no `token_file`, and no `GITHUB_TOKEN` in the test
    /// environment) fails to build; it must be counted, not abort the whole run.
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
        // The `sync --all` path builds `SyncOverrides` itself (no `into_overrides`),
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
        let job = build_reddit_job(None, &ovr, &Config::default()).expect("builds");
        assert_eq!(job.hook.as_deref(), Some("refresh-cookie"));
    }

    #[test]
    fn cleanup_falls_back_to_on_auth_failure_env() {
        let _env = lock_on_auth_failure_env();
        // The NixOS module exports `onAuthFailure` *only* as this env var — it never
        // reaches the generated TOML — and it runs the `cleanup --all` timer with that
        // same environment. Miss this rung and the hook silently never fires for the
        // deployment the whole feature exists for.
        std::env::set_var("PINBOARD_SYNC_ON_AUTH_FAILURE", "env-hook");
        let hook = cleanup_hook(None, None, &Config::default());
        std::env::remove_var("PINBOARD_SYNC_ON_AUTH_FAILURE");

        assert_eq!(hook.as_deref(), Some("env-hook"));
    }

    #[test]
    fn cleanup_env_hook_outranks_the_config_tiers() {
        let _env = lock_on_auth_failure_env();
        // Same precedence as `sync`, whose per-source flag is env-backed: the env var is
        // the top rung, above an account or `[defaults.<source>]` entry.
        std::env::set_var("PINBOARD_SYNC_ON_AUTH_FAILURE", "env-hook");
        let hook = cleanup_hook(
            Some("account-hook"),
            Some("source-hook"),
            &Config::default(),
        );
        std::env::remove_var("PINBOARD_SYNC_ON_AUTH_FAILURE");

        assert_eq!(hook.as_deref(), Some("env-hook"));
    }

    #[test]
    fn cleanup_uses_the_account_hook_when_no_env_is_set() {
        let _env = lock_on_auth_failure_env();
        std::env::remove_var("PINBOARD_SYNC_ON_AUTH_FAILURE");
        let hook = cleanup_hook(
            Some("account-hook"),
            Some("source-hook"),
            &Config::default(),
        );

        assert_eq!(hook.as_deref(), Some("account-hook"));
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

        let job = build_reddit_job(None, &ovr, &Config::default()).expect("builds");
        assert_eq!(job.hook.as_deref(), Some("env-hook"));
    }

    #[test]
    fn top_level_on_auth_failure_reaches_single_source_hook() {
        let _env = lock_on_auth_failure_env();
        // `--on-auth-failure` placed before the source subcommand must reach the
        // job just like the `--all` path; `into_overrides` alone only sees the
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

        let ovr = args
            .into_overrides()
            .with_top_level_hook(cmd.on_auth_failure.clone());
        let job = build_reddit_job(None, &ovr, &Config::default()).expect("builds");
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

        let ovr = args
            .into_overrides()
            .with_top_level_hook(cmd.on_auth_failure.clone());
        let job = build_reddit_job(None, &ovr, &Config::default()).expect("builds");
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

        let ovr = args
            .into_overrides()
            .with_top_level_hook(cmd.on_auth_failure.clone());
        let job = build_reddit_job(None, &ovr, &Config::default()).expect("builds");
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

        let ovr = args
            .into_overrides()
            .with_top_level_hook(cmd.on_auth_failure.clone());
        let job = build_reddit_job(None, &ovr, &Config::default()).expect("builds");
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

        let over = args
            .dates
            .overrides()
            .with_top_level(&cmd.dates.overrides());
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

        let over = args
            .dates
            .overrides()
            .with_top_level(&cmd.dates.overrides());
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

    /// Parse `sync reddit <extra>` and return the parsed source args.
    fn parse_reddit_sync(extra: &[&str]) -> RedditSyncArgs {
        use clap::Parser;
        let mut argv = vec!["pinboard-sync", "sync", "reddit"];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(argv).expect("parses").command {
            Command::Sync(SyncCmd {
                source: Some(SyncSource::Reddit(args)),
                ..
            }) => args,
            _ => panic!("expected `sync reddit` args"),
        }
    }

    #[test]
    fn value_taking_boolean_flags_parse_bare_and_explicit() {
        // Absent → None (falls through to the config tiers).
        assert_eq!(parse_reddit_sync(&[]).public, None);
        // Bare `--public` → Some(true) via default_missing_value.
        assert_eq!(parse_reddit_sync(&["--public"]).public, Some(true));
        // `--public=false` → Some(false): the explicit force-off a bare flag can't express.
        assert_eq!(parse_reddit_sync(&["--public=false"]).public, Some(false));
        // Same shape for the other booleans.
        assert_eq!(parse_reddit_sync(&["--toread"]).toread, Some(true));
        assert_eq!(
            parse_reddit_sync(&["--use-post-date=false"]).use_post_date,
            Some(false)
        );
    }

    #[test]
    fn bare_bool_flag_does_not_swallow_the_account_positional() {
        // `require_equals` keeps clap from consuming the account as the flag value.
        let args = parse_reddit_sync(&["--public", "alice"]);
        assert_eq!(args.account.as_deref(), Some("alice"));
        assert_eq!(args.public, Some(true));
        // The `=` form still forces the flag off while leaving the positional alone.
        let args = parse_reddit_sync(&["--public=false", "alice"]);
        assert_eq!(args.account.as_deref(), Some("alice"));
        assert_eq!(args.public, Some(false));
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

    /// The hook is a real side effect on a real credential expiry, so drive it through
    /// the actual `sh -c` invocation rather than a stand-in: the marker file existing is
    /// the only proof the operator's command ran.
    #[test]
    fn the_auth_failure_hook_fires_for_reauth_and_nothing_else() {
        let dir = scratch_dir("auth-hook");
        let marker = dir.join("fired");
        let hook = format!("touch {}", marker.display());

        // A rate limit is not a credential problem: firing here would send the operator
        // to rotate a token when the fix is to wait.
        handle_source_err(
            SourceError::RateLimited("resets at 14:23".into()),
            Some(&hook),
        );
        assert!(!marker.exists(), "a rate limit must not fire the hook");

        handle_source_err(
            SourceError::Other(anyhow!("some transient failure")),
            Some(&hook),
        );
        assert!(!marker.exists(), "an ordinary error must not fire the hook");

        handle_source_err(
            SourceError::ReauthRequired("cookie expired".into()),
            Some(&hook),
        );
        assert!(marker.exists(), "a dead credential must fire the hook");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fresh, empty temp directory unique to `label`, cleaned up by the caller.
    fn scratch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pinboard-sync-test-{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
}
