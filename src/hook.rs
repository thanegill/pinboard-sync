//! The `--on-auth-failure` hook: which command a run resolves (flag/env → account →
//! `[defaults.<source>]` → `[hooks]`), and firing it, only when a source says a credential
//! needs re-authenticating.

use std::path::Path;

use crate::config::Config;
use crate::jobs::SourceAccount;
use crate::source::SourceError;

/// The `--all` path has no per-source `--on-auth-failure` flag to carry the hook,
/// and the top-level flag deliberately doesn't bind `PINBOARD_SYNC_ON_AUTH_FAILURE`
/// (so an explicit per-source flag can outrank the env var). So `--all` reads the
/// env var here directly -- without it, the NixOS service (`sync --all`, hook via
/// that env var only) would silently never fire the hook.
pub fn on_auth_failure_from_env() -> Option<String> {
    std::env::var("PINBOARD_SYNC_ON_AUTH_FAILURE")
        .ok()
        .filter(|v| !v.is_empty())
}

/// Map a `SourceError` to an `anyhow::Error`, firing the auth-failure hook when
/// re-authentication is required.
///
/// Only [`SourceError::ReauthRequired`] fires it: no credential change clears a rate limit
/// or an edge block, so waking the operator to rotate a token would send them after the
/// wrong problem.
pub fn handle_source_err(e: SourceError, hook: Option<&str>) -> anyhow::Error {
    if let SourceError::ReauthRequired(msg) = &e {
        run_auth_failure_hook(hook, msg);
    }
    e.into_anyhow()
}

fn run_auth_failure_hook(cmd: Option<&str>, err: &str) {
    let Some(cmd) = cmd else {
        return;
    };
    eprintln!("Running --on-auth-failure hook...");
    let mut command = auth_failure_hook_command(cmd, err);
    match command.status() {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("auth-failure hook exited with {s}"),
        Err(e) => eprintln!(
            "failed to run auth-failure hook via {}: {e}",
            command.get_program().display()
        ),
    }
}

fn auth_failure_hook_command(cmd: &str, err: &str) -> std::process::Command {
    let mut command = std::process::Command::new(hook_shell(Path::new("/bin/sh")));
    command
        .arg("-c")
        .arg(cmd)
        .env("PINBOARD_SYNC_AUTH_ERROR", err)
        .env("PINBOARD_SYNC_EVENT", "reauth_required");
    command
}

/// The shell the hook runs under: `system_shell` (`/bin/sh`, as `system(3)` uses) so the
/// caller's `PATH` doesn't matter, else `sh` from `PATH` on a host without one.
fn hook_shell(system_shell: &Path) -> &Path {
    if system_shell.exists() {
        system_shell
    } else {
        Path::new("sh")
    }
}

/// The auth-failure hook: resolved flag/env (`ovr.on_auth_failure`) → per-account
/// override → per-source default → `[hooks]` global.
pub fn resolve_hook(
    flag: Option<String>,
    account_override: Option<&str>,
    source_override: Option<&str>,
    config: &Config,
) -> Option<String> {
    flag.or_else(|| account_override.map(str::to_string))
        .or_else(|| source_override.map(str::to_string))
        .or_else(|| config.hooks.on_auth_failure.clone())
}

/// The auth-failure hook for a `cleanup` run: `PINBOARD_SYNC_ON_AUTH_FAILURE` → account →
/// `[defaults.<source>]` → `[hooks]`. The `cleanup` subcommands take no
/// `--on-auth-failure` flag, so the env var is the top rung.
///
/// Reading the env var here is load-bearing, not a convenience: the NixOS module exports
/// `onAuthFailure` *only* into the unit environment — it never reaches the generated
/// TOML — and runs the `cleanup --all` timer with it. Resolving from config alone would
/// leave the hook dead for the exact deployment it is meant to serve.
pub fn cleanup_hook<A: SourceAccount>(account: Option<&A>, config: &Config) -> Option<String> {
    A::hook(account, on_auth_failure_from_env(), config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HackerNewsAccount, RedditAccount};
    use crate::jobs::{build_job, SyncOverrides};
    use crate::test_support::{lock_on_auth_failure_env, scratch_dir};
    use anyhow::anyhow;

    /// A reddit account with its own hook, in a config whose `[defaults.reddit]` has
    /// another: the two config tiers `cleanup_hook` reads below the env var.
    fn account_and_source_hooks() -> (RedditAccount, Config) {
        let account = RedditAccount {
            on_auth_failure: Some("account-hook".into()),
            ..Default::default()
        };
        let mut config = Config::default();
        config.defaults.reddit.on_auth_failure = Some("source-hook".into());
        (account, config)
    }

    /// HackerNews is public, so no command may hand it a hook, whatever the env var or
    /// `[hooks]` say.
    #[test]
    fn hackernews_resolves_no_hook_from_any_tier() {
        let _env = lock_on_auth_failure_env();
        std::env::set_var("PINBOARD_SYNC_ON_AUTH_FAILURE", "env-hook");
        let mut config = Config::default();
        config.hooks.on_auth_failure = Some("global-hook".into());
        let cleanup = cleanup_hook::<HackerNewsAccount>(None, &config);
        std::env::remove_var("PINBOARD_SYNC_ON_AUTH_FAILURE");
        let ovr = SyncOverrides {
            hackernews_username: Some("bob".into()),
            on_auth_failure: Some("flag-hook".into()),
            ..SyncOverrides::default()
        };
        let job = build_job::<HackerNewsAccount>(None, &ovr, &config).expect("builds");

        assert_eq!(cleanup, None);
        assert_eq!(job.hook, None);
    }

    #[test]
    fn cleanup_falls_back_to_on_auth_failure_env() {
        let _env = lock_on_auth_failure_env();
        // The NixOS module exports `onAuthFailure` *only* as this env var — it never
        // reaches the generated TOML — and it runs the `cleanup --all` timer with that
        // same environment. Miss this rung and the hook silently never fires for the
        // deployment the whole feature exists for.
        std::env::set_var("PINBOARD_SYNC_ON_AUTH_FAILURE", "env-hook");
        let hook = cleanup_hook::<RedditAccount>(None, &Config::default());
        std::env::remove_var("PINBOARD_SYNC_ON_AUTH_FAILURE");

        assert_eq!(hook.as_deref(), Some("env-hook"));
    }

    #[test]
    fn cleanup_env_hook_outranks_the_config_tiers() {
        let _env = lock_on_auth_failure_env();
        // Same precedence as `sync`, whose per-source flag is env-backed: the env var is
        // the top rung, above an account or `[defaults.<source>]` entry.
        std::env::set_var("PINBOARD_SYNC_ON_AUTH_FAILURE", "env-hook");
        let (account, config) = account_and_source_hooks();
        let hook = cleanup_hook(Some(&account), &config);
        std::env::remove_var("PINBOARD_SYNC_ON_AUTH_FAILURE");

        assert_eq!(hook.as_deref(), Some("env-hook"));
    }

    #[test]
    fn cleanup_uses_the_account_hook_when_no_env_is_set() {
        let _env = lock_on_auth_failure_env();
        std::env::remove_var("PINBOARD_SYNC_ON_AUTH_FAILURE");
        let (account, config) = account_and_source_hooks();
        let hook = cleanup_hook(Some(&account), &config);

        assert_eq!(hook.as_deref(), Some("account-hook"));
    }

    /// The hook is a real side effect on a real credential expiry, so drive it through
    /// the real hook command rather than a stand-in: the marker file existing is
    /// the only proof the operator's command ran.
    #[test]
    fn the_auth_failure_hook_fires_for_reauth_and_nothing_else() {
        let scratch = scratch_dir("auth-hook");
        let dir = scratch.path();
        let marker = dir.join("fired");
        let hook = format!("touch {}", marker.display());

        // A rate limit is not a credential problem: firing here would send the operator
        // to rotate a token when the fix is to wait.
        handle_source_err(
            SourceError::RateLimited("resets at 14:23".into()),
            Some(&hook),
        );
        assert!(!marker.exists(), "a rate limit must not fire the hook");

        handle_source_err(
            SourceError::Blocked("Reddit's edge blocked the request".into()),
            Some(&hook),
        );
        assert!(!marker.exists(), "an edge block must not fire the hook");

        handle_source_err(
            SourceError::Other(anyhow!("some transient failure")),
            Some(&hook),
        );
        assert!(!marker.exists(), "an ordinary error must not fire the hook");

        handle_source_err(
            SourceError::ReauthRequired("cookie expired".into()),
            Some(&hook),
        );
        assert!(marker.exists(), "a dead credential must fire the hook");
    }

    /// The NixOS module's units carry no shell on `PATH` (thanegill/pinboard-sync#77), so
    /// the hook must not depend on finding one there. The command uses only shell
    /// builtins, since nothing else is reachable through an empty `PATH` either.
    #[test]
    fn the_auth_failure_hook_runs_without_a_shell_on_path() {
        let scratch = scratch_dir("auth-hook-no-path");
        let dir = scratch.path();
        let empty_path = dir.join("empty-path");
        std::fs::create_dir(&empty_path).unwrap();
        let marker = dir.join("fired");
        let hook = format!(
            "echo \"$PINBOARD_SYNC_EVENT: $PINBOARD_SYNC_AUTH_ERROR\" > '{}'",
            marker.display()
        );

        let status = auth_failure_hook_command(&hook, "cookie expired")
            .env("PATH", &empty_path)
            .status()
            .expect("the hook's shell is found without PATH");
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "reauth_required: cookie expired\n"
        );
    }

    /// NixOS's bashless profile has no `/bin/sh`; there the hook still runs if the unit
    /// puts a shell on `PATH`.
    #[test]
    fn the_hook_shell_falls_back_to_path_without_a_system_shell() {
        let scratch = scratch_dir("hook-shell");
        let dir = scratch.path();
        let system_shell = dir.join("sh");
        assert_eq!(hook_shell(&system_shell), Path::new("sh"));

        std::fs::write(&system_shell, "").unwrap();
        assert_eq!(hook_shell(&system_shell), system_shell);
    }
}
