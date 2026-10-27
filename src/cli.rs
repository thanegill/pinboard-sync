//! The command line: clap's `Cli` and every subcommand's arguments, and the conversions
//! from them into the overrides the job builders and cleanup passes resolve settings from.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueHint};
use clap_complete::Shell;

use crate::config::SyncLimit;
use crate::jobs::SyncOverrides;
use crate::reddit::cleanup::RedditCleanupOverrides;

#[derive(Parser)]
#[command(name = "pinboard-sync", version, about, arg_required_else_help = true)]
pub struct Cli {
    /// Path to the TOML config file (env PINBOARD_SYNC_CONFIG).
    #[arg(long, global = true)]
    pub config: Option<String>,
    /// Increase log verbosity: `-v` for debug, `-vv` for trace, `-vvv` to also include
    /// dependency logs. Overridden by `RUST_LOG`.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
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
    /// Config helpers: print an example config, or check one.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// Print a fully-commented example config (every key with its default).
    Example,
    /// Validate a config file as a run would, without using secrets or contacting any
    /// service.
    Check {
        /// The TOML config file to validate.
        #[arg(value_name = "FILE", value_hint = ValueHint::FilePath)]
        file: PathBuf,
    },
}

#[derive(Args)]
pub struct BackupCmd {
    /// Back up the Pinboard account and every configured account across every source.
    #[arg(long)]
    pub all: bool,
    /// Show what would be written without touching the filesystem.
    #[arg(long)]
    pub dry_run: bool,
    #[command(flatten)]
    pub out: OutFlag,
    #[command(subcommand)]
    pub target: Option<BackupTarget>,
}

/// The snapshot directory, flattened into `backup` *and* every target so it is accepted
/// on either side of the subcommand (as `--dry-run` already is on `sync`).
#[derive(Args, Clone, Default)]
pub struct OutFlag {
    /// Directory to write the snapshot into (overrides `[backup].directory`).
    #[arg(long, value_name = "DIR")]
    pub out: Option<String>,
}

#[derive(Subcommand)]
pub enum BackupTarget {
    /// Back up saved Reddit posts and comments.
    Reddit(RedditBackupArgs),
    /// Back up starred GitHub repositories.
    Github(GitHubBackupArgs),
    /// Back up favorited HackerNews stories and comments.
    Hackernews(HackerNewsBackupArgs),
    /// Back up the Pinboard account itself (raw `posts/all`, verbatim).
    Pinboard(PinboardBackupArgs),
}

// Backing up a source never contacts Pinboard, so none of the source targets take a
// Pinboard token — the one place `backup` deliberately diverges from `sync`/`cleanup`.

#[derive(Args, Clone)]
pub struct RedditBackupArgs {
    /// Account name to select from the config (default: the first reddit account).
    pub account: Option<String>,
    /// Back up every reddit account in the config.
    #[arg(long)]
    pub all: bool,
    /// Reddit username whose saved items to back up (env REDDIT_USERNAME, or *_FILE).
    #[arg(long)]
    pub reddit_username: Option<String>,
    /// Reddit session cookie, `reddit_session=…` or its bare value (env REDDIT_COOKIE,
    /// or *_FILE).
    #[arg(long)]
    pub reddit_cookie: Option<String>,
    /// Shell command run when the Reddit cookie needs refreshing (a 401/403, or a
    /// redirect to the login page).
    #[arg(long, env = "PINBOARD_SYNC_ON_AUTH_FAILURE")]
    pub on_auth_failure: Option<String>,
    #[command(flatten)]
    pub out: OutFlag,
    /// Show what would be written without touching the filesystem.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Clone)]
pub struct GitHubBackupArgs {
    /// Account name to select from the config (default: the first github account).
    pub account: Option<String>,
    /// Back up every github account in the config.
    #[arg(long)]
    pub all: bool,
    /// GitHub personal access token (env GITHUB_TOKEN, or *_FILE).
    #[arg(long)]
    pub github_token: Option<String>,
    /// Shell command run when the GitHub token needs refreshing (a 401).
    #[arg(long, env = "PINBOARD_SYNC_ON_AUTH_FAILURE")]
    pub on_auth_failure: Option<String>,
    #[command(flatten)]
    pub out: OutFlag,
    /// Show what would be written without touching the filesystem.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Clone)]
pub struct HackerNewsBackupArgs {
    /// Account name to select from the config (default: the first hackernews account).
    pub account: Option<String>,
    /// Back up every hackernews account in the config.
    #[arg(long)]
    pub all: bool,
    /// HackerNews username whose favorites to back up (env HN_USERNAME, or *_FILE).
    #[arg(long, alias = "hn-username")]
    pub username: Option<String>,
    #[command(flatten)]
    pub out: OutFlag,
    /// Show what would be written without touching the filesystem.
    #[arg(long)]
    pub dry_run: bool,
}

/// Pinboard has no `account`/`--all`: `[pinboard]` is a single destination table, not an
/// array of accounts like the sources.
#[derive(Args, Clone)]
pub struct PinboardBackupArgs {
    /// Pinboard API token, "user:TOKEN" (env PINBOARD_TOKEN, or *_FILE).
    #[arg(long)]
    pub pinboard_token: Option<String>,
    #[command(flatten)]
    pub out: OutFlag,
    /// Show what would be written without touching the filesystem.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct SyncCmd {
    /// Run every configured account across every source (requires --config).
    #[arg(long)]
    pub all: bool,
    /// Show what would be written without touching Pinboard (with --all).
    #[arg(long)]
    pub dry_run: bool,
    /// Shell command run when a source's credential needs refreshing (a 401/403).
    ///
    /// The `PINBOARD_SYNC_ON_AUTH_FAILURE` env var backs the per-source flag, not
    /// this one, so an explicit per-source `--on-auth-failure` outranks the env
    /// var (`with_top_level_hook` prefers a present top-level value). The `--all`
    /// path, which has no per-source flag, reads that env var itself via
    /// `on_auth_failure_from_env`.
    #[arg(long)]
    pub on_auth_failure: Option<String>,
    #[command(subcommand)]
    pub source: Option<SyncSource>,
}

#[derive(Subcommand)]
pub enum SyncSource {
    /// Sync saved Reddit posts and comments.
    Reddit(RedditSyncArgs),
    /// Sync starred GitHub repositories.
    Github(GitHubSyncArgs),
    /// Sync favorited HackerNews stories and comments.
    Hackernews(HackerNewsSyncArgs),
}

#[derive(Args, Clone)]
pub struct RedditSyncArgs {
    /// Account name to select from the config (default: the first reddit account).
    pub account: Option<String>,
    /// Run every reddit account in the config.
    #[arg(long)]
    pub all: bool,
    /// Reddit username whose saved items to sync (env REDDIT_USERNAME, or *_FILE).
    #[arg(long)]
    pub reddit_username: Option<String>,
    /// Reddit session cookie, `reddit_session=…` or its bare value (env REDDIT_COOKIE,
    /// or *_FILE).
    #[arg(long)]
    pub reddit_cookie: Option<String>,
    #[command(flatten)]
    pub flags: SyncFlags,
    /// Shell command run when the Reddit cookie needs refreshing (a 401/403, or a
    /// redirect to the login page).
    #[arg(long, env = "PINBOARD_SYNC_ON_AUTH_FAILURE")]
    pub on_auth_failure: Option<String>,
    /// Fetch and print what would be posted, without writing to Pinboard.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Clone)]
pub struct GitHubSyncArgs {
    /// Account name to select from the config (default: the first github account).
    pub account: Option<String>,
    /// Run every github account in the config.
    #[arg(long)]
    pub all: bool,
    /// GitHub personal access token (env GITHUB_TOKEN, or *_FILE).
    #[arg(long)]
    pub github_token: Option<String>,
    #[command(flatten)]
    pub flags: SyncFlags,
    /// Shell command run when the GitHub token needs refreshing (a 401).
    #[arg(long, env = "PINBOARD_SYNC_ON_AUTH_FAILURE")]
    pub on_auth_failure: Option<String>,
    /// Fetch and print what would be posted, without writing to Pinboard.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Clone)]
pub struct HackerNewsSyncArgs {
    /// Account name to select from the config (default: the first hackernews account).
    pub account: Option<String>,
    /// Run every hackernews account in the config.
    #[arg(long)]
    pub all: bool,
    /// HackerNews username whose favorites to sync (env HN_USERNAME, or *_FILE).
    #[arg(long, alias = "hn-username")]
    pub username: Option<String>,
    #[command(flatten)]
    pub flags: SyncFlags,
    /// Fetch and print what would be posted, without writing to Pinboard.
    #[arg(long)]
    pub dry_run: bool,
}

/// The Pinboard and write/dating flags every `sync <source>` takes, flattened into each so
/// the declarations live in one place.
#[derive(Args, Clone, Default)]
pub struct SyncFlags {
    /// Pinboard API token, "user:TOKEN" (env PINBOARD_TOKEN, or *_FILE).
    #[arg(long)]
    pub pinboard_token: Option<String>,
    /// `auto` (the default: no cap; stop paging once caught up),
    /// `all` (no cap; read every page), or N (write at most N; read every page).
    /// Overrides config.
    #[arg(long)]
    pub limit: Option<SyncLimit>,
    /// Only backdate posts newer than N days; older use "now" (overrides config).
    #[arg(long, alias = "post-date-max-age-days")]
    pub max_age_days: Option<u64>,
    /// Mark new bookmarks to-read: `--toread` or `--toread=false` (overrides config).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub toread: Option<bool>,
    /// Create bookmarks public: `--public` or `--public=false` (overrides config).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub public: Option<bool>,
    /// Date bookmarks by the source post date: `--use-post-date[=BOOL]` (overrides config).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub use_post_date: Option<bool>,
}

#[derive(Args)]
pub struct CleanupCmd {
    /// Run cleanup for every configured account across every cleanup-capable source.
    #[arg(long)]
    pub all: bool,
    /// Show what would change without writing to Pinboard (with --all).
    #[arg(long)]
    pub dry_run: bool,
    #[command(flatten)]
    pub dates: DateFlags,
    #[command(subcommand)]
    pub source: Option<CleanupSource>,
}

#[derive(Subcommand)]
pub enum CleanupSource {
    /// Normalize existing reddit bookmarks (URLs, tags, NSFW, titles).
    Reddit(RedditCleanupArgs),
    /// Canonicalize existing GitHub repo bookmark URLs.
    Github(GitHubCleanupArgs),
    /// Normalize existing HackerNews bookmarks (rewrite item URLs to articles).
    Hackernews(HackerNewsCleanupArgs),
}

/// The date-setting flags shared by every cleanup command (and `cleanup --all`), flattened
/// into each so the declarations live in one place. They are the top tier of the date
/// settings (flag → account → per-source default → `[pinboard]` global), and all-`None`
/// means "no flag given", so resolution falls through to the other tiers.
#[derive(Args, Clone, Copy, Default)]
pub struct DateFlags {
    /// Only backdate posts newer than N days; older use "now" (overrides config).
    #[arg(long, alias = "post-date-max-age-days")]
    pub max_age_days: Option<u64>,
    /// Date bookmarks by the source post date: `--use-post-date[=BOOL]` (overrides config).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub use_post_date: Option<bool>,
    /// Re-date too-old posts to now: `--stale-to-now[=BOOL]` (overrides config).
    #[arg(
        long,
        alias = "cleanup-stale-to-now",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true"
    )]
    pub stale_to_now: Option<bool>,
}

impl DateFlags {
    /// Let a date flag placed before the source subcommand (on `CleanupCmd`) take
    /// effect, falling back to this per-source value. Mirrors sync's
    /// `SyncOverrides::with_top_level_hook`.
    pub fn with_top_level(self, top_overrides: &DateFlags) -> DateFlags {
        DateFlags {
            use_post_date: top_overrides.use_post_date.or(self.use_post_date),
            max_age_days: top_overrides.max_age_days.or(self.max_age_days),
            stale_to_now: top_overrides.stale_to_now.or(self.stale_to_now),
        }
    }
}

#[derive(Args, Clone)]
pub struct GitHubCleanupArgs {
    /// Account name whose token/tags to use (default: the first github account).
    pub account: Option<String>,
    /// GitHub personal access token (env GITHUB_TOKEN, or *_FILE).
    #[arg(long)]
    pub github_token: Option<String>,
    /// Pinboard API token (env PINBOARD_TOKEN, or *_FILE).
    #[arg(long)]
    pub pinboard_token: Option<String>,
    #[command(flatten)]
    pub dates: DateFlags,
    /// Show what would change without writing to Pinboard.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Clone)]
pub struct HackerNewsCleanupArgs {
    /// Account name whose tag config to use (default: the first hackernews account).
    pub account: Option<String>,
    /// Pinboard API token (env PINBOARD_TOKEN, or *_FILE).
    #[arg(long)]
    pub pinboard_token: Option<String>,
    /// Also link article bookmarks tagged with the link tag (default `find-hn`) to
    /// their HN discussion, via an Algolia URL lookup per tagged bookmark.
    #[arg(long)]
    pub link_discussions: bool,
    /// Override the marker tag used by --link-discussions (config: `tag_link`).
    #[arg(long, alias = "tag-link")]
    pub link_tag: Option<String>,
    #[command(flatten)]
    pub dates: DateFlags,
    /// Show what would change without writing to Pinboard.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Clone)]
pub struct RedditCleanupArgs {
    /// Account name whose cookie + domain/tags to use (default: the first reddit account).
    pub account: Option<String>,
    /// Reddit session cookie, `reddit_session=…` or its bare value (env REDDIT_COOKIE,
    /// or *_FILE). Needed for the
    /// `/api/info` lookups; not required with --no-nsfw --no-titles.
    #[arg(long)]
    pub reddit_cookie: Option<String>,
    /// Pinboard API token (env PINBOARD_TOKEN, or *_FILE).
    #[arg(long)]
    pub pinboard_token: Option<String>,
    /// Skip NSFW tagging (no Reddit /api/info call for over_18).
    #[arg(long)]
    pub no_nsfw: bool,
    /// Skip replacing generic placeholder titles.
    #[arg(long)]
    pub no_titles: bool,
    #[command(flatten)]
    pub dates: DateFlags,
    /// Show what would change without writing to Pinboard.
    #[arg(long)]
    pub dry_run: bool,
}

/// The date-setting flags a sync invocation supplies. `stale_to_now` is cleanup-only, so it
/// is always `None` here.
impl From<&SyncFlags> for DateFlags {
    fn from(flags: &SyncFlags) -> Self {
        DateFlags {
            use_post_date: flags.use_post_date,
            max_age_days: flags.max_age_days,
            stale_to_now: None,
        }
    }
}

// `backup` reuses the sync job builders (and so the whole secret ladder), but supplies
// only the credentials and the auth hook: every other override affects a *write* to
// Pinboard, which backup never performs.

impl From<RedditBackupArgs> for SyncOverrides {
    fn from(args: RedditBackupArgs) -> Self {
        SyncOverrides {
            reddit_username: args.reddit_username,
            reddit_cookie: args.reddit_cookie,
            on_auth_failure: args.on_auth_failure,
            ..SyncOverrides::default()
        }
    }
}

impl From<GitHubBackupArgs> for SyncOverrides {
    fn from(args: GitHubBackupArgs) -> Self {
        SyncOverrides {
            github_token: args.github_token,
            on_auth_failure: args.on_auth_failure,
            ..SyncOverrides::default()
        }
    }
}

impl From<HackerNewsBackupArgs> for SyncOverrides {
    fn from(args: HackerNewsBackupArgs) -> Self {
        SyncOverrides {
            hackernews_username: args.username,
            ..SyncOverrides::default()
        }
    }
}

/// The secret/operational overrides a single-source `sync reddit` supplies.
impl From<RedditSyncArgs> for SyncOverrides {
    fn from(args: RedditSyncArgs) -> Self {
        SyncOverrides {
            reddit_username: args.reddit_username,
            reddit_cookie: args.reddit_cookie,
            on_auth_failure: args.on_auth_failure,
            flags: args.flags,
            dry_run: args.dry_run,
            ..SyncOverrides::default()
        }
    }
}

/// The secret/operational overrides a single-source `sync github` supplies.
impl From<GitHubSyncArgs> for SyncOverrides {
    fn from(args: GitHubSyncArgs) -> Self {
        SyncOverrides {
            github_token: args.github_token,
            on_auth_failure: args.on_auth_failure,
            flags: args.flags,
            dry_run: args.dry_run,
            ..SyncOverrides::default()
        }
    }
}

/// The secret/operational overrides a single-source `sync hackernews` supplies.
impl From<HackerNewsSyncArgs> for SyncOverrides {
    fn from(args: HackerNewsSyncArgs) -> Self {
        SyncOverrides {
            hackernews_username: args.username,
            flags: args.flags,
            dry_run: args.dry_run,
            ..SyncOverrides::default()
        }
    }
}

impl From<&RedditCleanupArgs> for RedditCleanupOverrides {
    fn from(args: &RedditCleanupArgs) -> Self {
        RedditCleanupOverrides {
            cookie: args.reddit_cookie.clone(),
            mark_nsfw: !args.no_nsfw,
            fix_titles: !args.no_titles,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;

    #[test]
    fn limit_flag_takes_auto_all_or_a_count() {
        use clap::Parser;
        let limit = |value: &str| {
            let argv = ["pinboard-sync", "sync", "hackernews", "--limit", value];
            match Cli::try_parse_from(argv).map(|cli| cli.command) {
                Ok(Command::Sync(SyncCmd {
                    source: Some(SyncSource::Hackernews(args)),
                    ..
                })) => Ok(args.flags.limit),
                Ok(_) => panic!("expected `sync hackernews`"),
                Err(err) => Err(err.to_string()),
            }
        };
        let max = |count| Some(SyncLimit::Max(NonZeroUsize::new(count).unwrap()));

        assert_eq!(limit("auto"), Ok(Some(SyncLimit::Auto)));
        assert_eq!(limit("all"), Ok(Some(SyncLimit::All)));
        assert_eq!(limit("25"), Ok(max(25)));
        for bad in ["0", "everything"] {
            let err = limit(bad).unwrap_err();
            assert!(err.contains("`limit` must be `auto` (no cap;"), "{err}");
        }
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
        assert_eq!(parse_reddit_sync(&[]).flags.public, None);
        // Bare `--public` → Some(true) via default_missing_value.
        assert_eq!(parse_reddit_sync(&["--public"]).flags.public, Some(true));
        // `--public=false` → Some(false): the explicit force-off a bare flag can't express.
        assert_eq!(
            parse_reddit_sync(&["--public=false"]).flags.public,
            Some(false)
        );
        // Same shape for the other booleans.
        assert_eq!(parse_reddit_sync(&["--toread"]).flags.toread, Some(true));
        assert_eq!(
            parse_reddit_sync(&["--use-post-date=false"])
                .flags
                .use_post_date,
            Some(false)
        );
    }

    #[test]
    fn bare_bool_flag_does_not_swallow_the_account_positional() {
        // `require_equals` keeps clap from consuming the account as the flag value.
        let args = parse_reddit_sync(&["--public", "alice"]);
        assert_eq!(args.account.as_deref(), Some("alice"));
        assert_eq!(args.flags.public, Some(true));
        // The `=` form still forces the flag off while leaving the positional alone.
        let args = parse_reddit_sync(&["--public=false", "alice"]);
        assert_eq!(args.account.as_deref(), Some("alice"));
        assert_eq!(args.flags.public, Some(false));
    }

    /// A flag also answers to the spelling of the config key it overrides.
    #[test]
    fn each_flag_takes_its_config_spelling_too() {
        use clap::Parser;
        let command = |argv: &[&str]| {
            Cli::try_parse_from([&["pinboard-sync"][..], argv].concat())
                .expect("parses")
                .command
        };

        let Command::Sync(SyncCmd {
            source: Some(SyncSource::Hackernews(args)),
            ..
        }) = command(&[
            "sync",
            "hackernews",
            "--post-date-max-age-days",
            "5",
            "--hn-username",
            "u",
        ])
        else {
            panic!("expected `sync hackernews`");
        };
        assert_eq!(args.flags.max_age_days, Some(5));
        assert_eq!(args.username.as_deref(), Some("u"));

        let Command::Cleanup(CleanupCmd {
            source: Some(CleanupSource::Hackernews(args)),
            ..
        }) = command(&[
            "cleanup",
            "hackernews",
            "--post-date-max-age-days",
            "5",
            "--cleanup-stale-to-now",
            "--tag-link",
            "t",
        ])
        else {
            panic!("expected `cleanup hackernews`");
        };
        assert_eq!(args.dates.max_age_days, Some(5));
        assert_eq!(args.dates.stale_to_now, Some(true));
        assert_eq!(args.link_tag.as_deref(), Some("t"));

        let Command::Backup(BackupCmd {
            target: Some(BackupTarget::Hackernews(args)),
            ..
        }) = command(&["backup", "hackernews", "--hn-username", "u"])
        else {
            panic!("expected `backup hackernews`");
        };
        assert_eq!(args.username.as_deref(), Some("u"));
    }
}
