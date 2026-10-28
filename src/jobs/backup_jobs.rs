//! `backup` jobs: one per target, each a client that can [`dump`](BackupSource::dump) plus
//! the stem its files are named by. Built from the same [`SourceAccount`] path as `sync`
//! jobs, so account selection and the secret ladder behave identically, with Pinboard as
//! one more target.

use anyhow::{anyhow, bail, Context, Result};
use log::warn;

use super::{
    build_job, job_label, pinboard_client_for_token, warn_shared_env_secrets, SkippedTarget,
    SourceAccount, SourceClient, SyncOverrides,
};
use crate::backup::{self, BackupSource};
use crate::cli::{BackupCmd, BackupTarget, OutFlag};
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

/// A backup target's client. Pinboard is a `BookmarkStore`, not a `Source`, so it can't
/// join `SourceClient` — but it implements the same `BackupSource` port, so the driver
/// treats all four identically.
pub enum BackupClient {
    /// Boxed: a `SourceClient` is several times the size of a `PinboardClient`, so an
    /// unboxed variant would make every job pay the larger footprint.
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

/// What a `backup` invocation resolves to: the jobs, anything it could not cover, and the
/// target-level `--out`/`--dry-run`. Mirrors `run_sync`'s `(all, source)` match.
pub struct BackupPlan {
    pub jobs: Vec<BackupJob>,
    /// Targets that couldn't be built at all (a credential that won't resolve). Counted as
    /// failures, so a run that quietly missed one can't report itself complete.
    pub skipped: Vec<SkippedTarget>,
    pub out: Option<String>,
    pub dry_run: bool,
}

pub fn build_backup_plan(cmd: &BackupCmd, config: &Config) -> Result<BackupPlan> {
    match (cmd.all, &cmd.target) {
        (true, Some(_)) => bail!("--all cannot be combined with a target subcommand"),
        (true, None) => {
            let overrides = SyncOverrides {
                on_auth_failure: on_auth_failure_from_env(),
                ..SyncOverrides::default()
            };
            let mut plan = BackupPlan {
                jobs: Vec::new(),
                skipped: Vec::new(),
                out: None,
                dry_run: false,
            };
            build_every_backup_job::<RedditAccount>(&overrides, config, &mut plan);
            build_every_backup_job::<GitHubAccount>(&overrides, config, &mut plan);
            build_every_backup_job::<HackerNewsAccount>(&overrides, config, &mut plan);
            let has_source_targets = !plan.jobs.is_empty() || !plan.skipped.is_empty();
            match build_pinboard_backup_job(None, config)? {
                Some(job) => plan.jobs.push(job),
                // Silence here would be the worst outcome: a nightly `--all` that quietly
                // stops covering Pinboard (an unprovisioned sops secret makes the ladder
                // warn and yield `None`) while still exiting 0. Recorded as a failed
                // target so the manifest says `complete: false`.
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
        (false, Some(BackupTarget::Reddit(args))) => build_source_backup_plan::<RedditAccount>(
            args.account.as_deref(),
            args.all,
            &SyncOverrides::from(args.clone()),
            &args.out,
            args.dry_run,
            config,
        ),
        (false, Some(BackupTarget::Github(args))) => build_source_backup_plan::<GitHubAccount>(
            args.account.as_deref(),
            args.all,
            &SyncOverrides::from(args.clone()),
            &args.out,
            args.dry_run,
            config,
        ),
        (false, Some(BackupTarget::Hackernews(args))) => {
            build_source_backup_plan::<HackerNewsAccount>(
                args.account.as_deref(),
                args.all,
                &SyncOverrides::from(args.clone()),
                &args.out,
                args.dry_run,
                config,
            )
        }
        (false, Some(BackupTarget::Pinboard(args))) => {
            let job = build_pinboard_backup_job(args.pinboard_token.clone(), config)?
                .with_context(|| SecretSpec::PINBOARD_TOKEN.missing())?;
            Ok(BackupPlan {
                jobs: vec![job],
                skipped: Vec::new(),
                out: args.out.out.clone(),
                dry_run: args.dry_run,
            })
        }
        (false, None) => bail!("specify a target (e.g. `backup pinboard`) or pass --all"),
    }
}

/// The backup plan for one source, selecting by name / first / every account. Reuses
/// `build_job` so account selection and the whole secret ladder behave as in `sync`.
fn build_source_backup_plan<A: SourceAccount>(
    name: Option<&str>,
    all: bool,
    overrides: &SyncOverrides,
    out: &OutFlag,
    dry_run: bool,
    config: &Config,
) -> Result<BackupPlan> {
    let mut plan = BackupPlan {
        jobs: Vec::new(),
        skipped: Vec::new(),
        out: out.out.clone(),
        dry_run,
    };
    if all {
        if A::configured(config).is_empty() {
            bail!(
                "--all requires a --config with at least one configured {} account",
                A::SOURCE
            );
        }
        build_every_backup_job::<A>(overrides, config, &mut plan);
    } else {
        let account = config::select_account(A::configured(config), name)?;
        plan.jobs
            .push(build_backup_job(account, overrides, config)?);
    }
    Ok(plan)
}

/// A backup job for every configured account of one source, with each account that fails
/// to build recorded in `plan.skipped` instead, so the healthy ones are still backed up.
fn build_every_backup_job<A: SourceAccount>(
    overrides: &SyncOverrides,
    config: &Config,
    plan: &mut BackupPlan,
) {
    warn_shared_env_secrets::<A>(overrides, config);
    for account in A::configured(config) {
        match build_backup_job(Some(account), overrides, config) {
            Ok(job) => plan.jobs.push(job),
            Err(reason) => plan.skipped.push(SkippedTarget {
                label: job_label(Some(account)),
                reason,
            }),
        }
    }
}

/// Wrap one account's `SyncJob` as a backup job, keeping its client and auth hook and
/// discarding the write-only settings (`limit`, `toread`, dates) backup has no use for.
fn build_backup_job<A: SourceAccount>(
    account: Option<&A>,
    overrides: &SyncOverrides,
    config: &Config,
) -> Result<BackupJob> {
    let job = build_job(account, overrides, config)?;
    Ok(BackupJob {
        stem: format!(
            "{}-{}",
            A::SOURCE,
            backup::slug(account.and_then(config::Named::account_name))
        ),
        label: job.label.clone(),
        hook: job.hook.clone(),
        client: BackupClient::Source(Box::new(job.client)),
    })
}

/// The Pinboard backup job, or `None` when no token is configured — so `backup --all`
/// still backs up the sources on a machine that only holds source credentials.
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
    use crate::test_support::{lock_on_auth_failure_env, warnings};

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
        let _env = lock_on_auth_failure_env();
        let config = Config {
            reddit: vec![reddit_account()],
            ..Default::default()
        };
        std::env::set_var("PINBOARD_SYNC_ON_AUTH_FAILURE", "env-hook");
        let plan = build_backup_plan(&backup_all(), &config);
        std::env::remove_var("PINBOARD_SYNC_ON_AUTH_FAILURE");

        let plan = plan.expect("plans");
        assert_eq!(plan.jobs[0].hook.as_deref(), Some("env-hook"));
    }

    #[tokio::test]
    async fn backup_all_warns_about_an_env_secret_shared_by_several_accounts() {
        let _env = crate::secrets::FakeSecretEnv::set(&[("GITHUB_TOKEN", "SHAREDVALUE")]);
        let config = Config {
            github: vec![GitHubAccount::default(), GitHubAccount::default()],
            ..Default::default()
        };

        let (_, logs) = crate::test_support::capturing_logs(async {
            build_backup_plan(&backup_all(), &config)
        })
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
