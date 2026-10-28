//! The command line: bpaf's parser for `Cli` and every subcommand's arguments, the checks
//! it leaves to plain code, and the conversions from them into the overrides the job
//! builders and cleanup passes resolve settings from.
//!
//! bpaf hands a command's own flags their first occurrence anywhere on the line, before
//! its subcommand or after it, and the subcommand sees only what is left. So a flag that
//! a command and its subcommand both take reaches the subcommand only when it is given a
//! second time, which [`Cli::parse_from`] refuses, and a global (`--config`, `-v`) is
//! declared once, on the top level, and collected from everywhere. After a parse, then, a
//! subcommand's own copy of a shared flag (`dry_run`, `out`, `dates`, `sync`'s hook) is
//! always unset, but for an env-filled hook: the copy is there to list the flag in the
//! subcommand's `--help` and to catch a second occurrence.

use std::ffi::OsString;
use std::path::PathBuf;

use bpaf::{construct, long, positional, short, Args, Bpaf, Doc, ParseFailure, Parser, ShellComp};

use crate::config::SyncLimit;
use crate::hackernews::cleanup::HackerNewsCleanupOverrides;
use crate::hook::on_auth_failure_from_env;
use crate::jobs::SyncOverrides;
use crate::reddit::cleanup::RedditCleanupOverrides;

mod shell;

pub use shell::Shell;

/// The binary's name, as usage lines, completions and the man page call it.
pub const NAME: &str = env!("CARGO_PKG_NAME");

pub struct Cli {
    /// `--config`, from wherever it was given (env `PINBOARD_SYNC_CONFIG` is `main`'s).
    pub config: Option<String>,
    /// Every `-v`, wherever it was given.
    pub verbose: u8,
    pub command: Command,
}

impl Cli {
    /// Parse the process's arguments. A request for help, the version or a completion is
    /// printed to stdout and exits 0; a bad command line is printed to stderr and exits 2.
    pub fn parse_args() -> Cli {
        let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
        Cli::parse_from(&argv).unwrap_or_else(|failure| exit_with(failure, argv.is_empty()))
    }

    /// Parse `argv`, the arguments after the binary's name, refusing what bpaf itself
    /// accepts but the command line must not: a flag given twice across a command and its
    /// subcommand, `--config` given twice, and a source's `--all` beside an account name.
    /// `PINBOARD_SYNC_ON_AUTH_FAILURE` then backs each per-source `--on-auth-failure` left
    /// unset; a value from it is never a second placement.
    ///
    /// Some arguments are refused before bpaf sees them, by [`words_for_bpaf`].
    pub fn parse_from(argv: &[OsString]) -> Result<Cli, ParseFailure> {
        let argv = words_for_bpaf(argv)?;
        parser()
            .run_inner(Args::from(&argv[..]).set_name(NAME))?
            .into_cli()
    }
}

/// `argv` as the `&str`s bpaf parses, refusing two kinds of argument it must not see. One
/// that isn't UTF-8: bpaf quotes such a value in its error, and the value may be a secret.
/// And bpaf's `--bpaf-complete-style-<shell>`, which bpaf answers by printing its own
/// completion script and exiting the process; its bash, zsh and fish scripts `eval` the
/// line, and `completions <shell>` prints this crate's instead. That one is refused
/// anywhere, after a `--` too, because `help` hands its words back to bpaf ahead of a
/// `--help`, where bpaf would act on it.
fn words_for_bpaf(argv: &[OsString]) -> Result<Vec<&str>, ParseFailure> {
    let Some(words) = argv
        .iter()
        .map(|arg| arg.to_str())
        .collect::<Option<Vec<_>>>()
    else {
        return Err(usage_error("an argument is not valid UTF-8".to_string()));
    };
    if words
        .iter()
        .any(|word| word.starts_with("--bpaf-complete-style-"))
    {
        return Err(usage_error(
            "use `completions <shell>` for a completion script".to_string(),
        ));
    }
    Ok(words)
}

/// The `--help` of the command at `path`, for `pinboard-sync help <path>`.
fn help_of(path: Vec<String>) -> ParseFailure {
    let argv: Vec<String> = path.into_iter().chain(["--help".to_string()]).collect();
    match parser().run_inner(Args::from(&argv[..]).set_name(NAME)) {
        Err(help) => help,
        Ok(_) => ParseFailure::Stderr(Doc::from("no help for that command")),
    }
}

/// Print `failure` where it belongs and exit: 0 for help, the version or a completion,
/// 2 for a usage error. Bare `pinboard-sync` prints its help as an error.
fn exit_with(failure: ParseFailure, bare: bool) -> ! {
    let (text, failed) = match failure {
        ParseFailure::Stdout(doc, full) => (doc.monochrome(full), bare),
        ParseFailure::Completion(text) => (text, false),
        ParseFailure::Stderr(doc) => (format!("Error: {}", doc.monochrome(true)), true),
    };
    let text = format!("{}\n", text.trim_end());
    if failed {
        eprint!("{text}");
        std::process::exit(2);
    }
    if let Err(e) = crate::stdout::write(text) {
        eprintln!("Error: writing to stdout: {e}");
        std::process::exit(1);
    }
    std::process::exit(0)
}

/// The roff man page for the whole command line.
pub fn man_page() -> String {
    let source = concat!(env!("CARGO_PKG_NAME"), " ", env!("CARGO_PKG_VERSION"));
    parser().render_manpage(NAME, bpaf::doc::Section::General, None, Some(source), None)
}

pub enum Command {
    Sync(SyncCmd),
    Cleanup(CleanupCmd),
    Doctor,
    Backup(BackupCmd),
    Completions { shell: Shell },
    Man,
    Config { action: ConfigAction },
}

pub enum ConfigAction {
    Example,
    Check { file: PathBuf },
}

pub struct BackupCmd {
    pub all: bool,
    pub dry_run: bool,
    pub out: OutFlag,
    pub target: Option<BackupTarget>,
}

/// The snapshot directory, declared on `backup` *and* every target so `--help` lists it
/// on either side of the subcommand.
#[derive(Bpaf, Clone, Default)]
#[bpaf(ignore_rustdoc)]
pub struct OutFlag {
    /// Directory to write the snapshot into (overrides `[backup].directory`).
    #[bpaf(argument("DIR"))]
    pub out: Option<String>,
}

pub enum BackupTarget {
    Reddit(RedditBackupArgs),
    Github(GitHubBackupArgs),
    Hackernews(HackerNewsBackupArgs),
    Pinboard(PinboardBackupArgs),
}

// Backing up a source never contacts Pinboard, so none of the source targets take a
// Pinboard token -- the one place `backup` deliberately diverges from `sync`/`cleanup`.

#[derive(Bpaf)]
pub struct RedditBackupArgs {
    /// Back up every reddit account in the config.
    pub all: bool,
    /// Reddit username whose saved items to back up (env REDDIT_USERNAME, or *_FILE).
    #[bpaf(argument("REDDIT_USERNAME"))]
    pub reddit_username: Option<String>,
    /// Reddit session cookie, `reddit_session=…` or its bare value (env REDDIT_COOKIE,
    /// or *_FILE).
    #[bpaf(argument("REDDIT_COOKIE"))]
    pub reddit_cookie: Option<String>,
    /// Shell command run when the Reddit cookie needs refreshing (a 401/403, or a
    /// redirect to the login page; env PINBOARD_SYNC_ON_AUTH_FAILURE).
    #[bpaf(argument("ON_AUTH_FAILURE"))]
    pub on_auth_failure: Option<String>,
    #[bpaf(external(out_flag))]
    pub out: OutFlag,
    /// Show what would be written without touching the filesystem.
    pub dry_run: bool,
    /// Account name to select from the config (default: the first reddit account).
    #[bpaf(positional("ACCOUNT"))]
    pub account: Option<String>,
}

#[derive(Bpaf)]
pub struct GitHubBackupArgs {
    /// Back up every github account in the config.
    pub all: bool,
    /// GitHub personal access token (env GITHUB_TOKEN, or *_FILE).
    #[bpaf(argument("GITHUB_TOKEN"))]
    pub github_token: Option<String>,
    /// Shell command run when the GitHub token needs refreshing (a 401; env
    /// PINBOARD_SYNC_ON_AUTH_FAILURE).
    #[bpaf(argument("ON_AUTH_FAILURE"))]
    pub on_auth_failure: Option<String>,
    #[bpaf(external(out_flag))]
    pub out: OutFlag,
    /// Show what would be written without touching the filesystem.
    pub dry_run: bool,
    /// Account name to select from the config (default: the first github account).
    #[bpaf(positional("ACCOUNT"))]
    pub account: Option<String>,
}

#[derive(Bpaf)]
pub struct HackerNewsBackupArgs {
    /// Back up every hackernews account in the config.
    pub all: bool,
    /// HackerNews username whose favorites to back up (env HN_USERNAME, or *_FILE).
    #[bpaf(long("hn-username"), long("username"), argument("HN_USERNAME"))]
    pub username: Option<String>,
    #[bpaf(external(out_flag))]
    pub out: OutFlag,
    /// Show what would be written without touching the filesystem.
    pub dry_run: bool,
    /// Account name to select from the config (default: the first hackernews account).
    #[bpaf(positional("ACCOUNT"))]
    pub account: Option<String>,
}

/// Pinboard has no `account`/`--all`: `[pinboard]` is a single destination table, not an
/// array of accounts like the sources.
#[derive(Bpaf)]
#[bpaf(ignore_rustdoc)]
pub struct PinboardBackupArgs {
    /// Pinboard API token, "user:TOKEN" (env PINBOARD_TOKEN, or *_FILE).
    #[bpaf(argument("PINBOARD_TOKEN"))]
    pub pinboard_token: Option<String>,
    #[bpaf(external(out_flag))]
    pub out: OutFlag,
    /// Show what would be written without touching the filesystem.
    pub dry_run: bool,
}

pub struct SyncCmd {
    pub all: bool,
    pub dry_run: bool,
    /// See `hook::on_auth_failure_from_env` for how it ranks beside the env var.
    pub on_auth_failure: Option<String>,
    pub source: Option<SyncSource>,
}

pub enum SyncSource {
    Reddit(RedditSyncArgs),
    Github(GitHubSyncArgs),
    Hackernews(HackerNewsSyncArgs),
}

#[derive(Bpaf)]
pub struct RedditSyncArgs {
    /// Run every reddit account in the config.
    pub all: bool,
    /// Reddit username whose saved items to sync (env REDDIT_USERNAME, or *_FILE).
    #[bpaf(argument("REDDIT_USERNAME"))]
    pub reddit_username: Option<String>,
    /// Reddit session cookie, `reddit_session=…` or its bare value (env REDDIT_COOKIE,
    /// or *_FILE).
    #[bpaf(argument("REDDIT_COOKIE"))]
    pub reddit_cookie: Option<String>,
    #[bpaf(external(sync_flags))]
    pub flags: SyncFlags,
    /// Shell command run when the Reddit cookie needs refreshing (a 401/403, or a
    /// redirect to the login page; env PINBOARD_SYNC_ON_AUTH_FAILURE).
    #[bpaf(argument("ON_AUTH_FAILURE"))]
    pub on_auth_failure: Option<String>,
    /// Fetch and print what would be posted, without writing to Pinboard.
    pub dry_run: bool,
    /// Account name to select from the config (default: the first reddit account).
    #[bpaf(positional("ACCOUNT"))]
    pub account: Option<String>,
}

#[derive(Bpaf)]
pub struct GitHubSyncArgs {
    /// Run every github account in the config.
    pub all: bool,
    /// GitHub personal access token (env GITHUB_TOKEN, or *_FILE).
    #[bpaf(argument("GITHUB_TOKEN"))]
    pub github_token: Option<String>,
    #[bpaf(external(sync_flags))]
    pub flags: SyncFlags,
    /// Shell command run when the GitHub token needs refreshing (a 401; env
    /// PINBOARD_SYNC_ON_AUTH_FAILURE).
    #[bpaf(argument("ON_AUTH_FAILURE"))]
    pub on_auth_failure: Option<String>,
    /// Fetch and print what would be posted, without writing to Pinboard.
    pub dry_run: bool,
    /// Account name to select from the config (default: the first github account).
    #[bpaf(positional("ACCOUNT"))]
    pub account: Option<String>,
}

#[derive(Bpaf)]
pub struct HackerNewsSyncArgs {
    /// Run every hackernews account in the config.
    pub all: bool,
    /// HackerNews username whose favorites to sync (env HN_USERNAME, or *_FILE).
    #[bpaf(long("hn-username"), long("username"), argument("HN_USERNAME"))]
    pub username: Option<String>,
    #[bpaf(external(sync_flags))]
    pub flags: SyncFlags,
    /// Fetch and print what would be posted, without writing to Pinboard.
    pub dry_run: bool,
    /// Account name to select from the config (default: the first hackernews account).
    #[bpaf(positional("ACCOUNT"))]
    pub account: Option<String>,
}

/// The Pinboard and write/dating flags every `sync <source>` takes, one parser shared by
/// each so the declarations live in one place.
#[derive(Bpaf, Default)]
#[bpaf(ignore_rustdoc)]
pub struct SyncFlags {
    /// Pinboard API token, "user:TOKEN" (env PINBOARD_TOKEN, or *_FILE).
    #[bpaf(argument("PINBOARD_TOKEN"))]
    pub pinboard_token: Option<String>,
    /// `auto` (the default: no cap; stop paging once caught up), `all` (no cap; read
    /// every page), or N (write at most N; read every page). Overrides config.
    #[bpaf(argument("LIMIT"))]
    pub limit: Option<SyncLimit>,
    /// Only backdate posts newer than N days; older use "now" (overrides config).
    #[bpaf(
        long("post-date-max-age-days"),
        long("max-age-days"),
        argument("POST_DATE_MAX_AGE_DAYS")
    )]
    pub max_age_days: Option<u64>,
    #[bpaf(external(toread))]
    pub toread: Option<bool>,
    #[bpaf(external(public))]
    pub public: Option<bool>,
    #[bpaf(external(use_post_date))]
    pub use_post_date: Option<bool>,
}

pub struct CleanupCmd {
    pub all: bool,
    pub dry_run: bool,
    pub dates: DateFlags,
    pub source: Option<CleanupSource>,
}

pub enum CleanupSource {
    Reddit(RedditCleanupArgs),
    Github(GitHubCleanupArgs),
    Hackernews(HackerNewsCleanupArgs),
}

/// The date-setting flags shared by every cleanup command (and `cleanup --all`), one
/// parser shared by each so the declarations live in one place. They are the top tier of
/// the date settings (flag -> account -> per-source default -> `[pinboard]` global), and
/// all-`None` means "no flag given", so resolution falls through to the other tiers.
#[derive(Bpaf, Clone, Copy, Default)]
#[bpaf(ignore_rustdoc)]
pub struct DateFlags {
    /// Only backdate posts newer than N days; older use "now" (overrides config).
    #[bpaf(
        long("post-date-max-age-days"),
        long("max-age-days"),
        argument("POST_DATE_MAX_AGE_DAYS")
    )]
    pub max_age_days: Option<u64>,
    #[bpaf(external(use_post_date))]
    pub use_post_date: Option<bool>,
    #[bpaf(external(stale_to_now))]
    pub stale_to_now: Option<bool>,
}

impl DateFlags {
    /// Take each date flag from `cleanup` (`CleanupCmd`), else this per-source value. The
    /// command line refuses one on both. Mirrors `SyncOverrides::with_top_level_hook`.
    #[must_use]
    pub fn with_top_level(self, top_overrides: &DateFlags) -> DateFlags {
        DateFlags {
            use_post_date: top_overrides.use_post_date.or(self.use_post_date),
            max_age_days: top_overrides.max_age_days.or(self.max_age_days),
            stale_to_now: top_overrides.stale_to_now.or(self.stale_to_now),
        }
    }
}

#[derive(Bpaf)]
pub struct GitHubCleanupArgs {
    /// GitHub personal access token (env GITHUB_TOKEN, or *_FILE).
    #[bpaf(argument("GITHUB_TOKEN"))]
    pub github_token: Option<String>,
    /// Pinboard API token (env PINBOARD_TOKEN, or *_FILE).
    #[bpaf(argument("PINBOARD_TOKEN"))]
    pub pinboard_token: Option<String>,
    #[bpaf(external(date_flags))]
    pub dates: DateFlags,
    /// Show what would change without writing to Pinboard.
    pub dry_run: bool,
    /// Account name whose token/tags to use (default: the first github account).
    #[bpaf(positional("ACCOUNT"))]
    pub account: Option<String>,
}

#[derive(Bpaf)]
pub struct HackerNewsCleanupArgs {
    /// Pinboard API token (env PINBOARD_TOKEN, or *_FILE).
    #[bpaf(argument("PINBOARD_TOKEN"))]
    pub pinboard_token: Option<String>,
    /// Also link article bookmarks tagged with the link tag (default `find-hn`) to
    /// their HN discussion, via an Algolia URL lookup per tagged bookmark.
    pub link_discussions: bool,
    /// Override the marker tag used by --link-discussions (config: `tag_link`).
    #[bpaf(long("tag-link"), long("link-tag"), argument("TAG_LINK"))]
    pub link_tag: Option<String>,
    #[bpaf(external(date_flags))]
    pub dates: DateFlags,
    /// Show what would change without writing to Pinboard.
    pub dry_run: bool,
    /// Account name whose tag config to use (default: the first hackernews account).
    #[bpaf(positional("ACCOUNT"))]
    pub account: Option<String>,
}

#[derive(Bpaf)]
pub struct RedditCleanupArgs {
    /// Reddit session cookie, `reddit_session=…` or its bare value (env REDDIT_COOKIE,
    /// or *_FILE). Needed for the `/api/info` lookups; not required with --no-nsfw
    /// --no-titles.
    #[bpaf(argument("REDDIT_COOKIE"))]
    pub reddit_cookie: Option<String>,
    /// Pinboard API token (env PINBOARD_TOKEN, or *_FILE).
    #[bpaf(argument("PINBOARD_TOKEN"))]
    pub pinboard_token: Option<String>,
    /// Skip NSFW tagging (no Reddit /api/info call for over_18).
    pub no_nsfw: bool,
    /// Skip replacing generic placeholder titles.
    pub no_titles: bool,
    #[bpaf(external(date_flags))]
    pub dates: DateFlags,
    /// Show what would change without writing to Pinboard.
    pub dry_run: bool,
    /// Account name whose cookie + domain/tags to use (default: the first reddit account).
    #[bpaf(positional("ACCOUNT"))]
    pub account: Option<String>,
}

// --- the parser --------------------------------------------------------------

/// What bpaf parses, before [`Parsed::into_cli`] makes the checks it can't.
struct Parsed {
    config: Vec<String>,
    verbose: usize,
    invocation: Invocation,
}

enum Invocation {
    Run(Command),
    /// `help <path>`: print the `--help` of the command at `path`.
    Help(Vec<String>),
}

fn parser() -> bpaf::OptionParser<Parsed> {
    let config = config_flag();
    let verbose = verbose_flag();
    let command = construct!([
        sync_cmd(),
        cleanup_cmd(),
        doctor_cmd(),
        backup_cmd(),
        completions_cmd(),
        man_cmd(),
        config_cmd(),
    ])
    .map(Invocation::Run);
    let help = subcommand(
        "help",
        "Print this message or the help of the given subcommand(s).",
        positional::<String>("COMMAND").many().map(Invocation::Help),
    );
    let invocation = construct!([command, help]);
    construct!(Parsed {
        config,
        verbose,
        invocation
    })
    .to_options()
    .descr(env!("CARGO_PKG_DESCRIPTION"))
    .version(env!("CARGO_PKG_VERSION"))
    .fallback_to_usage()
}

fn config_flag() -> impl Parser<Vec<String>> {
    long("config")
        .help("Path to the TOML config file (env PINBOARD_SYNC_CONFIG).")
        .argument::<String>("CONFIG")
        .complete_shell(file_completion())
        .many()
}

fn verbose_flag() -> impl Parser<usize> {
    short('v')
        .long("verbose")
        .help(
            "Increase log verbosity: `-v` for debug, `-vv` for trace, `-vvv` to also include \
             dependency logs. Overridden by `RUST_LOG`.",
        )
        .req_flag(())
        .many()
        .map(|flags| flags.len())
}

/// The subcommand `name`, described by `about`, whose arguments `args` parses. It lists
/// the globals in its `--help` too: the top level has already taken every one given, so
/// these copies never see one and only document that the flags work here.
fn subcommand<T: 'static>(
    name: &'static str,
    about: &'static str,
    args: impl Parser<T> + 'static,
) -> impl Parser<T> {
    let config = config_flag();
    let verbose = verbose_flag();
    let globals = construct!(config, verbose).hide_usage();
    construct!(globals, args)
        .map(|(_, args)| args)
        .to_options()
        .descr(about)
        .command(name)
}

fn sync_cmd() -> impl Parser<Command> {
    let dry_run = long("dry-run")
        .help("Show what would be written without touching Pinboard (with --all).")
        .switch();
    let on_auth_failure = long("on-auth-failure")
        .help(
            "Shell command run when a source's credential needs refreshing (a 401/403; env \
             PINBOARD_SYNC_ON_AUTH_FAILURE).",
        )
        .argument::<String>("ON_AUTH_FAILURE")
        .optional();
    let target = all_or(
        "Run every configured account across every source (requires --config).",
        sync_sources,
    );
    let sync = construct!(dry_run, on_auth_failure, target).map(
        |(dry_run, on_auth_failure, (all, source))| SyncCmd {
            all,
            dry_run,
            on_auth_failure,
            source,
        },
    );
    subcommand(
        "sync",
        "Sync a source's saved/favorited items to Pinboard.",
        sync,
    )
    .map(Command::Sync)
}

fn sync_sources() -> impl Parser<SyncSource> {
    let reddit = subcommand(
        "reddit",
        "Sync saved Reddit posts and comments.",
        reddit_sync_args(),
    )
    .map(SyncSource::Reddit);
    let github = subcommand(
        "github",
        "Sync starred GitHub repositories.",
        git_hub_sync_args(),
    )
    .map(SyncSource::Github);
    let hackernews = subcommand(
        "hackernews",
        "Sync favorited HackerNews stories and comments.",
        hacker_news_sync_args(),
    )
    .map(SyncSource::Hackernews);
    construct!([reddit, github, hackernews])
}

fn cleanup_cmd() -> impl Parser<Command> {
    let dry_run = long("dry-run")
        .help("Show what would change without writing to Pinboard (with --all).")
        .switch();
    let dates = date_flags();
    let target = all_or(
        "Run cleanup for every configured account across every cleanup-capable source.",
        cleanup_sources,
    );
    let cleanup =
        construct!(dry_run, dates, target).map(|(dry_run, dates, (all, source))| CleanupCmd {
            all,
            dry_run,
            dates,
            source,
        });
    subcommand(
        "cleanup",
        "Normalize existing bookmarks for a source.",
        cleanup,
    )
    .map(Command::Cleanup)
}

fn cleanup_sources() -> impl Parser<CleanupSource> {
    let reddit = subcommand(
        "reddit",
        "Normalize existing reddit bookmarks (URLs, tags, NSFW, titles).",
        reddit_cleanup_args(),
    )
    .map(CleanupSource::Reddit);
    let github = subcommand(
        "github",
        "Canonicalize existing GitHub repo bookmark URLs.",
        git_hub_cleanup_args(),
    )
    .map(CleanupSource::Github);
    let hackernews = subcommand(
        "hackernews",
        "Normalize existing HackerNews bookmarks (rewrite item URLs to articles).",
        hacker_news_cleanup_args(),
    )
    .map(CleanupSource::Hackernews);
    construct!([reddit, github, hackernews])
}

fn backup_cmd() -> impl Parser<Command> {
    let dry_run = long("dry-run")
        .help("Show what would be written without touching the filesystem.")
        .switch();
    let out = out_flag();
    let target = all_or(
        "Back up the Pinboard account and every configured account across every source.",
        backup_targets,
    );
    let backup = construct!(dry_run, out, target).map(|(dry_run, out, (all, target))| BackupCmd {
        all,
        dry_run,
        out,
        target,
    });
    subcommand(
        "backup",
        "Snapshot every service to a directory: verbatim API responses plus normalized items.",
        backup,
    )
    .map(Command::Backup)
}

fn backup_targets() -> impl Parser<BackupTarget> {
    let reddit = subcommand(
        "reddit",
        "Back up saved Reddit posts and comments.",
        reddit_backup_args(),
    )
    .map(BackupTarget::Reddit);
    let github = subcommand(
        "github",
        "Back up starred GitHub repositories.",
        git_hub_backup_args(),
    )
    .map(BackupTarget::Github);
    let hackernews = subcommand(
        "hackernews",
        "Back up favorited HackerNews stories and comments.",
        hacker_news_backup_args(),
    )
    .map(BackupTarget::Hackernews);
    let pinboard = subcommand(
        "pinboard",
        "Back up the Pinboard account itself (raw `posts/all`, verbatim).",
        pinboard_backup_args(),
    )
    .map(BackupTarget::Pinboard);
    construct!([reddit, github, hackernews, pinboard])
}

/// A command's `--all` (described by `help`) and the subcommand `subcommands` builds, as
/// `(all, subcommand)`, either or both or neither. bpaf hands a command its `--all` from
/// anywhere on the line, so `sync reddit --all` (every reddit account) would read as
/// `sync --all reddit` (every source). When both branches take the same words bpaf keeps
/// the first listed, the subcommand alone, so a lone `--all` after it stays its own. An
/// `--all` the subcommand can't take, or a second one, goes to the command, and the run
/// refuses that with its own message.
fn all_or<S: 'static, P: Parser<S> + 'static>(
    help: &'static str,
    subcommands: impl Fn() -> P,
) -> impl Parser<(bool, Option<S>)> {
    let alone = subcommands().map(|subcommand| (false, Some(subcommand)));
    let all = long("all").help(help).req_flag(());
    let beside = subcommands().hide().optional();
    let with_all = construct!(all, beside).map(|((), subcommand)| (true, subcommand));
    construct!([alone, with_all])
        .optional()
        .map(|target| target.unwrap_or((false, None)))
}

fn doctor_cmd() -> impl Parser<Command> {
    subcommand(
        "doctor",
        "Check the Pinboard token and every configured account's credentials.",
        bpaf::pure(()).map(|()| Command::Doctor),
    )
}

fn completions_cmd() -> impl Parser<Command> {
    let shell = positional::<String>("SHELL")
        .help("The shell to print the script for: bash, zsh, fish or elvish.")
        .complete(|typed: &String| Shell::complete(typed))
        .parse(|name| name.parse::<Shell>());
    subcommand(
        "completions",
        "Print a shell completion script (bash, zsh, fish, elvish) to stdout.",
        shell.map(|shell| Command::Completions { shell }),
    )
}

fn man_cmd() -> impl Parser<Command> {
    subcommand(
        "man",
        "Print a man page (roff) to stdout.",
        bpaf::pure(()).map(|()| Command::Man),
    )
}

fn config_cmd() -> impl Parser<Command> {
    let example = subcommand(
        "example",
        "Print a fully-commented example config (every key with its default).",
        bpaf::pure(()).map(|()| ConfigAction::Example),
    );
    let file = positional::<PathBuf>("FILE")
        .help("The TOML config file to validate.")
        .complete_shell(file_completion());
    let check = subcommand(
        "check",
        "Validate a config file as a run would, without using secrets or contacting any \
         service.",
        file.map(|file| ConfigAction::Check { file }),
    );
    subcommand(
        "config",
        "Config helpers: print an example config, or check one.",
        construct!([example, check]),
    )
    .map(|action| Command::Config { action })
}

/// File names, as bpaf's `ShellComp::File` completes them, but with a newline after
/// bash's `_filedir`. bpaf 0.9 leaves it off, so the next candidate runs into it.
/// Fish and elvish complete no files under bpaf either way.
fn file_completion() -> ShellComp {
    ShellComp::Raw {
        bash: "local cur prev words cword ; _init_completion || return ; _filedir",
        zsh: "_files",
        fish: "",
        elvish: "",
    }
}

/// `--NAME[=BOOL]`: bare for `true`, `=true` or `=false` to say which. The value must
/// follow an `=`, so `--public alice` leaves `alice` to the account positional. bpaf's
/// `argument` would also take a separate word as the value, so the flag is matched whole,
/// as bpaf's own examples do for flags of unusual shape.
fn optional_bool(
    name: &'static str,
    alias: Option<&'static str>,
    help: &'static str,
) -> impl Parser<Option<bool>> {
    let matches = move |flag: &str| flag == name || Some(flag) == alias;
    bpaf::any::<String, _, _>(&format!("--{name}[=BOOL]"), move |token: String| {
        let flag = token.strip_prefix("--")?;
        let (flag, value) = match flag.split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (flag, None),
        };
        if !matches(flag) {
            return None;
        }
        Some(match value {
            None | Some("true") => Ok(true),
            Some("false") => Ok(false),
            Some(_) => Err(format!("`--{name}` takes `true` or `false`")),
        })
    })
    .help(help)
    .anywhere()
    .parse(|value| value)
    .optional()
}

fn toread() -> impl Parser<Option<bool>> {
    optional_bool(
        "toread",
        None,
        "Mark new bookmarks to-read: `--toread` or `--toread=false` (overrides config).",
    )
}

fn public() -> impl Parser<Option<bool>> {
    optional_bool(
        "public",
        None,
        "Create bookmarks public: `--public` or `--public=false` (overrides config).",
    )
}

fn use_post_date() -> impl Parser<Option<bool>> {
    optional_bool(
        "use-post-date",
        None,
        "Date bookmarks by the source post date: `--use-post-date[=BOOL]` (overrides config).",
    )
}

fn stale_to_now() -> impl Parser<Option<bool>> {
    optional_bool(
        "cleanup-stale-to-now",
        Some("stale-to-now"),
        "Re-date too-old posts to now: `--cleanup-stale-to-now[=BOOL]` (overrides config).",
    )
}

// --- what bpaf leaves to plain code ------------------------------------------

impl Parsed {
    fn into_cli(self) -> Result<Cli, ParseFailure> {
        let mut command = match self.invocation {
            Invocation::Run(command) => command,
            Invocation::Help(path) => return Err(help_of(path)),
        };
        let mut configs = self.config.into_iter();
        let config = configs.next();
        if configs.next().is_some() {
            return Err(usage_error(given_twice("--config")));
        }
        command.refuse_conflicts().map_err(usage_error)?;
        command.fill_hooks_from_env();
        Ok(Cli {
            config,
            verbose: u8::try_from(self.verbose).unwrap_or(u8::MAX),
            command,
        })
    }
}

fn usage_error(message: String) -> ParseFailure {
    ParseFailure::Stderr(Doc::from(message.as_str()))
}

fn given_twice(flag: &str) -> String {
    format!("'{flag}' was given twice; give it once")
}

/// Each flag in `flags` that was given, by its long name.
fn given<'a>(flags: &'a [(&'static str, bool)]) -> impl Iterator<Item = &'static str> + 'a {
    flags
        .iter()
        .filter(|(_, given)| *given)
        .map(|(name, _)| *name)
}

/// Refuse a flag that a command, through its `outer` flags, and its subcommand, through
/// its `inner` ones, were both given.
fn refuse_given_twice(
    outer: &[(&'static str, bool)],
    inner: &[(&'static str, bool)],
) -> Result<(), String> {
    match given(outer).find(|flag| given(inner).any(|other| other == *flag)) {
        Some(flag) => Err(given_twice(flag)),
        None => Ok(()),
    }
}

/// Refuse a source's `--all` beside an account name: it would have to ignore the name,
/// even one no account has.
fn refuse_all_with_account(all: bool, account: Option<&str>) -> Result<(), String> {
    if all && account.is_some() {
        return Err("`--all` cannot be combined with an account name".to_string());
    }
    Ok(())
}

impl DateFlags {
    fn given(&self) -> [(&'static str, bool); 3] {
        [
            ("--post-date-max-age-days", self.max_age_days.is_some()),
            ("--use-post-date", self.use_post_date.is_some()),
            ("--cleanup-stale-to-now", self.stale_to_now.is_some()),
        ]
    }
}

impl Command {
    fn refuse_conflicts(&self) -> Result<(), String> {
        match self {
            Command::Sync(cmd) => cmd.refuse_conflicts(),
            Command::Cleanup(cmd) => cmd.refuse_conflicts(),
            Command::Backup(cmd) => cmd.refuse_conflicts(),
            Command::Doctor
            | Command::Completions { .. }
            | Command::Man
            | Command::Config { .. } => Ok(()),
        }
    }

    /// Back each per-source `--on-auth-failure` the command line left unset with
    /// `PINBOARD_SYNC_ON_AUTH_FAILURE`, read through `secrets` so a test sees only its
    /// `FakeSecretEnv`.
    fn fill_hooks_from_env(&mut self) {
        let hook = match self {
            Command::Sync(SyncCmd {
                source: Some(SyncSource::Reddit(args)),
                ..
            }) => &mut args.on_auth_failure,
            Command::Sync(SyncCmd {
                source: Some(SyncSource::Github(args)),
                ..
            }) => &mut args.on_auth_failure,
            Command::Backup(BackupCmd {
                target: Some(BackupTarget::Reddit(args)),
                ..
            }) => &mut args.on_auth_failure,
            Command::Backup(BackupCmd {
                target: Some(BackupTarget::Github(args)),
                ..
            }) => &mut args.on_auth_failure,
            _ => return,
        };
        if hook.is_none() {
            *hook = on_auth_failure_from_env();
        }
    }
}

impl SyncCmd {
    fn refuse_conflicts(&self) -> Result<(), String> {
        let (all, account, dry_run, on_auth_failure) = match &self.source {
            None => return Ok(()),
            Some(SyncSource::Reddit(args)) => (
                args.all,
                args.account.as_deref(),
                args.dry_run,
                args.on_auth_failure.is_some(),
            ),
            Some(SyncSource::Github(args)) => (
                args.all,
                args.account.as_deref(),
                args.dry_run,
                args.on_auth_failure.is_some(),
            ),
            Some(SyncSource::Hackernews(args)) => {
                (args.all, args.account.as_deref(), args.dry_run, false)
            }
        };
        refuse_all_with_account(all, account)?;
        refuse_given_twice(
            &[
                ("--dry-run", self.dry_run),
                ("--on-auth-failure", self.on_auth_failure.is_some()),
            ],
            &[
                ("--dry-run", dry_run),
                ("--on-auth-failure", on_auth_failure),
            ],
        )
    }
}

impl CleanupCmd {
    fn refuse_conflicts(&self) -> Result<(), String> {
        let (dry_run, dates) = match &self.source {
            None => return Ok(()),
            Some(CleanupSource::Reddit(args)) => (args.dry_run, &args.dates),
            Some(CleanupSource::Github(args)) => (args.dry_run, &args.dates),
            Some(CleanupSource::Hackernews(args)) => (args.dry_run, &args.dates),
        };
        let outer = [
            [("--dry-run", self.dry_run)].as_slice(),
            &self.dates.given(),
        ]
        .concat();
        let inner = [[("--dry-run", dry_run)].as_slice(), &dates.given()].concat();
        refuse_given_twice(&outer, &inner)
    }
}

impl BackupCmd {
    fn refuse_conflicts(&self) -> Result<(), String> {
        let (all, account, dry_run, out) = match &self.target {
            None => return Ok(()),
            Some(BackupTarget::Reddit(args)) => {
                (args.all, args.account.as_deref(), args.dry_run, &args.out)
            }
            Some(BackupTarget::Github(args)) => {
                (args.all, args.account.as_deref(), args.dry_run, &args.out)
            }
            Some(BackupTarget::Hackernews(args)) => {
                (args.all, args.account.as_deref(), args.dry_run, &args.out)
            }
            Some(BackupTarget::Pinboard(args)) => (false, None, args.dry_run, &args.out),
        };
        refuse_all_with_account(all, account)?;
        refuse_given_twice(
            &[
                ("--dry-run", self.dry_run),
                ("--out", self.out.out.is_some()),
            ],
            &[("--dry-run", dry_run), ("--out", out.out.is_some())],
        )
    }
}

// --- into the job builders' overrides ----------------------------------------

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

impl From<HackerNewsCleanupArgs> for HackerNewsCleanupOverrides {
    fn from(args: HackerNewsCleanupArgs) -> Self {
        HackerNewsCleanupOverrides {
            link_discussions: args.link_discussions,
            link_tag: args.link_tag,
        }
    }
}

impl From<RedditCleanupArgs> for RedditCleanupOverrides {
    fn from(args: RedditCleanupArgs) -> Self {
        RedditCleanupOverrides {
            cookie: args.reddit_cookie,
            mark_nsfw: !args.no_nsfw,
            fix_titles: !args.no_titles,
        }
    }
}

#[cfg(test)]
mod tests;
