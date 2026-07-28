//! pinboard-sync: sync saved/favorited items from multiple services to Pinboard.

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

use bookmark::{Bookmark, BookmarkStore};
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
    /// Back up all Pinboard bookmarks to a file (raw `posts/all` JSON, verbatim).
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
    /// File to write the raw Pinboard JSON snapshot to (replaced atomically).
    path: PathBuf,
    /// Pinboard API token, "user:TOKEN" (env PINBOARD_TOKEN, or *_FILE).
    #[arg(long)]
    pinboard_token: Option<String>,
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
    let (jobs, ovr) = match (cmd.all, cmd.source) {
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
            let mut jobs = Vec::new();
            for acct in &config.reddit {
                jobs.push(build_reddit_job(Some(acct), &ovr, config)?);
            }
            for acct in &config.github {
                jobs.push(build_github_job(Some(acct), &ovr, config)?);
            }
            for acct in &config.hackernews {
                jobs.push(build_hackernews_job(Some(acct), &ovr, config)?);
            }
            (jobs, ovr)
        }
        (false, Some(SyncSource::Reddit(args))) => {
            let (account, all) = (args.account.clone(), args.all);
            let ovr = args
                .into_overrides()
                .with_top_level_hook(cmd.on_auth_failure.clone());
            let jobs = build_jobs(&config.reddit, account.as_deref(), all, |a| {
                build_reddit_job(a, &ovr, config)
            })?;
            (jobs, ovr)
        }
        (false, Some(SyncSource::Github(args))) => {
            let (account, all) = (args.account.clone(), args.all);
            let ovr = args
                .into_overrides()
                .with_top_level_hook(cmd.on_auth_failure.clone());
            let jobs = build_jobs(&config.github, account.as_deref(), all, |a| {
                build_github_job(a, &ovr, config)
            })?;
            (jobs, ovr)
        }
        (false, Some(SyncSource::Hackernews(args))) => {
            let (account, all) = (args.account.clone(), args.all);
            let ovr = args
                .into_overrides()
                .with_top_level_hook(cmd.on_auth_failure.clone());
            let jobs = build_jobs(&config.hackernews, account.as_deref(), all, |a| {
                build_hackernews_job(a, &ovr, config)
            })?;
            (jobs, ovr)
        }
        (false, None) => bail!("specify a source (e.g. `sync reddit`) or pass --all"),
    };

    let (pinboard, bookmarks) = open_pinboard(ovr.pinboard_token.clone(), config).await?;
    // `--dry-run` is accepted both before the source subcommand (on `SyncCmd`) and
    // after it (per-source); honor either placement. (`--verbose` is global.)
    run_sync_jobs(jobs, &pinboard, &bookmarks, ovr.dry_run || cmd.dry_run).await
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

    let mut run = AllRun::default();
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

/// Write a verbatim snapshot of every Pinboard bookmark (raw `posts/all` JSON) to a
/// file. Preserves exactly what Pinboard returns — no lossy conversion.
async fn run_backup(cmd: BackupCmd, config: &Config) -> Result<()> {
    check_backup_dir(&cmd.path)?;
    let pinboard = build_pinboard(cmd.pinboard_token, config)?;
    let body = pinboard
        .export_all()
        .await
        .context("exporting Pinboard bookmarks")?;
    write_backup(&cmd.path, &body)?;
    info!("backed up Pinboard bookmarks to {}", cmd.path.display());
    Ok(())
}

/// The directory `path` will be written into (`.` when `path` is bare).
fn backup_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Fail fast if the destination directory is missing, before the ~5-minute rate-limited
/// `posts/all` export, so a bad path doesn't waste the whole budget.
fn check_backup_dir(path: &Path) -> Result<()> {
    let dir = backup_dir(path);
    if !dir.is_dir() {
        bail!("backup directory {} does not exist", dir.display());
    }
    Ok(())
}

/// Atomically replace `path` with `body`. Guards a good backup: it refuses a body that
/// doesn't parse as a JSON array (a 2xx interstitial/proxy page, an empty response, or a
/// connection dropped mid-array can all pass `posts/all`'s status check), then writes a
/// private, fsync'd temp file and renames it over the target — an atomic, durable swap, so
/// a partial or crashed write leaves the previous snapshot intact rather than truncating
/// it. The snapshot holds every private bookmark, so both files are created mode 0600.
fn write_backup(path: &Path, body: &str) -> Result<()> {
    if serde_json::from_str::<Vec<serde_json::Value>>(body).is_err() {
        bail!(
            "Pinboard returned a non-JSON-array response ({} bytes); refusing to overwrite {}",
            body.len(),
            path.display()
        );
    }

    let dir = backup_dir(path);
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("pinboard-backup");
    let (tmp, mut file) = create_backup_tmp(&dir, file_name)?;
    write_backup_tmp(&tmp, &mut file, body)
        .and_then(|()| {
            std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
            sync_dir(&dir);
            Ok(())
        })
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
}

/// Open `tmp` as a brand-new mode-0600 regular file. `create_new` (O_EXCL) never follows a
/// pre-existing symlink and never reuses an existing file's contents or permissions, so the
/// 0600 promise holds; a path that already exists errors with `AlreadyExists` instead.
fn open_new_private(tmp: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)
}

/// Create a fresh, private temp file next to the backup target and return it with its path.
/// Each attempt mixes fresh entropy into the name, so a leftover temp from a crashed run
/// (even one with this pid) can't be reused; `open_new_private` guarantees the file is new.
fn create_backup_tmp(dir: &Path, file_name: &str) -> Result<(PathBuf, std::fs::File)> {
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let pid = std::process::id();

    create_new_temp(dir, || {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let nonce = nanos ^ u128::from(COUNTER.fetch_add(1, Ordering::Relaxed));
        format!(".{file_name}.tmp.{pid}.{nonce:x}")
    })
}

/// Open a fresh private temp file in `dir`, drawing candidate names from `next_name` and
/// retrying whenever the chosen path already exists (a leftover temp, a squatter). Bounded
/// so a name generator that keeps yielding a colliding name can't spin forever.
fn create_new_temp(
    dir: &Path,
    mut next_name: impl FnMut() -> String,
) -> Result<(PathBuf, std::fs::File)> {
    let mut last_err = None;
    for _ in 0..100 {
        let tmp = dir.join(next_name());
        match open_new_private(&tmp) {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_err = Some(e),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("creating backup temp file in {}", dir.display()));
            }
        }
    }
    Err(last_err.expect("loop ran at least once"))
        .with_context(|| format!("creating a unique backup temp file in {}", dir.display()))
}

/// Write `body` to the already-opened temp `file` and fsync it, so the bytes are on disk
/// before the caller renames it over the real target.
fn write_backup_tmp(tmp: &Path, file: &mut std::fs::File, body: &str) -> Result<()> {
    use std::io::Write;

    file.write_all(body.as_bytes())
        .with_context(|| format!("writing backup to {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("flushing backup to {}", tmp.display()))
}

/// Best-effort fsync of the directory so the rename itself survives a crash.
fn sync_dir(dir: &Path) {
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
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
            let mut run = AllRun::default();
            if let Some(acct) = config.github.first() {
                let opts = gh_cleanup_opts(&over, cmd.dry_run, Some(acct), config);
                run.record(
                    cleanup_github_for(&pinboard, &bookmarks, Some(acct), None, &opts).await,
                );
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
                run.record(
                    cleanup_one_reddit(Some(acct), &args, &over, &pinboard, &bookmarks, config)
                        .await,
                );
            }
            if let Some(acct) = config.hackernews.first() {
                run.record(
                    cleanup_one_hackernews(
                        Some(acct),
                        cmd.dry_run,
                        false, // linking is opt-in via `cleanup hackernews --link-discussions`
                        None,
                        &over,
                        &pinboard,
                        &bookmarks,
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
    dry_run: bool,
    account: Option<&GitHubAccount>,
    config: &Config,
) -> github::GitHubCleanupOpts {
    let dates = DateSettings::resolve(over, account, &config.defaults.github, config);
    github::GitHubCleanupOpts {
        dry_run,
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
    let opts = gh_cleanup_opts(
        &over,
        cleanup_dry_run(top_dry_run, args.dry_run),
        account,
        config,
    );
    cleanup_github_for(&pinboard, &bookmarks, account, args.github_token, &opts).await
}

/// Run github cleanup: build an API client from the account/env token and refresh
/// renamed repos / titles / language.
async fn cleanup_github_for(
    pinboard: &PinboardClient,
    bookmarks: &[Bookmark],
    account: Option<&GitHubAccount>,
    token_flag: Option<String>,
    opts: &github::GitHubCleanupOpts,
) -> Result<()> {
    let config = account
        .map(GitHubAccount::github_config)
        .unwrap_or_default();
    let token = resolve_secret(
        token_flag,
        "GITHUB_TOKEN",
        account.and_then(|a| a.token.clone()),
        account.and_then(|a| a.token_file.as_deref()),
    )
    .context("missing GitHub token (set --github-token, GITHUB_TOKEN, or `token`/`token_file` in the config)")?;
    let client = GitHubClient::new(token, config.clone())?;
    github::cleanup(pinboard, &client, &config, opts, bookmarks).await
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
    cleanup_one_reddit(
        account,
        &args,
        &args.dates.overrides().with_top_level(top),
        &pinboard,
        &bookmarks,
        config,
    )
    .await
}

async fn cleanup_one_reddit(
    account: Option<&RedditAccount>,
    args: &RedditCleanupArgs,
    over: &DateOverrides,
    pinboard: &PinboardClient,
    bookmarks: &[Bookmark],
    config: &Config,
) -> Result<()> {
    let reddit_config = account
        .map(RedditAccount::reddit_config)
        .unwrap_or_default();
    let dates = DateSettings::resolve(over, account, &config.defaults.reddit, config);
    let opts = cleanup::RedditCleanupOpts {
        dry_run: args.dry_run,
        mark_nsfw: !args.no_nsfw,
        fix_titles: !args.no_titles,
        base_tag: reddit_config.tags.first().cloned().unwrap_or_default(),
        subreddit_tag_prefix: reddit_config.subreddit_prefix.clone(),
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

    cleanup::run(pinboard, reddit.as_ref(), &opts, bookmarks).await
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
    cleanup_one_hackernews(
        account,
        cleanup_dry_run(top_dry_run, args.dry_run),
        args.link_discussions,
        args.link_tag,
        &over,
        &pinboard,
        &bookmarks,
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
async fn cleanup_one_hackernews(
    account: Option<&HackernewsAccount>,
    dry_run: bool,
    link_discussions: bool,
    link_tag: Option<String>,
    over: &DateOverrides,
    pinboard: &PinboardClient,
    bookmarks: &[Bookmark],
    config: &Config,
) -> Result<()> {
    let hn_config = resolve_hackernews_cleanup_config(account, link_tag)?;
    let dates = DateSettings::resolve(over, account, &config.defaults.hackernews, config);
    let hn = HackerNewsClient::for_cleanup(hn_config)?;
    hn.cleanup(
        pinboard,
        &HackerNewsCleanupOpts {
            dry_run,
            link_discussions,
            use_post_date: dates.use_post_date,
            max_age_days: dates.max_age_days,
            cleanup_stale_to_now: dates.stale_to_now,
        },
        bookmarks,
    )
    .await
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

    #[test]
    fn backup_takes_a_required_path_and_optional_token() {
        let cmd = parse_backup(&["out.json"]);
        assert_eq!(cmd.path, PathBuf::from("out.json"));
        assert_eq!(cmd.pinboard_token, None);

        let cmd = parse_backup(&["out.json", "--pinboard-token", "user:tok"]);
        assert_eq!(cmd.pinboard_token.as_deref(), Some("user:tok"));

        // The path is required.
        assert!(Cli::try_parse_from(["pinboard-sync", "backup"]).is_err());
    }

    /// A fresh, empty temp directory unique to `label`, cleaned up by the caller.
    fn scratch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pinboard-sync-test-{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_backup_replaces_atomically_and_leaves_no_temp() {
        let dir = scratch_dir("write-backup-ok");
        let target = dir.join("pinboard-backup.json");
        std::fs::write(&target, "OLD").unwrap();

        write_backup(&target, "[{\"href\":\"https://example.com/\"}]").unwrap();

        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "[{\"href\":\"https://example.com/\"}]"
        );
        assert!(
            std::fs::read_dir(&dir).unwrap().count() == 1,
            "no temp left"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_backup_writes_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch_dir("write-backup-perms");
        let target = dir.join("pinboard-backup.json");

        write_backup(&target, "[]").unwrap();

        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "backup must not be world-readable");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_backup_refuses_non_json_and_preserves_existing() {
        let dir = scratch_dir("write-backup-bad");
        let target = dir.join("pinboard-backup.json");
        std::fs::write(&target, "GOOD BACKUP").unwrap();

        // A 200 that isn't a JSON array (proxy page, empty body) or a connection dropped
        // mid-array (a truncated array that still starts with `[`) must not clobber it.
        for bad in [
            "",
            "  ",
            "<html>Back off</html>",
            "[{\"href\":\"https://example.com/\"}",
        ] {
            let err = write_backup(&target, bad).unwrap_err();
            assert!(err.to_string().contains("non-JSON"), "got: {err}");
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "GOOD BACKUP");
        // A rejected write must leave no temp file behind either.
        assert!(
            std::fs::read_dir(&dir).unwrap().count() == 1,
            "no temp left"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn backup_temp_refuses_to_reuse_a_preexisting_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch_dir("write-backup-squat");
        let squatted = dir.join(".pinboard-backup.json.tmp.squatter");
        std::fs::write(&squatted, "SECRET-LEAK").unwrap();
        std::fs::set_permissions(&squatted, PermissionsExt::from_mode(0o644)).unwrap();

        // create_new refuses a path that already exists rather than truncating it and
        // inheriting its world-readable mode, so a squatted temp can't leak into a snapshot.
        let err = open_new_private(&squatted).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&squatted).unwrap(), "SECRET-LEAK");
        let mode = std::fs::metadata(&squatted).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644, "existing file left untouched");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn backup_temp_does_not_follow_a_symlink() {
        let dir = scratch_dir("write-backup-symlink");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "PRECIOUS").unwrap();
        let link = dir.join(".pinboard-backup.json.tmp.link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        // O_EXCL refuses the pre-existing symlink instead of following it and truncating
        // the target.
        let err = open_new_private(&link).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "PRECIOUS");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn create_new_temp_retries_past_a_colliding_name() {
        let dir = scratch_dir("create-new-temp-retry");
        std::fs::write(dir.join("taken"), "SQUAT").unwrap();

        // First candidate collides with the pre-existing file; the loop must retry the
        // fresh one rather than reuse or truncate "taken".
        let mut names = ["taken", "fresh"].into_iter().map(str::to_string);
        let (tmp, _file) = create_new_temp(&dir, || names.next().unwrap()).unwrap();

        assert_eq!(tmp, dir.join("fresh"));
        assert_eq!(std::fs::read_to_string(dir.join("taken")).unwrap(), "SQUAT");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn create_new_temp_gives_up_after_exhausting_attempts() {
        let dir = scratch_dir("create-new-temp-exhaust");
        std::fs::write(dir.join("always"), "SQUAT").unwrap();

        // A generator that never yields a free name exhausts the bounded loop and surfaces
        // the last AlreadyExists rather than spinning forever.
        let err = create_new_temp(&dir, || "always".to_string()).unwrap_err();
        assert_eq!(
            err.downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind),
            Some(std::io::ErrorKind::AlreadyExists)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
