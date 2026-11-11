//! The command line: clap's types, and their conversions into the overrides settings
//! resolve from. Doc comments on clap types are the `--help` text.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;

use clap::error::ErrorKind;
use clap::parser::ValueSource;
use clap::{
    ArgAction, ArgMatches, Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueHint,
};
use clap_complete::Shell;

use crate::config::SyncLimit;
use crate::hackernews::cleanup::HackerNewsCleanupOverrides;
use crate::jobs::SyncOverrides;
use crate::reddit::cleanup::RedditCleanupOverrides;

#[derive(Parser)]
#[command(name = "pinboard-sync", version, about, arg_required_else_help = true)]
pub struct Cli {
    /// Path to the TOML config file (env `PINBOARD_SYNC_CONFIG`).
    #[arg(long, global = true)]
    pub config: Option<String>,
    /// Increase log verbosity: `-v` for debug, `-vv` for trace, `-vvv` to also include
    /// dependency logs. Overridden by `RUST_LOG`.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// The process's arguments through [`parse_with`], exiting with clap's usage error.
    pub fn parse_args() -> Self {
        parse_with(Self::command(), std::env::args_os()).unwrap_or_else(|error| error.exit())
    }
}

/// Parse `argv` against `command`, refusing a flag given at more than one level. `-v`
/// counts at every level instead, so `-v sync -vv` is `-vvv`.
pub fn parse_with<T: Into<OsString> + Clone>(
    command: clap::Command,
    argv: impl IntoIterator<Item = T>,
) -> Result<Cli, clap::Error> {
    let mut command = with_globals_at_every_level(command);
    let matches = command.try_get_matches_from_mut(argv)?;
    refuse_flags_given_twice(&mut command, &matches)?;
    let mut cli = Cli::from_arg_matches(&matches).map_err(|error| error.format(&mut command))?;
    let levels = levels_of(&matches);
    cli.config = levels
        .iter()
        .find_map(|level| level.get_one::<String>("config").cloned());
    cli.verbose = levels.iter().fold(0, |count, level| {
        count.saturating_add(level.get_count("verbose"))
    });
    Ok(cli)
}

/// `command` with its global args declared at every level, so each level's matches hold
/// only what was given there: clap copies a global to every level, hiding where it was
/// given and keeping only the deepest `-v` count.
fn with_globals_at_every_level(command: clap::Command) -> clap::Command {
    fn declare(command: clap::Command, args: &[clap::Arg]) -> clap::Command {
        args.iter()
            .fold(command, |command, arg| command.arg(arg.clone()))
            .mut_subcommands(|subcommand| declare(subcommand, args))
    }
    let globals: Vec<clap::Arg> = command
        .get_arguments()
        .filter(|arg| arg.is_global_set())
        .map(|arg| arg.clone().global(false))
        .collect();
    let command = globals.iter().fold(command, |command, arg| {
        command.mut_arg(arg.get_id().clone(), |arg| arg.global(false))
    });
    command.mut_subcommands(|subcommand| declare(subcommand, &globals))
}

/// The matches of each level the command line reached, top first.
fn levels_of(matches: &ArgMatches) -> Vec<&ArgMatches> {
    std::iter::successors(Some(matches), |level| {
        level.subcommand().map(|(_, next)| next)
    })
    .collect()
}

/// Refuse a flag given at two levels (`--dry-run` before and after a subcommand): neither
/// has a reason to win. The error is clap's, with the deepest subcommand's usage.
fn refuse_flags_given_twice(
    command: &mut clap::Command,
    matches: &ArgMatches,
) -> Result<(), clap::Error> {
    let names: Vec<&str> = levels_of(matches)
        .iter()
        .filter_map(|level| level.subcommand_name())
        .collect();
    let Some(message) = flag_given_twice(command, matches, &names) else {
        return Ok(());
    };
    #[expect(
        clippy::expect_used,
        reason = "`names` is the subcommand path clap matched"
    )]
    let deepest = names.iter().fold(command, |command, name| {
        command
            .find_subcommand_mut(name)
            .expect("clap matched this subcommand")
    });
    Err(deepest.error(ErrorKind::ArgumentConflict, message))
}

/// The error for the first long flag given at two levels along `names`, or `None`. An env
/// value or default is not a placement, so `PINBOARD_SYNC_ON_AUTH_FAILURE`, behind the
/// per-source flag, beside the top-level one is fine. `-v` adds up, and `--all` is two
/// flags (every source, or one source's accounts), a pair the run refuses itself.
fn flag_given_twice(
    command: &clap::Command,
    matches: &ArgMatches,
    names: &[&str],
) -> Option<String> {
    let mut first_level: HashMap<&clap::Id, usize> = HashMap::new();
    let mut level_command = command;
    for (depth, level) in levels_of(matches).into_iter().enumerate() {
        for arg in level_command.get_arguments() {
            let Some(long) = arg.get_long() else {
                continue;
            };
            let counted = matches!(arg.get_action(), ArgAction::Count);
            let on_command_line =
                level.value_source(arg.get_id().as_str()) == Some(ValueSource::CommandLine);
            if counted || long == "all" || !on_command_line {
                continue;
            }
            if let Some(&first) = first_level.get(arg.get_id()) {
                #[expect(
                    clippy::indexing_slicing,
                    reason = "a level above this one has a subcommand, which `names` holds"
                )]
                let subcommand = names[first];
                return Some(format!(
                    "'--{long}' was given both before and after '{subcommand}'; give it once"
                ));
            }
            first_level.insert(arg.get_id(), depth);
        }
        if let Some(name) = names.get(depth) {
            #[expect(
                clippy::expect_used,
                clippy::unwrap_in_result,
                reason = "`names` is the subcommand path clap matched"
            )]
            let next = level_command
                .find_subcommand(name)
                .expect("clap matched this subcommand");
            level_command = next;
        }
    }
    None
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

impl Command {
    /// Whether the command reads settings or secrets from the environment, where a `.env`
    /// puts them, so a `.env` it can't load must fail it. The others print what is built in
    /// or check a named file, and a stray `.env` above the build directory must not break
    /// the Nix package build, which runs `completions`, `man` and `config example`.
    pub const fn reads_environment(&self) -> bool {
        match self {
            Self::Sync(_) | Self::Cleanup(_) | Self::Doctor | Self::Backup(_) => true,
            Self::Completions { .. } | Self::Man | Self::Config { .. } => false,
        }
    }
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
/// on either side of the subcommand, though not both (as `--dry-run` is on `sync`).
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

// Backing up a source never contacts Pinboard, so no source target takes a Pinboard token.

#[derive(Args)]
pub struct RedditBackupArgs {
    /// Account name to select from the config (default: the first reddit account).
    pub account: Option<String>,
    /// Back up every reddit account in the config.
    #[arg(long, conflicts_with = "account")]
    pub all: bool,
    /// Reddit username whose saved items to back up (env `REDDIT_USERNAME`, or `*_FILE`).
    #[arg(long)]
    pub reddit_username: Option<String>,
    /// Reddit session cookie, `reddit_session=…` or its bare value (env `REDDIT_COOKIE`,
    /// or `*_FILE`).
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

#[derive(Args)]
pub struct GitHubBackupArgs {
    /// Account name to select from the config (default: the first github account).
    pub account: Option<String>,
    /// Back up every github account in the config.
    #[arg(long, conflicts_with = "account")]
    pub all: bool,
    /// GitHub personal access token (env `GITHUB_TOKEN`, or `*_FILE`).
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

#[derive(Args)]
pub struct HackerNewsBackupArgs {
    /// Account name to select from the config (default: the first hackernews account).
    pub account: Option<String>,
    /// Back up every hackernews account in the config.
    #[arg(long, conflicts_with = "account")]
    pub all: bool,
    /// HackerNews username whose favorites to back up (env `HN_USERNAME`, or `*_FILE`).
    #[arg(long = "hn-username", value_name = "HN_USERNAME")]
    pub username: Option<String>,
    #[command(flatten)]
    pub out: OutFlag,
    /// Show what would be written without touching the filesystem.
    #[arg(long)]
    pub dry_run: bool,
}

/// Pinboard has no `account`/`--all`: `[pinboard]` is a single destination table, not an
/// array of accounts like the sources.
#[derive(Args)]
pub struct PinboardBackupArgs {
    /// Pinboard API token, "user:TOKEN" (env `PINBOARD_TOKEN`, or `*_FILE`).
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
    /// Shell command run when a source's credential needs refreshing (a 401/403; env
    /// `PINBOARD_SYNC_ON_AUTH_FAILURE`).
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

#[derive(Args)]
pub struct RedditSyncArgs {
    /// Account name to select from the config (default: the first reddit account).
    pub account: Option<String>,
    /// Run every reddit account in the config.
    #[arg(long, conflicts_with = "account")]
    pub all: bool,
    /// Reddit username whose saved items to sync (env `REDDIT_USERNAME`, or `*_FILE`).
    #[arg(long)]
    pub reddit_username: Option<String>,
    /// Reddit session cookie, `reddit_session=…` or its bare value (env `REDDIT_COOKIE`,
    /// or `*_FILE`).
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

#[derive(Args)]
pub struct GitHubSyncArgs {
    /// Account name to select from the config (default: the first github account).
    pub account: Option<String>,
    /// Run every github account in the config.
    #[arg(long, conflicts_with = "account")]
    pub all: bool,
    /// GitHub personal access token (env `GITHUB_TOKEN`, or `*_FILE`).
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

#[derive(Args)]
pub struct HackerNewsSyncArgs {
    /// Account name to select from the config (default: the first hackernews account).
    pub account: Option<String>,
    /// Run every hackernews account in the config.
    #[arg(long, conflicts_with = "account")]
    pub all: bool,
    /// HackerNews username whose favorites to sync (env `HN_USERNAME`, or `*_FILE`).
    #[arg(long = "hn-username", value_name = "HN_USERNAME")]
    pub username: Option<String>,
    #[command(flatten)]
    pub flags: SyncFlags,
    /// Fetch and print what would be posted, without writing to Pinboard.
    #[arg(long)]
    pub dry_run: bool,
}

/// The Pinboard and write/dating flags every `sync <source>` takes, flattened into each so
/// the declarations live in one place.
#[derive(Args, Default)]
pub struct SyncFlags {
    /// Pinboard API token, "user:TOKEN" (env `PINBOARD_TOKEN`, or `*_FILE`).
    #[arg(long)]
    pub pinboard_token: Option<String>,
    /// `auto` (the default: no cap; stop paging once caught up),
    /// `all` (no cap; read every page), or N (write at most N; read every page).
    /// Overrides config.
    #[arg(long)]
    pub limit: Option<SyncLimit>,
    /// Only backdate posts newer than N days; older use "now" (overrides config).
    #[arg(long = "post-date-max-age-days", value_name = "POST_DATE_MAX_AGE_DAYS")]
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
/// settings (flag -> account -> per-source default -> `[pinboard]` global), and all-`None`
/// means "no flag given", so resolution falls through to the other tiers.
#[derive(Args, Clone, Copy, Default)]
pub struct DateFlags {
    /// Only backdate posts newer than N days; older use "now" (overrides config).
    #[arg(long = "post-date-max-age-days", value_name = "POST_DATE_MAX_AGE_DAYS")]
    pub max_age_days: Option<u64>,
    /// Date bookmarks by the source post date: `--use-post-date[=BOOL]` (overrides config).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub use_post_date: Option<bool>,
    /// Re-date too-old posts to now: `--cleanup-stale-to-now[=BOOL]` (overrides config).
    #[arg(
        long = "cleanup-stale-to-now",
        value_name = "CLEANUP_STALE_TO_NOW",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true"
    )]
    pub stale_to_now: Option<bool>,
}

impl DateFlags {
    /// Each date flag from whichever side of the source subcommand gave it: before
    /// (`top_overrides`) or after (`self`). The command line refuses both.
    #[must_use]
    pub fn with_top_level(self, top_overrides: &Self) -> Self {
        Self {
            use_post_date: top_overrides.use_post_date.or(self.use_post_date),
            max_age_days: top_overrides.max_age_days.or(self.max_age_days),
            stale_to_now: top_overrides.stale_to_now.or(self.stale_to_now),
        }
    }
}

#[derive(Args)]
pub struct GitHubCleanupArgs {
    /// Account name whose token/tags to use (default: the first github account).
    pub account: Option<String>,
    /// GitHub personal access token (env `GITHUB_TOKEN`, or `*_FILE`).
    #[arg(long)]
    pub github_token: Option<String>,
    /// Pinboard API token (env `PINBOARD_TOKEN`, or `*_FILE`).
    #[arg(long)]
    pub pinboard_token: Option<String>,
    #[command(flatten)]
    pub dates: DateFlags,
    /// Show what would change without writing to Pinboard.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct HackerNewsCleanupArgs {
    /// Account name whose tag config to use (default: the first hackernews account).
    pub account: Option<String>,
    /// Pinboard API token (env `PINBOARD_TOKEN`, or `*_FILE`).
    #[arg(long)]
    pub pinboard_token: Option<String>,
    /// Also link article bookmarks tagged with the link tag (default `find-hn`) to
    /// their HN discussion, via an Algolia URL lookup per tagged bookmark.
    #[arg(long)]
    pub link_discussions: bool,
    /// Override the marker tag used by --link-discussions (config: `tag_link`).
    #[arg(long = "tag-link", value_name = "TAG_LINK")]
    pub link_tag: Option<String>,
    #[command(flatten)]
    pub dates: DateFlags,
    /// Show what would change without writing to Pinboard.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct RedditCleanupArgs {
    /// Account name whose cookie + domain/tags to use (default: the first reddit account).
    pub account: Option<String>,
    /// Reddit session cookie, `reddit_session=…` or its bare value (env `REDDIT_COOKIE`,
    /// or `*_FILE`). Needed for the `/api/info` lookups; not required with --no-nsfw
    /// --no-titles.
    #[arg(long)]
    pub reddit_cookie: Option<String>,
    /// Pinboard API token (env `PINBOARD_TOKEN`, or `*_FILE`).
    #[arg(long)]
    pub pinboard_token: Option<String>,
    /// Skip NSFW tagging (no Reddit `/api/info` call for `over_18`).
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

/// A sync's date flags; `stale_to_now` is cleanup-only.
impl From<&SyncFlags> for DateFlags {
    fn from(flags: &SyncFlags) -> Self {
        Self {
            use_post_date: flags.use_post_date,
            max_age_days: flags.max_age_days,
            stale_to_now: None,
        }
    }
}

// `backup` reuses the sync job builders, supplying only credentials and the hook: every
// other override is for a write, which backup never makes.

impl From<RedditBackupArgs> for SyncOverrides {
    fn from(args: RedditBackupArgs) -> Self {
        Self {
            reddit_username: args.reddit_username,
            reddit_cookie: args.reddit_cookie,
            on_auth_failure: args.on_auth_failure,
            ..Self::default()
        }
    }
}

impl From<GitHubBackupArgs> for SyncOverrides {
    fn from(args: GitHubBackupArgs) -> Self {
        Self {
            github_token: args.github_token,
            on_auth_failure: args.on_auth_failure,
            ..Self::default()
        }
    }
}

impl From<HackerNewsBackupArgs> for SyncOverrides {
    fn from(args: HackerNewsBackupArgs) -> Self {
        Self {
            hackernews_username: args.username,
            ..Self::default()
        }
    }
}

/// The secret/operational overrides a single-source `sync reddit` supplies.
impl From<RedditSyncArgs> for SyncOverrides {
    fn from(args: RedditSyncArgs) -> Self {
        Self {
            reddit_username: args.reddit_username,
            reddit_cookie: args.reddit_cookie,
            on_auth_failure: args.on_auth_failure,
            flags: args.flags,
            mode: args.dry_run.into(),
            ..Self::default()
        }
    }
}

/// The secret/operational overrides a single-source `sync github` supplies.
impl From<GitHubSyncArgs> for SyncOverrides {
    fn from(args: GitHubSyncArgs) -> Self {
        Self {
            github_token: args.github_token,
            on_auth_failure: args.on_auth_failure,
            flags: args.flags,
            mode: args.dry_run.into(),
            ..Self::default()
        }
    }
}

/// The secret/operational overrides a single-source `sync hackernews` supplies.
impl From<HackerNewsSyncArgs> for SyncOverrides {
    fn from(args: HackerNewsSyncArgs) -> Self {
        Self {
            hackernews_username: args.username,
            flags: args.flags,
            mode: args.dry_run.into(),
            ..Self::default()
        }
    }
}

impl From<HackerNewsCleanupArgs> for HackerNewsCleanupOverrides {
    fn from(args: HackerNewsCleanupArgs) -> Self {
        Self {
            link_discussions: args.link_discussions,
            link_tag: args.link_tag,
        }
    }
}

impl From<RedditCleanupArgs> for RedditCleanupOverrides {
    fn from(args: RedditCleanupArgs) -> Self {
        Self {
            cookie: args.reddit_cookie,
            mark_nsfw: !args.no_nsfw,
            fix_titles: !args.no_titles,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;

    /// Each flag a command and its subcommand both take, given before the subcommand and
    /// after it, as `(before, subcommand, after)`.
    const ON_BOTH_SIDES: &[(&[&str], &str, &[&str])] = &[
        (&["sync", "--dry-run"], "reddit", &["--dry-run"]),
        (
            &["sync", "--on-auth-failure", "a"],
            "github",
            &["--on-auth-failure", "b"],
        ),
        (&["cleanup", "--dry-run"], "github", &["--dry-run"]),
        (
            &["cleanup", "--post-date-max-age-days", "3"],
            "reddit",
            &["--post-date-max-age-days", "4"],
        ),
        (
            &["cleanup", "--use-post-date"],
            "hackernews",
            &["--use-post-date=false"],
        ),
        (
            &["cleanup", "--cleanup-stale-to-now"],
            "github",
            &["--cleanup-stale-to-now"],
        ),
        (&["backup", "--dry-run"], "reddit", &["--dry-run"]),
        (&["backup", "--out", "/a"], "pinboard", &["--out", "/b"]),
    ];

    /// The long name of the flag `arg` gives, as the refusal names it.
    fn flag_of(arg: &str) -> &str {
        arg.split('=').next().unwrap()
    }

    #[test]
    fn a_flag_given_both_before_and_after_its_subcommand_is_refused() {
        for (before, subcommand, after) in ON_BOTH_SIDES {
            let argv: Vec<&str> = ["pinboard-sync"]
                .iter()
                .chain(*before)
                .chain([subcommand])
                .chain(*after)
                .copied()
                .collect();
            let Err(error) = crate::test_support::parse_cli(&argv) else {
                panic!("{argv:?} parsed");
            };
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{argv:?}"
            );
            let message = error.to_string();
            assert!(
                message.contains(&format!(
                    "'{}' was given both before and after '{subcommand}'",
                    flag_of(after[0])
                )),
                "{argv:?}: {message}"
            );
        }
    }

    #[test]
    fn a_flag_given_on_one_side_of_its_subcommand_parses() {
        for (before, subcommand, after) in ON_BOTH_SIDES {
            let top: Vec<&str> = ["pinboard-sync"]
                .iter()
                .chain(*before)
                .chain([subcommand])
                .copied()
                .collect();
            let nested: Vec<&str> = ["pinboard-sync", before[0], subcommand]
                .iter()
                .chain(*after)
                .copied()
                .collect();
            for argv in [top, nested] {
                assert!(crate::test_support::parse_cli(&argv).is_ok(), "{argv:?}");
            }
        }
    }

    #[test]
    fn config_given_at_two_levels_is_refused() {
        for argv in [
            &["--config", "a", "sync", "--config", "b", "reddit"][..],
            &["--config", "a", "sync", "reddit", "--config", "b"],
            &["--config=a", "sync", "reddit", "--config=b"],
            &["sync", "--config", "a", "github", "--config=b"],
            &["cleanup", "--config=a", "hackernews", "--config", "b"],
            &[
                "--config",
                "a",
                "config",
                "check",
                "--config",
                "b",
                "file.toml",
            ],
        ] {
            let argv: Vec<&str> = std::iter::once("pinboard-sync")
                .chain(argv.iter().copied())
                .collect();
            let Err(error) = crate::test_support::parse_cli(&argv) else {
                panic!("{argv:?} parsed");
            };
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{argv:?}"
            );
            assert!(
                error
                    .to_string()
                    .contains("'--config' was given both before and after"),
                "{argv:?}: {error}"
            );
        }
    }

    /// `--config` given once is taken from whichever level gave it, and a `--config` that is
    /// a value (of another flag, or a positional after `--`) is not a second one.
    #[test]
    fn config_given_once_parses_at_any_level() {
        for (argv, config) in [
            (&["--config", "a", "sync", "reddit"][..], Some("a")),
            (&["sync", "--config=a", "reddit"], Some("a")),
            (&["sync", "reddit", "--config", "a"], Some("a")),
            (&["sync", "reddit"], None),
            (
                &[
                    "--config",
                    "a",
                    "sync",
                    "reddit",
                    "--on-auth-failure=--config",
                ],
                Some("a"),
            ),
            (
                &["--config", "a", "config", "check", "--", "--config"],
                Some("a"),
            ),
        ] {
            let argv: Vec<&str> = std::iter::once("pinboard-sync")
                .chain(argv.iter().copied())
                .collect();
            let cli =
                crate::test_support::parse_cli(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert_eq!(cli.config.as_deref(), config, "{argv:?}");
        }
    }

    /// `-v` counts wherever it is given, clusters included: clap alone keeps only the
    /// deepest level's count.
    #[test]
    fn verbose_adds_up_across_levels() {
        for (argv, count) in [
            (&["-v", "sync", "-vv", "reddit", "-v"][..], 4),
            (&["-vv", "sync", "reddit"], 2),
            (&["sync", "reddit", "-vvv"], 3),
            (&["--verbose", "cleanup", "-v", "github", "--verbose"], 3),
            (&["sync", "reddit"], 0),
        ] {
            let argv: Vec<&str> = std::iter::once("pinboard-sync")
                .chain(argv.iter().copied())
                .collect();
            let cli =
                crate::test_support::parse_cli(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert_eq!(cli.verbose, count, "{argv:?}");
        }
    }

    /// A refusal names the subcommand right after the first `--config`, and shows the usage
    /// of the deepest one, as clap's own errors do.
    #[test]
    fn a_refusal_names_where_the_flag_was_first_given() {
        let argv = [
            "pinboard-sync",
            "--config",
            "a",
            "sync",
            "reddit",
            "--config",
            "b",
        ];
        let Err(error) = crate::test_support::parse_cli(argv) else {
            panic!("{argv:?} parsed");
        };
        let error = error.to_string();
        assert!(
            error.contains("'--config' was given both before and after 'sync'"),
            "{error}"
        );
        assert!(
            error.contains("Usage: pinboard-sync sync reddit [OPTIONS] [ACCOUNT]"),
            "{error}"
        );
    }

    /// `--all` before a source and after it are different flags, every source's accounts
    /// and that source's; the run refuses the pair, so the parse lets it through.
    #[test]
    fn all_on_both_sides_is_left_to_the_run() {
        for argv in [
            ["pinboard-sync", "sync", "--all", "reddit", "--all"],
            ["pinboard-sync", "backup", "--all", "github", "--all"],
        ] {
            assert!(crate::test_support::parse_cli(argv).is_ok(), "{argv:?}");
        }
    }

    /// The environment backs the per-source `--on-auth-failure`, and a value from it is
    /// not a placement: the top-level flag alongside it still parses, and wins.
    #[test]
    fn an_env_backed_value_is_not_a_second_placement() {
        // A variable cargo sets for every test process, so the test needn't set one.
        let command = Cli::command().mut_subcommand("sync", |sync| {
            sync.mut_subcommand("reddit", |reddit| {
                reddit.mut_arg("on_auth_failure", |arg| arg.env("CARGO_PKG_NAME"))
            })
        });
        let parsed = parse_with(
            command,
            [
                "pinboard-sync",
                "sync",
                "--on-auth-failure",
                "top",
                "reddit",
            ],
        );

        let Command::Sync(cmd) = parsed.expect("parses").command else {
            panic!("expected `sync`");
        };
        let Some(SyncSource::Reddit(args)) = cmd.source else {
            panic!("expected `sync reddit`");
        };
        assert_eq!(
            args.on_auth_failure.as_deref(),
            Some(env!("CARGO_PKG_NAME"))
        );
        let overrides = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure);
        assert_eq!(overrides.on_auth_failure.as_deref(), Some("top"));
    }

    #[test]
    fn limit_flag_takes_auto_all_or_a_count() {
        let limit = |value: &str| {
            let argv = ["pinboard-sync", "sync", "hackernews", "--limit", value];
            match crate::test_support::parse_cli(argv).map(|cli| cli.command) {
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
        let mut argv = vec!["pinboard-sync", "sync", "reddit"];
        argv.extend_from_slice(extra);
        let Command::Sync(SyncCmd {
            source: Some(SyncSource::Reddit(args)),
            ..
        }) = crate::test_support::parse_cli(argv)
            .expect("parses")
            .command
        else {
            panic!("expected `sync reddit` args");
        };
        args
    }

    #[test]
    fn value_taking_boolean_flags_parse_bare_and_explicit() {
        // Absent -> None (falls through to the config tiers).
        assert_eq!(parse_reddit_sync(&[]).flags.public, None);
        // Bare `--public` -> Some(true) via default_missing_value.
        assert_eq!(parse_reddit_sync(&["--public"]).flags.public, Some(true));
        // `--public=false` -> Some(false): the explicit force-off a bare flag can't say.
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

    #[test]
    fn each_renamed_flag_takes_its_new_name() {
        let command = |argv: &[&str]| {
            crate::test_support::parse_cli([&["pinboard-sync"][..], argv].concat())
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

    /// v0.6.0's name for each renamed flag is an unknown argument in every flag group it
    /// used to parse in.
    #[test]
    fn each_renamed_flag_refuses_its_old_name() {
        for argv in [
            &["sync", "reddit", "--max-age-days", "5"][..],
            &["sync", "github", "--max-age-days", "5"],
            &["sync", "hackernews", "--max-age-days", "5"],
            &["sync", "hackernews", "--username", "u"],
            &["backup", "hackernews", "--username", "u"],
            &["cleanup", "--max-age-days", "5", "reddit"],
            &["cleanup", "--stale-to-now", "reddit"],
            &["cleanup", "reddit", "--max-age-days", "5"],
            &["cleanup", "github", "--stale-to-now"],
            &["cleanup", "hackernews", "--stale-to-now=false"],
            &["cleanup", "hackernews", "--link-tag", "t"],
        ] {
            let old_flag = flag_of(
                argv.iter()
                    .find(|arg| arg.starts_with("--"))
                    .expect("a flag"),
            );
            let argv = [&["pinboard-sync"][..], argv].concat();
            let Err(error) = crate::test_support::parse_cli(&argv) else {
                panic!("{argv:?} parsed");
            };
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::UnknownArgument,
                "{argv:?}: {error}"
            );
            assert_eq!(
                error.get(clap::error::ContextKind::InvalidArg),
                Some(&clap::error::ContextValue::String(old_flag.to_owned())),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn a_source_all_refuses_an_account_name() {
        for command in ["sync", "backup"] {
            for source in ["reddit", "github", "hackernews"] {
                for argv in [
                    ["pinboard-sync", command, source, "main", "--all"],
                    ["pinboard-sync", command, source, "--all", "main"],
                ] {
                    let Err(error) = crate::test_support::parse_cli(argv) else {
                        panic!("{argv:?} must be refused");
                    };
                    assert_eq!(
                        error.kind(),
                        clap::error::ErrorKind::ArgumentConflict,
                        "{argv:?}: {error}"
                    );
                }
                assert!(
                    crate::test_support::parse_cli(["pinboard-sync", command, source, "--all"])
                        .is_ok(),
                    "{command} {source} --all"
                );
            }
        }
    }

    /// `PINBOARD_SYNC_ON_AUTH_FAILURE` backs each per-source `--on-auth-failure`, so an
    /// explicit flag outranks it, but not `sync`'s top-level one: the `--all` path reads
    /// the variable itself (`hook::on_auth_failure_from_env`).
    #[test]
    fn on_auth_failure_env_backs_only_the_per_source_flags() {
        use clap::CommandFactory;
        let cli = Cli::command();
        let env_of = |path: &[&str]| {
            let command = path.iter().fold(&cli, |command, name| {
                command.find_subcommand(name).expect("subcommand exists")
            });
            let flag = command
                .get_arguments()
                .find(|arg| arg.get_id() == "on_auth_failure")
                .expect("takes --on-auth-failure");
            flag.get_env().map(|var| var.to_string_lossy().into_owned())
        };
        let env = Some("PINBOARD_SYNC_ON_AUTH_FAILURE".to_owned());

        for path in [
            ["sync", "reddit"],
            ["sync", "github"],
            ["backup", "reddit"],
            ["backup", "github"],
        ] {
            assert_eq!(env_of(&path), env, "{path:?}");
        }
        assert_eq!(env_of(&["sync"]), None);
    }

    /// Clap reads only the hook's variable: a secret bound to a flag's `env` would skip the
    /// `SecretSpec` ladder and its `$VAR_FILE` rung, which the NixOS module relies on.
    #[test]
    fn clap_reads_only_the_hook_variable() {
        use std::collections::BTreeSet;
        fn env_vars(command: &clap::Command) -> BTreeSet<String> {
            command
                .get_arguments()
                .filter_map(clap::Arg::get_env)
                .map(|var| var.to_string_lossy().into_owned())
                .chain(command.get_subcommands().flat_map(env_vars))
                .collect()
        }
        assert_eq!(
            env_vars(&with_globals_at_every_level(Cli::command())),
            BTreeSet::from(["PINBOARD_SYNC_ON_AUTH_FAILURE".to_owned()])
        );
    }

    /// `sync --help` tells the user what the hook flag does and where else it comes from,
    /// not how the code resolves it.
    #[test]
    fn sync_help_describes_the_hook_without_internals() {
        use clap::CommandFactory;
        let help = Cli::command()
            .find_subcommand_mut("sync")
            .expect("sync")
            .render_long_help()
            .to_string();
        assert!(help.contains("PINBOARD_SYNC_ON_AUTH_FAILURE"), "{help}");
        for internal in [
            "with_top_level_hook",
            "on_auth_failure_from_env",
            "`--all` path",
        ] {
            assert!(!help.contains(internal), "{internal} in:\n{help}");
        }
    }

    /// `--help` offers each renamed flag under its new name only.
    #[test]
    fn help_names_each_renamed_flag_by_its_new_name() {
        use clap::CommandFactory;
        let help = |path: [&str; 2]| {
            let mut cli = Cli::command();
            let [command, source] = path;
            cli.find_subcommand_mut(command)
                .and_then(|command| command.find_subcommand_mut(source))
                .expect("a subcommand")
                .render_long_help()
                .to_string()
        };
        let cleanup = help(["cleanup", "hackernews"]);
        let sync = help(["sync", "hackernews"]);
        let backup = help(["backup", "hackernews"]);
        for (help, new, old) in [
            (&cleanup, "--post-date-max-age-days", "--max-age-days"),
            (&cleanup, "--cleanup-stale-to-now", "--stale-to-now"),
            (&cleanup, "--tag-link", "--link-tag"),
            (&sync, "--post-date-max-age-days", "--max-age-days"),
            (&sync, "--hn-username", "--username"),
            (&backup, "--hn-username", "--username"),
        ] {
            assert!(help.contains(&format!(" {new}")), "{new}");
            assert!(!help.contains(&format!(" {old}")), "{old}");
        }
    }

    #[test]
    fn only_the_commands_that_read_settings_read_the_environment() {
        for (argv, reads) in [
            (&["sync", "reddit"][..], true),
            (&["cleanup", "github"], true),
            (&["doctor"], true),
            (&["backup", "pinboard"], true),
            (&["completions", "bash"], false),
            (&["man"], false),
            (&["config", "example"], false),
            (&["config", "check", "a.toml"], false),
        ] {
            let cli = crate::test_support::parse_cli(["pinboard-sync"].iter().chain(argv))
                .unwrap_or_else(|error| panic!("{argv:?}: {error}"));
            assert_eq!(cli.command.reads_environment(), reads, "{argv:?}");
        }
    }
}
