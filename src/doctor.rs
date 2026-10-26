//! `doctor`: check the Pinboard token, the backup directory and every configured account's
//! credentials, printing a ✓/✗ line per check.

use std::path::Path;

use anyhow::{bail, Result};

use crate::backup;
use crate::config::{Config, GitHubAccount, HackerNewsAccount, RedditAccount};
use crate::jobs::{build_job, open_pinboard, SourceAccount, SyncOverrides};
use crate::source::{Source, SourceError};

/// Validate the Pinboard token and every configured account's credentials by
/// fetching each source. Prints a ✓/✗ line per check; exits non-zero if any failed.
pub async fn run_doctor(config: &Config) -> Result<()> {
    let mut failed = 0usize;

    match open_pinboard(None, config).await {
        Ok((_, bms)) => println!("✓ pinboard — {} bookmark(s)", bms.len()),
        Err(e) => {
            println!("✗ pinboard — {e:#}");
            failed += 1;
        }
    }

    // Only when a directory is configured: a machine that never runs `backup` shouldn't
    // fail `doctor` over it. Checked here so a misconfigured StateDirectory surfaces now
    // rather than as a quiet failure in the journal at the next timer firing.
    if let Some(dir) = &config.backup.directory {
        match backup::probe_writable(Path::new(dir)) {
            Ok(backup::DirProbe::Writable) => println!("✓ backup — {dir} is writable"),
            // Not yet created is healthy: `backup` makes it on its first run, and only a
            // typo'd or unwritable *parent* is a real problem. Failing here would red-flag
            // a correct config that simply hasn't run yet.
            Ok(backup::DirProbe::WillBeCreated) => {
                println!("✓ backup — {dir} will be created on the first run");
            }
            Err(e) => {
                println!("✗ backup — {dir}: {e:#}");
                failed += 1;
            }
        }
    }

    let ovr = SyncOverrides::default();
    failed += check_accounts::<RedditAccount>(&ovr, config).await;
    failed += check_accounts::<GitHubAccount>(&ovr, config).await;
    failed += check_accounts::<HackerNewsAccount>(&ovr, config).await;

    if failed > 0 {
        bail!("{failed} check(s) failed");
    }
    println!("All checks passed.");
    Ok(())
}

/// Probe each configured account by fetching its source; returns the count that
/// failed. Reuses the normal job builders, so it exercises the real auth path.
async fn check_accounts<A: SourceAccount>(ovr: &SyncOverrides, config: &Config) -> usize {
    let source = A::SOURCE;
    let accounts = A::configured(config);
    if accounts.is_empty() {
        println!("- {source} — no accounts configured");
        return 0;
    }
    let mut failed = 0;
    for account in accounts {
        let name = account.account_name().unwrap_or("(unnamed)");
        let result = match build_job(Some(account), ovr, config) {
            Ok(job) => job.client.fetch().await.map_err(SourceError::into_anyhow),
            Err(e) => Err(e),
        };
        match result {
            Ok(items) => println!("✓ {source} [{name}] — {} item(s)", items.len()),
            Err(e) => {
                println!("✗ {source} [{name}] — {e:#}");
                failed += 1;
            }
        }
    }
    failed
}
