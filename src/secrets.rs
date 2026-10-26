//! Secret resolution: the ladder every credential resolves through — CLI flag → `$VAR` →
//! `$VAR_FILE` → config inline → config `*_file` — and the names messages give each rung,
//! so an error can point at the setting a credential came from without quoting it.

use crate::config::{GitHubAccount, PinboardConfig, RedditAccount};
use crate::reddit;

/// A configured credential and the setting it came from, such as `$REDDIT_COOKIE_FILE`, so
/// a message can point at that setting without quoting the credential.
pub struct ConfiguredSecret {
    pub value: String,
    pub source: &'static str,
}

/// Which rung of the secret ladder a candidate came from, in the ladder's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretRung {
    Flag,
    Env,
    EnvFile,
    ConfigInline,
    ConfigFile,
}

impl SecretRung {
    pub const ALL: [SecretRung; 5] = [
        SecretRung::Flag,
        SecretRung::Env,
        SecretRung::EnvFile,
        SecretRung::ConfigInline,
        SecretRung::ConfigFile,
    ];
}

/// The Reddit cookie setting a rung reads, as messages name it.
pub fn reddit_cookie_source(rung: SecretRung) -> &'static str {
    match rung {
        SecretRung::Flag => "--reddit-cookie",
        SecretRung::Env => "$REDDIT_COOKIE",
        SecretRung::EnvFile => "$REDDIT_COOKIE_FILE",
        SecretRung::ConfigInline => "the config's `cookie`",
        SecretRung::ConfigFile => "the config's `cookie_file`",
    }
}

/// The GitHub token setting a rung reads, as messages name it.
pub fn github_token_source(rung: SecretRung) -> &'static str {
    match rung {
        SecretRung::Flag => "--github-token",
        SecretRung::Env => "$GITHUB_TOKEN",
        SecretRung::EnvFile => "$GITHUB_TOKEN_FILE",
        SecretRung::ConfigInline => "the config's `token`",
        SecretRung::ConfigFile => "the config's `token_file`",
    }
}

/// Every setting a Reddit cookie can come from, as "A, B, C, or the config's `x` or
/// `x_file`" for a message to follow "set".
pub fn reddit_cookie_settings() -> String {
    settings_list(reddit_cookie_source)
}

/// Every setting a GitHub token can come from, as "A, B, C, or the config's `x` or
/// `x_file`" for a message to follow "set".
pub fn github_token_settings() -> String {
    settings_list(github_token_source)
}

/// The setting each rung reads, in ladder order. The two config rungs share one entry,
/// "the config's `x` or `x_file`", rather than naming the config twice.
fn settings_list(setting: fn(SecretRung) -> &'static str) -> String {
    let [flag, env, env_file, config_inline, config_file] = SecretRung::ALL.map(setting);
    let config_file_key = config_file
        .strip_prefix("the config's ")
        .unwrap_or(config_file);
    format!("{flag}, {env}, {env_file}, or {config_inline} or {config_file_key}")
}

/// Resolve a value through the ladder: CLI flag → `$VAR` → `$VAR_FILE` → config
/// inline value → config file path. File-sourced values are read and trimmed; the
/// first non-empty candidate wins.
pub fn resolve_secret(
    flag: Option<String>,
    var: &str,
    cfg_inline: Option<String>,
    cfg_file: Option<&str>,
) -> Option<String> {
    first_nonempty(secret_ladder(flag, var, cfg_inline, cfg_file).map(|(_, value)| value))
}

pub fn missing_reddit_cookie() -> String {
    format!(
        "missing Reddit cookie (set {}, or pass --no-nsfw --no-titles and disable \
         use_post_date)",
        reddit_cookie_settings()
    )
}

pub fn missing_github_token() -> String {
    format!("missing GitHub token (set {})", github_token_settings())
}

/// The Reddit cookie through [`resolve_secret`]'s ladder, skipping a candidate that
/// configures no session (see [`reddit::is_configured_cookie`]) as a blank one is skipped.
pub fn resolve_reddit_cookie(
    flag: Option<String>,
    account: Option<&RedditAccount>,
) -> Option<ConfiguredSecret> {
    first_configured_cookie(secret_ladder(
        flag,
        "REDDIT_COOKIE",
        account.and_then(|a| a.cookie.clone()),
        account.and_then(|a| a.cookie_file.as_deref()),
    ))
}

/// The GitHub token through [`resolve_secret`]'s ladder, with the setting it came from.
pub fn resolve_github_token(
    flag: Option<String>,
    account: Option<&GitHubAccount>,
) -> Option<ConfiguredSecret> {
    first_configured_token(secret_ladder(
        flag,
        "GITHUB_TOKEN",
        account.and_then(|a| a.token.clone()),
        account.and_then(|a| a.token_file.as_deref()),
    ))
}

/// The first non-blank GitHub token, with the setting it came from.
fn first_configured_token(
    candidates: impl IntoIterator<Item = (SecretRung, Option<String>)>,
) -> Option<ConfiguredSecret> {
    first_configured(candidates, is_nonblank, github_token_source)
}

/// The first candidate that configures a Reddit session, with the setting it came from.
fn first_configured_cookie(
    candidates: impl IntoIterator<Item = (SecretRung, Option<String>)>,
) -> Option<ConfiguredSecret> {
    first_configured(
        candidates,
        reddit::is_configured_cookie,
        reddit_cookie_source,
    )
}

/// The first candidate `is_configured` accepts, with the setting `source` names its rung.
fn first_configured(
    candidates: impl IntoIterator<Item = (SecretRung, Option<String>)>,
    is_configured: impl Fn(&str) -> bool,
    source: impl Fn(SecretRung) -> &'static str,
) -> Option<ConfiguredSecret> {
    candidates.into_iter().find_map(|(rung, value)| {
        let value = value.filter(|value| is_configured(value))?;
        Some(ConfiguredSecret {
            value,
            source: source(rung),
        })
    })
}

fn secret_ladder(
    flag: Option<String>,
    var: &str,
    cfg_inline: Option<String>,
    cfg_file: Option<&str>,
) -> [(SecretRung, Option<String>); 5] {
    [
        (SecretRung::Flag, flag),
        (SecretRung::Env, secret_env(var)),
        (
            SecretRung::EnvFile,
            secret_env(&format!("{var}_FILE")).and_then(|p| read_file_secret(&p)),
        ),
        (SecretRung::ConfigInline, cfg_inline),
        (SecretRung::ConfigFile, cfg_file.and_then(read_file_secret)),
    ]
}

#[cfg(not(test))]
fn secret_env(var: &str) -> Option<String> {
    std::env::var(var).ok()
}

#[cfg(test)]
thread_local! {
    static FAKE_SECRET_ENV: std::cell::RefCell<std::collections::HashMap<String, String>> =
        std::cell::RefCell::default();
}

/// Tests read [`secret_ladder`]'s environment rungs only from the fake environment a
/// [`FakeSecretEnv`] sets, so a test that builds a job or resolves a secret can't
/// pick up, or depend on, the developer's real credentials.
#[cfg(test)]
fn secret_env(var: &str) -> Option<String> {
    FAKE_SECRET_ENV.with(|env| env.borrow().get(var).cloned())
}

/// The first value among the candidates that isn't blank.
pub fn first_nonempty(candidates: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    candidates.into_iter().flatten().find(|s| is_nonblank(s))
}

fn is_nonblank(value: &str) -> bool {
    !value.trim().is_empty()
}

/// Read a file's trimmed contents, or `None` if missing or empty.
fn read_file_secret(path: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let s = s.trim().to_string();
            (!s.is_empty()).then_some(s)
        }
        // A `*_FILE` path was given but can't be read — surface it rather than
        // silently falling through to a confusing "missing X" downstream.
        Err(e) => {
            eprintln!("warning: could not read secret file {path}: {e}");
            None
        }
    }
}

pub fn resolve_pinboard_token(flag: Option<String>, pb: &PinboardConfig) -> Option<String> {
    resolve_secret(
        flag,
        "PINBOARD_TOKEN",
        pb.token.clone(),
        pb.token_file.as_deref(),
    )
}

#[cfg(test)]
/// Sets variables in the environment [`secret_env`] shows this test's thread, and
/// removes them on drop, so a setting can't outlive the test whichever thread runs the
/// next one.
#[must_use]
pub struct FakeSecretEnv {
    vars: Vec<String>,
}

#[cfg(test)]
impl FakeSecretEnv {
    pub fn set(vars: &[(&str, &str)]) -> Self {
        FAKE_SECRET_ENV.with(|env| {
            let mut env = env.borrow_mut();
            for (var, value) in vars {
                env.insert(var.to_string(), value.to_string());
            }
        });
        FakeSecretEnv {
            vars: vars.iter().map(|(var, _)| var.to_string()).collect(),
        }
    }
}

#[cfg(test)]
impl Drop for FakeSecretEnv {
    fn drop(&mut self) {
        FAKE_SECRET_ENV.with(|env| {
            let mut env = env.borrow_mut();
            for var in &self.vars {
                env.remove(var);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_dir;

    #[test]
    fn settings_lists_name_every_rung_in_ladder_order() {
        assert_eq!(
            reddit_cookie_settings(),
            "--reddit-cookie, $REDDIT_COOKIE, $REDDIT_COOKIE_FILE, or the config's `cookie` \
             or `cookie_file`"
        );
        assert_eq!(
            github_token_settings(),
            "--github-token, $GITHUB_TOKEN, $GITHUB_TOKEN_FILE, or the config's `token` or \
             `token_file`"
        );
    }

    #[test]
    fn first_nonempty_prefers_earlier_nonempty_candidates() {
        assert_eq!(
            first_nonempty([Some("a".into()), Some("b".into())]),
            Some("a".into())
        );
        assert_eq!(
            first_nonempty([None, Some(String::new()), Some("b".into())]),
            Some("b".into())
        );
        assert_eq!(first_nonempty([None, Some(String::new())]), None);
        assert_eq!(
            first_nonempty([Some(" \n".into()), Some("b".into())]),
            Some("b".into())
        );
    }

    fn first_cookie_on(ladder: [Option<&str>; 5]) -> Option<(String, &'static str)> {
        first_configured_cookie(
            SecretRung::ALL
                .into_iter()
                .zip(ladder.map(|v| v.map(String::from))),
        )
        .map(|cookie| (cookie.value, cookie.source))
    }

    #[test]
    fn reddit_cookie_skips_candidates_that_set_no_session() {
        assert_eq!(
            first_cookie_on([
                Some("reddit_session="),
                Some("=;"),
                Some(" "),
                Some("a=1; reddit_session="),
                Some("abc"),
            ]),
            Some(("abc".into(), "the config's `cookie_file`"))
        );
        assert_eq!(
            first_cookie_on([Some("reddit_session="), Some("\"\""), None, None, None]),
            None
        );
    }

    #[test]
    fn secret_ladder_tags_each_value_with_its_rung() {
        let dir = scratch_dir("secret-ladder");
        let env_file = dir.path().join("env-file");
        std::fs::write(&env_file, "from-env-file\n").unwrap();
        let config_file = dir.path().join("config-file");
        std::fs::write(&config_file, "from-config-file\n").unwrap();
        let _env = FakeSecretEnv::set(&[
            ("TEST_SECRET", "from-env"),
            ("TEST_SECRET_FILE", env_file.to_str().unwrap()),
        ]);
        let ladder = secret_ladder(
            Some("from-flag".into()),
            "TEST_SECRET",
            Some("inline".into()),
            config_file.to_str(),
        );
        assert_eq!(
            ladder,
            [
                (SecretRung::Flag, Some("from-flag".into())),
                (SecretRung::Env, Some("from-env".into())),
                (SecretRung::EnvFile, Some("from-env-file".into())),
                (SecretRung::ConfigInline, Some("inline".into())),
                (SecretRung::ConfigFile, Some("from-config-file".into())),
            ]
        );
    }

    #[test]
    fn github_token_from_its_env_file_names_that_setting() {
        let dir = scratch_dir("github-token-file");
        let token_file = dir.path().join("token");
        std::fs::write(&token_file, "from-env-file\n").unwrap();
        let _env = FakeSecretEnv::set(&[("GITHUB_TOKEN_FILE", token_file.to_str().unwrap())]);
        let token = resolve_github_token(None, None).unwrap();
        assert_eq!(
            (token.value.as_str(), token.source),
            ("from-env-file", "$GITHUB_TOKEN_FILE")
        );
    }

    #[test]
    fn reddit_cookie_names_the_setting_it_came_from() {
        assert_eq!(
            first_cookie_on([Some("abc"), None, None, Some("reddit_session=x"), None]),
            Some(("abc".into(), "--reddit-cookie"))
        );
    }

    #[test]
    fn github_token_names_the_setting_it_came_from() {
        let ladder = [
            None,
            None,
            None,
            Some(" \n".to_string()),
            Some("tok".into()),
        ];
        let from_file = first_configured_token(SecretRung::ALL.into_iter().zip(ladder)).unwrap();
        assert_eq!(
            (from_file.value.as_str(), from_file.source),
            ("tok", "the config's `token_file`")
        );
    }
}
