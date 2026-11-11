//! `backup` jobs, one per target: a client that can [`dump`](BackupSource::dump) and the
//! stem its files are named by. Built through [`SourceAccount`] as `sync` jobs are, so
//! account selection and the secret ladder match, with Pinboard as one more target.

use anyhow::{anyhow, bail, Context, Result};
use log::warn;

use super::{
    build_every, build_job, build_selected, pinboard_client_for_token, Built, Selection,
    SkippedTarget, SourceAccount, SourceClient, SyncJob, SyncOverrides,
};
use crate::backup::{self, BackupSource};
use crate::bookmark::WriteMode;
use crate::cli::{BackupCmd, BackupTarget};
use crate::config::{self, Config, GitHubAccount, HackerNewsAccount, RedditAccount};
use crate::hook::on_auth_failure_from_env;
use crate::pinboard::PinboardClient;
use crate::secrets::{resolve_pinboard_token, SecretSpec};
use crate::source::SourceError;

/// One backup target: which client to dump and what to name its files.
pub struct BackupJob {
    pub client: BackupClient,
    /// Filename stem, e.g. `reddit-main` or `pinboard`.
    pub stem: String,
    pub label: String,
    pub hook: Option<String>,
}

/// A backup target's client: Pinboard is no `Source`, so it can't join `SourceClient`.
pub enum BackupClient {
    /// Boxed: a `SourceClient` is several times a `PinboardClient`'s size.
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

/// What a `backup` invocation resolves to: its jobs, what it couldn't cover, and its
/// target's own `--out` and `--dry-run`.
pub struct BackupPlan {
    pub jobs: Vec<BackupJob>,
    /// Targets that couldn't be built, each a failure, so the run can't pass as complete.
    pub skipped: Vec<SkippedTarget>,
    pub out: Option<String>,
    pub mode: WriteMode,
}

pub fn build_backup_plan(cmd: BackupCmd, config: &Config) -> Result<BackupPlan> {
    match (cmd.all, cmd.target) {
        (true, Some(_)) => bail!("--all cannot be combined with a target subcommand"),
        (true, None) => build_all_backup_plan(config),
        (false, Some(BackupTarget::Reddit(mut args))) => {
            let (account, out, mode) = (
                args.account.take(),
                args.out.out.take(),
                args.dry_run.into(),
            );
            build_source_backup_plan::<RedditAccount>(
                Selection::new(account.as_deref(), args.all)?,
                &SyncOverrides::from(args),
                out,
                mode,
                config,
            )
        }
        (false, Some(BackupTarget::Github(mut args))) => {
            let (account, out, mode) = (
                args.account.take(),
                args.out.out.take(),
                args.dry_run.into(),
            );
            build_source_backup_plan::<GitHubAccount>(
                Selection::new(account.as_deref(), args.all)?,
                &SyncOverrides::from(args),
                out,
                mode,
                config,
            )
        }
        (false, Some(BackupTarget::Hackernews(mut args))) => {
            let (account, out, mode) = (
                args.account.take(),
                args.out.out.take(),
                args.dry_run.into(),
            );
            build_source_backup_plan::<HackerNewsAccount>(
                Selection::new(account.as_deref(), args.all)?,
                &SyncOverrides::from(args),
                out,
                mode,
                config,
            )
        }
        (false, Some(BackupTarget::Pinboard(args))) => {
            let job = build_pinboard_backup_job(args.pinboard_token, config)?
                .with_context(|| SecretSpec::PINBOARD_TOKEN.missing())?;
            Ok(BackupPlan {
                jobs: vec![job],
                skipped: Vec::new(),
                out: args.out.out,
                mode: args.dry_run.into(),
            })
        }
        (false, None) => bail!("specify a target (e.g. `backup pinboard`) or pass --all"),
    }
}

/// The plan of `backup --all`: every account of every source, then Pinboard, with what
/// couldn't be built skipped.
fn build_all_backup_plan(config: &Config) -> Result<BackupPlan> {
    let overrides = SyncOverrides {
        on_auth_failure: on_auth_failure_from_env(),
        ..SyncOverrides::default()
    };
    let sources = build_every::<RedditAccount, _>(&overrides, config, build_backup_job)
        .and(build_every::<GitHubAccount, _>(
            &overrides,
            config,
            build_backup_job,
        ))
        .and(build_every::<HackerNewsAccount, _>(
            &overrides,
            config,
            build_backup_job,
        ));
    let mut plan = BackupPlan {
        jobs: sources.targets,
        skipped: sources.skipped,
        out: None,
        mode: WriteMode::Live,
    };
    let has_source_targets = !plan.jobs.is_empty() || !plan.skipped.is_empty();
    match build_pinboard_backup_job(None, config)? {
        Some(job) => plan.jobs.push(job),
        // Never silent: an unprovisioned sops secret would quietly drop Pinboard from a
        // nightly run that still exits 0. A failed target makes the manifest incomplete.
        None if has_source_targets => {
            warn!(
                "backup --all: no Pinboard token resolved — the Pinboard account \
                 is NOT in this snapshot"
            );
            plan.skipped.push(SkippedTarget {
                label: "pinboard".to_string(),
                reason: anyhow!("no credential resolved"),
            });
        }
        None => {}
    }
    if plan.jobs.is_empty() && plan.skipped.is_empty() {
        bail!(
            "backup --all found nothing to back up: configure an account with \
             --config, or set a Pinboard token"
        );
    }
    Ok(plan)
}

/// The backup plan for one source's `selection`, through `build_job` as in `sync`.
fn build_source_backup_plan<A: SourceAccount>(
    selection: Selection,
    overrides: &SyncOverrides,
    out: Option<String>,
    mode: WriteMode,
    config: &Config,
) -> Result<BackupPlan> {
    let Built { targets, skipped } =
        build_selected(selection, overrides, config, build_backup_job::<A>)?;
    Ok(BackupPlan {
        jobs: targets,
        skipped,
        out,
        mode,
    })
}

/// One account's `SyncJob` as a backup job, without the settings only a write uses.
fn build_backup_job<A: SourceAccount>(
    account: Option<&A>,
    overrides: &SyncOverrides,
    config: &Config,
) -> Result<BackupJob> {
    let SyncJob {
        client,
        label,
        hook,
        settings: _,
    } = build_job(account, overrides, config)?;
    Ok(BackupJob {
        stem: format!(
            "{}-{}",
            A::SOURCE,
            backup::slug(account.and_then(config::Account::account_name))
        ),
        label,
        hook,
        client: BackupClient::Source(Box::new(client)),
    })
}

/// The Pinboard backup job, or `None` with no token, so `backup --all` still covers the
/// sources on a machine without one.
fn build_pinboard_backup_job(flag: Option<String>, config: &Config) -> Result<Option<BackupJob>> {
    let Some(token) = resolve_pinboard_token(flag, &config.pinboard) else {
        return Ok(None);
    };
    Ok(Some(BackupJob {
        client: BackupClient::Pinboard(pinboard_client_for_token(token, config)?),
        stem: "pinboard".to_string(),
        label: "pinboard".to_string(),
        hook: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::OutFlag;
    use crate::test_support::warnings;

    fn backup_all() -> BackupCmd {
        BackupCmd {
            all: true,
            dry_run: false,
            out: OutFlag::default(),
            target: None,
        }
    }

    fn reddit_account() -> RedditAccount {
        RedditAccount {
            username: Some("alice".into()),
            cookie: Some("reddit_session=x".into()),
            ..Default::default()
        }
    }

    #[test]
    fn backup_all_takes_the_hook_from_on_auth_failure_env() {
        let _env =
            crate::secrets::FakeSecretEnv::set(&[("PINBOARD_SYNC_ON_AUTH_FAILURE", "env-hook")]);
        let config = Config {
            reddit: vec![reddit_account()],
            ..Default::default()
        };
        let plan = build_backup_plan(backup_all(), &config).expect("plans");
        assert_eq!(plan.jobs[0].hook.as_deref(), Some("env-hook"));
    }

    /// A source account that could not be built still makes it a run with sources, so a
    /// missing Pinboard token is counted against it too rather than passing unnoticed.
    #[test]
    fn backup_all_with_only_unbuilt_sources_still_counts_the_missing_pinboard_token() {
        let config = Config {
            github: vec![GitHubAccount {
                name: Some("home".into()),
                ..GitHubAccount::default()
            }],
            ..Default::default()
        };
        let plan = build_backup_plan(backup_all(), &config).expect("plans");
        let skipped: Vec<&str> = plan.skipped.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(skipped, ["github[home]", "pinboard"]);
        assert!(plan.jobs.is_empty(), "nothing could be built");
    }

    #[test]
    fn backup_all_with_no_account_and_no_pinboard_token_has_nothing_to_back_up() {
        let error = build_backup_plan(backup_all(), &Config::default())
            .err()
            .expect("nothing to back up");
        assert!(
            error
                .to_string()
                .starts_with("backup --all found nothing to back up"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn backup_all_warns_about_an_env_secret_shared_by_several_accounts() {
        let _env = crate::secrets::FakeSecretEnv::set(&[("GITHUB_TOKEN", "SHAREDVALUE")]);
        let config = Config {
            github: vec![GitHubAccount::default(), GitHubAccount::default()],
            ..Default::default()
        };

        let (_, logs) =
            crate::test_support::capturing_logs(async { build_backup_plan(backup_all(), &config) })
                .await;

        assert_eq!(
            warnings(&logs).first().copied(),
            Some(
                "WARN $GITHUB_TOKEN is set in the environment, so all 2 configured github \
                 accounts use it, whatever their own config sets"
            )
        );
    }
}
