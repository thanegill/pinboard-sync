use super::*;
use crate::test_support::warnings;
use std::num::NonZeroUsize;

/// Override values chosen so each setting is distinct from the others and from the
/// `Config::default()` globals — so a builder that wires a flag into the *wrong*
/// `SyncJob` field (e.g. `public` into the `read_later` slot) fails the assertions.
fn wiring_overrides() -> SyncOverrides {
    SyncOverrides {
        flags: SyncFlags {
            limit: Some(SyncLimit::Max(NonZeroUsize::new(7).unwrap())),
            toread: Some(true),        // global default is false
            public: Some(false),       // distinct from toread, so a swap is caught
            use_post_date: Some(true), // global default is false
            max_age_days: Some(99),    // global default is 30
            ..Default::default()
        },
        ..Default::default()
    }
}

fn assert_wired(job: &SyncJob) {
    let settings = &job.settings;
    assert_eq!(
        settings.limit,
        SyncLimit::Max(NonZeroUsize::new(7).unwrap()),
        "limit"
    );
    assert!(settings.read_later, "read_later");
    assert!(!settings.public, "public (from --public=false)");
    assert!(settings.dates.use_post_date, "use_post_date");
    assert_eq!(settings.dates.max_age_days, 99, "max_age_days");
}

/// The write settings of a job a test only fetches through: everything off but `limit`.
pub(super) fn fetch_only(limit: SyncLimit) -> JobSettings {
    JobSettings {
        limit,
        read_later: false,
        public: false,
        dates: crate::test_support::NO_DATING,
    }
}

#[test]
fn build_job_wires_reddit_overrides_to_job_fields() {
    let overrides = SyncOverrides {
        reddit_username: Some("alice".into()),
        reddit_cookie: Some("reddit_session=x".into()),
        ..wiring_overrides()
    };
    let job = build_job::<RedditAccount>(None, &overrides, &Config::default()).expect("builds");
    assert_wired(&job);
}

#[test]
fn build_job_wires_github_overrides_to_job_fields() {
    let overrides = SyncOverrides {
        github_token: Some("ghp_test".into()),
        ..wiring_overrides()
    };
    let job = build_job::<GitHubAccount>(None, &overrides, &Config::default()).expect("builds");
    assert_wired(&job);
}

#[test]
fn build_job_wires_hackernews_overrides_to_job_fields() {
    let overrides = SyncOverrides {
        hackernews_username: Some("alice".into()),
        ..wiring_overrides()
    };
    let job = build_job::<HackerNewsAccount>(None, &overrides, &Config::default()).expect("builds");
    assert_wired(&job);
}

#[test]
fn a_hackernews_job_keys_articles_on_a_configured_reddit_domain_as_reddit() {
    let config = Config {
        reddit: vec![RedditAccount {
            reddit_domain: Some("reddit.mirror.example".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let overrides = SyncOverrides {
        hackernews_username: Some("alice".into()),
        ..Default::default()
    };
    let job = build_job::<HackerNewsAccount>(None, &overrides, &config).expect("builds");
    assert_eq!(
        job.client
            .dedup_key(&crate::test_support::url(
                "https://reddit.mirror.example/r/rust/comments/abc/title/"
            ))
            .as_deref(),
        Some("/r/rust/comments/abc/title")
    );
}

/// Another account's sync, or a `cleanup reddit` run as it, leaves a save on that
/// account's mirror; this account's sync must still see it as the same save.
#[test]
fn a_save_on_another_reddit_accounts_mirror_is_not_re_added() {
    use crate::test_support::stored_at;

    let account = |name: &str, domain: &str| RedditAccount {
        username: Some(name.into()),
        cookie: Some("reddit_session=x".into()),
        reddit_domain: Some(domain.into()),
        ..Default::default()
    };
    let config = Config {
        reddit: vec![
            account("alice", "www.reddit.com"),
            account("bob", "reddit.mirror.example"),
        ],
        ..Default::default()
    };
    let alices_save = BookmarkDraft {
        bookmark: stored_at("https://www.reddit.com/r/rust/comments/abc/title/"),
        dedup_key: "/r/rust/comments/abc/title".into(),
    };
    let stored = [stored_at(
        "https://reddit.mirror.example/r/rust/comments/abc/title/",
    )];

    let alices =
        build_job(Some(&config.reddit[0]), &SyncOverrides::default(), &config).expect("builds");
    let new = crate::sync::prepare_new_drafts(
        &alices.client,
        vec![alices_save],
        &stored,
        &fetch_only(SyncLimit::All),
        time::OffsetDateTime::UNIX_EPOCH,
    );
    assert!(new.is_empty(), "re-added: {:?}", new[0].bookmark.url);
}

/// `sync --all`'s build phase must isolate a per-account build failure so the
/// healthy accounts still yield jobs. Here a GitHub account with no resolvable
/// token (no inline token, no `token_file`, and this test sets no fake `$GITHUB_TOKEN`)
/// fails to build; it must be counted, not abort the whole run.
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
    let (jobs, skipped) = build_all_jobs(&config, &SyncOverrides::default());
    assert_eq!(jobs.len(), 1, "healthy reddit account still builds");
    assert_eq!(jobs[0].label, "reddit[alice]");
    let skipped: Vec<&str> = skipped.iter().map(|target| target.label.as_str()).collect();
    assert_eq!(
        skipped,
        ["github[default]"],
        "the tokenless github account is skipped, not aborted"
    );
}

#[test]
fn build_jobs_for_every_account_of_a_source_isolates_a_failing_account() {
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
    let (jobs, skipped) =
        build_jobs::<GitHubAccount>(None, true, &SyncOverrides::default(), &config)
            .expect("a failing account does not abort the source");
    let labels: Vec<&str> = jobs.iter().map(|job| job.label.as_str()).collect();
    assert_eq!(labels, ["github[main]"]);
    let skipped: Vec<&str> = skipped.iter().map(|target| target.label.as_str()).collect();
    assert_eq!(skipped, ["github[tokenless]"]);
}

fn reddit_account(name: &str) -> RedditAccount {
    RedditAccount {
        name: Some(name.into()),
        username: Some(name.into()),
        cookie: Some(format!("reddit_session={name}")),
        ..Default::default()
    }
}

/// `$REDDIT_COOKIE_FILE` outranks each account's own cookie, so with two accounts both
/// silently use it: worth one warning for the source, naming the setting, not its value.
#[tokio::test]
async fn an_env_secret_shared_by_several_accounts_is_warned_about_once() {
    let scratch = crate::test_support::scratch_dir("shared-env-secret");
    let cookie_file = scratch.path().join("cookie");
    std::fs::write(&cookie_file, "reddit_session=SHAREDVALUE\n").unwrap();
    let _env = crate::secrets::FakeSecretEnv::set(&[(
        "REDDIT_COOKIE_FILE",
        cookie_file.to_str().unwrap(),
    )]);
    let config = Config {
        reddit: vec![reddit_account("alice"), reddit_account("bob")],
        ..Default::default()
    };

    let ((jobs, _), logs) = crate::test_support::capturing_logs(async {
        build_all_jobs(&config, &SyncOverrides::default())
    })
    .await;

    assert_eq!(jobs.len(), 2);
    assert_eq!(
        warnings(&logs),
        [
            "WARN $REDDIT_COOKIE_FILE is set in the environment, so all 2 configured \
             reddit accounts use it, whatever their own config sets"
        ]
    );
}

#[tokio::test]
async fn an_env_secret_is_not_warned_about_for_one_account_or_under_its_flag() {
    let _env = crate::secrets::FakeSecretEnv::set(&[("REDDIT_COOKIE", "reddit_session=x")]);
    let one = Config {
        reddit: vec![reddit_account("alice")],
        ..Default::default()
    };
    let two = Config {
        reddit: vec![reddit_account("alice"), reddit_account("bob")],
        ..Default::default()
    };
    let flagged = SyncOverrides {
        reddit_cookie: Some("reddit_session=flag".into()),
        ..Default::default()
    };

    let (_, logs) = crate::test_support::capturing_logs(async {
        build_all_jobs(&one, &SyncOverrides::default());
        build_all_jobs(&two, &flagged);
    })
    .await;

    assert_eq!(warnings(&logs), Vec::<&str>::new());
}

/// The ladder skips an env value it would not take (an empty `_FILE`, a cookie naming no
/// session), so each account keeps its own and there is nothing to warn about.
#[tokio::test]
async fn an_env_secret_the_ladder_skips_is_not_warned_about() {
    let scratch = crate::test_support::scratch_dir("skipped-env-secret");
    let empty_file = scratch.path().join("empty");
    std::fs::write(&empty_file, "\n").unwrap();
    let _env = crate::secrets::FakeSecretEnv::set(&[
        ("REDDIT_COOKIE", "reddit_session="),
        ("REDDIT_USERNAME_FILE", empty_file.to_str().unwrap()),
    ]);
    let config = Config {
        reddit: vec![reddit_account("alice"), reddit_account("bob")],
        ..Default::default()
    };

    let (_, logs) = crate::test_support::capturing_logs(async {
        build_all_jobs(&config, &SyncOverrides::default())
    })
    .await;

    assert_eq!(warnings(&logs), Vec::<&str>::new());
}

/// The warning names the rung the ladder takes: `$REDDIT_COOKIE` names no session, so
/// the cookie comes from `$REDDIT_COOKIE_FILE`. A blank `--github-token` falls through
/// to `$GITHUB_TOKEN`, so that is warned about too.
#[tokio::test]
async fn an_env_secret_warning_names_the_rung_the_ladder_takes() {
    let scratch = crate::test_support::scratch_dir("env-secret-rung");
    let cookie_file = scratch.path().join("cookie");
    std::fs::write(&cookie_file, "reddit_session=SHAREDVALUE\n").unwrap();
    let _env = crate::secrets::FakeSecretEnv::set(&[
        ("REDDIT_COOKIE", "reddit_session="),
        ("REDDIT_COOKIE_FILE", cookie_file.to_str().unwrap()),
        ("GITHUB_TOKEN", "SHAREDVALUE"),
    ]);
    let github_account = |name: &str| GitHubAccount {
        name: Some(name.into()),
        ..Default::default()
    };
    let config = Config {
        reddit: vec![reddit_account("alice"), reddit_account("bob")],
        github: vec![github_account("a"), github_account("b")],
        ..Default::default()
    };
    let blank_token_flag = SyncOverrides {
        github_token: Some(" ".into()),
        ..Default::default()
    };

    let (_, logs) =
        crate::test_support::capturing_logs(async { build_all_jobs(&config, &blank_token_flag) })
            .await;

    assert_eq!(
        warnings(&logs),
        [
            "WARN $REDDIT_COOKIE_FILE is set in the environment, so all 2 configured \
             reddit accounts use it, whatever their own config sets",
            "WARN $GITHUB_TOKEN is set in the environment, so all 2 configured github \
             accounts use it, whatever their own config sets",
        ]
    );
}

#[test]
fn each_skipped_target_counts_as_a_failed_account() {
    let skipped = |label: &str| SkippedTarget {
        label: label.into(),
        reason: anyhow::anyhow!("no token"),
    };
    let mut run = AllRun::default();
    let labels = run.record_skipped(
        vec![skipped("github[a]"), skipped("reddit[b]")],
        "could not be built",
    );
    assert_eq!(labels, ["github[a]", "reddit[b]"]);
    let error = run.finish().expect_err("a skipped target fails the run");
    assert_eq!(error.to_string(), "2 account(s) failed");
}

/// Every setting the ladder reads for a secret, `$VAR_FILE` included, is one an operator
/// can use, so a "missing X" message names all of them.
#[test]
fn a_missing_secret_names_every_setting_it_can_come_from() {
    let config = Config::default();
    let overrides = SyncOverrides::default();
    let error = |job: Result<SyncJob>| format!("{:#}", job.err().expect("no secret is set"));
    assert_eq!(
        error(build_job::<RedditAccount>(None, &overrides, &config)),
        "missing Reddit username (set --reddit-username, $REDDIT_USERNAME, \
         $REDDIT_USERNAME_FILE, or the config's `username`)"
    );
    assert_eq!(
        error(build_job::<HackerNewsAccount>(None, &overrides, &config)),
        "missing HackerNews username (set --hn-username, $HN_USERNAME, $HN_USERNAME_FILE, \
         or the config's `username`)"
    );
    assert_eq!(
        format!(
            "{:#}",
            build_pinboard_client(None, &config)
                .err()
                .expect("no token")
        ),
        "missing Pinboard token (set --pinboard-token, $PINBOARD_TOKEN, $PINBOARD_TOKEN_FILE, \
         or `[pinboard]`'s `token` or `token_file`)"
    );
}
