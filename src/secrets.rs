//! Secret resolution: the ladder every credential resolves through -- CLI flag -> `$VAR` ->
//! `$VAR_FILE` -> config inline -> config `*_file` -- and the names messages give each
//! rung, so an error can point at the setting a credential came from without quoting it.

use log::warn;

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
    #[cfg(test)]
    pub const ALL: [SecretRung; 5] = [
        SecretRung::Flag,
        SecretRung::Env,
        SecretRung::EnvFile,
        SecretRung::ConfigInline,
        SecretRung::ConfigFile,
    ];
}

/// One secret's ladder: the setting each rung reads, as messages name it, and the check a
/// candidate must pass for the ladder to take it. Every message that names where a secret
/// came from, or where to set a missing one, is built from these.
#[derive(Debug, Clone, Copy)]
pub struct SecretSpec {
    /// What the secret is, as "missing X" names it.
    what: &'static str,
    flag: &'static str,
    /// The variable the `$VAR` and `$VAR_FILE` rungs read.
    var: &'static str,
    env: &'static str,
    env_file: &'static str,
    config_inline: &'static str,
    /// `None` for a secret with no `*_file` key, which never reads that rung.
    config_file: Option<&'static str>,
    accepts: fn(&str) -> bool,
}

impl SecretSpec {
    pub const PINBOARD_TOKEN: SecretSpec = SecretSpec {
        what: "Pinboard token",
        flag: "--pinboard-token",
        var: "PINBOARD_TOKEN",
        env: "$PINBOARD_TOKEN",
        env_file: "$PINBOARD_TOKEN_FILE",
        config_inline: "`[pinboard]`'s `token`",
        config_file: Some("`[pinboard]`'s `token_file`"),
        accepts: is_nonblank,
    };
    pub const REDDIT_USERNAME: SecretSpec = SecretSpec {
        what: "Reddit username",
        flag: "--reddit-username",
        var: "REDDIT_USERNAME",
        env: "$REDDIT_USERNAME",
        env_file: "$REDDIT_USERNAME_FILE",
        config_inline: "the config's `username`",
        config_file: None,
        accepts: is_nonblank,
    };
    pub const REDDIT_COOKIE: SecretSpec = SecretSpec {
        what: "Reddit cookie",
        flag: "--reddit-cookie",
        var: "REDDIT_COOKIE",
        env: "$REDDIT_COOKIE",
        env_file: "$REDDIT_COOKIE_FILE",
        config_inline: "the config's `cookie`",
        config_file: Some("the config's `cookie_file`"),
        accepts: reddit::cookie::is_configured_cookie,
    };
    pub const GITHUB_TOKEN: SecretSpec = SecretSpec {
        what: "GitHub token",
        flag: "--github-token",
        var: "GITHUB_TOKEN",
        env: "$GITHUB_TOKEN",
        env_file: "$GITHUB_TOKEN_FILE",
        config_inline: "the config's `token`",
        config_file: Some("the config's `token_file`"),
        accepts: is_nonblank,
    };
    pub const HN_USERNAME: SecretSpec = SecretSpec {
        what: "HackerNews username",
        flag: "--hn-username",
        var: "HN_USERNAME",
        env: "$HN_USERNAME",
        env_file: "$HN_USERNAME_FILE",
        config_inline: "the config's `username`",
        config_file: None,
        accepts: is_nonblank,
    };

    /// The setting `rung` reads, as messages name it.
    pub fn source(self, rung: SecretRung) -> &'static str {
        match rung {
            SecretRung::Flag => self.flag,
            SecretRung::Env => self.env,
            SecretRung::EnvFile => self.env_file,
            SecretRung::ConfigInline => self.config_inline,
            SecretRung::ConfigFile => self.config_file.unwrap_or(self.config_inline),
        }
    }

    /// Every setting the secret can come from, in ladder order, as "A, B, C, or the
    /// config's `x` or `x_file`" for a message to follow "set". The two config rungs share
    /// one entry rather than naming the config twice.
    pub fn settings(self) -> String {
        let config = match self.config_file {
            Some(file) => {
                let (owner, _) = self.config_inline.rsplit_once(' ').unwrap_or_default();
                let file_key = file.strip_prefix(owner).unwrap_or(file).trim_start();
                format!("{} or {file_key}", self.config_inline)
            }
            None => self.config_inline.to_string(),
        };
        format!(
            "{}, {}, {}, or {config}",
            self.flag, self.env, self.env_file
        )
    }

    /// "missing X (set ...)", naming every setting the secret can come from.
    pub fn missing(self) -> String {
        format!("missing {} (set {})", self.what, self.settings())
    }

    /// Whether the ladder would take `value` for this secret rather than skip it.
    pub fn accepts(self, value: &str) -> bool {
        (self.accepts)(value)
    }

    /// The secret through the ladder -- CLI flag -> `$VAR` -> `$VAR_FILE` -> config inline
    /// value -> config file path -- with the setting it came from. Every value is trimmed;
    /// the first candidate [`Self::accepts`] takes wins.
    pub fn resolve(
        self,
        flag: Option<String>,
        cfg_inline: Option<String>,
        cfg_file: Option<&str>,
    ) -> Option<ConfiguredSecret> {
        self.first_accepted(secret_ladder(flag, self.var, cfg_inline, cfg_file))
    }

    /// The first of `candidates` the ladder takes, with the setting it came from.
    fn first_accepted(
        self,
        candidates: impl IntoIterator<Item = (SecretRung, Option<String>)>,
    ) -> Option<ConfiguredSecret> {
        candidates.into_iter().find_map(|(rung, value)| {
            let value = value?.trim().to_string();
            self.accepts(&value).then(|| ConfiguredSecret {
                value,
                source: self.source(rung),
            })
        })
    }

    /// `$VAR` or `$VAR_FILE`, whichever the ladder would take this secret from, if either.
    /// An unreadable `$VAR_FILE` is left for each account's own resolution to warn about.
    fn env_setting(self) -> Option<&'static str> {
        let read_quietly = |path: &str| try_read_file_secret(path).ok().flatten();
        let (rung, _) = secret_ladder_reading(read_quietly, None, self.var, None, None)
            .into_iter()
            .find(|(_, value)| {
                value
                    .as_deref()
                    .is_some_and(|value| self.accepts(value.trim()))
            })?;
        match rung {
            SecretRung::Env | SecretRung::EnvFile => Some(self.source(rung)),
            SecretRung::Flag | SecretRung::ConfigInline | SecretRung::ConfigFile => None,
        }
    }
}

pub fn missing_reddit_cookie() -> String {
    format!(
        "missing Reddit cookie (set {}, or pass --no-nsfw --no-titles and disable \
         use_post_date)",
        SecretSpec::REDDIT_COOKIE.settings()
    )
}

/// The Reddit cookie through its ladder, skipping a candidate that configures no session
/// (see [`reddit::cookie::is_configured_cookie`]) as a blank one is skipped.
pub fn resolve_reddit_cookie(
    flag: Option<String>,
    account: Option<&RedditAccount>,
) -> Option<ConfiguredSecret> {
    SecretSpec::REDDIT_COOKIE.resolve(
        flag,
        account.and_then(|a| a.cookie.clone()),
        account.and_then(|a| a.cookie_file.as_deref()),
    )
}

/// The GitHub token through its ladder, with the setting it came from.
pub fn resolve_github_token(
    flag: Option<String>,
    account: Option<&GitHubAccount>,
) -> Option<ConfiguredSecret> {
    SecretSpec::GITHUB_TOKEN.resolve(
        flag,
        account.and_then(|a| a.token.clone()),
        account.and_then(|a| a.token_file.as_deref()),
    )
}

fn secret_ladder(
    flag: Option<String>,
    var: &str,
    cfg_inline: Option<String>,
    cfg_file: Option<&str>,
) -> [(SecretRung, Option<String>); 5] {
    secret_ladder_reading(read_file_secret, flag, var, cfg_inline, cfg_file)
}

/// [`secret_ladder`], reading each `*_FILE` path with `read_file`.
fn secret_ladder_reading(
    read_file: fn(&str) -> Option<String>,
    flag: Option<String>,
    var: &str,
    cfg_inline: Option<String>,
    cfg_file: Option<&str>,
) -> [(SecretRung, Option<String>); 5] {
    [
        (SecretRung::Flag, flag),
        (SecretRung::Env, env_var(var)),
        (
            SecretRung::EnvFile,
            env_var(&format!("{var}_FILE")).and_then(|p| read_file(&p)),
        ),
        (SecretRung::ConfigInline, cfg_inline),
        (SecretRung::ConfigFile, cfg_file.and_then(read_file)),
    ]
}

/// Warn when an environment rung of `secrets` applies to more than one configured account
/// of `source`. The ladder puts `$VAR` and `$VAR_FILE` above every account's own value, so
/// each of those accounts silently uses the one global value. Names the settings, never
/// their values.
pub fn warn_if_env_shared(source: &str, secrets: &[SecretSpec], accounts: usize) {
    if accounts < 2 {
        return;
    }
    let shared: Vec<&str> = secrets.iter().filter_map(|s| s.env_setting()).collect();
    let (verb, pronoun) = match shared.len() {
        0 => return,
        1 => ("is", "it"),
        _ => ("are", "them"),
    };
    warn!(
        "{} {verb} set in the environment, so all {accounts} configured {source} accounts \
         use {pronoun}, whatever their own config sets",
        shared.join(", ")
    );
}

/// An environment variable the run reads: the secret rungs and the auth-failure hook.
#[cfg(not(test))]
pub(crate) fn env_var(var: &str) -> Option<String> {
    std::env::var(var).ok()
}

#[cfg(test)]
thread_local! {
    static FAKE_SECRET_ENV: std::cell::RefCell<std::collections::HashMap<String, String>> =
        std::cell::RefCell::default();
}

/// Tests read the environment only from the fake one a [`FakeSecretEnv`] sets, so a test
/// that builds a job, resolves a secret or picks a hook can't pick up, or depend on, the
/// developer's real environment.
#[cfg(test)]
pub(crate) fn env_var(var: &str) -> Option<String> {
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
    // A `*_FILE` path was given but can't be read -- surface it rather than
    // silently falling through to a confusing "missing X" downstream.
    try_read_file_secret(path).unwrap_or_else(|e| {
        warn!("could not read secret file {path}: {e}");
        None
    })
}

/// A file's trimmed contents, `None` if empty.
fn try_read_file_secret(path: &str) -> std::io::Result<Option<String>> {
    let contents = std::fs::read_to_string(path)?;
    let trimmed = contents.trim();
    Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
}

pub fn resolve_pinboard_token(flag: Option<String>, pb: &PinboardConfig) -> Option<String> {
    SecretSpec::PINBOARD_TOKEN
        .resolve(flag, pb.token.clone(), pb.token_file.as_deref())
        .map(|token| token.value)
}

#[cfg(test)]
/// Sets variables in the environment [`env_var`] shows this test's thread, and removes
/// them on drop, so a setting can't outlive the test whichever thread runs the next one.
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
            SecretSpec::REDDIT_COOKIE.settings(),
            "--reddit-cookie, $REDDIT_COOKIE, $REDDIT_COOKIE_FILE, or the config's `cookie` \
             or `cookie_file`"
        );
        assert_eq!(
            SecretSpec::GITHUB_TOKEN.settings(),
            "--github-token, $GITHUB_TOKEN, $GITHUB_TOKEN_FILE, or the config's `token` or \
             `token_file`"
        );
        assert_eq!(
            SecretSpec::PINBOARD_TOKEN.settings(),
            "--pinboard-token, $PINBOARD_TOKEN, $PINBOARD_TOKEN_FILE, or `[pinboard]`'s \
             `token` or `token_file`"
        );
        assert_eq!(
            SecretSpec::REDDIT_USERNAME.settings(),
            "--reddit-username, $REDDIT_USERNAME, $REDDIT_USERNAME_FILE, or the config's \
             `username`"
        );
        assert_eq!(
            SecretSpec::HN_USERNAME.settings(),
            "--hn-username, $HN_USERNAME, $HN_USERNAME_FILE, or the config's `username`"
        );
    }

    #[test]
    fn a_missing_secret_message_lists_its_settings() {
        assert_eq!(
            SecretSpec::GITHUB_TOKEN.missing(),
            "missing GitHub token (set --github-token, $GITHUB_TOKEN, $GITHUB_TOKEN_FILE, or \
             the config's `token` or `token_file`)"
        );
    }

    /// The environment rungs' names are spelled out for messages, so each must be the
    /// variable the ladder actually reads.
    #[test]
    fn each_secret_names_the_variables_it_reads() {
        for spec in [
            SecretSpec::PINBOARD_TOKEN,
            SecretSpec::REDDIT_USERNAME,
            SecretSpec::REDDIT_COOKIE,
            SecretSpec::GITHUB_TOKEN,
            SecretSpec::HN_USERNAME,
        ] {
            assert_eq!(spec.env, format!("${}", spec.var));
            assert_eq!(spec.env_file, format!("${}_FILE", spec.var));
        }
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
        SecretSpec::REDDIT_COOKIE
            .first_accepted(
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

    /// The shared-setting check reads `$VAR_FILE` too, but only each account's resolution
    /// may warn that it can't, or a multi-account run reports the same path once extra.
    #[tokio::test]
    async fn an_unreadable_env_file_is_warned_about_once_per_resolution() {
        let dir = scratch_dir("unreadable-env-file");
        let absent = dir.path().join("absent");
        let _env = FakeSecretEnv::set(&[("GITHUB_TOKEN_FILE", absent.to_str().unwrap())]);
        let account = GitHubAccount {
            token: Some("inline".into()),
            ..Default::default()
        };

        let (_, logs) = crate::test_support::capturing_logs(async {
            warn_if_env_shared("github", &[SecretSpec::GITHUB_TOKEN], 2);
            for _ in 0..2 {
                assert!(resolve_github_token(None, Some(&account)).is_some());
            }
        })
        .await;

        let unreadable = format!("WARN could not read secret file {}", absent.display());
        let warned: Vec<&str> = crate::test_support::warnings(&logs);
        assert_eq!(warned.len(), 2, "{warned:?}");
        assert!(
            warned.iter().all(|line| line.starts_with(&unreadable)),
            "{warned:?}"
        );
    }

    /// The environment rung a secret would come from: `$VAR` before `$VAR_FILE`, and only
    /// a value the ladder would take, so a blank one or a cookie naming no session is none.
    #[test]
    fn env_setting_names_the_env_rung_the_ladder_would_take() {
        let dir = scratch_dir("env-setting");
        let file = dir.path().join("token");
        std::fs::write(&file, "from-file\n").unwrap();
        let file = file.to_str().unwrap();
        let env_setting = |vars: &[(&str, &str)], spec: SecretSpec| {
            let _env = FakeSecretEnv::set(vars);
            spec.env_setting()
        };

        assert_eq!(env_setting(&[], SecretSpec::GITHUB_TOKEN), None);
        assert_eq!(
            env_setting(&[("GITHUB_TOKEN_FILE", file)], SecretSpec::GITHUB_TOKEN),
            Some("$GITHUB_TOKEN_FILE")
        );
        assert_eq!(
            env_setting(
                &[("GITHUB_TOKEN", "tok"), ("GITHUB_TOKEN_FILE", file)],
                SecretSpec::GITHUB_TOKEN
            ),
            Some("$GITHUB_TOKEN")
        );
        assert_eq!(
            env_setting(
                &[("GITHUB_TOKEN", " \n"), ("GITHUB_TOKEN_FILE", file)],
                SecretSpec::GITHUB_TOKEN
            ),
            Some("$GITHUB_TOKEN_FILE"),
            "a blank $GITHUB_TOKEN is skipped"
        );
        assert_eq!(
            env_setting(
                &[("REDDIT_COOKIE", "reddit_session=")],
                SecretSpec::REDDIT_COOKIE
            ),
            None,
            "a cookie that sets no session is no cookie"
        );
    }

    /// A pasted value often carries a newline or a stray space; every rung drops it, as the
    /// `*_FILE` rungs always did, so the value sent is the one configured.
    #[test]
    fn every_rung_takes_its_value_trimmed() {
        let env = FakeSecretEnv::set(&[("GITHUB_TOKEN", " from-env\n")]);
        let value = |token: Option<ConfiguredSecret>| token.map(|token| token.value);
        assert_eq!(
            value(resolve_github_token(Some("\tfrom-flag \n".into()), None)).as_deref(),
            Some("from-flag")
        );
        assert_eq!(
            value(resolve_github_token(None, None)).as_deref(),
            Some("from-env")
        );
        drop(env);
        let account = GitHubAccount {
            token: Some(" inline\r\n".into()),
            ..Default::default()
        };
        assert_eq!(
            value(resolve_github_token(None, Some(&account))).as_deref(),
            Some("inline")
        );
        assert_eq!(
            first_cookie_on([Some(" reddit_session=abc\n"), None, None, None, None]),
            Some(("reddit_session=abc".into(), "--reddit-cookie"))
        );
        assert_eq!(
            resolve_pinboard_token(Some("user:tok \n".into()), &PinboardConfig::default())
                .as_deref(),
            Some("user:tok")
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
        let from_file = SecretSpec::GITHUB_TOKEN
            .first_accepted(SecretRung::ALL.into_iter().zip(ladder))
            .unwrap();
        assert_eq!(
            (from_file.value.as_str(), from_file.source),
            ("tok", "the config's `token_file`")
        );
    }
}
