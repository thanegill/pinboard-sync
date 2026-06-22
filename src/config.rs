//! The `--config <path>` TOML: one Pinboard destination, an optional auth-failure
//! hook, and per-source arrays of accounts. Each account holds non-secret settings
//! (tags, domain, limit) plus secret values inline or as a `*_file` path. Secrets
//! resolve through the ladder in `main`; this module is the typed schema + the
//! account → typed-config mapping. Every field is optional, so an absent or empty
//! config yields all defaults.

use anyhow::{anyhow, Result};
use serde::Deserialize;

use crate::model::RedditConfig;

/// The parsed config file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub hooks: Hooks,
    #[serde(default)]
    pub pinboard: Pinboard,
    #[serde(default)]
    pub reddit: Vec<RedditAccount>,
}

/// Cross-cutting hooks.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    /// Shell command run on a re-auth failure (per-account override available).
    pub on_auth_failure: Option<String>,
}

/// The shared Pinboard destination.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pinboard {
    pub token: Option<String>,
    pub token_file: Option<String>,
    /// Write bookmarks public (default private).
    #[serde(default)]
    pub public: bool,
}

/// One reddit account: whose saves to read, the session cookie, and the
/// (non-secret) tag/domain vocabulary.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedditAccount {
    pub name: Option<String>,
    pub username: Option<String>,
    pub cookie: Option<String>,
    pub cookie_file: Option<String>,
    pub on_auth_failure: Option<String>,
    pub limit: Option<usize>,
    // Non-secret tag/domain config (`tag_*`), each defaulting in `reddit_config`.
    pub reddit_domain: Option<String>,
    pub tag_base: Option<String>,
    pub tag_subreddit_prefix: Option<String>,
    pub tag_comment: Option<String>,
    pub tag_nsfw: Option<String>,
    pub tag_author_prefix: Option<String>,
    pub tag_flair_prefix: Option<String>,
    pub tag_media_prefix: Option<String>,
    pub tag_media_types: Option<Vec<String>>,
    /// Extra tags appended to every bookmark from this account.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl Config {
    /// Parse a config from TOML text.
    pub fn parse(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| anyhow!("parsing config: {e}"))
    }
}

impl RedditAccount {
    /// Build the non-secret [`RedditConfig`] (bookmark domain + tag vocabulary),
    /// each field falling back to its built-in default.
    pub fn reddit_config(&self) -> RedditConfig {
        let d = RedditConfig::default();
        RedditConfig {
            domain: self.reddit_domain.clone().unwrap_or(d.domain),
            base: self.tag_base.clone().unwrap_or(d.base),
            subreddit_prefix: self
                .tag_subreddit_prefix
                .clone()
                .unwrap_or(d.subreddit_prefix),
            comment: self.tag_comment.clone().unwrap_or(d.comment),
            nsfw: self.tag_nsfw.clone().unwrap_or(d.nsfw),
            author_prefix: self.tag_author_prefix.clone().unwrap_or(d.author_prefix),
            flair_prefix: self.tag_flair_prefix.clone().unwrap_or(d.flair_prefix),
            media_prefix: self.tag_media_prefix.clone().unwrap_or(d.media_prefix),
            media_types: self.tag_media_types.clone().unwrap_or(d.media_types),
            extra: self.tags.clone(),
        }
    }
}

/// An account that can be selected by `name`.
pub trait Named {
    fn account_name(&self) -> Option<&str>;
}

impl Named for RedditAccount {
    fn account_name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

/// Pick the account named `name`, or the first account when `name` is `None`.
/// Returns `Ok(None)` only when there are no configured accounts and no name was
/// given (the caller then falls back to CLI flags / env for a single account).
pub fn select_account<'a, T: Named>(
    accounts: &'a [T],
    name: Option<&str>,
) -> Result<Option<&'a T>> {
    match name {
        Some(n) => accounts
            .iter()
            .find(|a| a.account_name() == Some(n))
            .map(Some)
            .ok_or_else(|| anyhow!("no account named '{n}' configured")),
        None => Ok(accounts.first()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_all_defaults() {
        let cfg = Config::parse("").unwrap();
        assert!(cfg.reddit.is_empty());
        assert!(cfg.pinboard.token.is_none());
        assert!(!cfg.pinboard.public);
        assert!(cfg.hooks.on_auth_failure.is_none());
    }

    #[test]
    fn parses_hooks_pinboard_and_multiple_reddit_accounts() {
        let cfg = Config::parse(
            r#"
            [hooks]
            on_auth_failure = "notify"

            [pinboard]
            token = "user:TOK"
            public = true

            [[reddit]]
            name = "main"
            username = "alice"
            cookie_file = "/run/secrets/cookie"
            reddit_domain = "www.reddit.com"
            tag_base = "rdt"
            tag_media_types = ["image"]
            tags = ["account:main"]

            [[reddit]]
            name = "alt"
            username = "bob"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.hooks.on_auth_failure.as_deref(), Some("notify"));
        assert_eq!(cfg.pinboard.token.as_deref(), Some("user:TOK"));
        assert!(cfg.pinboard.public);
        assert_eq!(cfg.reddit.len(), 2);

        let main = &cfg.reddit[0];
        assert_eq!(main.username.as_deref(), Some("alice"));
        let rc = main.reddit_config();
        assert_eq!(rc.domain, "www.reddit.com");
        assert_eq!(rc.base, "rdt");
        assert_eq!(rc.media_types, vec!["image"]);
        assert_eq!(rc.extra, vec!["account:main"]);
        // Unset tag fields keep their defaults.
        assert_eq!(rc.subreddit_prefix, "subreddit:");
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(Config::parse("[[reddit]]\nnonsense = 1").is_err());
    }

    #[test]
    fn select_account_by_name_or_first_or_error() {
        let accounts = vec![
            RedditAccount {
                name: Some("a".into()),
                ..Default::default()
            },
            RedditAccount {
                name: Some("b".into()),
                ..Default::default()
            },
        ];
        assert_eq!(
            select_account(&accounts, None)
                .unwrap()
                .unwrap()
                .name
                .as_deref(),
            Some("a")
        );
        assert_eq!(
            select_account(&accounts, Some("b"))
                .unwrap()
                .unwrap()
                .name
                .as_deref(),
            Some("b")
        );
        assert!(select_account(&accounts, Some("missing")).is_err());
        // No accounts + no name → None (caller uses flags/env).
        let empty: Vec<RedditAccount> = vec![];
        assert!(select_account(&empty, None).unwrap().is_none());
    }
}
