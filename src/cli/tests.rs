use super::*;
use std::num::NonZeroUsize;

use crate::secrets::FakeSecretEnv;
use crate::test_support::parse_cli;

/// Parse `pinboard-sync <argv>`.
fn parse(argv: &[&str]) -> Result<Cli, ParseFailure> {
    parse_cli(std::iter::once("pinboard-sync").chain(argv.iter().copied()))
}

/// The usage error `pinboard-sync <argv>` is refused with.
fn refusal(argv: &[&str]) -> String {
    match parse(argv) {
        Ok(_) => panic!("{argv:?} parsed"),
        Err(failure) => failure.unwrap_stderr(),
    }
}

/// The `--help` of `pinboard-sync <path>`.
fn help(path: &[&str]) -> String {
    let argv = [path, &["--help"]].concat();
    match parse(&argv) {
        Ok(_) => panic!("{argv:?} parsed"),
        Err(failure) => failure.unwrap_stdout(),
    }
}

fn command(argv: &[&str]) -> Command {
    parse(argv)
        .unwrap_or_else(|failure| panic!("{argv:?}: {}", failure.unwrap_stderr()))
        .command
}

/// Every command path `--help` can be asked of.
const COMMAND_PATHS: &[&[&str]] = &[
    &[],
    &["sync"],
    &["sync", "reddit"],
    &["sync", "github"],
    &["sync", "hackernews"],
    &["cleanup"],
    &["cleanup", "reddit"],
    &["cleanup", "github"],
    &["cleanup", "hackernews"],
    &["doctor"],
    &["backup"],
    &["backup", "reddit"],
    &["backup", "github"],
    &["backup", "hackernews"],
    &["backup", "pinboard"],
    &["completions"],
    &["man"],
    &["config"],
    &["config", "example"],
    &["config", "check"],
    &["help"],
];

#[test]
fn the_parser_keeps_bpafs_invariants() {
    parser().check_invariants(false);
}

/// bpaf builds a level's help only when it is asked for, and panics then on a parser
/// declared out of order, so every level is asked.
#[test]
fn every_command_has_a_help() {
    for path in COMMAND_PATHS {
        let help = help(path);
        assert!(help.contains("Usage: pinboard-sync"), "{path:?}: {help}");
    }
}

#[test]
fn help_prints_the_help_of_the_command_it_names() {
    for path in COMMAND_PATHS {
        let argv = [&["help"], *path].concat();
        let Err(ParseFailure::Stdout(text, _)) = parse(&argv) else {
            panic!("{argv:?} must print a help");
        };
        assert_eq!(text.monochrome(false), help(path), "{argv:?}");
    }
    assert!(parse(&["help", "nosuch"]).is_err());
}

#[test]
fn a_bare_invocation_prints_the_usage() {
    let Err(ParseFailure::Stdout(usage, _)) = parse(&[]) else {
        panic!("a bare invocation must print the usage");
    };
    assert!(usage.monochrome(false).contains("Available commands:"));
}

#[test]
fn limit_flag_takes_auto_all_or_a_count() {
    let limit = |value: &str| match parse(&["sync", "hackernews", "--limit", value]) {
        Ok(Cli {
            command:
                Command::Sync(SyncCmd {
                    source: Some(SyncSource::Hackernews(args)),
                    ..
                }),
            ..
        }) => Ok(args.flags.limit),
        Ok(_) => panic!("expected `sync hackernews`"),
        Err(failure) => Err(failure.unwrap_stderr()),
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
    match command(&[&["sync", "reddit"], extra].concat()) {
        Command::Sync(SyncCmd {
            source: Some(SyncSource::Reddit(args)),
            ..
        }) => args,
        _ => panic!("expected `sync reddit` args"),
    }
}

#[test]
fn value_taking_boolean_flags_parse_bare_and_explicit() {
    // Absent -> None (falls through to the config tiers).
    assert_eq!(parse_reddit_sync(&[]).flags.public, None);
    assert_eq!(parse_reddit_sync(&["--public"]).flags.public, Some(true));
    assert_eq!(
        parse_reddit_sync(&["--public=true"]).flags.public,
        Some(true)
    );
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
fn a_value_taking_boolean_flag_refuses_anything_but_true_or_false() {
    let error = refusal(&["sync", "reddit", "--public=maybe"]);
    assert!(
        error.contains("`--public` takes `true` or `false`"),
        "{error}"
    );
    let error = refusal(&["sync", "reddit", "--public", "--public=false"]);
    assert!(error.contains("cannot be used multiple times"), "{error}");
    // A flag that only starts with the name is another flag.
    let error = refusal(&["sync", "reddit", "--publicity"]);
    assert!(error.contains("--publicity"), "{error}");
}

#[test]
fn bare_bool_flag_does_not_swallow_the_account_positional() {
    // The value must follow an `=`, so the account is left to the positional.
    let args = parse_reddit_sync(&["--public", "alice"]);
    assert_eq!(args.account.as_deref(), Some("alice"));
    assert_eq!(args.flags.public, Some(true));
    let args = parse_reddit_sync(&["--public", "false"]);
    assert_eq!(args.account.as_deref(), Some("false"));
    assert_eq!(args.flags.public, Some(true));
    // The `=` form still forces the flag off while leaving the positional alone.
    let args = parse_reddit_sync(&["--public=false", "alice"]);
    assert_eq!(args.account.as_deref(), Some("alice"));
    assert_eq!(args.flags.public, Some(false));
}

/// Each renamed flag parses under its new name and, for one release, its old one.
#[test]
fn each_renamed_flag_takes_its_new_and_its_old_name() {
    for [max_age, stale, link, username] in [
        [
            "--post-date-max-age-days",
            "--cleanup-stale-to-now",
            "--tag-link",
            "--hn-username",
        ],
        [
            "--max-age-days",
            "--stale-to-now",
            "--link-tag",
            "--username",
        ],
    ] {
        let Command::Sync(SyncCmd {
            source: Some(SyncSource::Hackernews(args)),
            ..
        }) = command(&["sync", "hackernews", max_age, "5", username, "u"])
        else {
            panic!("expected `sync hackernews`");
        };
        assert_eq!(args.flags.max_age_days, Some(5), "{max_age}");
        assert_eq!(args.username.as_deref(), Some("u"), "{username}");

        // `cleanup` takes the date flags too, and with them their first occurrence.
        let Command::Cleanup(CleanupCmd {
            dates,
            source: Some(CleanupSource::Hackernews(args)),
            ..
        }) = command(&["cleanup", "hackernews", max_age, "5", stale, link, "t"])
        else {
            panic!("expected `cleanup hackernews`");
        };
        let dates = args.dates.with_top_level(&dates);
        assert_eq!(dates.max_age_days, Some(5), "{max_age}");
        assert_eq!(dates.stale_to_now, Some(true), "{stale}");
        assert_eq!(args.link_tag.as_deref(), Some("t"), "{link}");

        let Command::Backup(BackupCmd {
            target: Some(BackupTarget::Hackernews(args)),
            ..
        }) = command(&["backup", "hackernews", username, "u"])
        else {
            panic!("expected `backup hackernews`");
        };
        assert_eq!(args.username.as_deref(), Some("u"), "{username}");
    }
}

#[test]
fn a_source_all_refuses_an_account_name() {
    for command in ["sync", "backup"] {
        for source in ["reddit", "github", "hackernews"] {
            for argv in [
                [command, source, "main", "--all"],
                [command, source, "--all", "main"],
            ] {
                assert_eq!(
                    refusal(&argv),
                    "`--all` cannot be combined with an account name",
                    "{argv:?}"
                );
            }
            assert!(
                parse(&[command, source, "--all"]).is_ok(),
                "{command} {source} --all"
            );
        }
    }
}

/// A source's `--all` (every account of that source) and the command's (every source)
/// are two flags: the command's is an alternative to a subcommand, not beside one.
#[test]
fn a_commands_all_and_its_sources_all_are_told_apart() {
    let Command::Sync(cmd) = command(&["sync", "reddit", "--all"]) else {
        panic!("expected `sync`");
    };
    assert!(!cmd.all);
    assert!(matches!(
        cmd.source,
        Some(SyncSource::Reddit(RedditSyncArgs { all: true, .. }))
    ));

    let Command::Sync(cmd) = command(&["sync", "--dry-run", "--all"]) else {
        panic!("expected `sync`");
    };
    assert!(cmd.all && cmd.dry_run && cmd.source.is_none());

    let Command::Backup(cmd) = command(&["backup", "github", "--all"]) else {
        panic!("expected `backup`");
    };
    assert!(!cmd.all);
    assert!(matches!(
        cmd.target,
        Some(BackupTarget::Github(GitHubBackupArgs { all: true, .. }))
    ));

    // An `--all` the source can't take is the command's, beside the source.
    for argv in [
        &["sync", "--all", "reddit"][..],
        &["backup", "--all", "pinboard"],
        &["cleanup", "--all", "github"],
        &["cleanup", "github", "--all"],
        &["backup", "pinboard", "--all"],
    ] {
        let all_beside_a_subcommand = match command(argv) {
            Command::Sync(cmd) => cmd.all && cmd.source.is_some(),
            Command::Cleanup(cmd) => cmd.all && cmd.source.is_some(),
            Command::Backup(cmd) => cmd.all && cmd.target.is_some(),
            _ => false,
        };
        assert!(all_beside_a_subcommand, "{argv:?}");
    }
}

/// `--all` before a source and after it are different flags, every source's accounts
/// and that source's; the run refuses the pair, so the parse lets it through.
#[test]
fn all_on_both_sides_is_left_to_the_run() {
    for argv in [
        ["sync", "--all", "reddit", "--all"],
        ["backup", "--all", "github", "--all"],
        ["sync", "reddit", "--all", "--all"],
    ] {
        assert!(parse(&argv).is_ok(), "{argv:?}");
    }
}

/// Each flag a command and its subcommand both take, given before the subcommand and
/// after it, as `(before, subcommand, after)`.
const ON_BOTH_SIDES: &[(&[&str], &str, &[&str])] = &[
    (&["sync", "--dry-run"], "reddit", &["--dry-run"]),
    (&["sync", "--dry-run"], "hackernews", &["--dry-run"]),
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
        &["cleanup", "--max-age-days", "3"],
        "reddit",
        &["--post-date-max-age-days", "4"],
    ),
    (
        &["cleanup", "--post-date-max-age-days", "3"],
        "github",
        &["--max-age-days", "4"],
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
    (
        &["cleanup", "--cleanup-stale-to-now"],
        "github",
        &["--stale-to-now"],
    ),
    (&["backup", "--dry-run"], "reddit", &["--dry-run"]),
    (&["backup", "--out", "/a"], "pinboard", &["--out", "/b"]),
];

/// The long name of the flag `after` gives, as the refusal names it.
fn flag_of(after: &[&str]) -> &'static str {
    let flag = after[0].split('=').next().unwrap();
    [
        "--dry-run",
        "--on-auth-failure",
        "--post-date-max-age-days",
        "--use-post-date",
        "--cleanup-stale-to-now",
        "--out",
    ]
    .into_iter()
    .find(|name| {
        *name == flag
            || (*name == "--cleanup-stale-to-now" && flag == "--stale-to-now")
            || (*name == "--post-date-max-age-days" && flag == "--max-age-days")
    })
    .expect("a flag both levels take")
}

/// Given before and after the subcommand, or twice after it: bpaf hands the command the
/// first wherever it is, so both shapes reach the check the same way, and the refusal
/// can't say which side each was on.
#[test]
fn a_flag_given_both_before_and_after_its_subcommand_is_refused() {
    for (before, subcommand, after) in ON_BOTH_SIDES {
        let both_sides = [*before, &[*subcommand], after].concat();
        let both_after = [&before[..1], &[*subcommand], &before[1..], after].concat();
        for argv in [both_sides, both_after] {
            assert_eq!(
                refusal(&argv),
                format!("'{}' was given twice; give it once", flag_of(after)),
                "{argv:?}"
            );
        }
    }
}

#[test]
fn a_flag_given_on_one_side_of_its_subcommand_parses() {
    for (before, subcommand, after) in ON_BOTH_SIDES {
        let top = [*before, &[*subcommand]].concat();
        let nested = [&before[..1], &[*subcommand], after].concat();
        for argv in [top, nested] {
            assert!(parse(&argv).is_ok(), "{argv:?}");
        }
    }
}

/// The refusal names the flag, though not the level it was first given at.
#[test]
fn config_given_at_two_levels_is_refused() {
    for argv in [
        &["--config", "a", "sync", "--config", "b", "reddit"][..],
        &["--config", "a", "sync", "reddit", "--config", "b"],
        &["--config=a", "sync", "reddit", "--config=b"],
        &["sync", "--config", "a", "github", "--config=b"],
        &["cleanup", "--config=a", "hackernews", "--config", "b"],
        &["sync", "reddit", "--config", "a", "--config", "b"],
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
        assert_eq!(
            refusal(argv),
            "'--config' was given twice; give it once",
            "{argv:?}"
        );
    }
}

/// `--config` given once is taken from wherever it was given, and a `--config` that is a
/// value (of another flag, or a positional after `--`) is not a second one.
#[test]
fn config_given_once_parses_at_any_level() {
    for (argv, config) in [
        (&["--config", "a", "sync", "reddit"][..], Some("a")),
        (&["sync", "--config=a", "reddit"], Some("a")),
        (&["sync", "reddit", "--config", "a"], Some("a")),
        (&["doctor", "--config", "a"], Some("a")),
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
        let cli = parse(argv).unwrap_or_else(|e| panic!("{argv:?}: {}", e.unwrap_stderr()));
        assert_eq!(cli.config.as_deref(), config, "{argv:?}");
    }
    let Command::Config {
        action: ConfigAction::Check { file },
    } = command(&["--config", "a", "config", "check", "--", "--config"])
    else {
        panic!("expected `config check`");
    };
    assert_eq!(file, PathBuf::from("--config"));
}

/// `-v` counts wherever it is given, clusters included.
#[test]
fn verbose_adds_up_across_levels() {
    for (argv, count) in [
        (&["-v", "sync", "-vv", "reddit", "-v"][..], 4),
        (&["-vv", "sync", "reddit"], 2),
        (&["sync", "reddit", "-vvv"], 3),
        (&["--verbose", "cleanup", "-v", "github", "--verbose"], 3),
        (&["sync", "reddit"], 0),
    ] {
        let cli = parse(argv).unwrap_or_else(|e| panic!("{argv:?}: {}", e.unwrap_stderr()));
        assert_eq!(cli.verbose, count, "{argv:?}");
    }
}

/// `PINBOARD_SYNC_ON_AUTH_FAILURE` backs each per-source `--on-auth-failure`, so an
/// explicit flag outranks it, but not `sync`'s top-level one: the `--all` path reads the
/// variable itself (`hook::on_auth_failure_from_env`).
#[test]
fn on_auth_failure_env_backs_only_the_per_source_flags() {
    let _env = FakeSecretEnv::set(&[("PINBOARD_SYNC_ON_AUTH_FAILURE", "env-hook")]);
    let env = Some("env-hook");
    let hook = |argv: &[&str]| match command(argv) {
        Command::Sync(SyncCmd {
            source: Some(SyncSource::Reddit(args)),
            ..
        }) => args.on_auth_failure,
        Command::Sync(SyncCmd {
            source: Some(SyncSource::Github(args)),
            ..
        }) => args.on_auth_failure,
        Command::Backup(BackupCmd {
            target: Some(BackupTarget::Reddit(args)),
            ..
        }) => args.on_auth_failure,
        Command::Backup(BackupCmd {
            target: Some(BackupTarget::Github(args)),
            ..
        }) => args.on_auth_failure,
        _ => panic!("{argv:?} takes no per-source hook"),
    };
    for path in [
        ["sync", "reddit"],
        ["sync", "github"],
        ["backup", "reddit"],
        ["backup", "github"],
    ] {
        assert_eq!(hook(&path).as_deref(), env, "{path:?}");
        let flagged = [&path[..], &["--on-auth-failure", "flag-hook"]].concat();
        let expected = if path[0] == "sync" {
            env
        } else {
            Some("flag-hook")
        };
        assert_eq!(hook(&flagged).as_deref(), expected, "{flagged:?}");
        assert!(
            help(&path).contains("PINBOARD_SYNC_ON_AUTH_FAILURE"),
            "{path:?}"
        );
    }
    let Command::Sync(cmd) = command(&["sync", "--all"]) else {
        panic!("expected `sync`");
    };
    assert_eq!(cmd.on_auth_failure, None);
}

/// A value from the environment is not a placement: the top-level flag beside it still
/// parses, and wins.
#[test]
fn an_env_backed_value_is_not_a_second_placement() {
    let _env = FakeSecretEnv::set(&[("PINBOARD_SYNC_ON_AUTH_FAILURE", "from-env")]);
    let Command::Sync(cmd) = command(&["sync", "--on-auth-failure", "top", "reddit"]) else {
        panic!("expected `sync`");
    };
    let Some(SyncSource::Reddit(args)) = cmd.source else {
        panic!("expected `sync reddit`");
    };
    assert_eq!(args.on_auth_failure.as_deref(), Some("from-env"));
    let overrides = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure);
    assert_eq!(overrides.on_auth_failure.as_deref(), Some("top"));
}

/// No refusal quotes the value of a flag, which may be a secret.
#[test]
fn a_refusal_names_the_flag_not_its_value() {
    for argv in [
        &["backup", "reddit", "--pinboard-token=SECRET"][..],
        &["backup", "reddit", "--pinboard-token", "SECRET"],
        &["sync", "reddit", "--reddit-cookie", "SECRET", "--bogus"],
        &[
            "sync",
            "--on-auth-failure",
            "SECRET",
            "reddit",
            "--on-auth-failure",
            "SECRET2",
        ],
        &["--config", "SECRET", "sync", "--config", "SECRET2"],
        &["backup", "--out", "SECRET", "pinboard", "--out", "SECRET2"],
        &[
            "sync",
            "github",
            "--github-token",
            "SECRET",
            "main",
            "--all",
        ],
    ] {
        let error = refusal(argv);
        assert!(!error.contains("SECRET"), "{argv:?}: {error}");
    }
}

/// `sync --help` tells the user what the hook flag does and where else it comes from,
/// not how the code resolves it.
#[test]
fn sync_help_describes_the_hook_without_internals() {
    let help = help(&["sync"]);
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
    let cleanup = help(&["cleanup", "hackernews"]);
    let sync = help(&["sync", "hackernews"]);
    let backup = help(&["backup", "hackernews"]);
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

/// Every subcommand's `--help` lists the globals the top level takes from it.
#[test]
fn every_subcommand_help_lists_the_globals() {
    for path in COMMAND_PATHS {
        let help = help(path);
        for flag in ["--config=CONFIG", "-v, --verbose"] {
            assert!(help.contains(flag), "{path:?} lacks {flag}: {help}");
        }
    }
}

#[test]
fn completions_takes_a_known_shell() {
    for shell in Shell::ALL {
        let Command::Completions { shell: parsed } = command(&["completions", &shell.to_string()])
        else {
            panic!("expected `completions`");
        };
        assert_eq!(parsed, shell);
    }
    let error = refusal(&["completions", "powershell"]);
    assert!(error.contains("bash, zsh, fish or elvish"), "{error}");
    assert_eq!(Shell::complete("b"), [("bash", None)]);
}

/// No script `eval`s the words typed so far, which would run a `$(...)` among them, and
/// bpaf's own scripts, which do, can't be asked for.
#[test]
fn no_completion_script_evals_the_command_line() {
    for shell in Shell::ALL {
        let script = shell.script(NAME);
        assert!(!script.contains("eval"), "{shell}: {script}");
        // Asked of `words_for_bpaf`, not `parse_cli`: were the refusal missing, bpaf would
        // print its script and exit the test process with 0, which passes.
        let style = format!("--bpaf-complete-style-{shell}");
        for argv in [
            &[style.as_str()][..],
            &["sync", style.as_str()],
            &["help", "--", &style],
        ] {
            let argv: Vec<OsString> = argv.iter().map(OsString::from).collect();
            let Err(failure) = words_for_bpaf(&argv) else {
                panic!("{argv:?} reached bpaf");
            };
            let error = failure.unwrap_stderr();
            assert!(error.contains("completions <shell>"), "{argv:?}: {error}");
        }
    }
}

/// Each script asks the binary for candidates in an output format this bpaf answers, so
/// a bpaf upgrade that drops one fails here rather than in a user's shell.
#[test]
fn each_completion_script_speaks_a_revision_bpaf_answers() {
    for shell in Shell::ALL {
        let script = shell.script(NAME);
        assert!(script.contains(NAME), "{shell}: {script}");
        let revision: usize = script
            .split("--bpaf-complete-rev=")
            .nth(1)
            .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|digits| digits.parse().ok())
            .unwrap_or_else(|| panic!("{shell} names no revision: {script}"));
        let args = Args::from(&["sync", ""][..])
            .set_comp(revision)
            .set_name(NAME);
        let Err(ParseFailure::Completion(candidates)) = parser().run_inner(args) else {
            panic!("{shell}: revision {revision} completed nothing");
        };
        // Once: the copy of the sources `all_or` keeps for `--all` beside one is hidden.
        let reddit = "Sync saved Reddit posts";
        assert_eq!(
            candidates.matches(reddit).count(),
            1,
            "{shell}: {candidates}"
        );
    }
}

#[test]
fn the_man_page_covers_every_command() {
    let page = man_page();
    assert!(page.contains(".TH pinboard-sync 1"), "{page}");
    for path in COMMAND_PATHS {
        let title = [&["PINBOARD-SYNC"], *path]
            .concat()
            .join("\\ ")
            .to_uppercase();
        assert!(page.contains(&format!(".SH {title}\\ \n")), "{path:?}");
    }
}

/// A value that isn't UTF-8 is refused before bpaf, which would quote it.
#[test]
fn an_argument_that_is_not_utf8_is_refused_without_quoting_it() {
    use std::os::unix::ffi::OsStringExt;
    let cookie = OsString::from_vec(b"SECRET\xff".to_vec());
    for argv in [
        vec![
            "sync".into(),
            "reddit".into(),
            "--reddit-cookie".into(),
            cookie.clone(),
        ],
        vec!["sync".into(), "reddit".into(), {
            let mut flag = OsString::from("--reddit-cookie=");
            flag.push(&cookie);
            flag
        }],
    ] {
        let argv: Vec<OsString> = std::iter::once("pinboard-sync".into())
            .chain(argv)
            .collect();
        let Err(failure) = parse_cli(argv) else {
            panic!("a non-UTF-8 cookie parsed");
        };
        let error = failure.unwrap_stderr();
        assert_eq!(error, "an argument is not valid UTF-8");
    }
}

/// bpaf 0.9 writes bash's `_filedir` with no newline, so a candidate after it runs on
/// and bash completes no files; `file_completion` ends the line.
#[test]
fn bash_completes_files_for_config_check() {
    let args = Args::from(&["config", "check", ""][..])
        .set_comp(8)
        .set_name(NAME);
    let Err(ParseFailure::Completion(candidates)) = parser().run_inner(args) else {
        panic!("no completion");
    };
    assert!(candidates.contains("_filedir\n"), "{candidates}");
}

/// Each source's arguments carry every override they hold into the job builders'.
#[test]
fn each_sources_arguments_carry_their_overrides() {
    let some = |value: &str| Some(value.to_string());
    let flags = || SyncFlags {
        limit: Some(SyncLimit::All),
        ..SyncFlags::default()
    };
    let carried = |overrides: SyncOverrides| {
        (
            [
                overrides.reddit_username,
                overrides.reddit_cookie,
                overrides.github_token,
                overrides.hackernews_username,
                overrides.on_auth_failure,
            ],
            overrides.flags.limit,
            overrides.dry_run,
        )
    };
    let all = Some(SyncLimit::All);

    let reddit = RedditSyncArgs {
        all: false,
        reddit_username: some("user"),
        reddit_cookie: some("cookie"),
        flags: flags(),
        on_auth_failure: some("hook"),
        dry_run: true,
        account: None,
    };
    let expected = [some("user"), some("cookie"), None, None, some("hook")];
    assert_eq!(carried(reddit.into()), (expected, all, true));

    let github = GitHubSyncArgs {
        all: false,
        github_token: some("token"),
        flags: flags(),
        on_auth_failure: some("hook"),
        dry_run: true,
        account: None,
    };
    let expected = [None, None, some("token"), None, some("hook")];
    assert_eq!(carried(github.into()), (expected, all, true));

    let hackernews = HackerNewsSyncArgs {
        all: false,
        username: some("hn"),
        flags: flags(),
        dry_run: true,
        account: None,
    };
    let expected = [None, None, None, some("hn"), None];
    assert_eq!(carried(hackernews.into()), (expected, all, true));

    // A backup writes nothing to Pinboard, so it carries only credentials and the hook.
    let reddit = RedditBackupArgs {
        all: false,
        reddit_username: some("user"),
        reddit_cookie: some("cookie"),
        on_auth_failure: some("hook"),
        out: OutFlag::default(),
        dry_run: true,
        account: None,
    };
    let expected = [some("user"), some("cookie"), None, None, some("hook")];
    assert_eq!(carried(reddit.into()), (expected, None, false));

    let github = GitHubBackupArgs {
        all: false,
        github_token: some("token"),
        on_auth_failure: some("hook"),
        out: OutFlag::default(),
        dry_run: true,
        account: None,
    };
    let expected = [None, None, some("token"), None, some("hook")];
    assert_eq!(carried(github.into()), (expected, None, false));

    let hackernews = HackerNewsBackupArgs {
        all: false,
        username: some("hn"),
        out: OutFlag::default(),
        dry_run: true,
        account: None,
    };
    let expected = [None, None, None, some("hn"), None];
    assert_eq!(carried(hackernews.into()), (expected, None, false));
}
