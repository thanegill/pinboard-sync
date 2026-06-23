//! The `--config <path>` TOML: one Pinboard destination, an optional auth-failure
//! hook, and per-source arrays of accounts. Each account holds non-secret settings
//! (tags, domain, limit) plus secret values inline or as a `*_file` path. Secrets
//! resolve through the ladder in `main`; this module is the typed schema + the
//! account → typed-config mapping. Every field is optional, so an absent or empty
//! config yields all defaults.

use std::collections::HashSet;

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;

use crate::github::GithubConfig;
use crate::hackernews::HackernewsConfig;
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
    #[serde(default)]
    pub github: Vec<GithubAccount>,
    #[serde(default)]
    pub hackernews: Vec<HackernewsAccount>,
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
    pub tag_subreddit_prefix: Option<String>,
    pub tag_comment: Option<String>,
    pub tag_nsfw: Option<String>,
    pub tag_author_prefix: Option<String>,
    pub tag_flair_prefix: Option<String>,
    pub tag_media_prefix: Option<String>,
    pub tag_media_types: Option<Vec<String>>,
    /// Tags applied to every bookmark (default `["reddit"]`); a full override.
    pub tags: Option<Vec<String>>,
}

impl Config {
    /// Whether any account of any source is configured.
    pub fn has_accounts(&self) -> bool {
        !(self.reddit.is_empty() && self.github.is_empty() && self.hackernews.is_empty())
    }

    /// Parse a config from TOML text, validating tag fields.
    pub fn parse(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text).map_err(|e| anyhow!("parsing config: {e}"))?;
        config.validate()?;
        Ok(config)
    }

    /// Reject tag values containing whitespace — Pinboard tags can't contain spaces
    /// (its API splits the tag string on them), so a space here is silently
    /// corrupting and should fail loudly instead.
    fn validate(&self) -> Result<()> {
        check_unique_names("reddit", &self.reddit)?;
        check_unique_names("github", &self.github)?;
        check_unique_names("hackernews", &self.hackernews)?;

        for a in &self.reddit {
            check_domain("reddit.reddit_domain", &a.reddit_domain)?;
            check_tags("reddit.tags", &a.tags)?;
            check_tag("reddit.tag_subreddit_prefix", &a.tag_subreddit_prefix)?;
            check_tag("reddit.tag_comment", &a.tag_comment)?;
            check_tag("reddit.tag_nsfw", &a.tag_nsfw)?;
            check_tag("reddit.tag_author_prefix", &a.tag_author_prefix)?;
            check_tag("reddit.tag_flair_prefix", &a.tag_flair_prefix)?;
            check_tag("reddit.tag_media_prefix", &a.tag_media_prefix)?;
        }
        for a in &self.github {
            check_tags("github.tags", &a.tags)?;
            check_tag("github.tag_lang_prefix", &a.tag_lang_prefix)?;
        }
        for a in &self.hackernews {
            check_tags("hackernews.tags", &a.tags)?;
            check_tag("hackernews.tag_comment", &a.tag_comment)?;
            check_tag("hackernews.tag_author_prefix", &a.tag_author_prefix)?;
            check_tag("hackernews.tag_special_prefix", &a.tag_special_prefix)?;
            check_tag("hackernews.tag_link", &a.tag_link)?;
        }
        Ok(())
    }
}

/// Error if `value` (a tag or tag prefix) contains whitespace — Pinboard tags can't
/// contain spaces (its API splits the tag string on them), so a space here silently
/// corrupts and should fail loudly instead.
fn reject_whitespace(field: &str, value: &str) -> Result<()> {
    if value.chars().any(char::is_whitespace) {
        bail!("config: `{field}` must not contain whitespace (got {value:?})");
    }
    Ok(())
}

/// Error if an optional tag/prefix contains whitespace.
fn check_tag(field: &str, value: &Option<String>) -> Result<()> {
    value
        .as_deref()
        .map_or(Ok(()), |v| reject_whitespace(field, v))
}

/// Error if any tag in the list contains whitespace.
fn check_tags(field: &str, values: &Option<Vec<String>>) -> Result<()> {
    for v in values.iter().flatten() {
        reject_whitespace(field, v)?;
    }
    Ok(())
}

/// Error if two accounts of a source share a `name` (the second would be
/// unreachable by name in `sync <source> <name>`).
fn check_unique_names<T: Named>(source: &str, accounts: &[T]) -> Result<()> {
    let mut seen = HashSet::new();
    for account in accounts {
        if let Some(name) = account.account_name() {
            if !seen.insert(name) {
                bail!("config: duplicate {source} account name {name:?}");
            }
        }
    }
    Ok(())
}

/// Error if `value` isn't a bare host (it's interpolated as `https://<domain>…`, so
/// a scheme, slash, or whitespace would corrupt the bookmark URLs).
fn check_domain(field: &str, value: &Option<String>) -> Result<()> {
    if let Some(v) = value {
        if v.is_empty() || v.contains('/') || v.chars().any(char::is_whitespace) {
            bail!("config: `{field}` must be a bare host like \"old.reddit.com\" (got {v:?})");
        }
    }
    Ok(())
}

impl RedditAccount {
    /// Build the non-secret [`RedditConfig`] (bookmark domain + tag vocabulary),
    /// each field falling back to its built-in default.
    pub fn reddit_config(&self) -> RedditConfig {
        let d = RedditConfig::default();
        RedditConfig {
            domain: self.reddit_domain.clone().unwrap_or(d.domain),
            tags: self.tags.clone().unwrap_or(d.tags),
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
        }
    }
}

/// One GitHub account: a personal access token plus the (non-secret) tag config.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubAccount {
    pub name: Option<String>,
    pub token: Option<String>,
    pub token_file: Option<String>,
    pub on_auth_failure: Option<String>,
    pub limit: Option<usize>,
    pub tag_lang_prefix: Option<String>,
    /// Tags applied to every bookmark (default `["github-star"]`); a full override.
    pub tags: Option<Vec<String>>,
}

impl GithubAccount {
    /// Build the non-secret [`GithubConfig`] (tag vocabulary), each field falling
    /// back to its built-in default.
    pub fn github_config(&self) -> GithubConfig {
        let d = GithubConfig::default();
        GithubConfig {
            tags: self.tags.clone().unwrap_or(d.tags),
            lang_prefix: self.tag_lang_prefix.clone().unwrap_or(d.lang_prefix),
        }
    }
}

/// One HackerNews account: a public username plus the (non-secret) tag config.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HackernewsAccount {
    pub name: Option<String>,
    pub username: Option<String>,
    pub limit: Option<usize>,
    pub tag_comment: Option<String>,
    pub tag_author_prefix: Option<String>,
    pub tag_special_prefix: Option<String>,
    pub tag_link: Option<String>,
    /// Tags applied to every bookmark (default `["hackernews"]`); a full override.
    pub tags: Option<Vec<String>>,
}

impl HackernewsAccount {
    /// Build the non-secret [`HackernewsConfig`] (tag vocabulary), each field
    /// falling back to its built-in default.
    pub fn hackernews_config(&self) -> HackernewsConfig {
        let d = HackernewsConfig::default();
        HackernewsConfig {
            tags: self.tags.clone().unwrap_or(d.tags),
            comment: self.tag_comment.clone().unwrap_or(d.comment),
            author_prefix: self.tag_author_prefix.clone().unwrap_or(d.author_prefix),
            special_prefix: self.tag_special_prefix.clone().unwrap_or(d.special_prefix),
            link_tag: self.tag_link.clone().unwrap_or(d.link_tag),
        }
    }
}

/// An account that can be selected by name (`sync <source> <name>`). The selector is
/// the explicit `name`, falling back to the account's `username` where it has one.
pub trait Named {
    fn account_name(&self) -> Option<&str>;
}

/// Implement [`Named`] as `self.name`, optionally falling back to another field
/// (`=> username`) when `name` is unset.
macro_rules! impl_named {
    ($($t:ty $(=> $fallback:ident)?),+ $(,)?) => {
        $(impl Named for $t {
            fn account_name(&self) -> Option<&str> {
                self.name.as_deref()
                $(.or(self.$fallback.as_deref()))?
            }
        })+
    };
}
impl_named!(RedditAccount => username, GithubAccount, HackernewsAccount => username);

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
            tag_media_types = ["image"]
            tags = ["reddit", "account:main"]

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
        assert_eq!(rc.tags, vec!["reddit", "account:main"]);
        assert_eq!(rc.media_types, vec!["image"]);
        // Unset tag fields keep their defaults; an unset `tags` defaults to ["reddit"].
        assert_eq!(rc.subreddit_prefix, "subreddit:");
        assert_eq!(cfg.reddit[1].reddit_config().tags, vec!["reddit"]);
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(Config::parse("[[reddit]]\nnonsense = 1").is_err());
    }

    #[test]
    fn rejects_whitespace_in_tags() {
        assert!(Config::parse("[[reddit]]\ntags = [\"a b\"]").is_err());
        assert!(Config::parse("[[github]]\ntag_lang_prefix = \"lang :\"").is_err());
        assert!(Config::parse("[[hackernews]]\ntag_link = \"find hn\"").is_err());
        // No whitespace → fine (hyphens/colons allowed).
        assert!(Config::parse("[[reddit]]\ntags = [\"reddit\", \"a-b\"]").is_ok());
    }

    #[test]
    fn rejects_duplicate_account_names() {
        let dup = r#"
            [[reddit]]
            name = "main"
            [[reddit]]
            name = "main"
        "#;
        assert!(Config::parse(dup).is_err());
        // Distinct names (and unnamed accounts) are fine.
        let ok = r#"
            [[reddit]]
            name = "main"
            [[reddit]]
            name = "alt"
            [[reddit]]
        "#;
        assert!(Config::parse(ok).is_ok());
    }

    #[test]
    fn rejects_malformed_reddit_domain() {
        assert!(Config::parse("[[reddit]]\nreddit_domain = \"https://old.reddit.com\"").is_err());
        assert!(Config::parse("[[reddit]]\nreddit_domain = \"old.reddit.com/\"").is_err());
        assert!(Config::parse("[[reddit]]\nreddit_domain = \"\"").is_err());
        assert!(Config::parse("[[reddit]]\nreddit_domain = \"www.reddit.com\"").is_ok());
    }

    #[test]
    fn shipped_example_config_parses() {
        // The `config example` template must stay a valid Config.
        let cfg = Config::parse(include_str!("config.example.toml")).unwrap();
        assert_eq!(cfg.reddit.len(), 1);
        assert_eq!(cfg.github.len(), 1);
        assert_eq!(cfg.hackernews.len(), 1);
    }

    /// True if `key` appears as a TOML key in the template — either active
    /// (`key = …`) or commented out (`# key = …`).
    fn documents_key(example: &str, key: &str) -> bool {
        example.lines().any(|line| {
            let line = line.trim_start();
            let line = line.strip_prefix('#').map(str::trim_start).unwrap_or(line);
            line.strip_prefix(key)
                .map(str::trim_start)
                .is_some_and(|rest| rest.starts_with('='))
        })
    }

    #[test]
    fn example_config_documents_every_field() {
        // The macro destructures each struct with no `..`, so adding a field is a
        // *compile* error until it's listed here — and `stringify!` then derives the
        // key list from that same list, so the new key is checked against the
        // template below. Net effect: a new config field fails the build until it is
        // documented in config.example.toml (commented-out lines count).
        macro_rules! documented_fields {
            ($ty:path { $($field:ident),* $(,)? }) => {{
                let $ty { $($field: _),* } = <$ty>::default();
                [$(stringify!($field)),*]
            }};
        }

        let example = include_str!("config.example.toml");
        let hooks = documented_fields!(Hooks { on_auth_failure });
        let pinboard = documented_fields!(Pinboard {
            token,
            token_file,
            public
        });
        let reddit = documented_fields!(RedditAccount {
            name,
            username,
            cookie,
            cookie_file,
            on_auth_failure,
            limit,
            reddit_domain,
            tag_subreddit_prefix,
            tag_comment,
            tag_nsfw,
            tag_author_prefix,
            tag_flair_prefix,
            tag_media_prefix,
            tag_media_types,
            tags,
        });
        let github = documented_fields!(GithubAccount {
            name,
            token,
            token_file,
            on_auth_failure,
            limit,
            tag_lang_prefix,
            tags,
        });
        let hackernews = documented_fields!(HackernewsAccount {
            name,
            username,
            limit,
            tag_comment,
            tag_author_prefix,
            tag_special_prefix,
            tag_link,
            tags,
        });

        for &key in hooks
            .iter()
            .chain(&pinboard)
            .chain(&reddit)
            .chain(&github)
            .chain(&hackernews)
        {
            assert!(
                documents_key(example, key),
                "config.example.toml does not document the `{key}` config field"
            );
        }
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

    #[test]
    fn account_name_falls_back_to_username() {
        // Reddit/HN: an unnamed account is selectable by its username; an explicit
        // name still wins.
        let by_username = RedditAccount {
            username: Some("alice".into()),
            ..Default::default()
        };
        assert_eq!(by_username.account_name(), Some("alice"));
        let named = RedditAccount {
            name: Some("main".into()),
            username: Some("alice".into()),
            ..Default::default()
        };
        assert_eq!(named.account_name(), Some("main"));
        // GitHub has no username, so only an explicit name selects it.
        assert_eq!(
            GithubAccount {
                token: Some("ghp_x".into()),
                ..Default::default()
            }
            .account_name(),
            None
        );
    }
}
