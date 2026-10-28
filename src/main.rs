//! pinboard-sync: sync saved/favorited items from multiple services to Pinboard.

mod backup;
mod bookmark;
mod cleanup_pass;
mod cli;
mod commands;
mod config;
mod doctor;
mod domains;
mod github;
mod hackernews;
mod hook;
mod htmltext;
mod http;
mod jobs;
mod keys;
mod pinboard;
mod preview;
mod reddit;
mod secrets;
mod settings;
mod source;
mod stdout;
mod sync;
mod tags;
#[cfg(test)]
mod test_support;
mod timefmt;
mod urls;

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::CommandFactory;
use clap_complete::Shell;
use log::{debug, error, info};

use cli::{Cli, Command, ConfigAction};
use commands::{run_backup, run_cleanup, run_sync};
use config::Config;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let _ = dotenvy::dotenv();
    let cli = Cli::parse_args();
    init_logging(cli.verbose);

    let result = async {
        match cli.command {
            Command::Sync(cmd) => {
                log_start("sync");
                run_sync(cmd, &load_config(cli.config)?).await
            }
            Command::Cleanup(cmd) => {
                log_start("cleanup");
                run_cleanup(cmd, &load_config(cli.config)?).await
            }
            Command::Doctor => {
                log_start("doctor");
                doctor::run_doctor(&load_config(cli.config)?).await
            }
            Command::Backup(cmd) => {
                log_start("backup");
                run_backup(cmd, &load_config(cli.config)?).await
            }
            Command::Completions { shell } => print_completions(shell),
            Command::Man => print_man(),
            Command::Config {
                action: ConfigAction::Example,
            } => stdout::write(include_str!("config.example.toml"))
                .context("writing the example config"),
            Command::Config {
                action: ConfigAction::Check { file },
            } => check_config(&file).and_then(|summary| {
                stdout::write(format!("{summary}\n")).context("writing the config check")
            }),
        }
    }
    .await;
    exit_code(result)
}

/// The process exit status for a command's `result`, logging the error that fails it.
fn exit_code(result: Result<()>) -> ExitCode {
    if let Err(e) = result {
        error!("{e:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Initialize logging, written to stderr with a short `LEVEL message` format so stdout
/// stays clean for generated output (dry-run listings, completions, man, config).
/// Verbosity escalates with repeated `-v`: info -> debug (`-v`) -> trace (`-vv`) -> trace
/// for every crate, including dependencies (`-vvv`). `RUST_LOG` overrides the filter.
fn init_logging(verbose: u8) {
    use std::io::Write;
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(default_filter(verbose)),
    )
    .format(|buf, record| writeln!(buf, "{:5} {}", record.level(), record.args()))
    .init();
}

/// The log filter for `verbose` repeats of `-v`, when `RUST_LOG` doesn't set one.
fn default_filter(verbose: u8) -> &'static str {
    match verbose {
        0 => concat!(env!("CARGO_CRATE_NAME"), "=info"),
        1 => concat!(env!("CARGO_CRATE_NAME"), "=debug"),
        2 => concat!(env!("CARGO_CRATE_NAME"), "=trace"),
        _ => "trace",
    }
}

/// Log the startup banner (version + subcommand) for an operational run.
fn log_start(command: &str) {
    info!(
        "pinboard-sync {} starting ({command})",
        env!("CARGO_PKG_VERSION")
    );
}

/// Write a shell completion script for `shell` to stdout.
fn print_completions(shell: Shell) -> Result<()> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    let mut buf = Vec::new();
    clap_complete::generate(shell, &mut cmd, name, &mut buf);
    stdout::write(buf).context("writing the completion script")
}

/// Write a roff man page for the CLI to stdout.
fn print_man() -> Result<()> {
    let mut buf = Vec::new();
    clap_mangen::Man::new(Cli::command())
        .render(&mut buf)
        .context("rendering the man page")?;
    stdout::write(buf).context("writing the man page")
}

/// Load the config from `--config` / `$PINBOARD_SYNC_CONFIG` (a direct path to the
/// TOML file); absent = defaults. Unlike secrets, the config takes no `_FILE` form --
/// it is already a file path.
fn load_config(flag: Option<String>) -> Result<Config> {
    let Some(path) = config_path(flag, std::env::var("PINBOARD_SYNC_CONFIG").ok()) else {
        debug!("no --config / PINBOARD_SYNC_CONFIG; using built-in defaults");
        return Ok(Config::default());
    };
    let config = read_config(Path::new(&path))?;
    info!("loaded config {path}: {}", account_counts(&config));
    Ok(config)
}

/// The config file to load: the `--config` flag, else `$PINBOARD_SYNC_CONFIG`, else none.
/// An empty value counts as none, and an empty flag still outranks the env var, so
/// `--config ''` runs on the built-in defaults whatever the environment sets.
fn config_path(flag: Option<String>, env: Option<String>) -> Option<String> {
    flag.or(env).filter(|path| !path.is_empty())
}

fn read_config(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    Config::parse(&text)
}

fn account_counts(config: &Config) -> String {
    format!(
        "{} reddit, {} github, {} hackernews account(s)",
        config.reddit.len(),
        config.github.len(),
        config.hackernews.len()
    )
}

/// The `config check` verdict: `path` read and validated exactly as `--config` is, which
/// resolves no secret or `*_file` path and contacts no service.
fn check_config(path: &Path) -> Result<String> {
    let config = read_config(path)?;
    Ok(format!(
        "{}: OK ({})",
        path.display(),
        account_counts(&config)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{capturing_logs, capturing_stdout};

    #[test]
    fn each_v_raises_the_default_log_level() {
        let filters: Vec<&str> = (0..=4).map(default_filter).collect();
        assert_eq!(
            filters,
            [
                "pinboard_sync=info",
                "pinboard_sync=debug",
                "pinboard_sync=trace",
                "trace",
                "trace"
            ]
        );
    }

    #[tokio::test]
    async fn only_a_failed_command_exits_non_zero_and_it_logs_why() {
        let (code, logs) = capturing_logs(async {
            exit_code(Err(anyhow::anyhow!("no token").context("listing bookmarks")))
        })
        .await;
        assert_eq!(code, ExitCode::FAILURE);
        assert_eq!(logs, ["ERROR listing bookmarks: no token"]);

        let (code, logs) = capturing_logs(async { exit_code(Ok(())) }).await;
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(logs.is_empty(), "{logs:?}");
    }

    #[tokio::test]
    async fn the_startup_banner_names_the_version_and_the_command() {
        let ((), logs) = capturing_logs(async { log_start("sync") }).await;
        assert_eq!(
            logs,
            [format!(
                "INFO pinboard-sync {} starting (sync)",
                env!("CARGO_PKG_VERSION")
            )]
        );
    }

    #[tokio::test]
    async fn completions_and_the_man_page_go_to_stdout() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let (result, out) = capturing_stdout(async { print_completions(shell) }).await;
            result.expect("writes");
            assert!(out.contains("pinboard-sync"), "{shell}: {out}");
        }
        let (result, out) = capturing_stdout(async { print_man() }).await;
        result.expect("writes");
        assert!(out.contains(".TH pinboard-sync"), "{out}");
    }

    #[test]
    fn the_config_path_is_the_flag_then_the_env_var() {
        let some = |path: &str| Some(path.to_string());
        assert_eq!(
            config_path(some("flag.toml"), some("env.toml")),
            some("flag.toml")
        );
        assert_eq!(config_path(None, some("env.toml")), some("env.toml"));
        assert_eq!(config_path(None, None), None);
        assert_eq!(
            config_path(None, some("")),
            None,
            "an empty env var is unset"
        );
        assert_eq!(
            config_path(some(""), some("env.toml")),
            None,
            "`--config ''` turns the env var off"
        );
    }

    #[test]
    fn load_config_reads_the_flagged_file() {
        let dir = crate::test_support::scratch_dir("load-config");
        let path = dir.path().join("pinboard-sync.toml");
        std::fs::write(&path, "[[github]]\nname = \"main\"\n").unwrap();

        let config = load_config(Some(path.display().to_string())).expect("loads");
        assert_eq!(config.github.len(), 1, "{config:?}");
    }

    #[test]
    fn config_check_takes_the_file_as_a_positional() {
        let argv = ["pinboard-sync", "config", "check", "pinboard-sync.toml"];
        match crate::test_support::parse_cli(argv)
            .expect("parses")
            .command
        {
            Command::Config {
                action: ConfigAction::Check { file },
            } => assert_eq!(file, Path::new("pinboard-sync.toml")),
            _ => panic!("expected `config check`"),
        }
    }

    fn check_config_text(text: &str) -> Result<String> {
        let dir = crate::test_support::scratch_dir("config-check");
        let path = dir.path().join("pinboard-sync.toml");
        std::fs::write(&path, text).unwrap();
        check_config(&path)
    }

    #[test]
    fn config_check_reports_ok_with_the_account_counts() {
        let summary = check_config_text("[[hackernews]]\nusername = \"example\"\n").unwrap();
        assert!(
            summary
                .ends_with("pinboard-sync.toml: OK (0 reddit, 0 github, 1 hackernews account(s))"),
            "{summary}"
        );
    }

    #[test]
    fn config_check_does_not_read_the_secret_files_it_names() {
        let text = "[pinboard]\ntoken_file = \"/nonexistent/pinboard-token\"\n\
                    [[reddit]]\ncookie_file = \"/nonexistent/reddit-cookie\"\n\
                    [[github]]\ntoken_file = \"/nonexistent/github-token\"\n";
        check_config_text(text).unwrap();
    }

    #[test]
    fn config_check_failure_names_the_field_not_the_value() {
        for (text, field) in [
            (
                "[pinboard]\ntoken = \"user:INLINESECRET\"\ntokn = \"user:TYPOSECRET\"\n",
                "unknown field `tokn`",
            ),
            (
                "[[reddit]]\ntags = [\"reddit_session=SECRET x\"]\n",
                "`reddit[0].tags[0]` must not contain whitespace",
            ),
            (
                "[[reddit]]\nreddit_domain = \"SECRET.example:8080\"\n",
                "`reddit[0].reddit_domain` contains a `:`",
            ),
            (
                "[[github]]\nlimit = \"ghp_SECRET\"\n",
                "line 2, column 9: `limit` must be",
            ),
            (
                "[[github]]\ntoken = \"ghp_SECRET\"\n[[github]]\ntoken = \"ghp_SECRET2\"\n",
                "github[0] and github[1] would both back up as `github-default`",
            ),
            (
                "[[reddit]]\ncookie = \"reddit_session=SECRET\"\n[[reddit]]\n",
                "reddit[0] and reddit[1] would both back up as `reddit-default`",
            ),
        ] {
            let error = format!("{:#}", check_config_text(text).unwrap_err());
            assert!(error.contains(field), "{error}");
            assert!(!error.contains("SECRET"), "{error}");
        }
    }

    #[test]
    fn config_check_of_a_missing_file_names_the_path() {
        let dir = crate::test_support::scratch_dir("config-check-missing");
        let path = dir.path().join("absent.toml");
        let error = format!("{:#}", check_config(&path).unwrap_err());
        assert!(error.contains("reading config file"), "{error}");
        assert!(error.contains("absent.toml"), "{error}");
    }
}
