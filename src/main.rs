//! pinboard-sync: sync saved Reddit items to a Pinboard account.

mod cleanup;
mod http;
mod model;
mod pinboard;
mod reddit;
mod source;
mod sync;
#[cfg(test)]
mod test_support;

use std::process::ExitCode;

use anyhow::{anyhow, Context, Result};
use clap::{Args, Parser, Subcommand};

use pinboard::PinboardClient;
use reddit::RedditClient;
use source::SourceError;

#[derive(Parser)]
#[command(name = "pinboard-sync", version, about, arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sync saved Reddit items to Pinboard.
    Sync(SyncArgs),
    /// Normalize existing reddit bookmarks (URLs, tags, NSFW, titles).
    Cleanup(CleanupArgs),
}

#[derive(Args, Clone)]
struct SyncArgs {
    /// Pinboard API token, "user:TOKEN" (env PINBOARD_TOKEN, *_FILE, or ~/.pinboardrc).
    #[arg(long)]
    pinboard_token: Option<String>,
    /// Reddit username whose saved items to sync (env REDDIT_USERNAME, or *_FILE).
    /// Required; non-secret. Reads `old.reddit.com/user/<name>/saved.json`.
    #[arg(long)]
    reddit_username: Option<String>,
    /// Cookie header for Reddit, e.g. `reddit_session=…` (env REDDIT_COOKIE, or
    /// *_FILE). Reddit blocks cookieless requests, so this is required; copy
    /// `reddit_session` from a logged-in browser.
    #[arg(long)]
    reddit_cookie: Option<String>,
    /// Optional cap on new bookmarks written per run; 0 = all. Dedup against
    /// Pinboard handles correctness, so this is just a throttle (e.g. first run).
    #[arg(long, default_value_t = 0)]
    limit: usize,
    /// Base tag applied to every bookmark.
    #[arg(long, default_value = "reddit")]
    base_tag: String,
    /// Prefix for the subreddit tag.
    #[arg(long, default_value = "subreddit:")]
    subreddit_tag_prefix: String,
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

#[derive(Args)]
struct CleanupArgs {
    /// Pinboard API token, "user:TOKEN" (env PINBOARD_TOKEN, *_FILE, or ~/.pinboardrc).
    #[arg(long)]
    pinboard_token: Option<String>,
    /// Cookie header for Reddit, e.g. `reddit_session=…` (env REDDIT_COOKIE, or
    /// *_FILE). Needed for the `/api/info` lookups that mark NSFW and fix titles;
    /// not required with --no-nsfw --no-titles.
    #[arg(long)]
    reddit_cookie: Option<String>,
    /// Base tag applied to every reddit bookmark.
    #[arg(long, default_value = "reddit")]
    base_tag: String,
    /// Prefix for the subreddit tag.
    #[arg(long, default_value = "subreddit:")]
    subreddit_tag_prefix: String,
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

    let result = match cli.command {
        Command::Sync(args) => sync(args).await,
        Command::Cleanup(args) => run_cleanup(args).await,
    };

    if let Err(e) = result {
        eprintln!("error: {e:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn sync(args: SyncArgs) -> Result<()> {
    let pinboard_token = resolve_pinboard_token(args.pinboard_token.clone()).context(
        "missing Pinboard token (set --pinboard-token, PINBOARD_TOKEN, or ~/.pinboardrc)",
    )?;
    let pinboard = PinboardClient::new(pinboard_token, args.public)?;
    let cfg = sync::SyncConfig {
        limit: args.limit,
        base_tag: args.base_tag.clone(),
        subreddit_tag_prefix: args.subreddit_tag_prefix.clone(),
        dry_run: args.dry_run,
        verbose: args.verbose,
    };
    let hook = args.on_auth_failure.as_deref();

    let username = resolve_secret(args.reddit_username.clone(), "REDDIT_USERNAME")
        .context("missing Reddit username (set --reddit-username or REDDIT_USERNAME)")?;
    let cookie = resolve_secret(args.reddit_cookie.clone(), "REDDIT_COOKIE");
    if args.verbose {
        // Diagnostic only — never print cookie values, just the cookie *names*/length,
        // so a cookieless request (which 403s) is obvious. The username is non-secret.
        match &cookie {
            Some(c) => {
                let names: Vec<&str> = c
                    .split(';')
                    .filter_map(|p| p.trim().split('=').next())
                    .filter(|s| !s.is_empty())
                    .collect();
                eprintln!(
                    "saved source: user={username}; cookie present ({} bytes), names: {}",
                    c.len(),
                    names.join(" ")
                );
            }
            None => eprintln!(
                "saved source: user={username}; NO cookie (REDDIT_COOKIE/--reddit-cookie unset) \
                 — Reddit will almost certainly 403"
            ),
        }
    }
    let reddit = RedditClient::for_user(username, cookie)?;
    sync::run(&reddit, &pinboard, &cfg)
        .await
        .map_err(|e| handle_reddit_err(e, hook))?;
    Ok(())
}

async fn run_cleanup(args: CleanupArgs) -> Result<()> {
    let pinboard_token = resolve_pinboard_token(args.pinboard_token.clone()).context(
        "missing Pinboard token (set --pinboard-token, PINBOARD_TOKEN, or ~/.pinboardrc)",
    )?;
    let pinboard = PinboardClient::new(pinboard_token, false)?;

    let opts = cleanup::CleanupOpts {
        dry_run: args.dry_run,
        verbose: args.verbose,
        mark_nsfw: !args.no_nsfw,
        fix_titles: !args.no_titles,
        base_tag: args.base_tag.clone(),
        subreddit_tag_prefix: args.subreddit_tag_prefix.clone(),
    };

    // Reddit (for /api/info) is only needed when marking NSFW or fixing titles.
    let reddit = if opts.mark_nsfw || opts.fix_titles {
        let cookie = resolve_secret(args.reddit_cookie.clone(), "REDDIT_COOKIE").context(
            "missing Reddit cookie (set --reddit-cookie or REDDIT_COOKIE, or pass --no-nsfw --no-titles)",
        )?;
        Some(RedditClient::for_info(Some(cookie))?)
    } else {
        None
    };

    cleanup::run(&pinboard, reddit.as_ref(), &opts).await
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

/// Resolve a secret from, in order: the CLI flag, `$VAR`, then `$VAR_FILE`
/// (a path to a file whose trimmed contents are the value).
fn resolve_secret(flag: Option<String>, var: &str) -> Option<String> {
    let env_val = std::env::var(var).ok();
    let file_contents = std::env::var(format!("{var}_FILE"))
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok());
    choose_secret(flag, env_val, file_contents)
}

/// Pick a secret by precedence: a non-empty flag, then a non-empty `$VAR`, then
/// the trimmed non-empty contents of `$VAR_FILE`. Pure, so it is unit-tested
/// without touching the environment or filesystem.
fn choose_secret(
    flag: Option<String>,
    env_val: Option<String>,
    file_contents: Option<String>,
) -> Option<String> {
    if let Some(v) = flag {
        if !v.is_empty() {
            return Some(v);
        }
    }
    if let Some(v) = env_val {
        if !v.is_empty() {
            return Some(v);
        }
    }
    let trimmed = file_contents?.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
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

fn resolve_pinboard_token(flag: Option<String>) -> Option<String> {
    resolve_secret(flag, "PINBOARD_TOKEN").or_else(read_pinboardrc)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choose_secret_prefers_flag_then_env_then_file() {
        assert_eq!(
            choose_secret(Some("flag".into()), Some("env".into()), Some("file".into())),
            Some("flag".into())
        );
        assert_eq!(
            choose_secret(None, Some("env".into()), Some("file".into())),
            Some("env".into())
        );
        assert_eq!(
            choose_secret(None, None, Some("file".into())),
            Some("file".into())
        );
        assert_eq!(choose_secret(None, None, None), None);
    }

    #[test]
    fn choose_secret_skips_empty_values_and_trims_file() {
        // Empty flag and env fall through to the file, whose contents are trimmed.
        assert_eq!(
            choose_secret(
                Some(String::new()),
                Some(String::new()),
                Some("  tok\n".into())
            ),
            Some("tok".into())
        );
        // A whitespace-only file yields nothing.
        assert_eq!(choose_secret(None, None, Some("   \n".into())), None);
    }

    #[test]
    fn parse_pinboardrc_reads_api_token() {
        let ini = "[authentication]\napi_token = user:ABC123\n";
        assert_eq!(parse_pinboardrc(ini), Some("user:ABC123".into()));
        // No spaces around '=' also works.
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
        let long = "word ".repeat(100); // ~500 chars once collapsed
        let p = preview(&long);
        assert!(p.ends_with('…'));
        assert_eq!(p.chars().count(), 161); // 160 chars + the ellipsis
    }
}
