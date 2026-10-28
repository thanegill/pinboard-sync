use super::*;
use crate::cli::{BackupTarget, Command, RedditSyncArgs};
use crate::jobs::build_job;

#[test]
fn link_tag_override_rejects_whitespace_and_commas() {
    let refused = |tag: &str| {
        resolve_hackernews_cleanup_config(None, Some(tag.into()))
            .unwrap_err()
            .to_string()
    };
    assert_eq!(
        refused("find hn"),
        "`--tag-link` must not contain whitespace"
    );
    assert_eq!(refused("find,hn"), "`--tag-link` must not contain a comma");
}

#[tokio::test]
async fn a_hackernews_cleanup_refuses_a_bad_link_tag_before_its_pass() {
    let pinboard = crate::test_support::FakePinboard::default();
    let overrides = HackerNewsCleanupOverrides {
        link_tag: Some("find hn".into()),
        ..Default::default()
    };
    let error = cleanup_hackernews_account(
        None,
        overrides,
        crate::test_support::NO_DATING,
        &pinboard.cleanup_store(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "`--tag-link` must not contain whitespace"
    );
}

#[test]
fn link_tag_override_accepts_a_bare_tag() {
    let hn_config =
        resolve_hackernews_cleanup_config(None, Some("find-hn".into())).expect("valid tag");
    assert_eq!(hn_config.link_tag, "find-hn");
}

/// Parse `pinboard-sync <subcommand> <extra>` and return the command.
fn parse(subcommand: &str, extra: &[&str]) -> Command {
    let argv = ["pinboard-sync", subcommand]
        .into_iter()
        .chain(extra.iter().copied());
    crate::test_support::parse_cli(argv)
        .expect("parses")
        .command
}

/// Parse `sync <extra>` and return the top-level command.
fn parse_sync(extra: &[&str]) -> SyncCmd {
    let Command::Sync(cmd) = parse("sync", extra) else {
        panic!("expected `sync` command");
    };
    cmd
}

/// The `sync reddit` args of a parsed `sync`.
fn reddit_sync_args(cmd: &mut SyncCmd) -> RedditSyncArgs {
    let Some(SyncSource::Reddit(args)) = cmd.source.take() else {
        panic!("expected `sync reddit` args");
    };
    args
}

/// Both `--all` forms must carry the accounts they could not build out in
/// `SyncPlan::skipped`, or a run that synced only the healthy ones exits 0.
#[test]
fn sync_all_counts_the_accounts_it_could_not_build() {
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
async fn a_dry_sync_reports_no_write_tally() {
    let pinboard = PinboardClient::new("user:token".into(), std::time::Duration::ZERO).unwrap();
    let (result, logs) = crate::test_support::capturing_logs(run_sync_jobs(
        Vec::new(),
        AllRun::default(),
        &pinboard,
        &[],
        WriteMode::DryRun,
    ))
    .await;

    result.unwrap();
    assert!(
        logs.contains(&"INFO 0 new bookmark(s) to write (dry run)".to_string()),
        "{logs:?}"
    );
    assert!(
        !logs.iter().any(|line| line.contains("done: wrote")),
        "{logs:?}"
    );
}

#[tokio::test]
async fn run_sync_jobs_fails_the_run_for_accounts_it_could_not_build() {
    let pinboard = PinboardClient::new("user:token".into(), std::time::Duration::ZERO).unwrap();
    let error = run_sync_jobs(
        Vec::new(),
        AllRun {
            failed: 1,
            ..AllRun::default()
        },
        &pinboard,
        &[],
        WriteMode::Live,
    )
    .await
    .expect_err("an unbuilt account fails the run");
    assert_eq!(error.to_string(), "1 account(s) failed");
}

#[test]
fn top_level_all_threads_on_auth_failure_into_the_hook() {
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
    // The NixOS service runs `sync --all` with the hook supplied only via
    // `PINBOARD_SYNC_ON_AUTH_FAILURE`. The top-level flag doesn't bind that env,
    // so the `--all` arm must read it directly or the hook silently never fires.
    let _env = crate::secrets::FakeSecretEnv::set(&[("PINBOARD_SYNC_ON_AUTH_FAILURE", "env-hook")]);
    let cmd = parse_sync(&["--all"]);

    let overrides = SyncOverrides {
        dry_run: cmd.dry_run,
        on_auth_failure: cmd.on_auth_failure.or_else(on_auth_failure_from_env),
        reddit_username: Some("alice".into()),
        reddit_cookie: Some("reddit_session=x".into()),
        ..SyncOverrides::default()
    };

    let job = build_job::<RedditAccount>(None, &overrides, &Config::default()).expect("builds");
    assert_eq!(job.hook.as_deref(), Some("env-hook"));
}

#[test]
fn top_level_on_auth_failure_reaches_single_source_hook() {
    // `--on-auth-failure` placed before the source subcommand must reach the job just like
    // the `--all` path: the source's own copy never holds a command-line flag, so only
    // `with_top_level_hook` carries it.
    let mut cmd = parse_sync(&[
        "--on-auth-failure",
        "top-hook",
        "reddit",
        "--reddit-username",
        "alice",
        "--reddit-cookie",
        "reddit_session=x",
    ]);
    let args = reddit_sync_args(&mut cmd);
    assert_eq!(args.on_auth_failure, None);

    let overrides = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure);
    let job = build_job::<RedditAccount>(None, &overrides, &Config::default()).expect("builds");
    assert_eq!(job.hook.as_deref(), Some("top-hook"));
}

#[test]
fn on_auth_failure_after_the_source_reaches_its_hook() {
    let mut cmd = parse_sync(&[
        "reddit",
        "--reddit-username",
        "alice",
        "--reddit-cookie",
        "reddit_session=x",
        "--on-auth-failure",
        "source-hook",
    ]);
    let args = reddit_sync_args(&mut cmd);

    let overrides = SyncOverrides::from(args).with_top_level_hook(cmd.on_auth_failure);
    let job = build_job::<RedditAccount>(None, &overrides, &Config::default()).expect("builds");
    assert_eq!(job.hook.as_deref(), Some("source-hook"));
}

/// Parse `cleanup <extra>` and return the top-level command.
fn parse_cleanup(extra: &[&str]) -> CleanupCmd {
    let Command::Cleanup(cmd) = parse("cleanup", extra) else {
        panic!("expected `cleanup` command");
    };
    cmd
}

/// The `cleanup reddit` args of a parsed `cleanup`.
fn reddit_cleanup_args(cmd: &mut CleanupCmd) -> RedditCleanupArgs {
    let Some(CleanupSource::Reddit(args)) = cmd.source.take() else {
        panic!("expected `cleanup reddit` args");
    };
    args
}

#[test]
fn top_level_use_post_date_reaches_single_source_cleanup() {
    // `--use-post-date` placed before the source subcommand must reach the cleanup pass:
    // the source's own `dates` never holds a command-line flag, so only `with_top_level`
    // carries it.
    let mut cmd = parse_cleanup(&["--use-post-date", "reddit", "alice"]);
    assert_eq!(cmd.dates.use_post_date, Some(true));
    let args = reddit_cleanup_args(&mut cmd);
    assert_eq!(args.dates.use_post_date, None);

    let overrides = args.dates.with_top_level(&cmd.dates);
    assert_eq!(overrides.use_post_date, Some(true));
}

#[test]
fn a_cleanup_date_flag_after_the_source_still_applies() {
    let mut cmd = parse_cleanup(&["reddit", "alice", "--use-post-date=false"]);
    let args = reddit_cleanup_args(&mut cmd);

    let overrides = args.dates.with_top_level(&cmd.dates);
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
        assert_eq!(
            WriteMode::from_flags(cmd.dry_run, source_dry_run(&source)),
            WriteMode::DryRun,
            "`cleanup --dry-run {name}`"
        );
    }
}

#[test]
fn a_dry_run_after_the_source_still_applies() {
    let cmd = parse_cleanup(&["github", "--dry-run"]);
    let source = cmd.source.expect("a source subcommand");
    assert_eq!(
        WriteMode::from_flags(cmd.dry_run, source_dry_run(&source)),
        WriteMode::DryRun
    );
}

/// Parse `backup <extra>` and return the parsed command.
fn parse_backup(extra: &[&str]) -> BackupCmd {
    let Command::Backup(cmd) = parse("backup", extra) else {
        panic!("expected `backup` args");
    };
    cmd
}

#[tokio::test]
async fn run_sync_refuses_all_with_no_configured_account() {
    let error = run_sync(parse_sync(&["--all"]), &Config::default())
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "--all requires a --config with at least one configured account"
    );
}

/// `sync <source> --all` names the source it found no account of, as `backup` does.
#[tokio::test]
async fn run_sync_refuses_a_source_all_with_no_account_of_that_source() {
    for source in ["reddit", "github", "hackernews"] {
        let error = run_sync(parse_sync(&[source, "--all"]), &Config::default())
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("--all requires a --config with at least one configured {source} account")
        );
    }
}

#[tokio::test]
async fn run_backup_refuses_an_empty_out() {
    let cmd = parse_backup(&["pinboard", "--pinboard-token", "u:x", "--out", ""]);
    let error = run_backup(cmd, &Config::default()).await.unwrap_err();
    assert_eq!(error.to_string(), "`--out` must not be empty");
}

/// `--dry-run` before the target counts as much as after it. The run still fails over its
/// skipped targets, but writes nothing, the manifest included.
#[tokio::test]
async fn a_backup_dry_run_given_before_the_target_writes_no_manifest() {
    let scratch = crate::test_support::scratch_dir("backup-top-dry-run");
    let out = scratch.path().to_str().unwrap();
    let config = Config {
        github: vec![GitHubAccount::default()],
        ..Default::default()
    };

    let result = run_backup(parse_backup(&["--dry-run", "--out", out, "--all"]), &config).await;

    assert!(result.is_err(), "the skipped targets fail the run");
    assert!(!scratch.path().join("manifest.json").exists());
}

#[test]
fn backup_accepts_out_before_or_after_the_target() {
    let cmd = parse_backup(&["--all", "--out", "/snap"]);
    assert!(cmd.all);
    assert_eq!(cmd.out.out.as_deref(), Some("/snap"));

    // `--out` after the target is honored too, as `--dry-run` is on `sync`.
    let mut cmd = parse_backup(&["reddit", "main", "--out", "/snap"]);
    let top_out = cmd.out.out.take();
    let Some(BackupTarget::Reddit(args)) = cmd.target else {
        panic!("expected the reddit target");
    };
    assert_eq!(args.account.as_deref(), Some("main"));
    let dir = resolve_backup_dir([args.out.out, top_out], &Config::default()).expect("a dir");
    assert_eq!(dir, PathBuf::from("/snap"));

    // Backing up a source never contacts Pinboard, so it takes no token.
    assert!(crate::test_support::parse_cli([
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
    let err = build_backup_plan(cmd, &config)
        .err()
        .expect("should be rejected");
    assert!(
        err.to_string().contains("--all cannot be combined"),
        "{err}"
    );

    let cmd = parse_backup(&[]);
    let err = build_backup_plan(cmd, &config)
        .err()
        .expect("should be rejected");
    assert!(err.to_string().contains("specify a target"), "{err}");
}

#[test]
fn backup_pinboard_without_a_token_names_every_setting_it_can_come_from() {
    let error = build_backup_plan(parse_backup(&["pinboard"]), &Config::default())
        .err()
        .expect("no token is set");
    assert_eq!(
        format!("{error:#}"),
        "missing Pinboard token (set --pinboard-token, $PINBOARD_TOKEN, $PINBOARD_TOKEN_FILE, \
         or `[pinboard]`'s `token` or `token_file`)"
    );
}

#[test]
fn backup_dir_prefers_the_flag_then_the_config_then_errors() {
    let mut config = Config::default();
    resolve_backup_dir([None, None], &config).unwrap_err();

    config.backup.directory = Some("/from-config".into());
    assert_eq!(
        resolve_backup_dir([None, None], &config).unwrap(),
        PathBuf::from("/from-config")
    );
    assert_eq!(
        resolve_backup_dir([Some("/from-flag".into()), None], &config).unwrap(),
        PathBuf::from("/from-flag")
    );
    let refused = |flags: [Option<&str>; 2]| {
        resolve_backup_dir(flags.map(|flag| flag.map(String::from)), &config)
            .unwrap_err()
            .to_string()
    };
    assert_eq!(
        resolve_backup_dir([Some("/after".into()), Some("/before".into())], &config).unwrap(),
        PathBuf::from("/after"),
        "the `--out` after the target comes first, though the command line gives only one"
    );
    for blank in ["", " "] {
        for flags in [
            [Some(blank), None],
            [None, Some(blank)],
            [Some("/y"), Some(blank)],
        ] {
            assert_eq!(refused(flags), "`--out` must not be empty", "{flags:?}");
        }
    }
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
    let plan = build_backup_plan(parse_backup(&["--all"]), &config).expect("plans");
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

    let plan = build_backup_plan(parse_backup(&["--all"]), &config).expect("plans");
    let labels: Vec<&str> = plan.jobs.iter().map(|job| job.label.as_str()).collect();
    assert_eq!(labels, ["reddit[alice]", "pinboard"]);
    assert_eq!(skipped_labels(&plan.skipped), ["github[default]"]);

    // The planned jobs are dropped so nothing is contacted: what is under test is how
    // the run accounts for the target it could not build.
    let scratch = crate::test_support::scratch_dir("backup-skipped-account");
    let dir = scratch.path().join("snapshot");
    let result = back_up(Vec::new(), plan.skipped, &dir, WriteMode::Live).await;

    let err = result.expect_err("a skipped account fails the run");
    assert_eq!(err.to_string(), "1 account(s) failed");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
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
    let plan = build_backup_plan(parse_backup(&["github", "--all"]), &config).expect("plans");
    let labels: Vec<&str> = plan.jobs.iter().map(|job| job.label.as_str()).collect();
    assert_eq!(labels, ["github[main]"]);
    assert_eq!(skipped_labels(&plan.skipped), ["github[tokenless]"]);
}

/// `cleanup <argv>`'s error against `config_text`, with no Pinboard token: the first
/// refusal it hits, else the missing Pinboard token `posts/all` would need.
async fn cleanup_error(config_text: &str, argv: &[&str]) -> String {
    let config = Config::parse(config_text).expect("valid config");
    let error = run_cleanup(parse_cleanup(argv), &config)
        .await
        .expect_err("no Pinboard token");
    format!("{error:#}")
}

#[tokio::test]
async fn cleanup_refuses_a_zero_cap_with_stale_to_now_before_reading_pinboard() {
    let refused = |pass: &str, cap_from: &str, stale_from: &str| {
        format!(
            "{pass} resolves `post_date_max_age_days` to 0 (from {cap_from}) with \
             `cleanup_stale_to_now` on (from {stale_from}), so cleanup would re-date its \
             stale bookmarks on every run; raise the cap or turn `cleanup_stale_to_now` off"
        )
    };
    let flags = |pass: &str| {
        refused(
            pass,
            "`--post-date-max-age-days`",
            "`--cleanup-stale-to-now`",
        )
    };
    let both = ["--post-date-max-age-days", "0", "--cleanup-stale-to-now"];
    for source in ["github", "reddit", "hackernews"] {
        let pass = format!("`cleanup {source}`");
        assert_eq!(
            cleanup_error("", &[&[source][..], &both].concat()).await,
            flags(&pass)
        );
        assert_eq!(
            cleanup_error("", &[&both[..], &[source]].concat()).await,
            flags(&pass)
        );
        // The old name, a hidden alias, still names the new flag in the message.
        assert_eq!(
            cleanup_error(
                "[pinboard]\ncleanup_stale_to_now = true\n",
                &[source, "--max-age-days", "0"]
            )
            .await,
            refused(&pass, "`--post-date-max-age-days`", "`[pinboard]`")
        );
    }

    let every_source = "[[github]]\nname = \"a\"\ntoken = \"t\"\n[[reddit]]\nusername = \"u\"\n\
                        [[hackernews]]\nusername = \"u\"\n";
    assert_eq!(
        cleanup_error(every_source, &[&["--all"][..], &both].concat()).await,
        flags("`cleanup --all` (github pass)")
    );
    assert_eq!(
        cleanup_error(
            &format!("[defaults.reddit]\npost_date_max_age_days = 0\n{every_source}"),
            &["--all", "--cleanup-stale-to-now"]
        )
        .await,
        refused(
            "`cleanup --all` (reddit pass)",
            "`[defaults.reddit]`",
            "`--cleanup-stale-to-now`"
        )
    );

    for argv in [
        &["github", "--post-date-max-age-days", "0"][..],
        &["--all", "--cleanup-stale-to-now"],
    ] {
        let error = cleanup_error(every_source, argv).await;
        assert!(
            error.starts_with("missing Pinboard token"),
            "{argv:?}: {error}"
        );
    }
}

#[tokio::test]
async fn cleanup_refuses_a_bad_source_input_before_reading_pinboard() {
    // With no Pinboard token either, reaching `posts/all` first would fail on that token
    // instead, so each source's own error proves the rate-limited read was never made.
    for (argv, missing) in [
        (&["github"][..], "missing GitHub token"),
        (&["reddit"], "missing Reddit cookie"),
        (&["reddit", "--no-nsfw"], "missing Reddit cookie"),
        (&["reddit", "--no-titles"], "missing Reddit cookie"),
        (
            &["reddit", "--no-nsfw", "--no-titles", "--use-post-date"],
            "missing Reddit cookie",
        ),
        (
            &["reddit", "--no-nsfw", "--no-titles"],
            "missing Pinboard token",
        ),
    ] {
        let error = cleanup_error("", argv).await;
        assert!(error.starts_with(missing), "{argv:?}: {error}");
    }
    assert_eq!(
        cleanup_error("", &["hackernews", "--tag-link", "find hn"]).await,
        "`--tag-link` must not contain whitespace"
    );
}

mod net_tests {
    use super::*;
    use crate::test_support::{stored_at, tag_list, FakePinboard, NO_DATING};
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
             tag_lang_prefix = \"language:\"\ntag_author_prefix = \"owner:\"\n",
        )
        .unwrap();
        let pinboard = FakePinboard {
            all: vec![Bookmark {
                tags: tag_list(&["work-star"]),
                ..stored_at("https://github.com/o/r")
            }],
            ..Default::default()
        };
        let home = &config.github[0];
        let client = GitHubClient::with_base_url(
            "tok".into(),
            GitHubConfig::resolve(Some(home), &config.defaults.github),
            server.uri(),
        );

        cleanup_github_with_client(
            &pinboard.cleanup_store(),
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
        assert_eq!(updated[0].tags, ["work-star", "language:rust", "owner:o"]);
    }
}
