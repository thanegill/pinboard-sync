//! pinboard-sync: sync saved/favorited items from multiple services to Pinboard.

mod cleanup;
mod config;
mod github;
mod hackernews;
mod http;
mod model;
mod pinboard;
mod reddit;
mod source;
mod sync;
#[cfg(test)]
mod test_support;

use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

use config::{Config, GithubAccount, HackernewsAccount, RedditAccount};
use github::GitHubClient;
use hackernews::HnClient;
use pinboard::PinboardClient;
use reddit::RedditClient;
use source::SourceError;

#[derive(Parser)]
#[command(name = "pinboard-sync", version, about, arg_required_else_help = true)]
struct Cli {
    /// Path to the TOML config file (env PINBOARD_SYNC_CONFIG, or *_FILE).
    #[arg(long, global = true)]
    config: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sync a source's saved/favorited items to Pinboard.
    Sync(SyncCmd),
    /// Normalize existing bookmarks for a source.
    Cleanup(CleanupCmd),
    /// Print a shell completion script (bash, zsh, fish, …) to stdout.
    Completions { shell: Shell },
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
struct SyncCmd {
    /// Run every configured account across every source (requires --config).
    #[arg(long)]
    all: bool,
    /// Show what would be written without touching Pinboard (with --all).
    #[arg(long)]
    dry_run: bool,
    #[arg(short, long)]
    verbose: bool,
    #[command(subcommand)]
    source: Option<SyncSource>,
}

#[derive(Subcommand)]
enum SyncSource {
    /// Sync saved Reddit posts and comments.
    Reddit(RedditSyncArgs),
    /// Sync starred GitHub repositories.
    Github(GithubSyncArgs),
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
    /// Pinboard API token, "user:TOKEN" (env PINBOARD_TOKEN, *_FILE, or ~/.pinboardrc).
    #[arg(long)]
    pinboard_token: Option<String>,
    /// Cap on new bookmarks written this run; 0 = all.
    #[arg(long, default_value_t = 0)]
    limit: usize,
    /// Create bookmarks public (default: private).
    #[arg(long)]
    public: bool,
    /// Shell command run when the Reddit cookie needs refreshing (a 401/403).
    #[arg(long, env = "PINBOARD_SYNC_ON_AUTH_FAILURE")]
    on_auth_failure: Option<String>,
    /// Fetch and print what would be posted, without writing to Pinboard.
    #[arg(long)]
    dry_run: bool,
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Args, Clone)]
struct GithubSyncArgs {
    /// Account name to select from the config (default: the first github account).
    account: Option<String>,
    /// Run every github account in the config.
    #[arg(long)]
    all: bool,
    /// GitHub personal access token (env GITHUB_TOKEN, or *_FILE).
    #[arg(long)]
    github_token: Option<String>,
    /// Pinboard API token, "user:TOKEN" (env PINBOARD_TOKEN, *_FILE, or ~/.pinboardrc).
    #[arg(long)]
    pinboard_token: Option<String>,
    /// Cap on new bookmarks written this run; 0 = all.
    #[arg(long, default_value_t = 0)]
    limit: usize,
    /// Create bookmarks public (default: private).
    #[arg(long)]
    public: bool,
    /// Shell command run when the GitHub token needs refreshing (a 401).
    #[arg(long, env = "PINBOARD_SYNC_ON_AUTH_FAILURE")]
    on_auth_failure: Option<String>,
    /// Fetch and print what would be posted, without writing to Pinboard.
    #[arg(long)]
    dry_run: bool,
    #[arg(short, long)]
    verbose: bool,
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
    /// Pinboard API token, "user:TOKEN" (env PINBOARD_TOKEN, *_FILE, or ~/.pinboardrc).
    #[arg(long)]
    pinboard_token: Option<String>,
    /// Cap on new bookmarks written this run; 0 = all.
    #[arg(long, default_value_t = 0)]
    limit: usize,
    /// Create bookmarks public (default: private).
    #[arg(long)]
    public: bool,
    /// Fetch and print what would be posted, without writing to Pinboard.
    #[arg(long)]
    dry_run: bool,
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Args)]
struct CleanupCmd {
    /// Run cleanup for every configured account across every cleanup-capable source.
    #[arg(long)]
    all: bool,
    /// Show what would change without writing to Pinboard (with --all).
    #[arg(long)]
    dry_run: bool,
    #[arg(short, long)]
    verbose: bool,
    #[command(subcommand)]
    source: Option<CleanupSource>,
}

#[derive(Subcommand)]
enum CleanupSource {
    /// Normalize existing reddit bookmarks (URLs, tags, NSFW, titles).
    Reddit(RedditCleanupArgs),
}

#[derive(Args, Clone)]
struct RedditCleanupArgs {
    /// Account name to select from the config (default: the first reddit account).
    account: Option<String>,
    /// Run cleanup for every reddit account in the config.
    #[arg(long)]
    all: bool,
    /// Reddit session cookie (env REDDIT_COOKIE, or *_FILE). Needed for the
    /// `/api/info` lookups; not required with --no-nsfw --no-titles.
    #[arg(long)]
    reddit_cookie: Option<String>,
    /// Pinboard API token (env PINBOARD_TOKEN, *_FILE, or ~/.pinboardrc).
    #[arg(long)]
    pinboard_token: Option<String>,
    /// Skip NSFW tagging (no Reddit /api/info call for over_18).
    #[arg(long)]
    no_nsfw: bool,
    /// Skip replacing generic placeholder titles.
    #[arg(long)]
    no_titles: bool,
    /// Show what would change without writing to Pinboard.
    #[arg(long)]
    dry_run: bool,
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();

    let result = async {
        match cli.command {
            Command::Sync(cmd) => run_sync(cmd, &load_config(cli.config.clone())?).await,
            Command::Cleanup(cmd) => run_cleanup(cmd, &load_config(cli.config.clone())?).await,
            Command::Completions { shell } => {
                print_completions(shell);
                Ok(())
            }
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
        eprintln!("error: {e:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Write a shell completion script for `shell` to stdout.
fn print_completions(shell: Shell) {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
}

/// Load the config from `--config`/`$PINBOARD_SYNC_CONFIG`/`_FILE`; absent = defaults.
fn load_config(flag: Option<String>) -> Result<Config> {
    match resolve_secret(flag, "PINBOARD_SYNC_CONFIG", None, None) {
        Some(path) => {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading config file {path}"))?;
            Config::parse(&text)
        }
        None => Ok(Config::default()),
    }
}

// --- sync --------------------------------------------------------------------

async fn run_sync(cmd: SyncCmd, config: &Config) -> Result<()> {
    match (cmd.all, cmd.source) {
        (true, Some(_)) => bail!("--all cannot be combined with a source subcommand"),
        (true, None) => {
            if config.reddit.is_empty() && config.github.is_empty() && config.hackernews.is_empty()
            {
                bail!("--all requires a --config with at least one configured account");
            }
            let ovr = SyncOverrides {
                dry_run: cmd.dry_run,
                verbose: cmd.verbose,
                ..SyncOverrides::default()
            };
            let mut run = AllRun::default();
            for acct in &config.reddit {
                run.record(sync_one_reddit(Some(acct), &ovr, config).await);
            }
            for acct in &config.github {
                run.record(sync_one_github(Some(acct), &ovr, config).await);
            }
            for acct in &config.hackernews {
                run.record(sync_one_hackernews(Some(acct), &ovr, config).await);
            }
            run.finish()
        }
        (false, Some(SyncSource::Reddit(args))) => run_sync_reddit(args, config).await,
        (false, Some(SyncSource::Github(args))) => run_sync_github(args, config).await,
        (false, Some(SyncSource::Hackernews(args))) => run_sync_hackernews(args, config).await,
        (false, None) => bail!("specify a source (e.g. `sync reddit`) or pass --all"),
    }
}

async fn run_sync_reddit(args: RedditSyncArgs, config: &Config) -> Result<()> {
    let ovr = SyncOverrides {
        reddit_username: args.reddit_username,
        reddit_cookie: args.reddit_cookie,
        pinboard_token: args.pinboard_token,
        on_auth_failure: args.on_auth_failure,
        limit: args.limit,
        public: args.public,
        dry_run: args.dry_run,
        verbose: args.verbose,
        ..SyncOverrides::default()
    };
    if args.all {
        if config.reddit.is_empty() {
            bail!("--all requires a --config with at least one reddit account");
        }
        let mut run = AllRun::default();
        for acct in &config.reddit {
            run.record(sync_one_reddit(Some(acct), &ovr, config).await);
        }
        run.finish()
    } else {
        let account = config::select_account(&config.reddit, args.account.as_deref())?;
        sync_one_reddit(account, &ovr, config).await
    }
}

async fn run_sync_github(args: GithubSyncArgs, config: &Config) -> Result<()> {
    let ovr = SyncOverrides {
        github_token: args.github_token,
        pinboard_token: args.pinboard_token,
        on_auth_failure: args.on_auth_failure,
        limit: args.limit,
        public: args.public,
        dry_run: args.dry_run,
        verbose: args.verbose,
        ..SyncOverrides::default()
    };
    if args.all {
        if config.github.is_empty() {
            bail!("--all requires a --config with at least one github account");
        }
        let mut run = AllRun::default();
        for acct in &config.github {
            run.record(sync_one_github(Some(acct), &ovr, config).await);
        }
        run.finish()
    } else {
        let account = config::select_account(&config.github, args.account.as_deref())?;
        sync_one_github(account, &ovr, config).await
    }
}

async fn run_sync_hackernews(args: HackernewsSyncArgs, config: &Config) -> Result<()> {
    let ovr = SyncOverrides {
        hackernews_username: args.username,
        pinboard_token: args.pinboard_token,
        limit: args.limit,
        public: args.public,
        dry_run: args.dry_run,
        verbose: args.verbose,
        ..SyncOverrides::default()
    };
    if args.all {
        if config.hackernews.is_empty() {
            bail!("--all requires a --config with at least one hackernews account");
        }
        let mut run = AllRun::default();
        for acct in &config.hackernews {
            run.record(sync_one_hackernews(Some(acct), &ovr, config).await);
        }
        run.finish()
    } else {
        let account = config::select_account(&config.hackernews, args.account.as_deref())?;
        sync_one_hackernews(account, &ovr, config).await
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
    limit: usize,
    public: bool,
    dry_run: bool,
    verbose: bool,
}

async fn sync_one_reddit(
    account: Option<&RedditAccount>,
    ovr: &SyncOverrides,
    config: &Config,
) -> Result<()> {
    let pinboard_token = resolve_pinboard_token(ovr.pinboard_token.clone(), &config.pinboard)
        .context("missing Pinboard token (set --pinboard-token, PINBOARD_TOKEN, [pinboard] in the config, or ~/.pinboardrc)")?;
    let pinboard = PinboardClient::new(pinboard_token, ovr.public || config.pinboard.public)?;

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
    let limit = if ovr.limit > 0 {
        ovr.limit
    } else {
        account.and_then(|a| a.limit).unwrap_or(0)
    };
    let hook = resolve_hook(
        ovr.on_auth_failure.clone(),
        account.and_then(|a| a.on_auth_failure.as_deref()),
        config,
    );

    let cfg = sync::SyncConfig {
        limit,
        dry_run: ovr.dry_run,
        verbose: ovr.verbose,
    };
    let reddit = RedditClient::for_user(username, cookie, reddit_config)?;
    sync::run(&reddit, &pinboard, &cfg)
        .await
        .map_err(|e| handle_reddit_err(e, hook.as_deref()))?;
    Ok(())
}

async fn sync_one_github(
    account: Option<&GithubAccount>,
    ovr: &SyncOverrides,
    config: &Config,
) -> Result<()> {
    let pinboard_token = resolve_pinboard_token(ovr.pinboard_token.clone(), &config.pinboard)
        .context("missing Pinboard token (set --pinboard-token, PINBOARD_TOKEN, [pinboard] in the config, or ~/.pinboardrc)")?;
    let pinboard = PinboardClient::new(pinboard_token, ovr.public || config.pinboard.public)?;

    let token = resolve_secret(
        ovr.github_token.clone(),
        "GITHUB_TOKEN",
        account.and_then(|a| a.token.clone()),
        account.and_then(|a| a.token_file.as_deref()),
    )
    .context("missing GitHub token (set --github-token, GITHUB_TOKEN, or `token`/`token_file` in the config)")?;

    let github_config = account
        .map(GithubAccount::github_config)
        .unwrap_or_default();
    let limit = if ovr.limit > 0 {
        ovr.limit
    } else {
        account.and_then(|a| a.limit).unwrap_or(0)
    };
    let hook = resolve_hook(
        ovr.on_auth_failure.clone(),
        account.and_then(|a| a.on_auth_failure.as_deref()),
        config,
    );

    let cfg = sync::SyncConfig {
        limit,
        dry_run: ovr.dry_run,
        verbose: ovr.verbose,
    };
    let github = GitHubClient::new(token, github_config)?;
    sync::run(&github, &pinboard, &cfg)
        .await
        .map_err(|e| handle_reddit_err(e, hook.as_deref()))?;
    Ok(())
}

async fn sync_one_hackernews(
    account: Option<&HackernewsAccount>,
    ovr: &SyncOverrides,
    config: &Config,
) -> Result<()> {
    let pinboard_token = resolve_pinboard_token(ovr.pinboard_token.clone(), &config.pinboard)
        .context("missing Pinboard token (set --pinboard-token, PINBOARD_TOKEN, [pinboard] in the config, or ~/.pinboardrc)")?;
    let pinboard = PinboardClient::new(pinboard_token, ovr.public || config.pinboard.public)?;

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
    let limit = if ovr.limit > 0 {
        ovr.limit
    } else {
        account.and_then(|a| a.limit).unwrap_or(0)
    };

    let cfg = sync::SyncConfig {
        limit,
        dry_run: ovr.dry_run,
        verbose: ovr.verbose,
    };
    let hn = HnClient::new(username, hn_config)?;
    // HackerNews favorites are public, so there is no auth-failure hook to fire.
    sync::run(&hn, &pinboard, &cfg)
        .await
        .map_err(|e| handle_reddit_err(e, None))?;
    Ok(())
}

// --- cleanup -----------------------------------------------------------------

async fn run_cleanup(cmd: CleanupCmd, config: &Config) -> Result<()> {
    match (cmd.all, cmd.source) {
        (true, Some(_)) => bail!("--all cannot be combined with a source subcommand"),
        (true, None) => {
            // GitHub has no cleanup, so cleanup --all covers reddit only.
            if config.reddit.is_empty() {
                bail!("cleanup --all requires a --config with at least one reddit account");
            }
            let args = RedditCleanupArgs {
                account: None,
                all: true,
                reddit_cookie: None,
                pinboard_token: None,
                no_nsfw: false,
                no_titles: false,
                dry_run: cmd.dry_run,
                verbose: cmd.verbose,
            };
            run_cleanup_reddit(args, config).await
        }
        (false, Some(CleanupSource::Reddit(args))) => run_cleanup_reddit(args, config).await,
        (false, None) => bail!("specify a source (e.g. `cleanup reddit`) or pass --all"),
    }
}

async fn run_cleanup_reddit(args: RedditCleanupArgs, config: &Config) -> Result<()> {
    if args.all {
        if config.reddit.is_empty() {
            bail!("--all requires a --config with at least one reddit account");
        }
        let mut run = AllRun::default();
        for acct in &config.reddit {
            run.record(cleanup_one_reddit(Some(acct), &args, config).await);
        }
        run.finish()
    } else {
        let account = config::select_account(&config.reddit, args.account.as_deref())?;
        cleanup_one_reddit(account, &args, config).await
    }
}

async fn cleanup_one_reddit(
    account: Option<&RedditAccount>,
    args: &RedditCleanupArgs,
    config: &Config,
) -> Result<()> {
    let pinboard_token = resolve_pinboard_token(args.pinboard_token.clone(), &config.pinboard)
        .context("missing Pinboard token (set --pinboard-token, PINBOARD_TOKEN, [pinboard] in the config, or ~/.pinboardrc)")?;
    let pinboard = PinboardClient::new(pinboard_token, false)?;

    let reddit_config = account
        .map(RedditAccount::reddit_config)
        .unwrap_or_default();
    let opts = cleanup::CleanupOpts {
        dry_run: args.dry_run,
        verbose: args.verbose,
        mark_nsfw: !args.no_nsfw,
        fix_titles: !args.no_titles,
        base_tag: reddit_config.tags.first().cloned().unwrap_or_default(),
        subreddit_tag_prefix: reddit_config.subreddit_prefix.clone(),
    };

    // Reddit (for /api/info) is only needed when marking NSFW or fixing titles.
    let reddit = if opts.mark_nsfw || opts.fix_titles {
        let cookie = resolve_secret(
            args.reddit_cookie.clone(),
            "REDDIT_COOKIE",
            account.and_then(|a| a.cookie.clone()),
            account.and_then(|a| a.cookie_file.as_deref()),
        )
        .context("missing Reddit cookie (set --reddit-cookie, REDDIT_COOKIE, or pass --no-nsfw --no-titles)")?;
        Some(RedditClient::for_info(Some(cookie))?)
    } else {
        None
    };

    cleanup::run(&pinboard, reddit.as_ref(), &opts).await
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
fn handle_reddit_err(e: SourceError, hook: Option<&str>) -> anyhow::Error {
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
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The auth-failure hook: CLI flag (with its env) → per-account override → `[hooks]`.
fn resolve_hook(
    flag: Option<String>,
    account_override: Option<&str>,
    config: &Config,
) -> Option<String> {
    flag.or_else(|| account_override.map(str::to_string))
        .or_else(|| config.hooks.on_auth_failure.clone())
}

fn resolve_pinboard_token(flag: Option<String>, pb: &config::Pinboard) -> Option<String> {
    resolve_secret(
        flag,
        "PINBOARD_TOKEN",
        pb.token.clone(),
        pb.token_file.as_deref(),
    )
    .or_else(read_pinboardrc)
}

/// Read `api_token` from `~/.pinboardrc` (`[authentication]` section).
fn read_pinboardrc() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let contents = std::fs::read_to_string(std::path::Path::new(&home).join(".pinboardrc")).ok()?;
    parse_pinboardrc(&contents)
}

/// Extract `api_token` from `~/.pinboardrc` contents. Pure, so it is unit-tested
/// without a real home directory.
fn parse_pinboardrc(contents: &str) -> Option<String> {
    for line in contents.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("api_token") {
            let value = rest.trim_start().trim_start_matches('=').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
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
    fn parse_pinboardrc_reads_api_token() {
        let ini = "[authentication]\napi_token = user:ABC123\n";
        assert_eq!(parse_pinboardrc(ini), Some("user:ABC123".into()));
        assert_eq!(
            parse_pinboardrc("api_token=user:XYZ"),
            Some("user:XYZ".into())
        );
    }

    #[test]
    fn parse_pinboardrc_returns_none_without_token() {
        assert_eq!(parse_pinboardrc("[authentication]\n"), None);
        assert_eq!(parse_pinboardrc(""), None);
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
}
