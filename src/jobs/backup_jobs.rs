//! `backup` jobs: one per target, each a client that can [`dump`](BackupSource::dump) plus
//! the stem its files are named by. Built from the same [`SourceAccount`] path as `sync`
//! jobs, so account selection and the secret ladder behave identically, with Pinboard as
//! one more target.

use anyhow::{bail, Context, Result};
use log::warn;

use super::{build_job, pinboard_client, SourceAccount, SourceClient, SyncOverrides};
use crate::backup::{self, BackupSource};
use crate::cli::{BackupCmd, BackupTarget};
use crate::config::{self, Config, GitHubAccount, HackerNewsAccount, RedditAccount};
use crate::pinboard::PinboardClient;
use crate::secrets::resolve_pinboard_token;
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
    /// Targets that couldn't be built at all (no credential resolved). Counted as
    /// failures, so a run that quietly missed one can't report itself complete.
    pub skipped: Vec<String>,
    pub out: Option<String>,
    pub dry_run: bool,
}

pub fn backup_jobs(cmd: &BackupCmd, config: &Config) -> Result<BackupPlan> {
    let ovr = SyncOverrides::default();
    match (cmd.all, &cmd.target) {
        (true, Some(_)) => bail!("--all cannot be combined with a target subcommand"),
        (true, None) => {
            let mut jobs = Vec::new();
            let mut skipped = Vec::new();
            jobs.extend(every_backup_job::<RedditAccount>(&ovr, config)?);
            jobs.extend(every_backup_job::<GitHubAccount>(&ovr, config)?);
            jobs.extend(every_backup_job::<HackerNewsAccount>(&ovr, config)?);
            match pinboard_backup_job(None, config)? {
                Some(job) => jobs.push(job),
                // Silence here would be the worst outcome: a nightly `--all` that quietly
                // stops covering Pinboard (an unprovisioned sops secret makes
                // `resolve_secret` warn and yield `None`) while still exiting 0. Recorded
                // as a failed target so the manifest says `complete: false`.
                None if !jobs.is_empty() => {
                    warn!(
                        "backup --all: no Pinboard token resolved — the Pinboard account \
                         is NOT in this snapshot"
                    );
                    skipped.push("pinboard".to_string());
                }
                None => {}
            }
            if jobs.is_empty() {
                bail!(
                    "backup --all found nothing to back up: configure an account with \
                     --config, or set a Pinboard token"
                );
            }
            Ok(BackupPlan {
                jobs,
                skipped,
                out: None,
                dry_run: false,
            })
        }
        (false, Some(BackupTarget::Reddit(args))) => Ok(BackupPlan {
            jobs: source_backup_jobs::<RedditAccount>(
                args.account.as_deref(),
                args.all,
                &SyncOverrides::from(args.clone()),
                config,
            )?,
            skipped: Vec::new(),
            out: args.out.out.clone(),
            dry_run: args.dry_run,
        }),
        (false, Some(BackupTarget::Github(args))) => Ok(BackupPlan {
            jobs: source_backup_jobs::<GitHubAccount>(
                args.account.as_deref(),
                args.all,
                &SyncOverrides::from(args.clone()),
                config,
            )?,
            skipped: Vec::new(),
            out: args.out.out.clone(),
            dry_run: args.dry_run,
        }),
        (false, Some(BackupTarget::Hackernews(args))) => Ok(BackupPlan {
            jobs: source_backup_jobs::<HackerNewsAccount>(
                args.account.as_deref(),
                args.all,
                &SyncOverrides::from(args.clone()),
                config,
            )?,
            skipped: Vec::new(),
            out: args.out.out.clone(),
            dry_run: args.dry_run,
        }),
        (false, Some(BackupTarget::Pinboard(args))) => {
            let job = pinboard_backup_job(args.pinboard_token.clone(), config)?.context(
                "missing Pinboard token (set --pinboard-token, PINBOARD_TOKEN/_FILE, or \
                 [pinboard] in the config)",
            )?;
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

/// The backup jobs for one source, selecting by name / first / every account. Reuses
/// `build_job` so account selection and the whole secret ladder behave as in `sync`.
fn source_backup_jobs<A: SourceAccount>(
    name: Option<&str>,
    all: bool,
    ovr: &SyncOverrides,
    config: &Config,
) -> Result<Vec<BackupJob>> {
    if all {
        if A::configured(config).is_empty() {
            bail!(
                "--all requires a --config with at least one configured {} account",
                A::SOURCE
            );
        }
        every_backup_job::<A>(ovr, config)
    } else {
        let account = config::select_account(A::configured(config), name)?;
        Ok(vec![source_backup_job(account, ovr, config)?])
    }
}

/// A backup job for every configured account of one source, stopping at the first that
/// fails to build.
fn every_backup_job<A: SourceAccount>(
    ovr: &SyncOverrides,
    config: &Config,
) -> Result<Vec<BackupJob>> {
    A::configured(config)
        .iter()
        .map(|a| source_backup_job(Some(a), ovr, config))
        .collect()
}

/// Wrap one account's `SyncJob` as a backup job, keeping its client and auth hook and
/// discarding the write-only settings (`limit`, `toread`, dates) backup has no use for.
fn source_backup_job<A: SourceAccount>(
    account: Option<&A>,
    ovr: &SyncOverrides,
    config: &Config,
) -> Result<BackupJob> {
    let job = build_job(account, ovr, config)?;
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
fn pinboard_backup_job(flag: Option<String>, config: &Config) -> Result<Option<BackupJob>> {
    let Some(token) = resolve_pinboard_token(flag, &config.pinboard) else {
        return Ok(None);
    };
    Ok(Some(BackupJob {
        client: BackupClient::Pinboard(pinboard_client(token, config)?),
        stem: "pinboard".to_string(),
        label: "pinboard".to_string(),
        hook: None,
    }))
}
