//! `doctor`: check the Pinboard token, the backup directory and every configured account's
//! credentials, printing a ✓/✗ line per check.

use std::path::Path;

use anyhow::{bail, Result};

use crate::backup;
use crate::config::{Config, GitHubAccount, HackerNewsAccount, RedditAccount};
use crate::jobs::{
    build_job, open_pinboard, warn_shared_env_secrets, SourceAccount, SyncOverrides,
};
use crate::source::{Source, SourceError};
use crate::stdout::outln;

/// Check the Pinboard token and every account's credentials by fetching each, a ✓/✗ line
/// per check, failing if any did.
pub async fn run_doctor(config: &Config) -> Result<()> {
    let mut failed = 0usize;

    match open_pinboard(None, config).await {
        Ok((_, bms)) => outln!("✓ pinboard — {} bookmark(s)", bms.len()),
        Err(e) => {
            outln!("✗ pinboard — {e:#}");
            failed += 1;
        }
    }

    // Only when configured: a machine that never runs `backup` mustn't fail over it. Here,
    // a bad StateDirectory shows now rather than at the next timer firing.
    if let Some(dir) = &config.backup.directory {
        match backup::probe_writable(Path::new(dir)) {
            Ok(backup::DirProbe::Writable) => outln!("✓ backup — {dir} is writable"),
            // `backup` creates it on its first run; only a bad parent is a problem.
            Ok(backup::DirProbe::WillBeCreated) => {
                outln!("✓ backup — {dir} will be created on the first run");
            }
            Err(e) => {
                outln!("✗ backup — {dir}: {e:#}");
                failed += 1;
            }
        }
    }

    let overrides = SyncOverrides::default();
    failed += check_accounts::<RedditAccount>(&overrides, config).await;
    failed += check_accounts::<GitHubAccount>(&overrides, config).await;
    failed += check_accounts::<HackerNewsAccount>(&overrides, config).await;

    if failed > 0 {
        bail!("{failed} check(s) failed");
    }
    outln!("All checks passed.");
    Ok(())
}

/// Fetch each configured account through the real job builders, returning how many failed.
async fn check_accounts<A: SourceAccount>(overrides: &SyncOverrides, config: &Config) -> usize {
    let source = A::SOURCE;
    let accounts = A::configured(config);
    if accounts.is_empty() {
        outln!("- {source} — no accounts configured");
        return 0;
    }
    warn_shared_env_secrets::<A>(overrides, config);
    let mut failed = 0;
    for account in accounts {
        let name = account.account_name().unwrap_or("(unnamed)");
        let result = match build_job(Some(account), overrides, config) {
            Ok(job) => job.client.fetch().await.map_err(SourceError::into_anyhow),
            Err(e) => Err(e),
        };
        match result {
            Ok(items) => outln!("✓ {source} [{name}] — {} item(s)", items.len()),
            Err(e) => {
                outln!("✗ {source} [{name}] — {e:#}");
                failed += 1;
            }
        }
    }
    failed
}
